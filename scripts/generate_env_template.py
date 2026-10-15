"""重新生成 .env.example（服务模式模板）。

用法:
    python scripts/generate_env_template.py [--check]

- 默认写入仓库根的 .env.example。
- --check：只比对，若与现有文件不一致则以非零码退出（供 CI 防止模板漂移）。

模板内容由 src/oce/shared/config/env_template.py 依据 Settings 元数据生成，
settings.py 是唯一真源；本脚本只是薄包装。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "src"))

from oce.shared.config.env_template import render_env_template  # noqa: E402

TARGET = REPO_ROOT / ".env.example"


def _stdout() -> None:
    """Windows PowerShell 下强制 UTF-8 输出。"""
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except AttributeError:
        pass


def main() -> None:
    _stdout()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="只比对不写入；不一致则退出码 1（CI 防漂移）",
    )
    args = parser.parse_args()

    rendered = render_env_template(mode="service")

    if args.check:
        current = TARGET.read_text(encoding="utf-8") if TARGET.is_file() else ""
        if current != rendered:
            print(f"{TARGET.name} is out of date; run: python scripts/generate_env_template.py")
            sys.exit(1)
        print(f"{TARGET.name} is up to date.")
        return

    TARGET.write_text(rendered, encoding="utf-8")
    print(f"Generated {TARGET}")


if __name__ == "__main__":
    main()
