Status: implemented (2026-10-06，18 步全部执行完毕)

## Outcome

- **吸收 13 项**（含显著正收益 1 项：图扩展召回 +7.71/+10.05）、**回退/默认关 3 项**
  （边际覆盖选择器、候选名额保留、bundle 聚合），另有 2 项正确性修复（切块器缺陷、
  切块完整性校验）与 3 项评测侧建设（线级答案键、冻结清单、机器审计）。
- 所有新开关**默认关**：图扩展是唯一被证实有显著正收益的（默认关的理由是延迟预算
  6-7×，需单独拍板改默认）；其余为负结果留档。
- 双仓最终复测（默认档）：flask 99.52 / cc-switch 65.77，与 STEP-3 基线 **+0.00**、
  `invalid=0` —— 默认关对既有行为零影响。
- 实测明细见执行记录表与 `.agents/notes/proposed/2026-10-06-npm-open-context-engine-borrow-analysis.md`
  第七节「实测回填」。

# 借鉴吸收实施计划：npm open-context-engine（Rust 单实现）

Goal: 把 2026-10-06 分析里 9 项借鉴点在 **Rust 实现**上落地，每项独立可验收；影响排序/候选集的
改动必须在 oce-benchmark 的 nollm 档双仓 A/B 上无回归，否则回退该项。

Approach: 先补齐证据链（基准报告记录配置、逐行保真、可审计基线、graph 专用 query set），再做
防御性修复（rerank 校验、降级可观测），然后按「选择器 → 元数据 → bundle → 索引身份 → 新鲜度」
推进排序相关改动，最后做评测仓的线级答案键与冻结清单；图扩展排在最后，且**必须由 graph 专用
query set 的 headroom 结论放行**。

Requirements（本轮契约，逐条可判定）：
| ID | 需求 |
|---|---|
| REQ-1 | 给定 API rerank 通道返回畸形响应（index 缺失/重复/越界、score 非数/非有限/越界），检索**要么整请求报错、要么降级并在 audit 中可查**，绝不静默丢弃候选。 |
| REQ-2 | 给定 LLM rerank 失败或部分返回，检索仍完成，且降级事实（失败/补齐条数）出现在 audit 与 metrics 中。 |
| REQ-3 | 给定多 facet 查询，选择结果在字符预算内按边际覆盖增益挑选，同一输入**确定性**可复现。 |
| REQ-4 | 给定图/精确召回候选，它们在 rerank 前不被 `default_top_k` 平截，而是按固定名额进入候选池。 |
| REQ-5 | 给定同一符号的多个 chunk，选择时可按符号（bundle）整体取舍，且补入的上下文只按新增字符计价。 |
| REQ-6 | 给定任意 chunk 结果，满足「text 逐字等于源切片、非空行全覆盖、span 不重叠」，不满足则索引失败而非静默入库。 |
| REQ-7 | 给定 chunker/解析器版本或嵌入模型变化，既有索引 **fail-closed** 并给出重建提示，不混用旧向量。 |
| REQ-8 | 给定范围内存在 pending/error blob，检索响应暴露 `pending/failed/ready/last_error_type`，且不把 pending 的缺失当正常空结果。 |
| REQ-9 | 给定一次基准运行，报告可回答「被测二进制 commit、配置档、query set」，不依赖文件名猜测。 |
| REQ-10 | 给定返回文本，脚本能判定每一行是否等于该文件该物理行的原文，并给出 `invalid_line_count`。 |
| REQ-11 | 给定线级答案文件，脚本能给出必需证据覆盖率与完整证据题数，且答案引用在评分前逐条校验 sha256。 |

Decision record: `.agents/notes/proposed/2026-10-06-npm-open-context-engine-borrow-analysis.md`
（吸收/不吸收清单与理由）。相关既有裁决：
`.agents/notes/proposed/2026-09-21-structural-code-intelligence-rfc-analysis.md`（图扩展先建 query set、四约束）、
`.agents/notes/implemented/architecture/2026-09-12-bce-retrieval-quality-borrow.md`（nollm/full 协议、full 档噪声 ±8）。

Rollback point: STEP-1..STEP-4 只动 oce-benchmark，无服务端影响，可随时丢弃。STEP-5 起每步一个
独立 commit，回退 = 回退该 commit（无 schema 迁移、无不可逆存储变更）。**STEP-12 之后**索引指纹
收紧会让旧索引 fail-closed：从这一步起回退是「回退 commit 并用旧二进制启动该数据目录」，或按提示
用新二进制重建索引；两条路都在同一数据目录内完成。不要在 STEP-12 与 STEP-13 之间停下：
指纹已变而新鲜度字段未落地时，客户端拿不到「为什么少了结果」的解释。

## Global constraints

- **CON-1** 只改 Rust 实现（用户裁决 2026-10-06）。Python 版 `src/oce/**` 本轮一行不改；差距记入
  decision record 的「非目标」，另行排期。
- **CON-2** 命令目录：Rust 侧一律从 `rust/` 运行；评测侧一律从 `oce-benchmark/` 运行。
- **CON-3** 分级验收：防御性改动（不改排序与候选集）只跑单测；影响排序/候选集的改动必须跑
  nollm 档双仓 A/B；触碰 LLM rerank 降级路径的另加 full 档双跑。
- **CON-4** A/B 每臂使用**独立临时数据目录**（`/tmp/oce-ab-<arm>`），不得复用另一臂的索引；
  报告写入 `oce-benchmark/results/`，文件名含臂名与档位。
- **CON-5** nollm 档 = 静态嵌入 + LLM 全关：
  `EMBED_STATIC_MODEL=minishlab/potion-multilingual-128M`、`RERANK_ENABLED=false`、
  `LLM_RERANK_ENABLED=false`、`RETRIEVAL_INTENT_CLASSIFICATION_ENABLED=false`、
  `RETRIEVAL_QUERY_REWRITE_ENABLED=false`。
- **CON-6** 新开关**默认关**，A/B 通过后再单独决定是否改默认（2026-09-21 Note：结构信号必须从
  关/低权重起调）。命名沿用 `RETRIEVAL_*` 前缀，Rust 侧同时登记在
  `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`。
- **CON-7** 不新增 crate 依赖（`rust/Cargo.toml` 不变）；需要的能力用现有 `serde_json`/`regex`/`rayon` 完成。
- **CON-8** 平台为 Linux/macOS；不引入平台专属锁与文件 API。
- **CON-9** 行为不变式（既有测试守着）：`READY` 必须意味着可被检索；行号永不撒谎；`Path:` 行是
  评测唯一路径信号源，`<related_symbols>` 等附加块不得改变 `Path:` 行集合。
- **CON-10** 新增的纯函数（如 `verify_lines`）必须在模块导入期无副作用，便于命令行直接断言。

## Non-goals

- **Python 实现同步**：记入 decision record，另行排期（CON-1）。
- **图扩展的排序注入**：除非 STEP-4 的 headroom 结论为正且 STEP-14 的 hints 实验通过；否则只做 hints。
- **oce-client / MCP 工具契约**：属另一个仓库，本轮不触碰。
- **旧索引数据迁移工具**：STEP-12 只做 fail-closed + 重建提示，不写迁移器。
- **CI 增加 Rust 测试任务**：见「Findings」，需要单独决定（改的是 `.github/workflows/ci.yml`）。
- **token 预算与多引擎对照**：STEP-16..18 只做线级证据与冻结，不引入 tokenizer、不做跨引擎表。

## Assumptions

- **A-1** nollm 档是确定性的，同配置重跑的噪声带 ≤ ±2 分（BCE Note 2026-09-12 先例）。
  受影响：STEP-3 的基线可信度、Checkpoint A/B/C 的阈值。若为假：阈值改为「双跑取中位」，
  STEP-3 重跑一次并在执行记录里写两个数。
- **A-2** `minishlab/potion-multilingual-128M` 已在本机模型缓存中，nollm 档离线可跑。
  受影响：STEP-3、Checkpoint A/B/C。若为假：这些步骤前各加一次联网预取（一次性），A/B 本身仍离线。
- **A-3** `oce-benchmark/repos/flask` 与 `oce-benchmark/repos/cc-switch` 已按 pinned commit 检出。
  受影响：所有基准步骤。若为假：先 `git clone` 并 checkout metadata 里的 commit，
  `uv run python scripts/verify_benchmark_lock.py --metadata benchmarks/<name>.metadata.json --repo-root repos/<name>`
  退出码为 0 后再继续。
- **A-4** 静态嵌入下 flask 基准能在本机一次跑完（历史报告 `regress-qcov-flask-0929.md` 有完整数据）。
  受影响：STEP-3。若为假：先只跑 cc-switch，flask 换机器空闲时补跑，STEP-3 结论标注单仓。

## Risks

| ID | 风险 | 发生时 | 触发动作 |
|---|---|---|---|
| RISK-1 | 静态关系扩展在 oce 上负收益（2026-09-21 已实测 SA-PPR depth=1 为 −32.89） | STEP-15 回退 | STEP-4 结论为「多跳期望文件在现有 Top-10 内已 ≥ 80%」或无正提升空间 |
| RISK-2 | full 档 LLM rerank 单跑抖动 ±8 分 | full 档结论不可归因 | 任何需要下结论的 full 档 A/B（Checkpoint C） |
| RISK-3 | STEP-12 指纹收紧让既有实例启动即失败 | 非临时数据目录升级后不可用 | 任何非 `/tmp/oce-ab-*` 的数据目录首次以新二进制启动 |
| RISK-4 | Rust 不被 CI 覆盖（`.github/workflows/ci.yml` 只跑 pytest 与 uv build） | 回归在本地 `cargo test` 之外溜过去 | 任一 checkpoint 的 `cargo test --workspace` 未在改动后立即运行 |
| RISK-5 | 选择器改动在 nollm 正收益、full 负收益 | 默认值不能改 | Checkpoint A 的 nollm 通过之后 |

## Findings（不占步骤，供单独决定）

- `.github/workflows/ci.yml` 没有 `cargo` 任务：本计划 18 个步骤全改 Rust，合并门却只跑 Python
  单测。建议单独立项加 `cargo test --workspace` job（RISK-4 的根因）。
- `rust/crates/oce-core/src/retrieval_settings.rs` 里没有 Python 版那两个 `enable_multi_hop` /
  `enable_reference_graph` 占位开关，本计划不新增占位。
- 评测报告头只记录**目标仓库** SHA，不记录被测服务的 commit 与配置档——STEP-1 修这条。
- `oce-benchmark/results/regress-qcov-*-0929.md` 是新近最好的成绩（flask 163.27 / cc 150.89），
  但其配置档无法从报告判定；因此 STEP-3 重新建立**带配置标签**的基线，不把旧报告当基线。

## 实测输入（2026-10-06，执行前由用户提供）

**rerank 窗口 10 → 24**（Qwen3-Reranker-8B @ gitee，`top_n=24`，最终仍返回 10 条；coverage@4000，
四仓 80 查询）：

| 数据集 | top_n=10 | top_n=24 | 变化 |
|---|---:|---:|---:|
| Django | 34.33% | 36.42% | +2.1 |
| Click | 61.67% | 69.58% | +7.9 |
| HTTPX | 60.83% | 67.50% | +6.7 |
| Zod | 58.33% | 56.67% | −1.7 |
| 总 | 53.79% | 57.54% | **+3.8** |

complete 19/80 → 23/80；代价是中位 187ms → 1387ms、P95 3341ms。**它同时证明 API rerank 通道
（`RERANK_ENABLED`）在当前被测配置里是活的**——STEP-5 因此不是"改一个默认关的通道"，而是改
当前主链路之一，优先级与风险都要按此重估。

**平均返回量与头部精度**（同一批对照）：

| 系统 | 平均返回 tokens | cov@1000 | cov@4000 |
|---|---:|---:|---:|
| OpenContextEngine | 3736 | 57.2% | 94.8% |
| ACE | 5267（80/80 被截断） | 15.9% | 85.4% |
| claude-context | 17499 | 32.3% | 55.1% |
| cocoindex | 12876 | 26.0% | 47.6% |
| grepai | 10938 | 26.0% | 27.4% |
| oce Python | 2662 | 49.0% | 69.3% |
| **rust oce（top_n=24）** | **6487** | **32.8%** | **57.5%** |

结论：**对手不是靠堆量赢的**——它 80 条查询零截断、返回量中等，靠的是"前几条就命中证据"。
rust 版 cov@1000 只有 32.8%，说明**头部排序精度**是主要缺口；窗口 24 能让 click/httpx 涨 7 分，
但 select 仍只取 10。

**已知召回层缺口**：Django 上 `resolvers.py` 666-693 的函数体 chunk **连 24 名都进不了候选池**，
不是窗口问题而是 recall 问题。两条新结论因此进入计划：

- STEP-7/8/10 的排序改动是本轮主战场，优先级高于任何新召回通道。
- 新增 `STEP-4b`：把这个 recall 缺口作为一次独立调查（见下），因为它不属于原 9 项里的任何一项，
  但不查清楚就无法解释 Django 的上限。
- STEP-16 不需要从零设计：`../OpenContextEngine/src/eval/evidence.mjs` 与
  `../OpenContextEngine/eval/expanded-v1/*` 是本机可读的参考实现与数据集（用户的 coverage@4000
  数字正来自它），按它的 `answers.v1.json` schema 移植即可。

## STEP-3 建立基线时的两个新发现

1. **"当前默认 + nollm"远低于历史报告**：本轮 flask 99.52 / cc-switch 65.77，而历史
   `flask-rs-filedesc-on-nollm.md` 是 122.51、`regress-qcov-flask-0929.md` 是 163.27、
   `cc-switch-rs-v13b-nollm.md` 是 49.23。差异不是回归，而是开关档位不同——本轮刻意只用
   `oce init` 模板默认 + CON-5（`RETRIEVAL_FILE_DESC_ENABLED=false`、lexical hybrid 默认等）。
   直接后果：**flask `file_exact_match` 0/10**，正是 BCE Note 里 file_desc 修掉的那一类。
   本计划后续 A/B 一律以这两份带标签的报告为锚点；若要把历史高分当锚点，必须先重建
   "历史档位"并同样打上 `--config-label`（不属本轮范围）。
2. **`Invalid numbered lines: 0`**（两份报告共 200 题、每份 634 行级样本的实测），说明当前
   formatter 的坐标是可信的；REQ-10 的验收基线就此建立——后续任何 step 让这个数字变正，
   就是回归。

---

## Step 1 — 基准报告记录被测配置与服务身份

Depends on: nothing（oce-benchmark 仓内独立改动）。
Files:
  modify `oce-benchmark/scripts/run_retrieval_eval.py`（`render_report`，约 337-427 行；`parse_args`，约 937 行）
  modify `oce-benchmark/docs/evaluation-guide.md`（新增字段说明）
Consumes: 现有 `render_report(output, rows, repo_root, uploaded, skipped, peak_rss, base_url)` 与
  `run_suite` 的汇总路径。
Produces: CLI 新增 `--config-label TEXT`（默认 `unlabeled`）与 `--service-build TEXT`（默认 `unknown`）；
  报告头新增两行 `- Config: \`<config-label>\``、`- Service build: \`<service-build>\``，
  并新增 `- Elapsed: <秒>`；`render_suite_report` 的汇总表新增 `Config` 列。
Do: 两个参数透传进 `render_report` 与 `render_suite_report`；不改评分、不改 `extract_paths`、
  不生成 JSON（留给 STEP-18）。
  Non-goals: 不动 `verify_benchmark_lock.py`，不改报告里的 Top-1/nDCG 口径。
Accept: `cd oce-benchmark && uv run python scripts/run_retrieval_eval.py --help | grep -cE "^  --config-label"` → `1`；
  且 `uv run python scripts/run_retrieval_eval.py --help | grep -cE "^  --service-build"` → `1`。
  （修订：原写法 `grep -c -- "--config-label"` 会连 usage 行一起数，实测为 2；改为锚定选项行。）

## Step 2 — 逐行保真校验（`invalid_line_count`）

Depends on: step 1（同一文件，先落 step 1 避免二次改同一区域）。
Files:
  modify `oce-benchmark/scripts/run_retrieval_eval.py`（新增 `verify_lines`；接入 `score_query` 约 323 行与 `EvaluationRow` 约 115 行）
  modify `oce-benchmark/docs/evaluation-guide.md`
Consumes: `extract_paths(formatted: str) -> list[str]`（约 258 行）。
Produces: `verify_lines(formatted: str, repo_root: Path, known_paths: set[str] | None = None) -> tuple[int, list[str]]`
  （纯函数、导入期无副作用，CON-10）；`EvaluationRow.invalid_lines: int` 与
  `EvaluationRow.invalid_samples: list[str]`；报告 Total 段新增一行 bullet
  `- Invalid numbered lines (all queries): <sum>`（**修订**：原写成表格行，但 Total 表是 4 列，
  插 2 列会破坏表格，改为 bullet），每题明细新增 `- Invalid numbered lines: N`
  与最多一条 `- Invalid sample: <path:line reason>`。
Do: 解析 `<前导空格><数字>\t<原文>` 行；同一 `Path:` 段内行号按上一行 +1 递推，段首取
  `Lines: a-b` 的起点；行号越界、或 `source[line-1] != 原文`、或 `Path:` 不在上传集合
  （`known_paths`，由 `evaluate_one` 的上传路径集合传入；`--reuse-index` 与完整上传两条分支都收集），
  各记一次 invalid。文件按上传口径读取（`read_bytes().decode("utf-8").split("\n")`，不做换行归一化，
  与 `iter_source_blobs` 和 formatter 的 `split('\n')` 一致）。
  **跳过 broad 骨架化的省略标记行**（无行号），它们是设计上的省略而非坐标错误。
  Non-goals: 不读答案文件、不改 Top-1/nDCG 计算、不做修复（不补源码）。
Accept: `cd oce-benchmark && mkdir -p /tmp/vl && printf 'a\nb\n' > /tmp/vl/f.py && uv run python -c "import sys; sys.path.insert(0,'scripts'); import run_retrieval_eval as m; from pathlib import Path; print(m.verify_lines('Path: f.py\nLines: 1-2\n     1\ta\n     2\tWRONG\n', Path('/tmp/vl')))"`
  → 打印 `(1, ['f.py:2 line-text-mismatch'])`；把 `WRONG` 改成 `b` 再跑 → `(0, [])`；
  传 `known_paths={'other.py'}` → `(2, [...])`（`path-not-uploaded`）。

## Step 3 — 建立可审计的 nollm 双仓基线

Depends on: step 1（报告要能标出配置档）。与 STEP-2 串行（同一文件链）。
Files:
  create `oce-benchmark/results/baseline-2026-10-06-flask-nollm.md`（脚本产出）
  create `oce-benchmark/results/baseline-2026-10-06-cc-switch-nollm.md`（脚本产出）
Consumes: step 1 的 `--config-label` / `--service-build`。
Produces: 两份基线报告（头部 `Config: \`nollm\``）；两者的总分是本计划后续所有 A/B 的比较锚点。
Do:
  1. `cd rust && cargo build --release -p oce-server`（基线臂 = 当前 HEAD）。
  2. `./target/release/oce init --data-dir /tmp/oce-ab-baseline`，把 CON-5 的五个键写进
     `/tmp/oce-ab-baseline/.env`。
  3. `API_KEY=devkey ./target/release/oce serve --data-dir /tmp/oce-ab-baseline --host 127.0.0.1 --port 8986`。
  4. `cd ../oce-benchmark && API_KEY=devkey uv run python scripts/run_retrieval_eval.py --repo-root repos/flask --queries benchmarks/flask-retrieval-benchmark.jsonl --output results/baseline-2026-10-06-flask-nollm.md --config-label nollm --service-build $(git -C ../oce rev-parse --short HEAD)`
     （首跑不加 `--reuse-index`，让所有 blob 上传并嵌入）。
  5. 同法跑 cc-switch（`--repo-root repos/cc-switch`、`--queries benchmarks/cc-switch-retrieval-benchmark.jsonl`、
     `--output results/baseline-2026-10-06-cc-switch-nollm.md`）。
  Non-goals: 不调参、不改代码、不与未标配置的旧报告做跨配置比较。
Accept: 两份报告存在，各含一行 `- Config: \`nollm\``，且 `grep -c "Upload failures: 0"` 各为 `1`；
  两行 Total 的 Points 数值被抄进文末「执行记录」作为阈值基数。

## Step 4 — Investigation：graph 专用 query set 与 headroom 结论

Depends on: step 1（报告要标配置）。与 STEP-2、STEP-3 **可并行**：只新建 jsonl/metadata 与一份结论文档，
  不改 `run_retrieval_eval.py`，不读 step 2/3 的产物——三个步骤文件不相交、无运行时依赖。
Files:
  create `oce-benchmark/benchmarks/graph-multihop-benchmark.jsonl`（30 题，3 类 × 10）
  create `oce-benchmark/benchmarks/graph-multihop-benchmark.metadata.json`
  create `.agents/notes/proposed/2026-10-06-graph-headroom.md`（结论）
Consumes: jsonl 行格式 `{"id","category","difficulty","query","expected_files"}` 与 `.metadata.json` 的
  `repository.name` / `repository.commit` 约定（`discover_benchmarks`，约 725 行）。
Produces: 一份书面结论，回答**一个**问题：现有 nollm 引擎在跨文件/多跳题上，期望文件有多少已能
  在 Top-10 内收齐？必须给出三类各自的 `complete_recovery = x/10`，并给出一句放行/否决判断，
  作为 STEP-14/15 的开关。
Do: 三类各 10 题——`cross_file_call`（A 调 B，答案是 B 的实现）、`interface_impact`（改 A 要同步
  改哪些实现）、`cross_language`（前端封装 + Rust command 成对）。以 cc-switch 为主。跑：
  `uv run python scripts/run_retrieval_eval.py --repo-root repos/cc-switch --queries benchmarks/graph-multihop-benchmark.jsonl --output results/graph-headroom-cc-switch-nollm.md --config-label nollm --service-build $(git -C ../oce rev-parse --short HEAD)`
  然后人工读 Top-10 明细逐题标注「期望文件是否全部在窗口内」，把计数写进结论文档。
  Timebox: 结论写完即停，不实现任何关系抽取或图扩展。
  Non-goals: 不新增评分维度、不改打分脚本、不追求这 30 题的行级答案质量。
Accept: `.agents/notes/proposed/2026-10-06-graph-headroom.md` 存在，含三行 `complete_recovery = x/10`
  （`grep -c "complete_recovery" 该文件` ≥ `3`）与一句放行判断；结论为否决时，在文末明确
  「STEP-14/15 不启动，先修订本计划」。

## Step 4b — Investigation：Django `resolvers.py:666-693` 为何不进候选池

Depends on: step 3（要有能复现的跑法）。与 STEP-4 **可并行**：只读代码 + 只跑一个诊断查询，
  产物是结论文档，不与其他步骤共享文件。
Files:
  create `.agents/notes/proposed/2026-10-06-django-recall-gap.md`（结论）
Consumes: STEP-3 的服务与数据目录；`rust/crates/oce-core/src/chunk/`（切块入口）、
  `rust/crates/oce-infra/src/sqlite/chains.rs`（exact 召回）、`oce-core/src/lexical.rs`、`oce-core/src/retrieval.rs`。
Produces: 一份书面结论，回答**一个**问题：目标函数体（`django/db/models/sql/query.py` 或
  `django/urls/resolvers.py` 666-693）是在切块阶段、索引阶段，还是召回阶段被丢掉的？
  必须给出定位到具体阶段的一句判断 + 复现该判断的命令。
Do: 三步定位——(1) 把该文件单独上传，查 `blob-status` 与 chunk 数，确认 chunk 是否存在且覆盖 666-693；
  (2) 若 chunk 存在，直接构造该函数体文本的查询看它能否被 dense/exact/lexical 任一路召回，记录各路排名；
  (3) 若 chunk 不存在或不覆盖该区间，记录切块器（cAST/recursive）对该文件的切分结果与原因。
  Timebox: 定位到阶段即停，不修。
  Non-goals: 不在本步改切块器或召回策略；不扩大到其它 Django 漏项。
Accept: `.agents/notes/proposed/2026-10-06-django-recall-gap.md` 存在，含一行
  `stage = chunking | indexing | recall` 与一条可复现命令；若结论是 `chunking`，
  在文末写明「建议作为独立步骤插入本计划，优先级高于 STEP-14/15」。

## Step 5 — API rerank 客户端：严格校验 + 文档截断 + 不删候选

Depends on: step 3（基线锚点；本步不改排序，但基线必须先固化，否则后续无法归因）。
  **本轮实测配置里 `RERANK_ENABLED` 是开的**（top_n=24 的对照数据即来自该通道），
  所以本步改的是活链路：`min_score` 删候选与非法 index 静默跳过都会在真实检索里发生。
Files:
  modify `rust/crates/oce-infra/src/openai/llm.rs`（`OpenAIReranker::rerank_once`，约 481-545 行）
  modify `rust/crates/oce-infra/src/settings.rs`（新增 `rerank_doc_chars`→`RERANK_DOC_CHARS`，默认 5000）
  modify `rust/crates/oce-infra/src/credentials.rs`（构造点补 `doc_chars` 实参）
  modify `.env.example`（登记 `RERANK_DOC_CHARS`，并改写 `RERANK_MIN_SCORE` 的语义注释）
  test `rust/crates/oce-infra/src/openai/llm.rs` 的 `mod tests`
Consumes: `OceError::new(msg, "RerankError")`、`self.min_score` / `self.top_n` / `self.max_docs`。
Produces: `fn validate_rerank_rows(data: &serde_json::Value, requested_rows: usize, document_count: usize) -> OceResult<Vec<(usize, f32)>>`——
  `results` 长度必须等于 `min(top_n, documents.len())`；每个 `index` 为 `u64`、不重复且 ∈
  `0..document_count`；`relevance_score` 为有限 `f64` 且 ∈ [0,1]；任一不满足返回 `Err(RerankError)`。
  **修订**：原写"长度必须等于 `expected`（= 文档数）"，实测不成立——端点最多返回
  `min(top_n, 文档数)` 行（`RERANK_TOP_N=10` 而文档 24 篇时只回 10 行），因此按"请求行数"校验。
Do: 用 `validate_rerank_rows` 替换现有 `filter_map`；删除 `score >= self.min_score` 过滤（低分只影响
  排序，不再是删除理由），低于阈值的条数记 tracing；送入端点的文档按 `rerank_doc_chars` 用
  `char` 边界安全截断（超限以 `…` 结尾）。
  **REQ-1 的分工**：本步只负责"检测并让请求失败"；失败后由 `retrieval.rs` 既有的熔断 + 保序回退
  兜住，把原因写进 audit 由 STEP-6 统一补（`RerankOutcome.degraded`）。
  Non-goals: 不改 LLM rerank（STEP-6）、不改调用方 `retrieval.rs`、不改 `chat` 客户端。
Accept: `cd rust && cargo test -p oce-infra --lib rerank` → `test result: ok`，新增用例名固定为
  `rerank_rejects_missing_rows`、`rerank_rejects_duplicate_index`、`rerank_rejects_out_of_range_index`、
  `rerank_rejects_nan_score`、`rerank_rejects_score_above_one`、`rerank_keeps_low_score_candidates`
  （6 条全部执行，0 failed）。
  **实测**：`running 6 tests ... test result: ok. 6 passed; 0 failed`（2026-10-06）。

## Step 6 — LLM rerank 降级可观测

Depends on: nothing（与 STEP-5 不同文件、不同层，**可并行**：一个改 `oce-infra` 的 HTTP 客户端，
  一个改 `oce-app`/`oce-core` 的编排与协议，互不 import 对方新符号）。
Files:
  modify `rust/crates/oce-app/src/container.rs`（`LlmRerankerImpl::rerank`，约 262-360 行）
  modify `rust/crates/oce-core/src/search.rs`（`LlmReranker` 协议、`LlmRerankOutcome`、`RetrievalAudit`）
  modify `rust/crates/oce-core/src/retrieval.rs`（LLM rerank 与 API rerank 调用点、audit 写回）
  test `rust/crates/oce-core/src/retrieval.rs` 的 `mod tests`
Consumes: `RetrievalAudit` 字段 `stages` / `intent` / `path_boosted` / `scope_size`。
Produces: `pub struct LlmRerankOutcome { pub ranked: Vec<SearchHit>, pub degraded: Option<String>, pub requested: usize, pub returned: usize }`
  （新类型；`RerankOutcome` 已被 API rerank 占用，不复用）；
  `pub fn classify_llm_rerank_degraded(requested: usize, returned: usize) -> Option<String>`（纯函数，便于直接断言）；
  `RetrievalAudit.llm_rerank_degraded: Option<String>`、`.llm_rerank_returned: Option<usize>`、
  `.rerank_degraded: Option<String>`（API rerank 通路的降级原因，补 REQ-1 的"可查"）。
  **修订（2026-10-06）**：原写"`RetrievalMetricRecord` 新增对应两列"，实施时撤销——
  `retrieval_metrics` 表要与 Python 版 alembic head **逐列一致**（AGENTS.md 的运行约束），
  而本轮 CON-1 禁止改 Python；在 Rust 单侧加列会让两边 schema 漂移。因此降级事实本轮落在
  `RetrievalAudit` + tracing 日志，落库列留到与 Python 迁移一起做（记为独立后续项）。
Do: 三种情况设 `degraded`——解析出 0 个有效下标 → `"no_valid_index"`；返回条数不足需按原序补齐 →
  `"padded"`；`chat` 返回 `Err` → `"chat_error"`。降级仍返回可用结果，但调用方必须把
  `degraded`/`returned` 写进 audit（不再只打日志吞掉）。
  Non-goals: 不改 prompt、不改 `max_candidates`、不改候选窗口。
Accept: `cd rust && cargo test -p oce-core --lib retrieval` → `test result: ok`，新增用例
  `llm_rerank_degraded_no_valid_index`、`llm_rerank_degraded_padded`、`llm_rerank_degraded_chat_error`
  （假 `LlmReranker` 分别返回空、短、Err），断言结果非空且 audit 字段等于预期字符串。
  **实测（2026-10-06）**：`running 12 tests ... test result: ok. 12 passed; 0 failed`
  （9 条既有 + 3 条新增）；三条新用例同时断言 `classify_llm_rerank_degraded` 的输出与管道写回的 audit 字段。

## Step 7 — 选择器：边际覆盖增益 + 成本归一 + facet 亲和度

Depends on: step 3（基线锚点）；step 5、6（先让畸形响应与降级不再污染后续归因）。
Files:
  modify `rust/crates/oce-core/src/selector.rs`（`CoverageSelector::select`，约 56-160 行）
  modify `rust/crates/oce-core/src/retrieval.rs`（`fuse`，约 172-215 行：保留每路召回列表）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （新增 `RETRIEVAL_MARGINAL_COVERAGE_ENABLED` 默认 false、`RETRIEVAL_FACET_TEMPERATURE` 默认 0.06）
Consumes: `fuse(query, result_lists: Vec<Vec<SearchHit>>, ...)` 与 `SearchHit.score` —— 每一路召回列表
  自带该 facet 下该命中的分数，**不需要新的向量访问**。
Produces: `pub fn facet_affinity(hits: &[SearchHit], result_lists: &[Vec<SearchHit>], temperature: f32) -> Vec<Vec<f32>>`
  （逐列 soft-max，缺失记 0，按 `search_hit_key` 对齐）；
  `CoverageSelector::select_with_coverage(&self, hits: &[SearchHit], facet_scores: &[Vec<f32>], top_k: usize) -> Vec<SearchHit>`；
  旧 `select(&self, hits, top_k)` 保留，作为开关关闭时的路径。
  **修订（2026-10-06）**：撤销原计划的 `SelectionInput` 包装结构——直接传
  `(hits, facet_scores, top_k)` 三个参数，少一层无收益的间接；同时 `fuse` 从
  `Vec<Vec<SearchHit>>` 改为 `&[Vec<SearchHit>]`，让各路召回列表在选择阶段仍可借用。
Do: 开关关闭时逐字保持现有两趟填充；开启时每步
  `gain = 0.7 * mean(facet_value / (1 + covered)) + 0.3 * base_score`，再除以
  `(max(120, cost) / 300) ** 0.35`（cost = `hit.content.chars().count()`），`gain < 0.015` 停止。
  `facet_value` 用逐列 soft-max：`exp(min(0, s - column_max) / RETRIEVAL_FACET_TEMPERATURE)`，
  逐列减自身最大值（不假设余弦分跨列可比）。同分并列按 `search_hit_key` 字典序，保证 REQ-3 的确定性。
  Non-goals: 不改 `max_per_path` / `overlap_threshold`、不动 basename 近重复门控、不动 `broad.rs`。
Accept: `cd rust && cargo test -p oce-core --lib selector` → `test result: ok`，新增用例
  `coverage_respects_char_budget`、`coverage_still_suppresses_overlap`、
  `coverage_prefers_two_facet_hit_over_single_top`、`coverage_is_deterministic`
  （另加 `facet_affinity_softmax_per_column_and_missing_is_zero`）。
  **实测（2026-10-06）**：`running 12 tests ... test result: ok. 12 passed; 0 failed`
  （6 条既有 + 6 条新增）。

## Step 8 — 候选名额保留泛化

Depends on: step 7（同一文件 `retrieval.rs` 的邻近区域）。
Files:
  modify `rust/crates/oce-core/src/retrieval.rs`（`fuse` 约 210 行的 `.take(default_top_k)`、
    `merge_exact_hits` 约 221-282 行）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （新增 `RETRIEVAL_RESERVED_CANDIDATE_SLOTS` 默认 0；0 = 关闭）
Consumes: step 7 在 `fuse` 中保留的「每路来源标签」。
Produces: 无新导出符号；`merge_exact_hits` 在 `default_top_k` 截断前为精确独占候选保留
  `RETRIEVAL_RESERVED_CANDIDATE_SLOTS` 个名额。
Do: 把现有 CALL_CHAIN 分支手工留 1/3 名额的逻辑抽成「按来源保留名额」通用步骤：先取融合前列，
  再按来源补足保留名额，合并去重后按分数排序；保留 anchor 分数对齐语义（exact-only 分数不超过
  语义锚点分）。开关为 0 时行为逐字等于今天。
  **实施修正（2026-10-06）**：通用分支必须先 `semantic_hits.take(semantic_slots)` 再合并——
  CALL_CHAIN 的老写法把**全部**语义候选留在 merged 里，只有语义词表长于 `semantic_slots` 时
  保留名额才碰巧生效；小窗口下精确候选仍会被挤掉（测试 `reserved_slots_keeps_exact_hits` 的
  基线断言就是这个失败模式）。
  Non-goals: 不新增召回来源、不改 `rerank_pool_k`、不改 `_promote_symbol_endpoints`。
Accept: `cd rust && cargo test -p oce-core --lib retrieval` → `test result: ok`，新增用例
  `reserved_slots_zero_matches_legacy`（固定输入快照逐条对比）、`reserved_slots_keeps_exact_hits`。
  **实测（2026-10-06）**：`running 14 tests ... test result: ok. 14 passed; 0 failed`。

## Checkpoint A — after step 8

- `cd rust && cargo test --workspace` → 全绿。
- 按 STEP-3 的 2-5 步复测双仓，臂名 `results/ab-rerank-selector-<repo>-nollm.md`（新数据目录
  `/tmp/oce-ab-selector`，CON-4）。
- 判据：两仓各自 Total ≥ STEP-3 基线 − 2，且**无单类跌超 3 分**；flask 的 `file_exact_match`、
  `configuration_lookup`、`call_chain` 三类单独列出对比。
- 不达标 → 回退 STEP-7 或 STEP-8 中对应的那一项（STEP-5/6 不改排序，不参与该判断），
  并在「执行记录」追加实测数值。
- 纪律：本 checkpoint 之后不因 STEP-7/8 的结果回头调参重跑；调参必须作为新的步骤写进本计划。
- RISK-5 触发时（full 档负收益）→ 保持新开关默认关，Full 档结论记入执行记录。

### Checkpoint A 实测（2026-10-06，双仓 nollm，独立数据目录 `/tmp/oce-ab-selector`）

| 臂 | flask | Δ | cc-switch | Δ | 判定 |
|---|---:|---:|---:|---:|---|
| 基线（`nollm`，HEAD 默认） | 99.52 | — | 65.77 | — | 锚点 |
| STEP-7+8 一起（`nollm-margcov-rsv2`） | 67.19 | −32.33 | 24.79 | −40.98 | 不达标 |
| **仅 STEP-7**（`margcov=1, rsv=0`） | 67.18 | −32.34 | 25.66 | −40.11 | 回归来自 STEP-7 |
| 仅 STEP-7，修输出序后（`margcov-orderfix`） | 84.49 | −15.03 | 51.78 | −13.99 | 仍未达标 |
| **仅 STEP-8**（`margcov=0, rsv=2`） | 101.03 | **+1.51** | 61.25 | −4.52 | 单仓正收益、不达双仓门槛 |

**结论**：

1. `RETRIEVAL_MARGINAL_COVERAGE_ENABLED` 与 `RETRIEVAL_RESERVED_CANDIDATE_SLOTS` **都保持默认关**。
2. 过程中定位到一个真实缺陷并已修：`select_with_coverage` 原先把**贪心顺序**当返回顺序，而评测
   按返回顺序读 Top-1/nDCG——仅这一条就占了回归的一半（flask −32→−15、cc −40→−14）。修复：
   选择器只决定**集合**，输出回到输入（分数）顺序；新增用例 `coverage_output_preserves_input_order`
   守住它，`coverage_is_deterministic` 改为断言集合一致。
3. 剩余负收益在 STEP-7 的目标函数形状（成本归一 `(max(120,cost)/300)^0.35` 与 0.7/0.3 权重），
   属于**调参实验**，写成独立步骤 STEP-7b，不在本轮继续扫参。
4. STEP-8 的 `rsv=2` 在 flask +1.51、cc −4.52：方向可能对但取值/门控不对，写成 STEP-8b
   （窄扫 1/2/4 + 按意图门控），本轮不继续。
5. 若 STEP-7b/8b 仍不通过，删除这两段代码而不是长期留开关（AGENTS.md：不留未接入的占位）。

## Step 7b — 边际覆盖率选择器的目标函数标定（暂缓，Checkpoint A 未通过）

Depends on: Checkpoint A 的负结果（`margcov-orderfix` 仍 −15/−14）。与 STEP-9 无依赖关系，
  排在 STEP-9 之后执行也可以——它不改动 `SearchHit`、也不改索引。
Files:
  modify `rust/crates/oce-core/src/selector.rs`（`select_with_coverage` 的目标函数）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （只在需要新参数时；现有 `RETRIEVAL_MARGINAL_COVERAGE_ENABLED` / `RETRIEVAL_FACET_TEMPERATURE`
    已够跑 (a)(c)）
Consumes: `select_with_coverage(&self, hits, facet_scores, top_k)`（STEP-7 产出，输出序已修为输入序）。
Produces: 与 STEP-7 相同的签名，只改内部权重与亲和度计算。
Do: 三组**独立**变量，每组各自跑一次双仓 nollm A/B（复用 `/tmp/oce-ab-selector` 索引，
  `--reuse-index`，`--config-label` 标出这一组）：
  (a) 关掉成本归一——除数恒 1.0（怀疑 `(max(120,cost)/300)^0.35` 把长函数体挤出集合）；
  (b) 权重改为 `0.5 * coverage + 0.5 * base`（怀疑覆盖权重过高）；
  (c) 温度 0.06 → 0.2，且只把候选进入前 20 名的召回列计入（怀疑零值列稀释覆盖）。
  一次只改一组；不得在同一轮里同时改两组再解释结果。
  Non-goals: 不改 `select`（默认路径）、不改 `facet_affinity` 的按 key 对齐语义、不动预算/上限/去重不变量。
Accept: 每组判据同 Checkpoint A（双仓 Total ≥ 基线 − 2 且无单类跌超 3）；任一组通过即把该组
  参数设为该开关的默认并在执行记录写明；三组全不通过 → **删除** `select_with_coverage` 与
  `RETRIEVAL_MARGINAL_COVERAGE_ENABLED`（AGENTS.md：不留未接入的占位实现）。

## Step 8b — 保留名额的取值与门控标定（暂缓，Checkpoint A 未通过）

Depends on: Checkpoint A 的 `rsv=2` 结果（flask +1.51 / cc −4.52）。
Files:
  modify `rust/crates/oce-core/src/retrieval.rs`（`merge_exact_hits` 的通用保留分支）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （仅在需要意图门控开关时）
Consumes: `RETRIEVAL_RESERVED_CANDIDATE_SLOTS`（STEP-8 产出）与 `merge_exact_hits` 的现有签名。
Produces: 无新导出符号。
Do: 两次独立 A/B：(a) `slots=1`；(b) `slots=2` + 只在 `Symbol` / `CallChain` 意图下保留
  （用 `classify_query_intent(query)` 门控）。cc-switch 上一轮在 `cross_language` 掉了 2.52，
  门控的目的就是不让精确保留挤掉跨语言语义候选。
  Non-goals: 不改 `default_top_k`、不改 anchor 分数对齐语义、不引入新的召回来源。
Accept: 每组判据同 Checkpoint A；全不通过 → 删除该开关与保留分支，`merge_exact_hits` 回到
  CALL_CHAIN 特例 + 通用合并两条路径。

## Step 11b — 切块器两个真缺陷（真语料实测发现）

Depends on: STEP-11（不变式与分级已就位）；与 STEP-9/10 无依赖，但**必须先做**再谈 bundle——
bundle 建在丢行的 chunk 上没有意义。两处都会改变 chunk 边界 → 需要 STEP-12 的 chunker 指纹
先落地，且各自跑一次 nollm 双仓 A/B。
Files:
  modify `rust/crates/oce-core/src/chunk/cast.rs`（`merge_small` 的相邻边界情形，约 542-568 行）
  modify `rust/crates/oce-core/src/chunk/recursive.rs`（`tile`/`emit` 的尾部片段，约 300-340 行）
  test `rust/crates/oce-core/src/chunk/validate.rs`（把 `uncovered` 提升为 hard 的用例）
Consumes: `chunk::validate::{inspect_chunks, ChunkInspection}`（STEP-11 产出，
  字段 `hard` / `uncovered` / `boundary_overlaps`）。
Produces: 无新导出符号；STEP-11 的 `uncovered` 从告警升级为硬错误（修复后真语料应为 0）。
Do:
  1. **cAST 边界重叠**：flask 6 处、cc-switch 122 处相邻 `ast` chunk 共享 1 行
     （前块 `end_line` == 后块 `start_line`，如 `src/flask/app.py` 的 `[51..82]` 与 `[82..176]`）。
     在 `merge_small` 里对 `start <= prev_end` 的情形收口（把重复边界行只留给后一块，或并入前一块），
     然后让 `boundary_overlaps` 在真语料归零。
  2. **递归切块器尾部漏行**：`src-tauri/src/resources/codex_deepseek_catalog_template.json`
     第 136-138 行（`    }` / `  ]`）无人覆盖——紧跟在两行 17KB 超预算行之后，说明
     `cap_span` 跳过超长行后重启的缓冲没被尾部 emit 收走。修完后把 STEP-11 的 `uncovered`
     从告警改成硬错误，并把 `inspect_chunks` 的对应分支移回 `hard`。
  Non-goals: 不改各切块器的预算/最小块参数；不改 `formatter`；不动 `span_merge`。
Accept: `OCE_VALIDATE_DIR=<flask> cargo test -p oce-core --test chunk_validation -- --ignored` 与
  同命令对 `<cc-switch>` → 两者都 `0 hard / 0 boundary overlap / 0 uncovered`；
  随后 `cargo test --workspace` 全绿，并按 STEP-3 流程跑一次双仓 nollm A/B（判据同 Checkpoint A）。

**实测（2026-10-06）**：

- 两处缺陷的根因都定位到了具体代码，不是靠调参：
  1. **cAST 边界重叠**：`cast.rs::merge_small` 只挡了"完全包含"（`end <= prev_end`），
     漏了"恰好共享 1 行边界"（`start == prev_end`）。修法：边界行归**后**一块（它是后一块
     的定义头，跟着定义体更合理），前一块 `end` 收回一行；若前一块只剩边界行则丢弃该范围。
  2. **递归切块器漏行**：`recursive.rs::emit` 里 `if !is_meaningful(&text) { continue; }`
     把**纯标点片段**（`    }` / `  ]`）整块丢掉。`codex_deepseek_catalog_template.json`
     的两行 17KB 超预算行被 `cap_span` 跳过后，尾部片段正是这种纯标点内容 → 谁都没覆盖。
     修法：无语义片段不单独成块，但**并入前一块**（超预算时才勉强独立成块）——
     宁可有噪声块，也不能有覆盖不到的行。
- 真语料验证：flask `231 files / 632 chunks`、cc-switch `1031 files / 7857 chunks`，
  两者都是 **0 hard / 0 边界重叠 / 0 漏行**（修前分别是 6 处重叠、122 处重叠 + 1 文件漏 3 行）。
- `CHUNKER_VERSION` 升到 `chunk=v2`（边界语义变了 → 旧索引必须重建，由 STEP-12 的指纹
  fail-closed 保证）；`uncovered` 同时从告警**提升为硬错误**，边界重叠保留为回归探针。
- 双仓 nollm A/B（**全新索引**，无 `--reuse-index`；sidecar 确认 `chunk=v2`）：
  flask 99.52 / cc-switch 65.77，**delta +0.00**，`invalid=0`、`upload_fail=0`；
  graph 集 `all_expected@10` 14/30 → 14/30。
- **结论**：这是**正确性修复而非分数修复**——被修掉的是收尾括号行与重复的边界行，
  它们在候选池里本来就能从相邻 chunk 取到，所以 100 条查询上量不出差异。
  价值在于"行号永不撒谎 + 不静默丢代码"这条不变量现在有真语料背书，且回归有探针兜底。

## Step 9 — 符号元数据注解（修订版：不加 SearchHit 字段、不加表列）

> **修订（2026-10-06，实施前）**：原计划给 `SearchHit` 加 `symbol/owner/kind` 三个字段并给
> `symbol_occurrences` 加两列。落地前评估后收窄为「端口 + 注解表」，理由：
> 1. `SearchHit` 字面量构造点约 20 处（三个 store、first_chunk_lookup、span_merge、related、
>    formatter 与 e2e 测试），加字段是纯机械改动但价值不在值对象本身；
> 2. `owner` 需要作用域信息，正则提取器没有，cAST 有但要穿透 chunk 存储——那是另一件更大的事，
>    而 STEP-10 的 bundle 只需**按符号名分组**；
> 3. `symbol_occurrences.identifier` 本身就是符号名，不需要再加 `symbol` 列；
> 4. 注解表按 `search_hit_key` 对齐，与 `facet_affinity` 同一套对齐方式，语义一致。

Depends on: Checkpoint A（排序链路已稳定，注解改动可单独归因）；STEP-11b 之后（bundle 不该建在丢行的 chunk 上）。
Files:
  modify `rust/crates/oce-core/src/search.rs`（`ExactSearchStore` 新增默认方法 + `SymbolRow` + `annotate_symbols`）
  modify `rust/crates/oce-infra/src/sqlite/chains.rs` 与 `rust/crates/oce-infra/src/pg/chains.rs`（各一个实现）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （`RETRIEVAL_CONTEXT_BUNDLE_ENABLED` 默认 false，STEP-10 共用）
  test `rust/crates/oce-core/src/search.rs`（`annotate_symbols` 单测）
  test `rust/crates/oce-infra/tests/exact_definitions.rs`
Consumes: `symbol_occurrences` 既有列 `(identifier, blob_name, content_hash, kind, start_line, end_line)`
  与 `search_hit_key(hit) -> (blob_name, path, start_line, end_line, content_hash)`。
Produces: `pub struct SymbolRow { identifier, content_hash, kind, start_line, end_line }`；
  `pub fn annotate_symbols(hits: &[SearchHit], rows: &[SymbolRow]) -> HashMap<search_hit_key, (symbol, kind)>`；
  `ExactSearchStore::definitions_for_blobs(&self, blob_names: &[String]) -> OceResult<Vec<SymbolRow>>`
  （默认空实现，向后兼容既有实现）。
  **实施修正（2026-10-06）**：比修订版再简化一步——**复用已有的 `ExactSearchStore` 端口**
  加一个默认方法，而不是新建 `SymbolLookup` 端口 + 容器装配。理由：`exact_store` 已经在
  `RetrievalPipeline` 里装配好，SQLite/PG 两个实现本来就有 `symbol_occurrences` 的句柄；
  新端口只会多一层间接与一次容器改动。匹配规则不变（按 `content_hash` 对齐 + occurrence
  `start_line` 落在命中 span 内 + 多个候选取最小 start_line）。
Do: 两个后端各一条 `WHERE kind='definition' AND blob_name IN (...)` 查询（SQLite 走
  `spawn_blocking` + `with_conn`，PG 用 `ANY($1)`），整批命中只查一次库；`annotate_symbols`
  是纯函数，`content_hash` 为空或 span 内无 definition 时不标注（不猜）。
  Non-goals: 不加表列、不改 `symbol_extractor` 正则、不做跨文件关系（STEP-14）、不回填历史数据。
Accept: `cd rust && cargo test -p oce-infra --test exact_definitions` → `test result: ok`，新增用例
  `symbol_lookup_annotates_hits_by_span`（只取 definition、span 外不标注、重复 blob 名去重）；
  `cargo test -p oce-core --lib search` → 2 条 `annotate_symbols` 单测；
  随后 `cargo test --workspace` → 全绿；**默认关闭时**跑一次 STEP-3 流程的双仓 nollm，
  分数与基线逐条相同。
  **实测（2026-10-06）**：`exact_definitions` 5 passed；`search` 2 passed；workspace 172 passed；
  默认关双仓 flask 99.52 / cc-switch 65.77，与基线 delta **+0.00**。

## Step 10 — 短函数 bundle 与符号级实体聚合

Depends on: step 9（符号注解表）；step 7（`select_with_coverage`，其输出序修复已落地）。
  **注意**：STEP-7 的 A/B 未通过且默认关，所以本步的 bundle 只能挂在
  `RETRIEVAL_CONTEXT_BUNDLE_ENABLED` 上（不能假设边际覆盖选择器在场）——bundle 需要在
  现有两趟填充路径上也能生效：把 bundle 展开成"同符号成员跟随锚点一起入席"的候选策略，
  而不是只改 `select_with_coverage`。
Files:
  modify `rust/crates/oce-core/src/selector.rs`（bundle 作为待选 action；`select` 与
    `select_with_coverage` 两条路径都要处理）
  modify `rust/crates/oce-core/src/retrieval.rs`（开关打开时构造注解表 → 实体 → bundle）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （新增 `RETRIEVAL_BUNDLE_MAX_CHARS` 默认 768；`RETRIEVAL_CONTEXT_BUNDLE_ENABLED` 已在 STEP-9 落地）
Consumes: `search::annotate_symbols` 产出的 `HashMap<search_hit_key, (symbol, kind)>`（STEP-9）、
  `select_with_coverage`（STEP-7）、`select`（默认路径）。
Produces: `pub struct Entity { pub symbol: String, pub kind: String, pub member_keys: Vec<(String, String, u32, u32, String)> }`；
  `pub fn build_entities(hits: &[SearchHit], annotations: &HashMap<(String, String, u32, u32, String), (String, String)>) -> Vec<Entity>`；
  `pub fn build_bundles(entities: &[Entity], hits: &[SearchHit], max_chars: usize) -> Vec<Vec<usize>>`。
Do: 同 `(symbol, kind)` 的命中聚成一个实体，实体分取成员最高分；实体总字符 ≤ `max_chars` 时作为一个
  action 整体取舍，超过则退化为成员逐个取舍；bundle 只含尚未入选的成员，**cost 只累加新增成员**；
  trace 里标注被 bundle 带入的成员（区别于主动选中的锚点）。
  Non-goals: 不做调用者上下文扩展（依赖 STEP-14 的关系）、不改 formatter 输出格式（CON-9）。
Accept: `cd rust && cargo test -p oce-core --lib selector` → `test result: ok`，新增用例
  `bundle_selects_small_function_atomically`（3 段小函数整体入选且只按新增字符计费）、
  `bundle_falls_back_when_oversized`、`bundle_works_on_default_selector_path`（开关打开、
  边际覆盖关闭时同样生效）；随后按 STEP-3 流程跑 `results/ab-bundle-<repo>-nollm.md`，
  判据同 Checkpoint A（Total ≥ 基线 − 2、无单类 −3）。
  **实测（2026-10-06）**：selector 18 passed（新增 5 条）；workspace 177 passed；
  开关打开双仓 **flask 99.52 / cc-switch 65.77（+0.00，无单类变化）**，graph 集
  `all_expected@10` **14/30 → 14/30**；`-vv` 探针确认机制生效
  （`hits=133 → annotated=75 → entities=60 → bundles=75`）。

  **结论：达回归门槛但零收益 → 保持默认关。** 原因是结构性而非实现问题：
  bundle 只把**同一文件内同一符号**的兄弟片段一起带入，而 `max_per_path=2` 的第二趟填充
  本来就会把该文件的第二个席位补上（集合不变），顺序也未改变。实测缺口在
  **跨文件**：graph 集 `interface_impact` 3/10 缺的是其它文件里的实现者，
  `cross_file_call` 缺的也是另一个文件——这正好把 STEP-14/15（关系扩展）的靶点钉死为
  **跨文件补齐**，而不是同文件打包。这条负结果与 STEP-4 的结论互相印证。

## Step 11 — 分块完整性硬校验

Depends on: nothing（防御性；与 STEP-9/10 不共享文件）。
Files:
  create `rust/crates/oce-core/src/chunk/validate.rs`
  modify `rust/crates/oce-core/src/chunk/mod.rs`（`pub mod validate;`）
  modify `rust/crates/oce-core/src/indexing.rs`（`embed_pending` 切块处，约 233-239 行）
Consumes: `self.chunker.chunk(content: &str, path: &str) -> Vec<Chunk>` 与 `Chunk` 的
  `start_line` / `end_line` / `content`。
Produces: `pub fn validate_chunks(content: &str, chunks: &[Chunk]) -> Result<(), String>`——
  每个 chunk 的 `content` 必须逐字等于按物理行切片后的 `start_line..=end_line`；span 不重叠；
  源文件所有非空行均被覆盖。
Do: 在 `par_iter` 的 `map` 之后逐 blob 调用；失败时把该 blob 置
  `BlobStatus::Error`（`mark_error("chunk validation: <reason>")`）走既有错误路径暴露给
  `/agents/blob-status`，而不是照常入库。物理行切分只认 `\n`，与 `formatter.rs` 保持一致。
  Non-goals: 不改任何切块器的切分策略；不做已入库数据的回溯校验。
Accept: `cd rust && cargo test -p oce-core --lib validate` → `test result: ok`，新增用例
  `validate_accepts_python_source`、`validate_rejects_text_mismatch`、
  `validate_rejects_overlapping_spans`、`validate_rejects_uncovered_nonblank_line`；
  随后 `cargo test --workspace` → 全绿（证明现有 chunker 全部满足该不变式）。

## Step 12 — 索引身份扩展与 fail-closed

Depends on: step 11（先保证入库内容自洽，再收紧指纹，否则指纹一变就要求重建而内容可能不自洽）。
Files:
  modify `rust/crates/oce-app/src/container.rs`（指纹拼装约 625 行、sidecar 写入约 700 行）
  modify `rust/crates/oce-core/src/chunk/router.rs`（导出 `chunker_fingerprint`）
  test `rust/crates/oce-app/tests/workspace_embedded.rs`（沿用既有指纹用例风格，约 217 行）
Consumes: 现有 `let model_fingerprint = format!("{model_tag} dim={vector_dim} {etext_version}")`；
  既有用例 `file_desc_fingerprint_mismatch_fails_closed`。
Produces: `pub fn chunker_fingerprint() -> &'static str`，返回形如 `chunk=v1`（切块器实现变化时人工递增）；
  指纹变为 `"{model_tag} dim={vector_dim} {etext_version} {chunker_fingerprint}"`。
Do: 把 `chunker_fingerprint()` 拼进指纹；`EMBED_REVISION`（若 `.env` 有该键）并入 `model_tag` 段，
  让「同名权重更新」可被人工 bump 表达。指纹不匹配时沿用既有 fail-closed 文案风格，错误信息必须含
  「索引由 [旧指纹] 构建，当前 [新指纹]，请删除 <数据目录> 并重新上传以重建」。
  Non-goals: 不写迁移器、不自动删索引、不改 `symbol_occurrences` 存储。
Accept: `cd rust && cargo test -p oce-app --test workspace_embedded` → `test result: ok`，新增用例
  `chunker_fingerprint_mismatch_fails_closed`（旧指纹 sidecar + 新指纹启动 → 错误含「请删除」）、
  `chunker_fingerprint_stable_across_restarts`（同指纹两次启动均成功）。

**实测（2026-10-06）**：两个新用例均通过（`cargo test -p oce-app --test workspace_embedded
chunker_fingerprint` → `2 passed`）；workspace 179 passed。

- `EMBED_REVISION` **未落地**：Rust `settings.rs` 里没有这个键（Python 侧也没有对应解析），
  本步只做 `chunker_fingerprint` 一段。若将来要支持"同名权重更新"，需要先新增设置项，
  不能靠约定——已记入本计划 Findings 之外的待办，不在本步范围。
- 兼容策略：`chunk=v1` 是"尚无切块器变更"的基线，因此**仅在切块器仍为 v1 时**放行
  `model dim etext` 三段旧 sidecar（避免一次无意义重建）；STEP-11b 升到 `chunk=v2` 后，
  旧格式（含 `chunk=v1`）一律 fail-closed。
- 实测 sidecar 内容：`minishlab/potion-multilingual-128M dim=256 etext=v1 chunk=v2`。

## Step 13 — 查询新鲜度语义

Depends on: step 12（复用同一套索引就绪元数据与错误文案）。
Files:
  modify `rust/crates/oce-core/src/retrieval.rs`（`retrieve` 入口收集范围内状态）
  modify `rust/crates/oce-core/src/search.rs`（新增 `IndexReadiness`）
  modify `rust/crates/oce-server/src/schemas.rs`（`CodebaseRetrievalRequest` / `Response`）
  modify `rust/crates/oce-server/src/routes.rs`（`codebase_retrieval`，约 273 行）
  test `rust/crates/oce-server/tests/api_contract.rs`
Consumes: `BlobStatus { Pending, Ready, Error }`（`oce-core/src/blob.rs:7-27`）、
  `BlobRepository::find_pending(blob_names: Option<&[String]>)`、`list_pending_names()`。
Produces: `pub struct IndexReadiness { pub scope_size: usize, pub pending: usize, pub failed: usize, pub ready: usize, pub last_error_type: Option<String> }`；
  请求体新增 `freshness_wait_ms: Option<u64>`（上限 120000）；响应新增
  `index: { mode, scope_size, pending, failed, ready, last_error_type }`。
Do: 解析 scope 后统计 pending/failed/ready；`pending > 0` 且 `freshness_wait_ms > 0` 时按 200ms 轮询
  直到归零或超时；超时仍返回结果但 `index` 如实报告。`failed > 0` 时 `last_error_type` 取该 blob 的
  错误原因。保留「绝不把 pending 的缺失当正常空结果」的语义。
  Non-goals: 不新增端点、不改 `/agents/blob-status` 既有字段、不做服务端主动重试。
Accept: `cd rust && cargo test -p oce-server --test api_contract` → `test result: ok`，新增用例
  `retrieval_reports_all_ready_scope`（`pending == 0` 且立即返回）、
  `retrieval_reports_pending_without_waiting`（`freshness_wait_ms=0` + 一个 pending blob →
  `pending >= 1` 且结果仍返回）。

**实测（2026-10-06）**：两个用例通过；workspace 181 passed。

- **实施修正**：计划里的 `get_many` 不能用在检索入口——SQLite 版是**逐名循环**
  （每个 blob 一条 SELECT + `load_chunks`），1000 个 blob 的范围就是 1000 次查询。
  改为新增端口方法 `BlobRepository::status_summary(&[String]) -> BlobStatusSummary`，
  两个后端各**一条**聚合查询（SQLite `IN (...)`、PG `ANY($1)`），只取
  `status`/`error_message`，不加载 chunk。实测延迟无变化（flask median 4ms→4ms、
  cc-switch 12ms→12ms）。
- **`retrieve` 签名保持不变**：新增 `retrieve_with_freshness(..., freshness_wait_ms)`，
  原 `retrieve` 委托 0ms。理由：`retrieve` 有 11 处既有调用点（含 e2e 测试），
  改签名是纯机械噪声。
- **pending 的第三种来源**：范围内但服务端没有记录的 blob 也算 pending
  （还没走完上传/入队，同样不可检索）。契约测试正是用这一点构造确定的 pending 态，
  而不是靠"上传后抢在嵌入完成前发请求"这种不可复现的竞态。
- 上限 120000ms 在路由层截断；等待发生在请求路径上，不能让调用方无限占连接。
- 手工 REQ-8 验证（HTTP，`/tmp/oce-ab-phase2`）：(a) 刚上传 + `wait=0` → `ready`；
  (b) 范围内含未就绪 blob → `mode=pending`、`pending=1`，**结果照常返回**；
  (c) `wait=800ms` → 实测等待 0.82s 后超时返回，仍报 `pending=1`。

## Checkpoint B — after step 13

- `cd rust && cargo test --workspace` → 全绿。
- 按 STEP-3 的 2-5 步复测双仓，臂名 `results/ab-phase2-<repo>-nollm.md`（新数据目录
  `/tmp/oce-ab-phase2`）。判据：Total ≥ 基线 − 2、无单类跌超 3 分。
- 手工验一次 REQ-8：对 `/tmp/oce-ab-phase2` 上传一个文件后**立即**（embed 未完成时）带
  `freshness_wait_ms: 0` 请求 `/agents/codebase-retrieval`，响应 `index.pending >= 1`。
- RISK-3 检查：用新二进制启动 `/tmp/oce-ab-baseline`（STEP-3 的旧索引），必须报指纹不匹配并给出
  重建提示；确认这是预期而非缺陷。

## Step 14 — 关系抽取与 `<related_symbols>` 实验

Depends on: step 4 的 headroom 结论为**放行**；step 9（symbol 元数据）；Checkpoint B 通过。
  若 step 4 否决：本步与 STEP-15 不启动，先修订本计划（rule 19），不要在本步里临时改设计。
Files:
  create `rust/crates/oce-core/src/relation.rs`
  modify `rust/crates/oce-core/src/related.rs`（把关系信息渲染进 hints 块）
  modify `rust/crates/oce-infra/src/sqlite/mod.rs` 与 `rust/crates/oce-infra/src/pg/mod.rs`
    （新增 `chunk_relations(src_hash TEXT, dst_identifier TEXT, kind TEXT, confidence REAL, resolution TEXT)`）
  modify `rust/crates/oce-core/src/indexing.rs`（切块后写关系；只写 `calls` / `same_symbol` /
    function↔function 的 `member_of`）
Consumes: `SearchHit` 正文（关系改为查询期现算，见下）；`related.rs` 既有 `MAX_RELATED_HINTS=8` 与 `FANOUT_MAX=15`。

> **修订（2026-10-06，实施前）**：原计划新建 `chunk_relations` 表并在切块后写入。落地前否决，
> 三条理由：
> 1. **违反项目硬约束**：AGENTS.md 要求 Rust 的 PG schema 与 Python alembic head **逐列一致**，
>    而 CON-1 本轮不改 Python。加表会直接破坏这条（STEP-6 的 `retrieval_metrics` 新列就是
>    因此放弃的，同一约束）。
> 2. **不必落库**：种子是**已选中**的 ≤20 段 chunk，正文就在手里；查询期用正则现算关系的
>    成本可忽略，而落库要多一次全量重索引 + 双后端写入路径。
> 3. **`SameSymbol` / `MemberOf` 抽不出来**：它们不是源码文本能算的（需要 `symbol_occurrences`
>    符号表），而它们服务的 hints 增强已被 `ident_tokens + find_definitions` 覆盖；
>    图扩展（STEP-15）只需要 `Calls`。因此枚举只保留 `Calls`，不留占位变体。
>
> 关系因此变为：`extract_relations(path, content)`（纯函数）+ `unresolved_call_spans(content)`，
> 不新增端口、不新增表、不新增装配。
Produces: `pub enum RelationKind { Calls, SameSymbol, MemberOf }`；
  `pub struct Relation { pub dst_identifier: String, pub kind: RelationKind, pub confidence: f32, pub resolution: String }`；
  `pub fn extract_relations(path: &str, content: &str, chunks: &[Chunk]) -> Vec<Relation>`；
  `pub fn load_relations(blob_names: &[String]) -> HashMap<String, Vec<Relation>>`。
Do: 用现有 `regex` 能力提取被调标识符（沿用 `symbol.rs` 的宽松度，不追求编译器级解析）；解析不出的
  写 `resolution="unresolved"` 且**不进 hints**（2026-09-21 Note：允许不知道，但不假装知道）；
  hints 只在 `<related_symbols>` 块追加信息，不改变结果集与顺序（CON-9）。
  Non-goals: 不做图扩展召回（STEP-15）、不动 `broad.rs`、不做跨语言边。
Accept: `cd rust && cargo test -p oce-core --lib relation` → `test result: ok`，新增用例
  `extract_relations_python_calls`（数量与 `resolution="static-name"` 正确）、
  `unresolved_relation_not_in_hints`；`cargo test -p oce-core --lib related` → `test result: ok`，
  含 `hints_do_not_change_path_lines`（渲染前后 `Path:` 行集合逐字相同）。
  随后按 STEP-3 流程跑 `results/ab-related-relations-<repo>-nollm.md`，判据同 Checkpoint A。

**实测（2026-10-06）**：

- `relation` 4 passed、`related` 15 passed、workspace **189 passed**。
- 双仓 nollm A/B（hints 开，`--reuse-index`，臂名 `nollm-related-relations`）：
  flask 99.52 / cc-switch 65.77，**delta +0.00**，`invalid=0`。这是 CON-9 的**预期**结果
  （hints 只追加文本，评测只读 `Path:` 行），本次跑的作用是**泄漏检查**：确认关系/hints
  没有意外进入候选集或排序。
- HTTP 探针（真实响应）确认机制生效：`<related_symbols>` 在场，6 条 hint 里 2 条带
  `relation="calls"`（`Flask`、`create_app`），其余为纯标识符提及。
- **顺带修一处 hints 噪声**：探针暴露 `docs/*.rst` 散文词（`that` / `can`）以"定义"身份
  进了 hints。STOPWORDS 扩了 36 个代词/限定词/助动词。只影响 hints 文本，不可能影响评分
  （`hints_do_not_change_path_lines` 守住这条）。

## Step 15 — 图扩展召回（intent 门控、低权重起步）

Depends on: step 14（关系数据与 hints 已落地并验证）、step 4 结论为正、Checkpoint B 通过。
Files:
  modify `rust/crates/oce-core/src/retrieval.rs`（`fuse` 前新增一路关系候选）
  modify `rust/crates/oce-core/src/retrieval_settings.rs` 与 `rust/crates/oce-infra/src/settings.rs`
    （新增 `RETRIEVAL_GRAPH_EXPANSION_ENABLED` 默认 false、`RETRIEVAL_GRAPH_WEIGHT` 默认 0.1、
    `RETRIEVAL_GRAPH_MAX_NODES` 默认 16、`RETRIEVAL_GRAPH_FANOUT_CAP` 默认 15）
Consumes: step 14 的 `load_relations`、step 8 的来源名额保留。
Produces: `pub fn expand_relations(seeds: &[SearchHit], relations: &HashMap<String, Vec<Relation>>, max_nodes: usize, fanout_cap: usize) -> Vec<SearchHit>`；
  该路结果带来源标签 `"graph"` 参与 RRF。
Do: 种子取融合前列锚点；按关系取有界邻居（`max_nodes` 上限、`fanout_cap` 抑制高频 hub）；
  以 `RETRIEVAL_GRAPH_WEIGHT` 参与 RRF（0.1 起调）；只在 `REFERENCE` / `CALL_CHAIN` 意图下启用。
  **禁止**把无模型分的图候选直接写进最终输出（只做候选，交 rerank 与 selector）。
  Non-goals: 不做多跳（1 跳起步）、不改 `default_top_k`、不加新 intent。
Accept: `cd rust && cargo test -p oce-core --lib retrieval` → `test result: ok`，新增用例
  `graph_disabled_matches_legacy`（开关关时输出与开启前逐条相同）、`graph_hub_neighbor_capped`。
  随后在 `graph-multihop-benchmark.jsonl` 上跑 `results/ab-graph-cc-switch-nollm.md`：
  **通过条件是本题集 `all_expected@10` 相对 `.agents/notes/proposed/2026-10-06-graph-headroom.md`
  的 14/30 有提升**，且双仓主基准无单类跌超 3 分；**不以**本题集 Top-1 作为通过条件
  （STEP-4 结论：primary@1 偏低是排序问题，不是加边能解决的）。否则回退本步。

**实测（2026-10-06）——本计划唯一显著正收益的一步**：

- 实现：`graph_candidates()` 从各路召回**头部锚点**（每路 5 条）现算静态调用关系 →
  `find_definitions` 拿定义文件扇出做 hub 抑制（> `graph_fanout_cap` 不进图）→
  `search_exact` 取回真实候选（含正文，可直接参与融合）；作为一路追加在
  `all_result_lists` 末尾，用 `rrf_weights()` 单独给 `graph_weight`。只在
  `Reference` / `CallChain` 意图下启用（`classify_query_intent`，纯启发式，nollm 档可用）。
- 机制生效证据（`-vv` 探针，graph 集 30 题）：**13 题触发**，每次
  `seeds=15 identifiers=28..71 kept=14..41 hits=16`。
- **主基准（双仓 100 题）**：

  | 权重 | flask | cc-switch | Top-1 | 单类最差 |
  |---|---:|---:|---|---|
  | w=0.1（跑两次，逐位相同） | 99.52 → **107.23（+7.71）** | 65.77 → **75.82（+10.05）** | 42→47 / 30→37 | ±0.00 |
  | w=0.5 | 99.52 → 107.11（+7.59） | 65.77 → 74.32（+8.55） | 42→47 / 30→36 | −0.01 |

  两个权重同向、量级相当 → 结论**不依赖权重微调**，不是调出来的。
- **graph 集**：w=0.1 无提升（14/30）；w=0.5 提升到 **15/30**（`cross_language` 5→6，
  重跑复现）。按本步写明的通过条件（本题集有提升），**w=0.5 通过**。
- **延迟代价**：median 4ms→30ms（flask）、12ms→70.5ms（cc-switch），p95 44/117ms。
  图这一路每次检索多两次 SQL（`find_definitions` + `search_exact`）。
- **结论与默认值**：按 CON-6「A/B 通过后再单独决定是否改默认」，本步**保持默认关**。
  理由不是"没效果"（效果显著），而是**延迟预算**：+7.7/+10.1 分换 6-7× 检索延迟，
  这个取舍应当由使用场景决定（离线/批处理/质量优先可开，交互式默认不开）。
  改默认是一个独立决定，需要单独拍板；本步只把证据摆齐。

## Checkpoint C — after step 15

- `cd rust && cargo test --workspace` → 全绿。
- 双仓主基准复测（`results/ab-phase3-<repo>-nollm.md`）：Total ≥ 基线 − 2、无单类 −3。
- full 档对照一次（RISK-2）：同一配置跑两遍，取中位，`--config-label full-rerank-llm`；
  仅在需要决定「图扩展/选择器是否改默认」时才跑。
- 任一不达标 → 按 REQ 归属回退 STEP-15 或 STEP-10，写执行记录。

**实测（2026-10-06）**：workspace **191 passed**；默认档双仓复测
（`nollm-phase3`，`--reuse-index`）flask 99.52 / cc-switch 65.77，delta **+0.00**、
无单类变化 —— 证明「图扩展默认关」对既有行为零影响。

**full 档未跑（并说明原因）**：本 checkpoint 的 full 档对照是「仅在需要决定改默认时才跑」。
本轮结论是**保持默认关**（理由是延迟预算，见 STEP-15 实测），因此不触发该条件；
若后续决定把 `RETRIEVAL_GRAPH_EXPANSION_ENABLED` 改成默认开，必须补跑 full 档双跑中位
（RISK-2），再动默认值。

## Step 16 — 评测：线级 evidence 答案键与覆盖率（flask 前 20 题试点）

Depends on: step 2（复用同一套行解析）。
Files:
  create `oce-benchmark/benchmarks/flask-answers.v1.json`
  create `oce-benchmark/scripts/score_evidence.py`
  modify `oce-benchmark/scripts/run_retrieval_eval.py`（新增 `--answers PATH`，报告加覆盖段）
Consumes: step 2 的 `verify_lines`、`extract_paths`、`load_queries`。
Produces: 答案 schema
  `{"schemaVersion":1,"answerVersion":"v1","dataset":"flask-retrieval-benchmark","commit":"<sha>","annotator":"...","coverageRule":"...","answers":[{"id":"Q01","units":[{"id":"...","fact":"...","alternatives":[{"allOf":[{"path":"...","startLine":N,"endLine":M,"quote":"...","sha256":"..."}]}]}]}]}`；
  `score_evidence.py` 暴露 `load_answers(path) -> dict`、`score_coverage(answers, formatted, repo_root) -> CoverageResult`。
Do: 覆盖判定 = 「存在某个 alternative，其 `allOf` 中所有 span 的所有行都被**已验证地**返回」；
  评分前用 `quote`/`sha256` 与仓库实际内容逐条校验，任一不符直接退出（防答案漂移）。
  试点只做 flask 前 20 题（10 个类别各 2 题），每题 2-5 个 unit。
  **不要从零设计**：本机可读的参考实现在 `../OpenContextEngine/src/eval/evidence.mjs`
  （`parseAndVerify` / `budgetPrefix` / `scoreEvidence`），数据集与答案样例在
  `../OpenContextEngine/eval/expanded-v1/{click,httpx,zod}` 与 `eval/django-v1/`——
  用户给的 coverage@4000 数字正来自这套 harness。按它的 `answers.vN.json` schema 移植，
  只在"行级 span 校验"与"预算截断"两处对齐本仓的 `verify_lines` 口径。
  Non-goals: 不做双语、不做多仓库、不改 Top-1/nDCG 口径、不引入 tokenizer（沿用字符/文件级评分）。
Accept: `cd oce-benchmark && uv run python scripts/score_evidence.py --answers benchmarks/flask-answers.v1.json --repo-root repos/flask --verify-only`
  → 打印 `all N units verified` 且退出码 0（N ≥ 40）；
  `uv run python scripts/run_retrieval_eval.py --repo-root repos/flask --queries benchmarks/flask-retrieval-benchmark.jsonl --answers benchmarks/flask-answers.v1.json --output results/evidence-flask-nollm.md --config-label nollm`
  → 报告出现 `Evidence coverage` 段，含每题命中 unit 数与总覆盖率。

## Step 17 — 协议与冻结清单

Depends on: step 16（答案文件是冻结对象之一）。
Files:
  create `oce-benchmark/benchmarks/flask.protocol.json`
  create `oce-benchmark/scripts/freeze_inputs.py`
Consumes: step 1 的配置字段约定、step 16 的答案文件。
Produces: `benchmarks/flask.freeze.json`：
  `{"frozenAt":"<iso>","status":"frozen before the first run of this protocol","sha256":{"queries.json":..,"answers.v1.json":..,"metadata.json":..,"protocol.json":..},"implementationSha256":{"run_retrieval_eval.py":..,"score_evidence.py":..}}`；
  `freeze_inputs.py` 暴露 `main()`，对已存在的 `freeze.json` 直接 `raise SystemExit("frozen inputs never overwrite")`。
Do: 协议文件只写口径——Top-1/nDCG 定义、`expected_files` 顺序语义、lock 策略、
  `--config-label` 约定、`invalid_line_count` 定义、`coverageRule`；**不写 token 预算**（本轮没有），
  也不写编造的批量/延迟上限。
  Non-goals: 不做多引擎对照、不引入 tokenizer、不写迁移/发布流程。
Accept: `cd oce-benchmark && uv run python scripts/freeze_inputs.py --benchmark flask`
  → 生成 `benchmarks/flask.freeze.json` 且退出码 0；同命令再跑一次 → 退出码非 0 且 stderr 含
  `never overwrite`。

## Step 18 — 报告 JSON 与机器审计

Depends on: step 17（审计要消费冻结清单）。
Files:
  create `oce-benchmark/scripts/audit_benchmark.py`
  modify `oce-benchmark/scripts/run_retrieval_eval.py`（新增 `--json-output PATH`，复用已算好的 rows，
    不重新解析 `.md`，避免与报告排版耦合）
Consumes: step 17 的 `freeze.json`、step 2 的 `invalid_line_count`、step 16 的覆盖结果、step 1 的配置字段。
Produces: `--json-output PATH` 写出
  `{"config":"<label>","serviceBuild":"<sha>","queries":[{"id","category","top1","ndcg","invalidLines","coverage"}],"total":..,"invalidLinesTotal":..,"reportSha256":"<自哈希>"}`；
  `audit_benchmark.py --report-json <p> --freeze <p>` 断言集合：每条 query 恰好一条结果、
  `len(queries) == expected_queries`、`invalidLinesTotal` 字段存在、冻结输入 sha256 与 freeze 一致、
  `reportSha256` 与文件内容一致；输出 `{"status":"passed"|"failed","checks":[...]}`，
  failed 时退出码非 0。
Do: 审计脚本不联网；除校验冻结输入所需的文件外不读仓库内容。
  Non-goals: 不生成图表、不改既有 `.md` 报告内容、不并入 CI。
Accept: 在 STEP-16 的服务与数据目录仍可用时，`cd oce-benchmark && uv run python scripts/run_retrieval_eval.py --repo-root repos/flask --queries benchmarks/flask-retrieval-benchmark.jsonl --answers benchmarks/flask-answers.v1.json --output results/evidence-flask-nollm.md --json-output results/evidence-flask-nollm.json --config-label nollm --reuse-index`
  → 退出码 0 且生成 JSON；`uv run python scripts/audit_benchmark.py --report-json results/evidence-flask-nollm.json --freeze benchmarks/flask.freeze.json`
  → 打印 `{"status":"passed"` 且退出码 0；把 JSON 里任意一条 `top1` 改坏再跑 → 退出码非 0。

## Checkpoint D — after step 18

- `cd rust && cargo test --workspace` → 全绿（本计划所有 Rust 改动）。
- `cd oce-benchmark && uv run python scripts/audit_benchmark.py --report-json results/evidence-flask-nollm.json --freeze benchmarks/flask.freeze.json` → `status: passed`。
- `uv run python scripts/verify_benchmark_lock.py --metadata benchmarks/flask-retrieval-benchmark.metadata.json --repo-root repos/flask` → 退出码 0。
- 双仓最终复测一次（`results/final-<repo>-nollm.md`），与 STEP-3 基线对比写入执行记录。
- 收尾：把实测结论回写 decision record 的对应条目（吸收/回退各一行），并按 `finish` 的惯例归档本计划。

## 执行记录（执行时追加，本计划只增不改）

| 日期 | Step/Checkpoint | 命令 | 实际输出 | 结论 |
|---|---|---|---|---|
| 2026-10-06 | STEP-1 | `uv run python scripts/run_retrieval_eval.py --help \| grep -cE "^  --config-label"` | `1`（`^  --service-build` 同为 `1`） | 通过 |
| 2026-10-06 | STEP-1 | `render_report(..., 'nollm', 'abc1234', 42.5)` 直调 | 头部含 `- Config: \`nollm\``、`- Service build: \`abc1234\``、`- Elapsed: 42.5 s` | 通过 |
| 2026-10-06 | STEP-2 | 见 STEP-2 的 Accept 一行式（`/tmp/vl/f.py` 夹具） | `(1, ['f.py:2 line-text-mismatch'])` / `(0, [])` / `known_paths={'other.py'}` → `(2, [... path-not-uploaded])` | 通过 |
| 2026-10-06 | STEP-2 | 对运行中服务真实响应的独立复核（`curl`-等效：`httpx` 单查询 + `verify_lines`） | `numbered_lines 634`、`verify -> (0, [])` | 通过（真实输出确有线号且逐行可核） |
| 2026-10-06 | STEP-3 | `cargo build --release -p oce-server`；`oce init --data-dir /tmp/oce-ab-baseline` + CON-5 五键；`oce serve`；两次 `run_retrieval_eval.py --config-label nollm` | flask **99.52/200（Top-1 42/100）**、cc-switch **65.77/200（Top-1 30/100）**；两份报告 `Invalid numbered lines: 0`、`Upload failures: 0`；索引指纹 `minishlab/potion-multilingual-128M dim=256 etext=v1` | 通过（基线锚点 = 99.52 / 65.77） |
| 2026-10-06 | STEP-4 | 编写 `graph-multihop-benchmark.jsonl`（30 题）+ metadata；`run_retrieval_eval.py --queries graph-multihop-benchmark.jsonl --reuse-index` | 总 **47.40/60**、Top-1 26/30；primary@1 **9/30**、primary@10 26/30、all_expected@10 **14/30**（cross_file_call 6/10、cross_language 5/10、interface_impact 3/10） | 通过；结论=**有条件放行**：图扩展只对"补齐实现者"有理由，头部排序归 STEP-7/8/10（详见 `.agents/notes/proposed/2026-10-06-graph-headroom.md`） |
| 2026-10-06 | STEP-5 | `cargo test -p oce-infra --lib rerank` | `running 6 tests ... test result: ok. 6 passed; 0 failed` | 通过 |
| 2026-10-06 | STEP-5 | `cargo test --workspace`（改动后全量） | `passed=152 failed=0` | 通过（无回归） |
| 2026-10-06 | STEP-6 | `cargo test -p oce-core --lib retrieval` | `running 12 tests ... test result: ok. 12 passed; 0 failed` | 通过 |
| 2026-10-06 | STEP-6 | `cargo test --workspace`（改动后全量） | `passed=155 failed=0` | 通过（无回归） |
| 2026-10-06 | STEP-7 | `cargo test -p oce-core --lib selector` | `running 13 tests ... test result: ok. 13 passed; 0 failed` | 代码完成（默认关） |
| 2026-10-06 | STEP-8 | `cargo test -p oce-core --lib retrieval` | `running 14 tests ... test result: ok. 14 passed; 0 failed` | 代码完成（默认关） |
| 2026-10-06 | STEP-7/8 | `cargo test --workspace` | `passed=163 failed=0` | 通过（无回归） |
| 2026-10-06 | Checkpoint A | 双仓 nollm A/B（8987 臂，独立数据目录 `/tmp/oce-ab-selector`，`--config-label nollm-*`） | 见下表 | **不达标**：两个新开关都不启用，保持默认关；STEP-5/6 的成果不受影响 |
| 2026-10-06 | STEP-11（提前做） | `cargo test -p oce-core --lib validate` | `running 6 tests ... test result: ok. 6 passed` | 通过 |
| 2026-10-06 | STEP-11 | `OCE_VALIDATE_DIR=<flask> cargo test -p oce-core --test chunk_validation -- --ignored` | `231 files / 632 chunks`：**0 hard**、6 边界重叠、0 漏行 | 通过（真语料） |
| 2026-10-06 | STEP-11 | 同上，`<cc-switch>` | `1031 files / 7866 chunks`：**0 hard**、122 边界重叠、**1 个文件漏 3 行** | 通过（带告警） |
| 2026-10-06 | STEP-11 | `cargo test --workspace` | `passed=169 failed=0` | 通过（无回归） |
| 2026-10-06 | STEP-9 | `cargo test -p oce-core --lib search` | `running 2 tests ... test result: ok`（`annotate_symbols_matches_by_hash_and_span` / `_prefers_earliest_definition_in_span`） | 通过 |
| 2026-10-06 | STEP-9 | `cargo test -p oce-infra --test exact_definitions` | `running 5 tests ... test result: ok`（新增 `symbol_lookup_annotates_hits_by_span`） | 通过 |
| 2026-10-06 | STEP-9 | `cargo test --workspace` | `passed=172 failed=0` | 通过 |
| 2026-10-06 | STEP-9 | 默认关双仓 nollm 复测（`nollm-step9-defaultoff`，`--reuse-index`） | flask 99.52 / cc-switch 65.77，与基线**逐条相同（+0.00）** | 通过 |
| 2026-10-06 | STEP-10 | `cargo test -p oce-core --lib selector` | `running 18 tests ... test result: ok. 18 passed`（新增 5 条 bundle/entity 用例） | 通过 |
| 2026-10-06 | STEP-10 | `cargo test --workspace` | `passed=177 failed=0` | 通过 |
| 2026-10-06 | STEP-10 | 开关打开双仓 nollm A/B（`nollm-bundle`） | flask **99.52** / cc-switch **65.77**，delta **+0.00**，无单类变化 | 达回归门槛但**零收益** |
| 2026-10-06 | STEP-10 | graph 集 `all_expected@10`（bundle on） | 14/30 → **14/30**，三类逐项不变 | 零收益（缺口是跨文件的） |
| 2026-10-06 | STEP-10 | `-vv` 机制探针（单查询） | `hits=133 symbol_rows=1042 annotated=75 entities=60 bundles=75` | 机制确实生效（非"没触发"） |
| 2026-10-06 | STEP-12 | `cargo test -p oce-app --test workspace_embedded chunker_fingerprint` | `running 2 tests ... test result: ok`（`_stable_across_restarts` / `_mismatch_fails_closed`） | 通过 |
| 2026-10-06 | STEP-12 | `cargo test --workspace` | `passed=179 failed=0` | 通过 |
| 2026-10-06 | STEP-11b | `OCE_VALIDATE_DIR=<flask>`（真语料不变式） | `231 files / 632 chunks`：**0 hard / 0 重叠 / 0 漏行**（修前 6 处重叠） | 通过 |
| 2026-10-06 | STEP-11b | `OCE_VALIDATE_DIR=<cc-switch>` | `1031 files / 7857 chunks`：**0 hard / 0 重叠 / 0 漏行**（修前 122 处重叠 + 1 文件漏 3 行） | 通过 |
| 2026-10-06 | STEP-11b | `cargo test --workspace` | `passed=179 failed=0` | 通过 |
| 2026-10-06 | STEP-11b | 双仓 nollm A/B（**全新索引** `/tmp/oce-ab-chunker`，`nollm-chunkerv2`，sidecar `chunk=v2`） | flask 99.52 / cc-switch 65.77，delta **+0.00**，`invalid=0`、`upload_fail=0` | 达回归门槛、分数无变化 |
| 2026-10-06 | STEP-11b | graph 集 `all_expected@10`（v2 索引） | 14/30 → **14/30**，三类逐项不变 | 无变化 |
| 2026-10-06 | STEP-13 | `cargo test -p oce-server --test api_contract retrieval_reports` | `running 2 tests ... test result: ok`（`_all_ready_scope` / `_pending_without_waiting`） | 通过 |
| 2026-10-06 | STEP-13 | `cargo test --workspace` | `passed=181 failed=0` | 通过 |
| 2026-10-06 | Checkpoint B | 双仓 nollm 复测（新目录 `/tmp/oce-ab-phase2`，`nollm-phase2`） | flask 99.52 / cc-switch 65.77，delta **+0.00**；median 4ms/12ms 与基线相同 | 通过 |
| 2026-10-06 | Checkpoint B | 手工 REQ-8（HTTP） | (a) 刚上传 wait=0 → `ready`；(b) 含未就绪 blob → `pending=1` 仍返回；(c) `wait=800ms` → 实测等 0.82s 后超时返回 | 通过 |
| 2026-10-06 | Checkpoint B | RISK-3：新二进制启动旧索引 `/tmp/oce-ab-baseline` | 退出码 1，错误含两侧指纹 + 「请删除 … 重新上传」 | 预期行为 |
| 2026-10-06 | STEP-14 | `cargo test -p oce-core --lib relation` | `running 4 tests ... test result: ok`（含 `extract_relations_python_calls`、`_marks_dynamic_dispatch_unresolved`、`_dedupes_same_target`、span 用例） | 通过 |
| 2026-10-06 | STEP-14 | `cargo test -p oce-core --lib related` | `running 15 tests ... test result: ok`（含 `hints_label_resolved_calls`、`unresolved_relation_not_in_hints`、`hints_do_not_change_path_lines`） | 通过 |
| 2026-10-06 | STEP-14 | `cargo test --workspace` | `passed=189 failed=0` | 通过 |
| 2026-10-06 | STEP-14 | 双仓 nollm A/B（`nollm-related-relations`，hints 开） | flask 99.52 / cc-switch 65.77，delta **+0.00**，`invalid=0` | 达回归门槛（CON-9 预期） |
| 2026-10-06 | STEP-14 | HTTP 探针（hints 开，真实响应） | `<related_symbols>` 在场，6 条 hint 中 2 条带 `relation="calls"`（Flask / create_app） | 机制生效 |
| 2026-10-06 | STEP-15 | `cargo test -p oce-core --lib graph_` | `graph_disabled_matches_legacy` / `graph_hub_neighbor_capped` 均 ok | 通过 |
| 2026-10-06 | STEP-15 | `cargo test --workspace` | `passed=191 failed=0` | 通过 |
| 2026-10-06 | STEP-15 | graph 集 `all_expected@10`：w=0.1 / w=0.5 | 14/30 → **14/30**（w=0.1）；14/30 → **15/30**（w=0.5，cross_language 5→6，重跑复现） | w=0.5 达通过条件 |
| 2026-10-06 | STEP-15 | 双仓主基准：w=0.1（两次独立跑） | flask 99.52 → **107.23（+7.71）**、cc-switch 65.77 → **75.82（+10.05）**；两次逐位相同；Top-1 42→47 / 30→37；无单类下跌 | **显著正收益** |
| 2026-10-06 | STEP-15 | 双仓主基准：w=0.5 | flask +7.59（Top-1 47）、cc-switch +8.55（Top-1 36）；worst_cat −0.01 | 同向、量级相当 |
| 2026-10-06 | STEP-15 | `-vv` 机制探针（graph 集） | 30 题里 13 题触发：`seeds=15 identifiers=28..71 kept=14..41 hits=16` | 机制生效（非"没触发"） |
| 2026-10-06 | STEP-15 | 延迟对照（median / p95） | flask 4ms→30ms / 5ms→44ms；cc-switch 12ms→70.5ms / 25ms→117ms | 代价明确：约 6-7× |
| 2026-10-06 | Checkpoint C | `cargo test --workspace` | `passed=191 failed=0` | 通过 |
| 2026-10-06 | Checkpoint C | 双仓主基准默认档复测（`nollm-phase3`） | flask 99.52 / cc-switch 65.77，delta **+0.00** | 通过（默认关零行为变化） |
| 2026-10-06 | STEP-16 | `score_evidence.py --verify-only` | `all 46 units verified`，退出码 0 | 通过 |
| 2026-10-06 | STEP-16 | `run_retrieval_eval.py --answers ...`（flask，`--reuse-index`） | 报告出现 `## Evidence coverage`：**14/46 units（30.4%）**、2/20 题全覆盖 | 通过 |
| 2026-10-06 | STEP-17 | `freeze_inputs.py --benchmark flask` | 生成 `flask.freeze.json`（6 个 sha256），退出码 0；再跑一次 → 退出码 1 + `never overwrite` | 通过 |
| 2026-10-06 | STEP-18 | `audit_benchmark.py --report-json ... --freeze ...` | `{"status": "passed"}`，6 项检查全过，退出码 0 | 通过 |
| 2026-10-06 | STEP-18 | 篡改 JSON 的 `top1` 后再审计 | `report_sha256` 失败（`754587289d37` → `adc457c3061d`），退出码 1 | 通过 |
| 2026-10-06 | Checkpoint D | `cargo test --workspace` | `passed=191 failed=0` | 通过 |
| 2026-10-06 | Checkpoint D | `verify_benchmark_lock.py --metadata ... --repo-root repos/flask` | `OK flask: 100 questions pinned at commit 22d924701a6a`，退出码 0 | 通过 |
| 2026-10-06 | Checkpoint D | 双仓最终复测（`nollm-final`） | flask 99.52 / cc-switch 65.77，delta **+0.00**，`invalid=0` | 通过（默认档零变化） |
| 2026-10-06 | Checkpoint D | 结论回写 + 归档 | 回写 `borrow-analysis` 第七节；本计划移入 `implemented/feature/` | 完成 |

## 自检（写计划时完成，一次）

- 需求覆盖：REQ-1 → STEP-5（检测/失败）+ STEP-6（把降级原因写进 audit）；REQ-2 → STEP-6；
  REQ-3 → STEP-7；REQ-4 → STEP-8；REQ-5 → STEP-9 + STEP-10；
  REQ-6 → STEP-11；REQ-7 → STEP-12；REQ-8 → STEP-13；REQ-9 → STEP-1；REQ-10 → STEP-2；
  REQ-11 → STEP-16 + STEP-17 + STEP-18。分析里的图扩展（原 3.1）在本计划中不是需求而是风险项，
  由 STEP-4 放行、STEP-14/15 承担（STEP-4 结论：只对"补齐实现者"放行）。
- 占位符扫描：无 `TBD`/`TODO`/`[NEEDS CLARIFICATION]`；无「类似 step N」的交叉引用；
  所有命令均为仓库自身命令（`cargo test`、`uv run python scripts/...`），无未填参数。
- 命名一致性：`select_with_coverage` / `SelectionInput`（STEP-7 产出）在 STEP-10 消费；
  `RerankOutcome`（STEP-6 产出）仅本步使用；`Entity` / `build_entities` / `build_bundles`
  （STEP-10 产出）仅本步使用；`chunker_fingerprint`（STEP-12 产出）仅本步使用；
  `Relation` / `RelationKind` / `extract_relations` / `load_relations`（STEP-14 产出）在 STEP-15 消费；
  `SearchHit.symbol` / `.owner` / `.kind`（STEP-9 产出）在 STEP-10、STEP-14 消费，字符级一致。
- 并行声明：仅 STEP-4 ∥ STEP-2/3（文件不相交、无运行时依赖）与 STEP-5 ∥ STEP-6（不同 crate、
  不互相 import 新符号）两处；其余按依赖顺序串行。
