# Agent Note: 公开 npm 项目 open-context-engine（AnnaSuSu）借鉴分析与吸收决策

- 日期：2026-10-06
- 状态：已分析，待排期（本 Note 只钉决策与验收标准，不含代码改动）
- 输入：`../OpenContextEngine`（npm 包 `open-context-engine@0.1.2`，commit `fc48e58`，作者 AnnaSuSu）
- 结论：**吸收 6 项（其中 2 项 P0）、评测方法论 3 项（归 oce-benchmark）、明确不吸收 9 项**；
  图扩展一项受 [2026-09-21 Note](2026-09-21-structural-code-intelligence-rfc-analysis.md) 约束，
  必须先有 graph-specific query set 才能评估

---

## 〇、命名歧义（先说清楚）

被分析项目与本报**同名**（OpenContextEngine），但两者是独立实现、不同层次：

| 维度 | 公开 npm 版（本文输入） | 本报 oce |
|---|---|---|
| 形态 | 单机 stdio MCP 前置层，npm 分发，**仅 macOS/Linux** | 服务端（FastAPI + Rust），wheel/Docker，Windows 一等公民 |
| 索引 | numpy 全量内存索引，`units.json` + `vectors.npy` | Milvus 3.0 / Milvus Lite / pgvector / TriviumDB，带 payload 过滤 |
| 工作集 | 每仓库一个本机 worker，轮询 1s 全量 scan | 客户端算 blob 哈希上传，checkpoint 声明工作集 |
| 模型调用 | 远程 HTTPS（显式拒绝 localhost/loopback） | 自托管，`model_credentials` 单表 + 运维面 |
| 强项 | 检索算法簇（图扩展 / 实体打包 / 边际覆盖选择）、评测方法论 | 服务化、多租户、凭据治理、Rust/Python 双实现 |

**结论：借算法与评测，不借部署形态。** 产品层的 per-repo worker 编排、模型端点策略、
npm 打包纪律对本报要么不适用、要么归属 oce-client。

---

## 一、它的实测口径是信号，不是结论

`docs/BENCHMARKS.md` 与 `docs/eval/METHOD_COMPARISON.md` 报：同批 40 题 / 80 查询 / 4k token，
OpenContextEngine 94.79%（完整 69/80）vs `oce-ai/oce` 69.31%（26/80）。三个必须钉住的读法：

1. **那条 oce 是显式降配**：`scripts/benchmarks/run-native.py` 把 `LLM_RERANK_ENABLED`、
   `RETRIEVAL_INTENT_CLASSIFICATION_ENABLED`、`RETRIEVAL_QUERY_REWRITE_ENABLED`、monitoring
   全部关掉；而它自己那条用的是 `shared-intent-v4` 实验链路 + `/rerank-batch`。
   榜单把「同一项目两种配置」并排成两行，对外要用这张表回应时**必须加配置差异列**。
2. **开发集**：同文档自述 `purpose: "... not a held-out test"`，任务在开发过程中反复使用过。
3. 因此它只能提供「哪里可能有 headroom」的线索。**本 Note 所有吸收项沿用 BCE Note 的协议：
   每项独立 commit、nollm + full 双档 A/B、负收益即回退并留档。**

---

## 二、P0：低风险、不依赖新数据（建议先做）

### 2.1 rerank 客户端：严格校验 + 阶段截断 + 不静默删候选

**代码事实（本报）**：`src/oce/infrastructure/embed/openai_reranker.py`

- `:72` `score >= self._min_score`（默认 0.05，`shared/config/settings.py:128`）把低分候选
  **整个删掉**——下游 selector 再多预算也补不回来。
- `:70` 非法 / 越界 / 重复 `index` **静默跳过**：provider 协议漂移表现为「排序没变」，无感知。
- `:64-66` `except (httpx.HTTPError, ValueError)` 只 `logger.warning` 后回退原序，**不进 audit**；
  `domain/services/llm/reranker.py` 同样 except 后回落。
- `:106-115` `_document_text` 拼 `File:/Lines:` + 全文，**无长度上限**。

**借法**（对方 `src/retrieval/reranker.py:17-31`、`engine.py:35,83`）：

- 校验「结果数 == 请求数、`index` 唯一且 `type is int`、score 是 `int/float`（拒 bool）、
  `isfinite` 且 ∈[0,1]」；任一不符**抛错**而非跳过（对方测试把 `index: True`、NaN/Inf、
  `'.5'`、-0.1、1.1 全列为必须抛错）。
- 文档按阶段截断：embedding 侧 2200 字符 / rerank 侧 5000 字符，常量进 `settings.py`。
- `min_score` 不再删除候选，只作为 audit 标记；是否入选交给 selector 的 gain。
- 保留本报的 `relevance_score`/`score` 双字段兼容（对方没有，不要退掉）。
- 降级必须可观测：Rust 侧已有 `semantic_degraded`（`retrieval.rs`）先例，Python 侧对齐。

**落地**：`src/oce/infrastructure/embed/openai_reranker.py`、`domain/services/llm/reranker.py`、
`shared/config/settings.py`；Rust 对应 rerank 客户端。

**验收**：单测覆盖上述 5 类畸形响应（必须抛错而非静默）+ 一次 benchmark A/B 确认无回归。

### 2.2 选择器：边际覆盖增益 + 成本归一贪心

**代码事实（本报）**：`domain/services/selector/coverage_selector.py:43-66` 是**固定顺序两趟填充**
（先每文件一个、再允许同文件），预算按 `len(hit.content)` **字符**，`score` 是 RRF 融合后的单一分数：
没有 facet 维度、没有边际增益重算、没有成本归一。Rust 版 `selector.rs` 多了 basename Jaccard
去冗（比对方更细），但选择逻辑同样固定。

**借法**（对方 `cascade.py:65-103`、`batched.py:183-209`、`engine.py:130-154`）：

- 每步 `gain = .7 * mean(values / (1 + covered)) + .3 * base`，再除以 `(max(120, cost)/300) ** .35`，
  低于阈值停止——「共同满足多个 facet」的证据优先于单点高分，长片段不能靠体量吃预算。
- facet 区分度**不再多打模型**：用已存向量对 facet 做逐列 soft-max 温度亲和度
  （`exp(min(0, d - max_col) / T)`，对方 T=.06）。这正好补上本报
  `retrieval.py:437` 之后「facet 只进 RRF、选择阶段看不见 facet」的断层。
- **常数不照抄**（见第六节）：复刻目标函数形状，T / 权重 / 阈值在 oce-benchmark 上标定。
- 预算单位：本报是字符（`max_context_chars`），不要为对齐引入 tiktoken；若要 token 预算，
  只在 API 能给出真实 token 计量时做。

**落地**：`selector/coverage_selector.py` + `rust/crates/oce-core/src/selector.rs`；
输入需要 per-query 分数 → `SearchHit` 增字段或旁路 `dict[key, list[float]]`
（`domain/services/search.py`）。

**验收**：单测（预算硬约束、overlap 抑制、facet 覆盖单调性不退化为纯分数排序）+ benchmark A/B。

### 2.3 候选名额保留（把 CALL_CHAIN 的特例泛化）

**代码事实（本报）**：`retrieval.py:386`（`_merge_exact_hits`）与 `:455`（`_fuse`）在 rerank
**之前**就按 `default_top_k` 平截——图/精确召回的候选没有保护名额。唯一例外是 CALL_CHAIN
分支手工给 exact-only 留 1/3（`retrieval.py:348-374`），思路对但只在一处。

**借法**（对方 `batched.py:85-108` 的 `retain_candidates`）：核心候选与结构/精确发现分池，
后者按锚点分 × 边置信度排序占固定名额，再合并。**只做名额保留，不引入「图候选无模型分直接进输出」**
（见第六节第 6 条）。

**落地**：`domain/services/retrieval.py` 的 `_fuse` / `_merge_exact_hits`；Rust 同址。

**验收**：CALL_CHAIN 与 SYMBOL 意图的 benchmark 分类分不降，REFERENCE 类目观察。

---

## 三、P1：结构与一致性（中等工作量）

### 3.1 静态关系与图扩展召回 —— 受既有裁决约束

**代码事实（本报）**：

- 只有正则抽取的 `symbol_occurrences`（`infrastructure/persistence/models.py`，kind ∈
  endpoint/definition/reference），**一条边都没有**；`SearchHit` 无 symbol/kind/owner。
- `domain/services/retrieval_strategy.py:25-26` 的 `enable_multi_hop` / `enable_reference_graph`
  声明了但**全仓无人读取**（已 grep 确认）——AGENTS.md 明令「不保留未接入 composition root
  的占位实现」，这两个开关要么接上、要么删掉。
- `rust/crates/oce-core/src/related.rs` 只追加 `<related_symbols>` 提示块、**明确不动排序**。

**对方设计**（`src/retrieval/languages/schema.py`、`python.py`、`batched.py:20-33`）：

- 7 类 typed relation：`calls / member_of / inherits / implements / same_symbol / imports /
  references_type`，每条带 `confidence` + `resolution`（`syntax` / `static-name` /
  `compiler-symbol` / `unresolved+原因`）。
- 图扩展**只在 execution 边上做**：`calls` / `same_symbol` / function↔function 的 `member_of`，
  刻意避开 type/import hub；种子取各 facet top-3，每种子有界邻居，带 provenance。
- 索引期硬校验值得抄：`text` 必须逐字等于 `'\n'.join(source[start-1:end])`、span 不重叠、
  **所有非空源码行必须被覆盖**、`edges` 必须是 relations 的精确投影。

> **裁决前置条件（必须先读 2026-09-21 Note）**：本报已实测过一次图扩散召回
> （TriviumDB SA-PPR `expand_depth=1`，**-32.89 分**，159.60→126.71），并明确
> 「验证 graph 价值前必须先建 graph-specific query set（multi-hop / cross_language /
> interface impact）」。本项因此**不直接实现图召回**，顺序是：
>
> 1. 建 graph-specific query set（否则大概率测出假阴性）；
> 2. 先在 `related_symbols` 提示通道加 import 关系（零评分风险的最小实验）；
> 3. 有正信号后再做召回期扩展，且 **intent 门控（仅 REFERENCE/CALL_CHAIN）+ 权重 0.1 起调**，
>    遵守 bounded expansion 四约束（max_depth / max_nodes / fanout cap / edge kind allowlist）。

**落地（真做时的归属）**：抽取 `domain/chunk/`（新 `relation.py` 或扩展 `astchunk`）→
**新建 per-blob 关系表**（跨文件关系不能挂 content-addressed chunk；`models.py` + alembic 迁移）
→ `retrieval.py` 融合前加一路；Rust `oce-core` 新增 `relation.rs`、`retrieval.rs` 接入、
`strategy.rs` 接上两个开关。

### 3.2 短函数 bundle 与符号级实体聚合

**对方的两个机制**：

- **短函数 bundle**（`batched.py:35-83,189-209`）：≤256 token 的函数族整体作为一个 action，
  可沿 `calls` 调用者补 ≤2 层、整组 ≤768 token，**只按新增 token 计价**，trace 标
  `contextOnly` / `selectionAnchor`。其自报覆盖 90.14%→94.03%，代价延迟 +0.75s。
- **符号级实体聚合**（`entities.py`）：同 `(symbol, kind)` 的多 span 聚成一个候选（max-pool），
  长函数用「与查询最匹配的片段」拼代表文档，超大实体退化回原子 span。

**本报缺口**：一个长函数的多个 chunk 是彼此独立的检索单元，无法表达「这个符号被命中了」；
`span_merge.rs` 只做**行级**相邻合并与小片段补 3 行，不是符号级完整化。

**落地**：依赖 3.1 的 symbol/owner 元数据；宿主 `selector/coverage_selector.py`（bundle 作为
action）+ `retrieval.py`；`SearchHit` 需要 symbol/owner。

**验收**：先在 SYMBOL / CALL_CHAIN 类目看分类分；bundle 带来的延迟写入 audit（对齐对方
「诚实记录延迟回退」的做法）。

### 3.3 索引身份：chunker/parser 指纹 + embedding 身份（含一处现有缺陷）

**代码事实（本报）**：

- `ChunkModel`（`models.py`）只有 `content_hash` 主键 + `embedded` 布尔位，**没有 chunker /
  parser 版本字段**；chunk 内容寻址跨 blob 复用。
- Milvus collection 名固定 `oce_chunks`（`settings.py:53`），维度同时存在于
  `MILVUS_DENSE_DIM`（`:58`）与 `EMBED_DIMENSIONS`（`:85`）。
  → **换同维度模型时旧向量被静默复用、不重嵌**；换维度要靠人工 drop collection。
- `infrastructure/milvus3/client.py:18-31` 对超长内容**静默截断到 65535 字节**（返回
  `truncated` 标记但主链路不消费）——与「排除项必须带 reason」的理念相反。

**借法**（对方 `live.py:58-66,174`、`languages/__init__.py:23-42`、`engine.py:165-170`）：

- 索引身份 = 快照 + 解析器/分块器源码指纹与版本 + 语言选项 + embedding
  `{provider, model, dimensions, revision}`；任何一项变化即整体失效（fail-closed），不混用旧向量。
- 分块完整性硬校验：逐字等于切片、非空行全覆盖、span 不重叠（新 `domain/chunk/validate.py`，
  在 `IndexingPipeline.ingest` 后调用）。

**落地**：`domain/services/indexing.py`、`domain/chunk/router.py`（导出 manifest）、
`infrastructure/persistence/models.py` + alembic 迁移、`infrastructure/milvus3/schema.py`、
`shared/config/settings.py`；Rust 对齐。**注意**：Milvus 写入与 SQL `embedded=true` 无跨存储事务，
落地时配一个按 `(content_hash, identity)` 对账的补偿扫描。

### 3.4 查询新鲜度语义

**代码事实（本报）**：`/agents/codebase-retrieval` 对「刚上传、还没 embed 完」的 blob 没有等待
与显式失败语义——不命中就当空。唯一 503 是「完全没有可用 embedding 凭据」
（`api/router.py`）。`blob-status` 只回答 unknown/nonindexed，不含错误类型与进度。

**借法**（对方 `mcp.mjs:29-30`、`live.py:110-140`、`retrieval-server.py`）：请求体加可选
`freshness_wait_ms`；响应加 `index` 块 `{mode, scope_size, pending, failed, ready,
last_error_type}`；范围内有 pending/error 时返回可重试 503。**绝不把 pending blob 的缺失
当正常空结果。**

**落地**：`api/router.py`、`api/schemas.py`、`application/queries/search.py` 与 `status.py`；
客户端等待策略属 oce-client。

---

## 四、评测方法论（记决策，实施在 oce-benchmark 仓）

对方真正的产出是**一条可机器校验的证据链**（`protocol → snapshot → queries → answers.vN →
freeze → engine-freeze → report → scores.vN`）。三项值得搬：

1. **线级 evidence-unit 真值 + 覆盖率**：答案 = 若干 `unit(fact)`，每 unit 若干等价
   `alternatives`，每 alternative 是若干 `{path, startLine, endLine, quote, sha256}` 的 `allOf`；
   命中 ⟺ 某 alternative 的全部行都被「已验证地」返回。补上 oce-benchmark 现在
   `expected_files` 文件级真值的精度黑洞，且 `quote+sha256` 让「答案引用的源码早改了」机械可检。
2. **逐行保真校验**（最高性价比，约 40 行）：返回必须是 `Path: x` + `^\s*(\d+)\t原文`，
   行号或文本不符即不计证据，报 `invalid_line_count`。对方用它报出 Claude Context 3907 处错行。
3. **双清单冻结 + run manifest + audit**：`freeze.json`（题目/答案/语料/评分器）与
   `engine-freeze.json`（被测引擎）分离；`runs.json` 显式声明哪些 run 参与对照；
   audit 脚本回验哈希、要求 `completed == queries`、把不完整 run 标 `incomplete` 而非 0 分。
   配套口径：label-blind token 截断、预算内/外双记分（区分「没召回」与「被截掉」）、
   strict/nonblank 双口径、wins/losses/changes 三分类。

建议顺序：**逐行保真 → 线级答案键 + coverage → protocol/freeze → manifest/audit**。

---

## 五、明确不吸收

| # | 项 | 理由 |
|---|---|---|
| 1 | numpy 全量内存索引（`vectors @ qvectors.T`） | O(N) 全扫、单进程、无过滤；本报四种后端都是过滤检索，抄了是倒退 |
| 2 | 四个近乎复制粘贴的引擎类（`Engine`→`Cascade`→`Batched`→`Routed`） | 本报用 Protocol 注入已更干净；抽 hook，不复制管线 |
| 3 | 魔法常数照抄（`.7/.3`、`(max(120,cost)/300)**.35`、阈值 `.015/.005`、温度 `.06/.12`） | 在 3–4 仓 / 60–80 开发题上手工调；必须在本报 benchmark 标定 |
| 4 | `tiktoken cl100k_base` 计费 | 对 Qwen tokenizer 是错配；用 provider usage 或维持字符预算 |
| 5 | 每秒全量 scan + 整语言组重解析 + 全量 `vectors.npy` 重写 | 本报 per-chunk 内容寻址复用粒度更细；只取「身份定义 / 原子换代 / 查询后校验」 |
| 6 | graph-completed 候选**无模型分直接进输出**（`source:'graph'`） | 在 nDCG@10 / Top-1 口径下是精度风险；图候选只做名额保留 + 参与重排 |
| 7 | `confidence` 的双重身份（注释说是「来源类别」，消费侧当概率连乘） | 移植时要么固定成离散权重表，要么标定成概率，不继承含糊 |
| 8 | 服务端做 `git ls-files` 发现、per-repo worker 编排、模型端点必须远程的策略 | 发现与编排属 oce-client；端点策略与本报自托管/内网冲突 |
| 9 | `fcntl` 排他锁（其只支持 macOS/Linux 的根因） | 本报 Windows 一等公民；用 `msvcrt` / `O_EXCL` 锁文件 + PID 心跳 / PG advisory lock / Redis 锁 |

同时不要为了「对齐」而退化本报已强于对方的部分：意图路由（7 类 + broad regime + manifest
prior）、basename 近重复去冗、`span_merge` 空洞回退（行号永不撒谎）、`related_symbols`
fanout/停用词门控。

---

## 六、执行顺序与验收标准

| 序 | 项 | 依赖 | 验收 |
|---|---|---|---|
| 1 | rerank 客户端校验 + 截断 + 不删候选（2.1） | 无 | 畸形响应单测 + A/B 无回归 |
| 2 | 选择器边际覆盖贪心（2.2） | 无 | 预算/overlap 单测 + A/B |
| 3 | 候选名额保留泛化（2.3） | 无 | 分类分不降 |
| 4 | 评测侧逐行保真（四.2） | oce-benchmark | 先量化当前 `invalid_line_count` |
| 5 | 索引身份 + 分块完整性（3.3） | alembic 迁移 | 换模型的 fail-closed 单测 |
| 6 | 新鲜度语义（3.4） | 3.3 的状态元数据 | 503 可重试语义测试 |
| 7 | graph-specific query set → hints 实验 → 图召回（3.1） | 前置 query set | 见 09-21 Note 的四约束 |
| 8 | bundle / 实体聚合（3.2） | 3.1 的 symbol/owner 元数据 | SYMBOL / CALL_CHAIN 分类分 |
| 9 | 线级答案键 + 冻结清单（第四节） | 4 | 全系统统一重算 |

每项独立 commit、独立 A/B；负收益即回退并在本 Note / BCE Note 追加实测记录。

---

## 七、实测回填（2026-10-06，18 步全部执行完毕）

执行计划：`.agents/notes/implemented/feature/2026-10-06-npm-oce-borrow-implementation-plan.md`。
基线锚点（STEP-3，nollm 双仓 100 题）：**flask 99.52 / cc-switch 65.77**。

### 吸收（已落地）

| 项 | 实测 | 说明 |
|---|---|---|
| rerank 严格校验 + 不删低分候选（STEP-5） | 6 单测 | 畸形响应 fail-closed，只记 tracing 不删候选 |
| LLM 降级可观测（STEP-6） | 12 单测 | `rerank_degraded` / `llm_rerank_degraded` / `llm_rerank_returned` 进 audit |
| 逐行保真校验（STEP-2） | 真响应 634 行 0 invalid | `invalid_line_count` 进报告 |
| 报告记配置（STEP-1） | 头部 `- Config:` / `- Service build:` / `- Elapsed:` | 跨报告可比性前提 |
| 符号注解（STEP-9） | 默认关双仓 +0.00 | 基础件，供 bundle/图用 |
| 切块完整性硬校验（STEP-11） | 真语料 231+1031 文件：**0 硬错误** | 三条不变式；`uncovered` 已提升为硬错误 |
| 切块器两处真缺陷修复（STEP-11b） | 真语料重叠 6+122 处、漏行 1 文件 → **全清零** | cAST 边界重叠 + 递归尾部丢纯标点片段 |
| 切块器指纹 fail-closed（STEP-12） | sidecar `chunk=v2`；旧索引启动即拒 | RISK-3 实测确认 |
| 新鲜度语义（STEP-13） | REQ-8 手验三例通过 | `index.pending` 如实上报，绝不把未就绪当查不到 |
| 关系抽取 + hints（STEP-14） | 双仓 +0.00（CON-9 预期） | 只追加文本，`Path:` 行逐字不变 |
| 线级答案键 + 覆盖率（STEP-16） | 46 units verified；覆盖 14/46（30.4%） | 与用户给的 cov@1000 32.8% 同量级 |
| 协议 + 冻结清单（STEP-17） | 冻结成功；二次运行拒绝覆盖 | 铁律：冻结文件永不覆盖 |
| JSON 报告 + 机器审计（STEP-18） | `status: passed`；篡改 top1 → failed | 自哈希 + 冻结输入校验 |

### 显著正收益（默认关，待拍板）

| 项 | 实测 | 结论 |
|---|---|---|
| **图扩展召回（STEP-15）** | flask **+7.71**、cc-switch **+10.05**（两次独立跑逐位相同；Top-1 42→47 / 30→37；无单类下跌）；graph 集 `all_expected@10` 14/30 → 15/30 | **吸收**。默认仍关的理由是**延迟预算**（median 4ms→30ms / 12ms→70.5ms，约 6-7×），不是效果。改默认需单独拍板 + 补跑 full 档双跑中位 |

### 回退 / 默认关（负结果留档）

| 项 | 实测 | 结论 |
|---|---|---|
| 边际覆盖选择器（STEP-7） | 修输出序后 flask −15.03 / cc −13.99 | **回退**：默认关。附带修掉一个真 bug（选择器把贪心序当返回顺序，单这一条占一半回归） |
| 候选名额保留（STEP-8） | flask +1.51 / cc −4.52 | **回退**：默认关（不达双仓门槛） |
| bundle / 实体聚合（STEP-10） | 机制生效（75 bundles/查询）但双仓 +0.00、graph 集 14/30→14/30 | **默认关**：bundle 只补同文件同符号，而实测缺口是跨文件的——这条负结果把 STEP-15 的靶点钉死了 |

### 方法论收获（比分数更耐放）

1. **`+0.00` 有两种**：一种是真的没效果，一种是**没触发**。四个 `+0.00` 臂里，
   bundle 与图扩展都靠 `-vv` 探针证明"确实执行了"，否则无法区分。
2. **同一个机制在两个集上可以完全相反**：图扩展在主基准 +7.7/+10.1、在 graph 集只 +0/+1。
   只测一个集就会得出相反结论。
3. **"改集合"与"改顺序"是两件事**：选择器只动集合不动顺序的那次修复就挽回了 17 分。
4. **正确性修复不涨分**：切块器两处缺陷修完分数纹丝不动——因为被修的是收尾括号与重复边界行，
   相邻 chunk 本来就能取到。价值在不变量有真语料背书，不在分数。
