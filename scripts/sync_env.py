"""把本地 .env 同步到最新模板版式，保留其中已填的真实值（含密钥）。

痛点：.env 常从 .env.example 整份手抄，继承了全套 tier 分段与注释；而 .env.example
由 settings.py 元数据自动生成，字段会在 Tier 间迁移、注释会改写、总览会变。两者一旦
重排就漂移。但运行时只有未注释的 KEY=VALUE 生效，注释纯属装饰——于是让模板做版式、
.env 的真实值做数据，合并即可：版式永远跟模板，密钥永不丢。

用法:
    python scripts/sync_env.py                 # 同步仓库根 .env（service 模板）
    python scripts/sync_env.py --mode personal --path ~/.oce/data/.env
    python scripts/sync_env.py --diff          # 脱敏预览差异，不写入
    python scripts/sync_env.py --check         # 只比对，不一致退出码 1（CI 防漂移）
"""

from __future__ import annotations

import argparse
import difflib
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "src"))

from dotenv import dotenv_values  # noqa: E402

from oce.shared.config.env_template import render_env_template  # noqa: E402

DEFAULT_ENV = REPO_ROOT / ".env"
_BAR = "=" * 24

# tier 段里的字段行：折叠 `# KEY=val`（井号后正好一个空格，区别于总览的 `#   KEY=`）
# 与展开 `KEY=val`（tier1）。两类的 KEY 都用于回填真实值。
_FOLD_RE = re.compile(r"^# ([A-Za-z_][A-Za-z0-9_]*)=(.*)$")
_EXPAND_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$")


def _field(line: str) -> re.Match | None:
    return _FOLD_RE.match(line) or _EXPAND_RE.match(line)


def _fmt(value: str) -> str:
    """回填值：含空格/#/引号/$ 等时加双引号，避免 dotenv 误读行内注释或变量插值。"""
    if value == "" or not re.search(r"[\s#\"'`$]", value):
        return value
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _load_active(path: Path) -> dict[str, str]:
    """读出 .env 里未注释的 KEY=VALUE（dotenv 解析，正确处理引号/export/行内注释）。"""
    return {k: v for k, v in dotenv_values(path).items() if v is not None}


def render_synced(active: dict[str, str], mode: str) -> str:
    """以最新模板为骨架，把 active 真实值回填进对应字段行；孤儿键追加到末尾。"""
    out: list[str] = []
    seen: set[str] = set()
    for line in render_env_template(mode).splitlines():
        m = _field(line)
        if not m:
            out.append(line)  # 注释 / 分段头 / 总览 / 空行：原样照搬
            continue
        key, tpl_value = m.group(1), m.group(2)
        seen.add(key)
        # active 与否完全由 .env 决定，sync 只改版式不改语义：命中则展开回填真实值，
        # 否则注释掉（用模板示例值，不含密钥）。绝不照搬模板的 tier1 展开占位，
        # 以免把 replace-with-... 之类占位符误激活成真实配置、悄悄改变运行时行为。
        out.append(f"{key}={_fmt(active[key])}" if key in active else f"# {key}={tpl_value}")

    orphans = [k for k in active if k not in seen]
    if orphans:
        out += [
            "",
            f"# {_BAR} 模板未覆盖（手动保留） {_BAR}",
            "# 模板已不含这些键（多为废弃项），保留你的设置以免静默丢失：",
            *(f"{k}={_fmt(active[k])}" for k in orphans),
        ]
    return "\n".join(out).rstrip() + "\n"


def _mask(line: str) -> str:
    """脱敏：字段行的值替换为占位，只暴露 KEY 与版式变化，绝不泄露密钥。"""
    m = _field(line)
    if not m:
        return line
    prefix = "# " if line.startswith("#") else ""
    return f"{prefix}{m.group(1)}={'***' if m.group(2).strip() else ''}"


def main() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8")  # Windows PowerShell 下强制 UTF-8
    except AttributeError:
        pass
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--path", default=str(DEFAULT_ENV), help="目标 .env（默认仓库根 .env）")
    parser.add_argument(
        "--mode", choices=["service", "personal"], default="service",
        help="模板模式；个人模式 .env（~/.oce/data）用 personal",
    )
    parser.add_argument("--check", action="store_true", help="只比对不写入，不一致退出码 1")
    parser.add_argument("--diff", action="store_true", help="脱敏预览差异，不写入")
    args = parser.parse_args()

    path = Path(args.path).expanduser().resolve()
    if not path.is_file():
        sys.exit(f"env file not found: {path}\n先 `oce init` 或从 .env.example 复制一份再同步。")

    active = _load_active(path)
    rendered = render_synced(active, args.mode)
    current = path.read_text(encoding="utf-8")

    if args.diff:
        diff = list(
            difflib.unified_diff(
                [_mask(x) for x in current.splitlines()],
                [_mask(x) for x in rendered.splitlines()],
                fromfile=f"{path.name} (current)",
                tofile=f"{path.name} (synced)",
                lineterm="",
            )
        )
        print("\n".join(diff) if diff else f"{path.name} 已是最新，无需同步。")
        return

    if current == rendered:
        print(f"{path.name} is up to date.")
        return
    if args.check:
        print(f"{path.name} is out of date; run: python scripts/sync_env.py")
        sys.exit(1)
    path.write_text(rendered, encoding="utf-8")
    print(f"Synced {path} ({len(active)} active keys, {args.mode} template).")


if __name__ == "__main__":
    main()
