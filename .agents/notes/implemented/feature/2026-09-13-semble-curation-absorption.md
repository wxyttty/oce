# Agent Note: semble_rs 策展层吸收第二批：元目录降权、相邻 span 合并+补全、CJK 投影评估

Status: implemented

## Problem

semble_rs（`codesearch/semble_rs`，单二进制离线检索引擎）2026-09-07~09-12 的 8 个提交
完成了 BCE 策展层吸收（`d43f214` 等）与 CJK 投影质量改造，且用本仓 oce-benchmark
同评测器对拍可测（`scripts/eval/oce_bench_adapter.py`）。逐项比对后确认三项可吸收/
需评估：元目录降权（M2）、相邻 span 合并+小片段补全（M1）、CJK 投影改造（lift/df
下限/加性 bonus）。semble 同基准 nollm 对拍领先 oce-rs 约 +6/+2.5 分（flask 64.2% vs
61.3%、cc 38.3% vs 35.8%），差距主要来自其策展层——本批验证其中机制可移植的部分。

## Decision

三项按独立 flag / 独立 A/B 落地，全部默认关：

1. **元目录降权**（`priority.rs` + `RETRIEVAL_META_DIR_PENALTY_ENABLED`）：`.github/
   .gitlab/.gitea/.gitee/.circleci/.teamcity/.drone/.buildkite` 目录段命中 ×0.5；
   查询明确点名 CI/工作流（`is_meta_intent_query`：github|action|workflow|ci|
   pipeline|dependabot|工作流|流水线|持续集成）时豁免。实现形态与 semble 不同：
   oce 的因子是纯函数组合（`source_priority_factor × meta_dir_factor`），
   `apply_source_priority`/`apply_confidence_floor` 从 fn 指针改为泛型闭包，query
   贯穿主检索路；**路径增强路不套 meta 因子**——定位查询（「tests.yaml 在哪里」）
   中任何文件都可能是目标，文档中立原则优先。quoi 同理：oce 的 source_priority
   对 manifest/yml 不降权，semble「注入清单被惩罚过滤」的互锁教训在本仓不存在。
2. **相邻 span 合并 + 小片段补全**（`span_merge.rs` + `FileContentLookup` 端口 +
   `RETRIEVAL_SPAN_MERGE_ENABLED`）：同 blob 相距 ≤2 行的选中片段合并成连续段，
   <6 行片段两侧各补 3 行；内容从 `blob_chunks → chunks` 行号重构（blobs 表不存
   原文，**实测 70/221 blob 的 chunk 覆盖有内部空洞**——行 map 逐行校验，缺行整组
   回退原样，行号永不撒谎）；select 池加宽 8 条，合并缩窗后按 `enforce_budget`
   （条数 + 字符预算硬限制）收口，次优排名回填。M1 教训（合并必须在截断前 +
   回填）完整移植。
3. **CJK 投影改造：评估后不做**。semble 的三项（lift 排名替代共现计数、df≥2
   下限、投影从独立 RRF facet 改加性 decaying bonus）全部是**索引侧共现挖掘**
   的质量控制；oce 的 `lexical.rs` 是人工策划静态别名表 + embedding 查询文本
   注入——无挖掘噪声（不需要 lift/df 门），无 RRF facet 拆票（向量是整体的）。
   架构位置不同，无对应问题。若未来引入语料挖掘式投影扩展，此三条是现成护栏。

## Key measured facts

- 元目录降权 A/B（nollm，双仓单变量）：flask 122.56 → 122.63（+0.07，2 题微涨
  Q26/Q76，均非 meta 意图查询），cc-switch 71.70 → 71.70（逐题零差异）。
  机制验证：.github 入窗次数 flask 24 → 14、cc 10 → 7；Q53（期望答案就是
  .github/workflows/tests.yaml 的 meta 意图查询）经豁免保持不变。
- span 合并 A/B（nollm，双仓单变量）：flask 122.59 → 122.56（±0.03 噪声带），
  cc-switch 71.70 → 71.70；窗口零变化——**结构性中性**：oce 的两轮覆盖选择器
  第一轮已按「每文件一席」分配，大库（230/1031 blob）下同文件相邻片段根本不会
  同时入窗，与 semble 平铺排序（同文件 chunk 聚集、cap=1 挤占）的病灶不同。
  受益面是小 scope 工作区（agent 上传几个文件、池内同文件多 chunk）与 broad
  宽窗口；机制正确性由集成测试验证（相邻 chunk 合并成单节 + 行内容完整）。
- chunk 切分单位实测：cAST 对 6600 字符 Python 文件切成 4 个 ~39 行 chunk
  （~1820 字符/个）——fixture 设计依据。
- semble README 对拍数据（同 oce-benchmark 评测器经 adapter）：multilingual-128M
  nollm + 策展层全开 flask 64.2% / cc 38.3%，比 oce-rs nollm 高约 +6/+2.5 分。
  差距构成里本仓可对齐的（元目录、合并）已落地且基准面中性，剩余差距来自
  semble 的 BM25 通道与 z-score 融合等架构性差异——本仓已裁掉 BM25（dense-only
  路线），不回退。

## Alternatives considered

- **元目录降权放进 `source_priority_factor`（纯路径函数）**：否——豁免需要查询
  语境，纯路径函数会把 CI 意图查询（Q53）也降权。泛型闭包改动小且主链路清晰。
- **span 合并在路径增强路同样生效**：暂缓。路径定位查询（「xx 文件在哪」）要的
  是精确 span，合并/补全的收益面在探索/概览消费；先观察主路 A/B 与真实使用。
- **LateOn-Code-edge 本地重排器**：不吸收。16.8M ColBERT ONNX CPU ~110ms/查、
  CosQA +8.1pt，但无 CJK 训练（中文实测 -10，semble 自带 contains_cjk 门）——
  oce-benchmark 200 题全中文，接入后基准面恒为 no-op；需要 ort 依赖，等真实
  英文场景需求再立项。
- **PathSemanticIndex**：不吸收——semble 是在补齐 oce `path_doc.rs` 的对等能力
  （其注释自述 "OCE path_doc.rs parity"），方向相反。
- **broad 触发加 dense 弱头门（semble 的 top-1 cosine < 0.20 路线）**：暂缓。
  oce ④ 已交付 intent + 词表 + locator 否决闸路线且验证过；cosine 弱头可作为
  未来增强（与词表 AND），待真实工作区数据再议。

## Consequences

- 收益：CI/模板噪声文件在普通代码查询中让位（flask .github 入窗 24→14）；
  小 scope 工作区的碎片化输出合并为连续可读段；行号真实性有逐行校验兜底。
- 代价：每次检索多一次 ≤8 个 blob 的行文本查询（仅 flag 开启时）；两个新 flag
  的默认关闭面（未验证为默认开的收益前不动的既定纪律）。
- 验证：`cargo test --workspace` 143 通过（meta 因子 3 例、span_merge 6 例、
  集成 2 例）；clippy 零 error。四组 A/B 报告落 `oce-benchmark/results/
  *-rs-metapen-{off,on}-nollm.md`、`*-rs-spanmerge-{off,on}-nollm.md`。
- 事故预防（semble 同日事故的镜鉴）：其 `GIT_EXTERNAL_DIFF`（sem）接管 diff 后
  一次 `git checkout --` 丢失整个工作区——本仓同装 sem 技能，取「改动前」状态
  一律 `git worktree` 或先导 patch。
