# Graph 专用 query set 的 headroom 结论（STEP-4）

- 日期：2026-10-06
- 状态：已测量，结论=**有条件放行**
- 输入：`oce-benchmark/benchmarks/graph-multihop-benchmark.jsonl`（30 题，3 类 × 10，cc-switch @ `40cac1a`）
- 被测：rust oce `3a14765`，`Config: nollm`（静态 potion-multilingual-128M dim=256 + LLM 全关）
- 复现命令：

```bash
cd rust && ./target/release/oce init --data-dir /tmp/oce-ab-baseline   # 已建，含 nollm 五键
./target/release/oce serve --data-dir /tmp/oce-ab-baseline --port 8986
cd ../oce-benchmark && API_KEY=sk-opencontextengine uv run python scripts/run_retrieval_eval.py \
  --repo-root repos/cc-switch --queries benchmarks/graph-multihop-benchmark.jsonl \
  --output results/graph-headroom-cc-switch-nollm.md \
  --config-label nollm --service-build 3a14765 --reuse-index
```

总成绩：**47.40 / 60（79.0%）**，Top-1 26/30，`Invalid numbered lines: 0`。

## 一、三个可判定指标（题级的逐题重算，不依赖报告聚合）

`expected_files` 首项是"最深实现文件"（`services/*`、`database/dao/*`、`proxy/*`、
`session_manager/providers/*` 或 trait/模型定义），命令层与前端 wrapper 只作支撑项。

| 类别 | n | primary@1 | primary@10 | all_expected@10 |
|---|---:|---:|---:|---:|
| cross_file_call | 10 | **0/10** | 8/10 | **complete_recovery = 6/10** |
| cross_language | 10 | 1/10 | 8/10 | **complete_recovery = 5/10** |
| interface_impact | 10 | 8/10 | 10/10 | **complete_recovery = 3/10** |
| **合计** | 30 | **9/30 (30%)** | **26/30 (87%)** | **complete_recovery = 14/30 (47%)** |

## 二、结论

**放行，但只对"完整性"这一半放行。**

1. **召回基本够，头部排序不够**：最深实现有 26/30 落在 Top-10，但只有 9/30 排在第一。
   cross_file_call 的 primary@1 是 **0/10** —— 命令/包装文件稳定压过真实实现。
   这是**排序（rerank/选择器）问题，不是图问题**：加边不会把已经在窗口里的东西提到第一。
   → 该缺口由 STEP-7/8/10 承担，不是 STEP-15 的理由。
2. **完整性确实缺**：`all_expected@10 = 14/30`。interface_impact 最差（3/10）：问"改这个要
   同步改哪些文件"时，trait 定义基本总能找到（primary@1 8/10），但**实现者凑不齐**
   （claude/codex/gemini 三个 adapter 常常只回一个）。这正是 `implements`/`member_of`
   关系扩展能补的洞。
   → 这是 **STEP-14/15 的唯一正当理由**，验收也应按"补齐实现者"而不是按 Top-1 定。
3. **不该承诺的**：图扩展**不**解决 primary@1 偏低；若 STEP-15 上马后拿主基准 Top-1 说事，
   预期会失望。STEP-15 的验收基准是本题集上 `all_expected@10` 的提升。

## 三、未收齐的 16 题（STEP-15 的靶点）

| 题 | 类别 | 收齐 | 说明 |
|---|---|---|---|
| G03 | cross_file_call | 2/3 | 缺 `database/dao/mod.rs` |
| G04 | cross_file_call | 2/3 | 缺 `services/sql_helpers.rs` |
| G07 | cross_file_call | 1/2 | 最深实现 `services/omo.rs` 未进窗口 |
| G09 | cross_file_call | 1/4 | 只回 1/4：`s3.rs`/`s3_sync.rs`/`s3_auto_sync.rs` 缺 |
| G12 | interface_impact | 3/4 | 缺 `forwarder.rs` 或某个 adapter |
| G14 | interface_impact | 3/4 | 缺 `connectivity-check.ts` 或 command |
| G15 | interface_impact | 2/4 | 多 provider 构造点只回一个 |
| G16 | interface_impact | 2/4 | 缺 DAO 或前端类型 |
| G18 | interface_impact | 2/3 | 缺前端类型 |
| G19 | interface_impact | 3/4 | 缺 env_manager 或前端类型 |
| G20 | interface_impact | 3/4 | 缺 DAO 或前端类型 |
| G24 | cross_language | 2/3 | 最深实现 `services/proxy.rs` 未进窗口 |
| G25 | cross_language | 3/4 | 缺 `session_manager/mod.rs` 或 command |
| G28 | cross_language | 2/3 | 缺前端 wrapper |
| G29 | cross_language | 2/3 | 最深实现 `database/dao/proxy.rs` 未进窗口 |
| G30 | cross_language | 2/4 | 缺 `session_usage.rs` 或前端 wrapper |

## 四、对计划的影响（已回写）

- STEP-14（关系抽取 + hints）照常执行：零排序风险，且是验证"结构信号有没有用"的最小实验。
- STEP-15 的验收改为**双条件**：本题集 `all_expected@10` 相对本报告提升，且两个主基准无单类
  跌超 3 分；**不以**本题集 Top-1 作为通过条件。
- 本结论不覆盖 Django 的 `resolvers.py:666-693` 缺口（那是 recall 层的独立调查，见 STEP-4b）。
