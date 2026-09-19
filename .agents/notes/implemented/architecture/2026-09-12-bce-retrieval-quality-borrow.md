# Agent Note: BCE 检索策略借鉴四件套：file description enrichment、related symbols、rerank 截断+近重复抑制、broad mode

Status: implemented（①②③④ 全部实施完毕。④ broad mode：nollm 档 A/B 零回归零差异——基准集不含探索型查询，触发面分析见下；机制探针验证 regime 按设计工作，默认关待真实使用验证。③ 近重复抑制 nollm 验证通过、分数截断经用户裁决不适用 LLM rerank 主链路）

## Problem

BCE 调研（见 [bce-robustness-borrow](../../implemented/architecture/2026-09-12-bce-robustness-borrow.md)）确认了四块**动排序/召回语义**的能力值得吸收，但全部必须走 oce-benchmark A/B 验证——本仓库已有 camelCase 拆词、intent 单开、图扩散、HyDE 四例「设计合理、实测负收益」的前车之鉴。本 Note 钉住实施方案、验证协议与接受/拒绝标准，作为后续实施的契约。

### 现状基线（2026-09-09/09-06 实测，oce-benchmark 双仓各 100 题）

| 配置 | flask | cc-switch | 备注 |
|---|---:|---:|---|
| 完全体（API 嵌入 + LLM 全开）v12 | 114.67 (57.3%) | 111.39 (55.7%) | 2026-09-09 |
| 完全体（更早一轮） | 171.53 (85.8%) | 76.34 (38.2%) | 2026-09-06，flask 异常高（当日配置不同） |
| nollm 基线 v13b | 114.35 (57.2%) | 49.23 (24.6%) | 静态/本地嵌入 + 混合检索 |

基线取 **v12-full**（flask 114.67 / cc 111.39）为对照锚点；每次实验前重跑一次同配置基线（LLM 输出有随机性，±2 分视为噪声带）。

### 各能力针对的实测痛点（v12-full 锚点的分类明细）

| 类别 | flask v12 | cc v12 | 主要受益项 |
|---|---:|---:|---|
| file_exact_match | **0/10** (0.183) | 2/10 (0.200) | ①③ |
| configuration_lookup | 4/10 (0.642) | 5/10 (0.500) | ① |
| cross_language | **3/10** (0.407) | 9/10 (0.752) | ① |
| architecture_understanding | 7/10 (0.784) | **4/10** (0.400) | ④ |
| call_chain | 8/10 (0.890) | 7/10 (0.652) | ②（hints） |

1. **file description enrichment**（BCE `filedesc.go` 规则层）：flask `file_exact_match` v12 只有 **0/10**——Q01「Python 包配置文件在哪里」pyproject.toml 不在 top-10；Q52「许可证文件在哪里」0 分（LICENSE.txt 不在窗口）；Q57「锁文件」0 分。根因：清单/配置文件的**内容**（TOML 键值、依赖列表）与中文查询零词法重叠，dense 也够不着。OCE 当前 `embedding_text = "File: {path}\n\n{content}"`，无任何桥接句。
2. **related symbols hints**（BCE `relatedSymbolHints`）：call_chain 类查询期望「前端 api 封装 + Rust command 实现」跨语言成对出现，cc-switch call_chain 完全体 7/10——一跳结构关联（引用的定义、调用者）靠 rerank 语义猜。OCE 已有 `symbol_occurrences` 倒排（identifier → 定义位置），基础设施现成。
3. **rerank 动态截断 + 近重复抑制**（BCE `rerank.go` cutoff + `curate.go` dupBasenameCap）：flask Q52 期望 LICENSE.txt（根目录），返回的前 10 里混入 examples/javascript/LICENSE.txt、examples/tutorial/LICENSE.txt——**同名近重复文件挤占窗口**（flask 基准里 `__init__.py`×3 路径、`views.py`×3 路径、`app.py`×2 路径）。rerank 后无悬崖截断，弱噪声全数进入 select。
4. **broad mode**（BCE `broad.go`）：architecture_understanding 类（「sansio 抽象层和完整实现层怎么分工」）是**答案散布在多文件**的探索型查询，focused 调参（tight 截断、10 hits、全文摘录）天然饿死这类查询。OCE 的 Overview intent 目前只有 `boost_docs` 一个开关。

## Proposal

四项按依赖顺序分四个独立 PR/commit 实施，**每项独立 A/B**，任何一项负收益即回退该项（保留 Note 记录实测数据）。

### ① file description enrichment（规则层，零 LLM 成本）

**实施状态**（2026-09-12）：代码已落地，默认关；**nollm 档 A/B 已验证，双仓正收益**（详见下万实测数据）。
- `oce-core/src/file_desc.rs`：规则表全量移植（清单名→桥接句 30 项、`<tool>.config.<ext>` 14 项、README 首段/changelog/tsconfig/CI workflow/全局样式表/schema SQL/入口点约定）；单测 8 例。
- `embedding_text`（indexing.rs）：`"File: {path}\n{desc}\n\n{content}"`，描述非空时注入；README 首段从 staging 全文提取（同 blob 所有 chunk 共享）。
- rerank 双注入：`ApiRerankAdapter` 文档头 + `LlmRerankerImpl` 候选 `description` 属性（与嵌入同源）。
- 模型指纹：`{model_tag} dim={dim} etext=v{1|2}`。**条件式迁移**（对计划「旧索引 fail-closed」的精确化）：开关关闭时写 `etext=v1`，旧格式 sidecar（无 etext）与之语义等价（都是无描述注入）散免重建——否则默认关闭的开关会强迫全部用户零语义变化重嵌，破坏 A/B 基线复用；开关开启时写 `etext=v2`，旧格式/`etext=v1` 一律 fail-closed（与换模型同语义）。反向（v2 索引存在时关开关）同样 fail-closed，不留混用窗口。
- 开关：`RETRIEVAL_FILE_DESC_ENABLED`（默认 false），`.env.example` 与 `oce init` 模板已补。
- 测试：workspace_embedded 新增 2 例（描述注入命中 + 指纹 fail-closed，注意 TriviumDB Rom 模式懒建 .tdb——指纹检查需实际写入过数据才生效）；全 workspace 86 通过。

**nollm 档实测**（2026-09-12，potion-multilingual-128M 静态嵌入，独立临时索引目录，报告落 `results/*-rs-filedesc-on-nollm.md`）：

| 仓 | v13b 基线 | filedesc-on | delta |
|---|---:|---:|---:|
| flask | 114.35 (57.2%) | **122.51 (61.3%)** | **+8.16** |
| cc-switch | 49.23 (24.6%) | **71.53 (35.8%)** | **+22.30** |
| 合计 | 163.58 | 194.04 | **+30.46** |

分类要点（vs v13b）：flask file_exact_match 1.83→4.50、configuration_lookup 10.42→12.08、architecture 14.84→16.26、error_handling 13.56→15.64；cc-switch file_exact_match 0.50→3.20、call_chain 3.21→8.02、symbol_location 13.10→17.38、cross_language 8.74→10.39。靶点题直接命中：flask Q01「Python 包配置文件」pyproject.toml 从未入窗→Top-1；cc-switch Q01 Cargo.toml 未入窗→第 2 位、Q02 package.json 第 2 位→Top-1。
已知未解决：flask Q52（LICENSE.txt——法律文件无规则覆盖且 source_priority 0.1 降权）、Q57（uv.lock——`.lock` 在索引忽略列表，属准入策略问题）。flask cross_language 7.08→6.17 微降（-0.91，噪声带内）。
nollm 档接受标准（双仓总分 ≥ 基线 + 2 且无单类跌超 3 分）：**通过**（最大单类降幅 -1.59，远在阈值内）。

**改动点（原计划）：**
- `oce-core/src/file_desc.rs`（新增）：移植 BCE `fileDescByName`/`fileDescByConfigStem`/`firstParagraph` 的规则表——已知清单文件名 → 英文桥接句（`pyproject.toml` → "Python project manifest: build system, dependencies and tool configuration"），README 首段复用，`<tool>.config.<ext>` 识别，全局样式表/入口点约定。**只做规则层**，LLM 摘要层（BCE `summarize.go`）不做——个人模式的 API 成本敏感，且规则层已覆盖基准里 12/100（flask）与 48/100（cc-switch）的清单类期望文件。
- `embedding_text()`（`indexing.rs`）：`"File: {path}\n{description}\n\n{content}"`——description 非空时插入 path 与正文之间。
- rerank 文档（`ApiRerankAdapter`/`LlmRerankerImpl` 的 document text）：同样注入 description。
- **模型指纹 sidecar 版本化**：`model_fingerprint` 从 `{model_tag} dim={dim}` 扩为 `{model_tag} dim={dim} etext=v2`。旧索引（无 etext 标记）启动时 fail-closed 提示重建——与换模型同语义。静态嵌入重建零 API 成本；API 嵌入用户需重嵌（文档注明）。

**验证：**
- 基准：flask + cc-switch 双仓，nollm 与 full 两档。重点看 file_exact_match / configuration_lookup / cross_language 三类。
- 接受标准：双仓总分 ≥ 基线 + 2 分（噪声带外）且无单类跌超 3 分；或总分持平但 file_exact_match + configuration_lookup 合计 +4 分以上。
- 已知风险：`.lock` 后缀在 `source_filter` 的忽略列表里（uv.lock 不进索引）——Q57 这类「锁文件」题 enrichment 救不了，属准入策略问题，不在本项范围（另立讨论：lock 文件是否该豁免索引）。

### ② related symbols hints（输出层追加，不动排序）

**实施状态**（2026-09-12）：代码已落地，默认关；**nollm 档无回归验证通过**（flask ±0.00 / cc +0.17，门槛 ≥ 基线 - 2）。
- `oce-core/src/related.rs`（新增）：`ident_tokens`（BCE identTokens 移植，多字节安全）+ `related_symbol_hints`（频次降序、上限 8、fanout>15 门控、窗口内定义排除、停用词过滤）。单测 9 例。
- 端口：`ExactSearchStore` 加 `find_definitions(identifiers, scope) -> Vec<SymbolDefinition{identifier, kind, path, file_fanout}>`（默认空实现，端口可选）；SQLite 实现走 `symbol_occurrences` JOIN blobs，子查询 DISTINCT 后窗口函数算 fanout（SQLite 窗口函数不支持 COUNT DISTINCT，先去重再 COUNT；fanout 按 (identifier, blob_name, kind) 去重计数，同一 blob 多 chunk 定义不膨胀），endpoint 优先 + LIMIT 500（只截行不截标识符，窗口函数全量计算后排序，不影响门控）。集成测试 4 例（fanout 计数/门控边界、scope 隔离、endpoint 优先、超限 scope）。
- 管线：`RetrievalPipeline::attach_related_symbols`（select 后旁路，标识符来源 = 选中 hits content + 查询本身；失败静默跳过），结果写 `audit.related_symbols`；HTTP 面（service.rs）与 MCP 面（workspace.rs）共用 formatter。
- formatter：`format_retrieval_full(hits, notes, related)`——`<related_symbols hint="...">` 块追加在 sections 之后，属性转义；空 sections + 有 hints 时仍输出（罕见：窗口被 confidence floor 滤空）。既有 `format_retrieval`/`format_retrieval_with_notes` 签名不变（related 默认空）。
- 停用词过滤（实测驱动的补充）：symbol 提取的宽松正则会命中 docstring 里行首的 "Subclass and has..."（identifier="and"）与解构赋值伪定义（"added"/"active"），这些伪定义在 exact 召回里被分数体系淹没但在 hints 里直接可见——停用词表（英文常见词 + 语言关键字 + Rust/TS 高频短名 err/ok/command/format 等，大小写不敏感）在 hints 侧拦截，不动索引语义。
- 已知限制：fanout 门控的信号是「定义处文件数」非「引用处文件数」（BCE 是后者；OCE 的 symbol_occurrences 只记定义不记引用），定义少但引用广的通用词靠停用词表兑底，长尾噪声（hermes/claude 这类项目专名被误提）仍可能出现在 hints 里——hints 是辅助探索信号，不追求零噪声。
- 开关：`RETRIEVAL_RELATED_SYMBOLS_ENABLED`（默认 false），`.env.example` 与 `oce init` 模板已补。
- 测试：workspace_embedded 新增 2 例（hints 端到端 + 默认关闭无块）+ exact_definitions 集成 4 例；全 workspace 103 通过。

**nollm 档无回归实测**（2026-09-12，单仓干净索引，file_desc=on + related=on vs ① 的 filedesc-on）：

| 仓 | filedesc-on（① 基线） | +related=on | delta |
|---|---:|---:|---:|
| flask | 122.51 | **122.51** | ±0.00（逐题得分零差异） |
| cc-switch | 71.53 | **71.70** | +0.17（Q42/Q43 尾部顺序微调） |

验证门槛（≥ 基线 - 2，防实现 bug 拖累主链路）：**通过**。
方法论教训：首轮对照跑在双仓混合索引上（flask 230 + cc 1031 blob 同一数据目录），出现 -2.48 假回归（Q01 pyproject.toml 被 cc 噪声挤出窗口）——A/B 必须单仓干净索引，混合索引的 BM25 词法分布会污染对照。已重跑确认。

**改动点（原计划）：**
- `oce-core/src/related.rs`（新增）：对最终选中的 hits，从其 content 提取标识符（复用 `symbol.rs` 的 identTokens 逻辑），查 `symbol_occurrences` 找「被引用但定义不在窗口内」的符号，输出 `RelatedSymbol { name, kind, path }` 列表（上限 8，BCE 同款）。带 fanout 门控：被 >15 个文件引用的通用符号（String、Result 之类）不进 hints。
- formatter：`<related_symbols>` 块追加在 sections 之后——评测脚本只解析 `Path: ` 行，**不影响评分**；价值在 agent 消费面（grep leads）。因此本项**不设基准接受门槛**（预期 ±0），验证点是：单测覆盖（hints 正确性、fanout 门控）+ 双仓基准无回归（≥ 基线 - 2 分即算过，防实现 bug 拖累主链路）。
- 端口：`ExactSearchStore` 协议加一个 `find_definitions(identifiers, scope) -> Vec<(identifier, kind, path)>`（SQLite 实现走 `symbol_occurrences` 索引查询，kind=endpoint/definition 优先）。

### ③ rerank 动态截断 + 近重复抑制

**实施状态**（2026-09-13，终态）：代码已落地，默认关。**近重复抑制（select 层）：nollm 档 A/B 验证零回归，为 ③ 的实际交付物**——它不依赖 rerank 分数，nollm/full 两档均生效。**分数截断（rerank_cutoff）：经用户裁决不再验证**——full 档主链路是 LLM rerank（RERANK_ENABLED 的 API rerank 已被取代、默认关），LLM rerank 只返回顺序不产生校准分，cutoff 对它不适用；且 LLM rerank 的 prompt 本身已要求 "If fewer than {top_k} are relevant, output fewer — do not pad"（模型自主少返回 = 语义截断），返回不足时的补齐逻辑等价于 min_keep。cutoff 代码保留为 API rerank 通道的增强（该通道存在时零成本生效），不作为 ③ 的验收项。
- 截断（`retrieval.rs`）：`rerank_cutoff()` —— head×0.35 与绝对下限 0.10 双线取 max，最少保留 6 条（BCE 原值）。位置在 API rerank 之后、source priority 之前（判据是纯端点分数，不与路径降权因子纠缠）。
- **打分边界显式化**（关键实现决策）：`Reranker` 协议返回值从 `Vec<SearchHit>` 改为 `RerankOutcome { ranked, unscored }` —— 只有 `ranked` 携带端点校准分，`unscored`（未被端点返回的候选）保持融合分。悬崖截断只判 `ranked`；`NoopReranker`/保序回退全部走 `unscored`，天然不触发截断。这比「在调用侧猜哪些分数是校准分」可靠——OCE 的 LLM 重排只返回顺序不返回校准分（④ 已否决 broadEngageTop 的同一理由），融合分与校准分同域但语义不同，混合列表无法判悬崖。
- 近重复（`selector.rs`）：select 循环内加 `DupGuard` —— 同 basename（小写）分桶记录已入席内容的 (content_hash, 行集合)；字节级相同（content_hash 相等）直接拦，行集合 Jaccard ≥ 0.7 且桶已满 2 席（BCE dupBasenameCap）拦。两轮共用同一 guard，第二轮补齐不放宽（BCE 教训）。
- 与 source_priority 的交互：近重复在 select 层做，与 priority 排序独立；LICENSE.txt 主文件 0.1 降权仍在 priority 阶段生效（Q52 的根 LICENSE 排不进头部是准入/降权问题，近重复抑制只保证 examples/*/LICENSE.txt 拷贝不重复占席）。
- 测试：retrieval 7 例（截断双线/下限接管/min_keep 保底与回填/零头不截断/line_set 语义/Jaccard 0.7 边界）+ selector 4 例（字节级拷贝永不占第二席/近拷贝变体限 2 席/同名不同文不误伤/第二轮不放宽）；全 workspace 114 通过。
- 已知边界：`ApiRerankAdapter` 只把端点返回的 top_n 条放进 `ranked`（窗口外的候选全部 `unscored`），截断只作用于这个窗口内 —— 窗口大小由 RERANK_TOP_N 控制，与 LLM 重排候选窗对齐。

**nollm 档 A/B**（2026-09-12，potion-multilingual-128M 静态嵌入，单仓干净索引，基线 = ①② 的 filedesc-related-on-nollm）：

| 仓 | 基线 | rerankcut-on | delta |
|---|---:|---:|---:|
| flask | 122.51 | **122.59** | +0.08（仅 2 题尾部微调 ±0.04） |
| cc-switch | 71.70 | **71.70** | ±0.00（逐题零差异） |

结论：**近重复抑制无回归**（门槛 ≥ 基线 - 2）。截断不生效（nollm 无 API rerank）——本档实际只验证近重复抑制，符合预期。

**full 档 A/B**（2026-09-13，minimax-m3 LLM + Qwen3-Embedding-8B）：

| 配置 | flask | cc-switch |
|---|---:|---:|
| rerankcut-off | 145.28 | 108.25 |
| rerankcut-on | 153.71 | 104.29 |
| delta | +8.44 | -3.96 |

**这组数据不构成 ③ 的 A/B**：`RERANK_ENABLED=false`（API rerank 已被 LLM rerank 取代，默认关）→ cutoff 是死开关，ON/OFF 跑的是同一代码路径。差异 100% 来自 minimax-m3 LLM rerank 的非确定性输出（28/100 题涨跌互现，±2 分级抖动；flask +8.44 / cc -3.96 是噪声幅度，不是 cutoff 效果）。**副产发现：LLM rerank 随机性比预想大，单次 full 档基准的噪声带可达 ±8 分——full 档 A/B 结论必须同配置双跑以上或用固定种子。**

**用户裁决（2026-09-13）**：full 档就是 LLM rerank，API rerank 通道已被取代且效果更差；cutoff 依赖端点校准分，在 LLM rerank 主链路上无用武之地——不再为验证 cutoff 而启用 API rerank。LLM rerank 的截断语义由 prompt 的 "output fewer — do not pad" + 补齐逻辑天然承担。

**附带修复**（full 档实验过程中发现，已合入）：
- LLM 客户端改 SSE 流式（`openai/llm.rs`）：非流式 + 思考模型未关思考时，单次调用生成几分钟思考链而客户端零字节等待，120s 总超时先到 → 重试双重烧配额。流式后首块秒到，块间超时（timeout_seconds）替代总超时，思考进度可见。
- 思考模型关思考按模型分派：Qwen3 系 `enable_thinking=false`；MiniMax-M3 `thinking.type=disabled`（省略时思考默认开，不同字段）。探测从域名表扩展到模型名（qwen3.8-flash 这类自定义代理域名探测不到）。三态语义修正：True = 强制注入（按模型分派参数），False = 不注入（严格校验端点用），Auto = 探测。
- rerank/rewrite 调用补 `max_tokens=512`：输出本来就极短（编号列表/三行变体），思考模型场景下是第二道闸。
- 评测脚本 httpx 超时 120s → 600s（服务端流式 LLM 最坏单题可超 120s）。
- 实验教训：`LLM_ENABLE_THINKING=false` 是 qwen3.8-flash 时代的遗留配置，换 minimax-m3 后会静默阻断 thinking 参数注入——模型换了要复查思考开关配置。

**改动点（原计划）：**
- **截断**（`retrieval.rs`，LLM rerank 之后、source priority 之前）：rerank 分数悬崖截断——head×0.35 以下丢弃（BCE `rerankCutoffRatio`），绝对下限 0.10，最少保留 6 条（`rerankMinKeep`）。仅当 rerank 实际生效（LLM/API rerank 返回了分数）才截断；保序回退时不截断（无分数信号）。
  - 实施修正：原计划的「LLM rerank 返回了分数」不成立——OCE 的 LLM 重排只返回顺序子集（prompt 要求编号列表），不产生校准分。截断只对 API rerank（RERANK_ENABLED，Jina/Cohere 风格 /v1/rerank 端点）生效；这是与 BCE 架构差异（BCE 主重排层就是 API reranker）的必然结果。
- **近重复**（`selector.rs` CoverageSelector）：select 循环里加同 basename 判定——同 basename 且行集合 Jaccard ≥ 0.7 的候选限 2 席（BCE `dupBasenameCap`）；字节级相同内容（content_hash 相同）直接跳过。第一轮（保覆盖）与第二轮（补齐）都执行近重复门控，但第二轮放宽 per-file cap 时**不**放宽近重复 cap（BCE 教训：回填同质内容是纯浪费）。
- 注意与 `source_priority_factor` 的交互：LICENSE.txt 主 README 不降权、多语言 README 0.2——近重复抑制在 select 层做，与 priority 排序独立。

**验证：**
- 基准：双仓 full 档为主（截断依赖 rerank 分数）。重点看 file_exact_match（flask Q52 类：根 LICENSE.txt vs examples/*/LICENSE.txt）与 nDCG@10（窗口被近重复挤占的直接指标）。
- 接受标准：双仓总分 ≥ 基线 + 2 分；或总分持平且 file_exact_match + nDCG@10 双升。若 flask 升 cc 降（或反之），按「无单类跌超 3 分」仲裁。
- 风险：cc-switch 的 `mod.rs`×4、`provider.rs`×5 是**同名但内容不同**的真实现（不是拷贝）——Jaccard 0.7 门控必须只压「内容重叠」不压「同名不同文」，单测必须覆盖此判据。

### ④ broad mode（Overview/架构类查询专用 regime）

**实施状态**（2026-09-13）：代码已落地，默认关（`RETRIEVAL_BROAD_MODE_ENABLED`）。**nollm 档 A/B 零回归零差异**（flask 122.56=122.56 / cc-switch 71.70=71.70，逐题窗口零变化）；**机制探针验证 regime 按设计工作**（见下）。基准集 200 题中不存在 broad 的目标查询形态——「architecture +4」验收标准在该基准集上结构性不可测，见「触发面分析」。

- `oce-core/src/broad.rs`（新增）：BCE broad.go 移植 + 两处数据驱动修正。`query_wants_structure`（架构词表，BCE structuralIntentTerms 中英双语，裸「配置」缺席）、`is_locator_query`（定位语气否决闸，**BCE 无此表**——OCE 修正）、`is_manifest_path`（BCE manifestNames 全表 + application.java 后缀 + `.config.`/tsconfig + 全局样式表）、`manifest_prior_list`（深度优先排序 + 同 basename 限 2 席，**BCE 为纯字典序无 cap**——OCE 修正）、`skeletonize`（头 12 行 + 查询词命中行 ≤10，≥3 行省略段收拢为标记行引用真实行号区间）、`skeleton_terms`（BCE tokens 移植：snake/camel 拆词、单字符丢弃）、`elided_run`（标记行解析，formatter 行号重同步用）。单测 7 例。
- 触发（`retrieval.rs`）：`broad_mode_enabled && !is_locator_query && (LLM intent == Overview || query_wants_structure)`。**定位语气一票否决对 LLM Overview 分支同样生效**——「某个东西在哪」有明确目标，不是探索型，与触发源无关。
- regime：`selector_broad`（per-path = min(用户设置, 2)，覆盖优先于深度）、`BROAD_FINAL_SELECT_K=20`、manifest prior 作为一路 facet 权重结果表进 RRF（候选来自语义召回池）、select 后骨架化（在 related symbols 之后——hints 标识符取自全文，省略段不丢信号）。
- formatter：`RetrievalNotes.broad` 提示（BCE 同文案）+ 标记行行号重同步（省略段之后的行恢复真实行号，标记行原样无行号）。锚定 section 自身 path 防内容行误撞。
- 审计：`RetrievalAudit.broad` → service/workspace 两面共用。
- 测试：broad 单测 7 + formatter 4（broad 提示/重同步/误撞防护）+ workspace_embedded 3（默认关不触发/触发时 20 窗口 + manifest 入窗 + 骨架化 + 行号重同步/focused 查询不误触发）；全 workspace 132 通过。

**实施修正（均数据驱动，与原计划/BCE 的偏差）**：

1. **定位语气否决闸（flask Q05 教训）**。首轮实现按 Note 原契约（Overview ∨ 词表，无否决），flask nollm A/B 即出损伤：Q05「从带前缀的环境变量批量加载配置（FLASK_ 开头变量）的实现代码在哪里？」——「环境变量」命中词表，但这是定位型查询；manifest prior 把 5 个 pyproject.toml（根 + 4 个 examples 子项目）全拉进窗口顶部，答案 src/flask/config.py 从 #1 掉到 #6，Top-1 得而复失（2.00 → 0.36，总分 -1.65）。BCE 用 rerank 强头部把这类查询挡在 broad 之外，OCE 无校准分；启发式意图分类也救不了（「实现」是 overview 关键词，优先级高于「在哪里」的 PATH 判定，Q05 被分类为 Overview）。最终用表面形状的定位语气标记（在哪/哪个文件/哪些文件/定义在/实现在/where/which file/what file）一票否决，对 LLM 分支同样生效。
2. **manifest prior 深度排序 + 同 basename 限 2 席**。BCE 的 boost 同值候选按 path 字典序，monorepo 里 examples/* 子项目清单与根清单同权重，照样泛滥。修正后根清单最浅排最前、同名清单最多 2 席——先验的语义本来就是「项目」的清单，不是「目录形状」。
3. **骨架化行号真实性**。BCE format 用 `<file>` CDATA 无逐行行号，骨架化行号漂移无感；OCE formatter 逐行编号，标记行之后的行号必须重同步（否则省略段之后全部行号漂移，"read path:start-end" 的引用价值归零）。实现为 formatter 感知标记行（`elided_run` 解析 + 锚定自身 path 防误撞），pipeline 只生成标记。

**nollm 档 A/B**（2026-09-13，potion-multilingual-128M 静态嵌入，单仓干净索引，file_desc/related/rerankcut 全开的 ③ 终态配置；OFF 基线复现：flask 122.56 vs 记录 122.59、cc 71.70 vs 71.70，偏差 ≤0.03）：

| 仓 | broadmode-off | broadmode-on | delta |
|---|---:|---:|---:|
| flask | 122.56 | **122.56** | ±0.00（逐题窗口零变化） |
| cc-switch | 71.70 | **71.70** | ±0.00（逐题窗口零变化） |

**触发面分析（为什么零差异是结构性的）**：对 200 题跑词表 + 否决闸分析——命中架构词表的仅 6 题（flask 4 / cc 2），**全部是文件定位型**（「配置文件在哪里」「锁文件在哪里」），否决闸正确拦截（否则重现 Q05 损伤）；而目标类 architecture_understanding 的 20 题（两仓各 10）**没有一题是探索型**——全部是「定义在哪里/是哪个文件」式 locator（如 cc「系统托盘的实现和事件处理在哪里？」「后端所有 Tauri 命令模块的统一聚合入口是哪个文件？」）。**基准集的 architecture 类实际测的是「定位架构定义文件」，不是「探索架构」；broad regime 的目标查询形态在基准集中不存在。**「architecture +4」验收标准结构性不可测。full 档 A/B 同理无意义（LLM intent 也只会把 locator 题判 Overview，仍被否决闸拦截；跑了只测出 LLM rerank ±8 噪声——③ 的教训），故跳过。

**机制探针**（2026-09-13，flask 仓 230 blob，nollm 同配置，8 条手工探索型查询，记录落 `results/broadmode-exploratory-probe-nollm.md`）：

- 7/8 查询触发 broad（「sansio 抽象层和完整实现层是怎么分工的」未命中词表保持 focused——词表是 BCE 的栈/部署/通信面，不含「分工」类组织词，符合当前设计）。
- 宽窗口生效：hits 10 → 20（候选不足时自然截断，观测到 14/18）。
- manifest prior 方向正确：「项目的依赖管理方式和技术栈是怎样的」root pyproject.toml 从 #8 → #2、README.md 入窗且子项目清单有节制（basename cap 生效）；「整体架构」README.md #2 → #1。
- 骨架化生效：所有触发查询的长摘录出现 `... (N lines omitted, read path:a-b)`，broad note 注入。
- 对照（focused 基线）的「整体架构」查询 top-1 是 examples/javascript/js_example/__init__.py（纯噪声）——探索型查询在 focused regime 下的确被饿死，与 BCE 的动机判断一致。

**结论**：④ 按「默认关 + 探索型查询专用 regime」交付。基准评分面零影响（A/B 零差异 + 否决闸保护 locator 查询），agent 消费面对真实探索型查询有方向性收益（探针）。待真实工作区使用积累反馈后再评估是否转默认开。

**附带修复**：`chunk/jsp.rs` 的 `mask_jsp_code` 用了回溯引用（`(?P=kind)`）——regex crate 不支持，`Regex::new(...).unwrap()` 在首个 .jsp 文件切块时必 panic（clippy invalid_regex 已 deny）。展开为三种 kind 的独立正则逐个掩码，语义等价，加回归测试。

**改动点（原计划）**：
- 触发：LLM intent 分类为 Overview **或**启发式 `queryWantsStructure`（BCE 的架构词表：架构/技术栈/依赖/部署/通信/framework/architecture/deploy…中英双语）。OCE 已有 7 类 intent，Overview 已在列，只加启发式 OR 分支（无 LLM 时的 fallback）。
- regime 差异（`retrieval.rs` strategy 表扩展）：
  - `final_select_k`：10 → 20；`max_chunks_per_path`：3 → 2（覆盖优先于深度）
  - manifest prior：BCE `manifestBoost` 的清单文件名表作为一路额外 RRF 融合（package.json/go.mod/cargo.toml/Dockerfile/application.yml…），仅 broad 模式注入
  - skeletonize：选中 hits 超过 28 行的摘录压缩为「头 12 行 + 查询词命中行」，省略段标注 `... (N lines omitted, read path:start-end)`——**Path 行不变，评测兼容**
  - formatter：broad 模式注入提示「摘录已裁剪，读引用行号区间获取全文」（复用 ① 做的 RetrievalNotes 机制，加 broad 字段）
- 不做 BCE 的子查询分解（OCE 已有 query decomposition + rewrite，语义重叠）；不做 speculative prefetch（OCE 检索预算 2ms 级，无延迟对冲需求）。

**验证：**
- 基准：双仓 full + nollm 两档。重点看 architecture_understanding（flask 7/10、cc 1/10 是最大洼地）与 file_exact_match（manifest prior 的直接受益者）。
- 接受标准：architecture_understanding 双仓合计 +4 分以上且总分不降；skeletonize 不伤 nDCG（省略行不占 Path 行）。
- 风险：skeletonize 改变 agent 看到的内容形态——基准评分兼容（只看 Path），但实际使用中 agent 需要二次读文件。这正是 broad 模式的设计意图（候选列表 + 跟进读取），Note 里明确记录此取舍。

## 实施顺序与依赖

```
① file description enrichment ──┐
                                 ├─→ ③ rerank 截断+近重复 ─→ ④ broad mode
② related symbols hints ────────┘
```

- ①② 互不依赖，可并行；① 改 embedding_text 触发全量重嵌，**必须最先做**（后续实验都要在新索引上跑，避免中途换索引语义污染对照）。
- ③ 依赖 ①（rerank 文档带 description 后截断行为才稳定）；② 的端口扩展独立但验证要避开 ① 的重嵌窗口。
- ④ 依赖 ③（broad 模式的宽窗口更需要近重复抑制，否则 20 hits 全是拷贝文件）。
- 每项一个 commit + benchmark 报告落 `oce-benchmark/results/`，命名 `*-rs-{feature}-{full|nollm}.md`。

## 验证协议（所有项共用）

1. 实验前重跑基线（v12 配置），确认与锚点偏差 < 2 分；超差则更新锚点并记录原因。
2. 单变量：一次只开一个 feature flag（`RETRIEVAL_FILE_DESC_ENABLED` / `RETRIEVAL_RELATED_SYMBOLS_ENABLED` / `RETRIEVAL_RERANK_CUTOFF_ENABLED` / `RETRIEVAL_BROAD_MODE_ENABLED`，全部默认 off，实验显式开）。
3. 双仓 × 两档（full/nollm）× 100 题；nollm 档用静态多语言嵌入（potion-multilingual-128M）隔离 LLM 随机性。
4. 报告记录：总分、Top-1、nDCG、分类明细、与本 Note 基线的 delta。
5. 拒绝标准：双仓合计负收益，或单仓 -3 分以上且另一仓增益 < +3——回退代码，Note 转 rejected（含实测数据，防重犯）。

## Alternatives considered

- **LLM 摘要层（BCE summarize.go）一并做**：拒绝（首期）。规则层零成本覆盖基准痛点；LLM 层每 chunk 一次调用，个人模式成本敏感，且质量增益未证实。规则层验证为正收益后再评估。
- **BCE 的 relation expansion（一跳结构扩展：拉引用定义/调用者进结果集）**：暂缓。它与 related symbols hints 用同一基础设施，但**直接改结果集构成**（hints 只加提示）——风险高一档。② 验证 infra 可靠后再立项，届时 hints 的 fanout 门控经验直接复用。
- **broad mode 用 rerank head 弱分触发（BCE 的 broadEngageTop < 0.50）**：拒绝。OCE 的 LLM rerank 输出是「排序后的子集」不是校准分数（prompt 要求返回编号列表），head 分数不可比；用 intent 分类 + 启发式词表触发更可控。
- **近重复抑制放 rerank 层（BCE 在 rerank cutoff 里数 distinct）**：拒绝。OCE 的 select 层（CoverageSelector）已有 per-path 计数与重叠抑制的骨架，同层扩展最自然，且 nollm 档（无 rerank 分数）也受益。

## Acceptance criteria

见各分项「验证/接受标准」。终局核对（2026-09-13）：

- ① nollm 档双仓 +30.46（**远超 +2 门槛**），靶点类全升；② 零回归；③ 近重复抑制零回归（cutoff 经用户裁决不验收）；④ 基准面零差异（触发面结构性错位，机制探针方向性验证通过）。
- 汇总门槛「full 档合计 ≥ 基线 + 8 分」**不再适用**：③ 的 full 档实验发现 LLM rerank 单次噪声带 ±8 分，full 档汇总门槛需要同配置双跑以上才能判别——而 ① 的增益已在 nollm 档（确定性）充分验证，无必要为汇总门槛重复烧 full 档配额。
- 任何单项负收益可独立回退，不阻塞其余三项。④ 默认关，回退成本 = 一个 env 变量。

## Risks

- **① 的索引重建成本**：API 嵌入用户全量重嵌（flask 230 blob / cc 1031 blob，Qwen3-4B 实测全量 3-35s，成本可控但需文档提示）；静态嵌入零成本。sidecar 版本化 fail-closed 与既有换模型语义一致，用户已有一致预期。
- **③ 截断误伤**：LLM rerank 分数分布因模型而异（Qwen2.5-7B vs 更大模型），0.35/0.10/6 三个常数可能需按模型调优——先钉 BCE 原值，基准不过再扫参。
- **④ 的 broad 误触发**：启发式词表过宽会把 focused 查询带进 broad regime（每文件 2 chunk + 骨架化，深度受损）。词表从 BCE 精简（去掉「配置」这类歧义词，BCE 自己也踩过这个坑并从表里删了），基准 cross_language / semantic_feature 两类是误触发的观察哨。**实测命中了这个风险的脸**：「环境变量」「依赖」等词同样出现在定位型查询里（flask Q05），已用定位语气否决闸修复（见 ④ 实施修正 1）；而目标类 architecture_understanding 全是 locator 题、broad 无从发力——该类基准与 broad regime 的适配错位是本 Note 的结构发现，后基准若扩充探索型题目（「整体架构是怎样的」「技术栈是什么」），④ 可直接重跑验收。
- **基准集过拟合**：四项能力都直接瞄准基准类别弱点，双仓 200 题样本小。接受标准里「无单类跌超 N 分」的仲裁条款就是为此设的；若怀疑过拟合，用 jcfx 等真实工作区做人工抽查（不在本 Note 范围）。
