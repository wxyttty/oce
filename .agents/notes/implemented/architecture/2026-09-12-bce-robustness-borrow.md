# Agent Note: 吸收 BCE 的稳健性能力：模型故障冷却、弱匹配提示、AI 工具目录过滤与 enable_thinking 兼容

Status: implemented

## Problem

调研 `better-context-engine`（BCE，Go，多租户云服务）后确认四块低成本、高确定性的稳健性能力值得吸收到 Rust 个人模式，且不动检索排序语义（检索策略类借鉴——file description enrichment、related symbols、broad mode 等——全部留待 oce-benchmark A/B 验证后再动，本 Note 不覆盖）：

1. **外部模型故障无冷却**：embed/rerank/LLM 任一外部服务挂掉时，之后每次检索都要等满超时（LLM 超时上限 120s）再走降级路径。BCE 的做法是失败一次后 30s 冷却，冷却期内直接跳过该通路（词法/结构路始终可用）。
2. **弱匹配/降级对消费方不可见**：`confidence_floor` 只做静默过滤。弱匹配被 padding 进窗口和「真的找到了实现」对 agent 外观相同——真实代码可能在别的仓库/服务里。语义路静默缺席（嵌入故障）同理。
3. **AI 工具目录污染索引**：`.claude/.cursor/.windsurf/.trae/.roo/.zed/.fleet/.vs` 等目录的 settings.local.json、规则文件、缓存在 config 类查询上得分虚高（BCE 实测结论），却从不是有用的检索上下文。`source_filter` 此前只有 `.idea/.vscode`。
4. **Qwen3 混合思考模型空响应**：BCE 在三个 LLM 调用点注入 `enable_thinking: false`——Qwen3 系混合模型会把整个 max_tokens 烧在 `<think>` 上再返回空 content。OCE 的 `chat()` 用 `reasoning` 字段兜底空 content，但那是「拿思考链当结果用」，语义不同于「直接关思考」。

## Decision

四项能力全部落地，均为低侵入改动：

- **冷却门**（`oce-core/src/cooldown.rs` 新增）：进程内 `CooldownGate`（Mutex<Option<Instant>>，锁中毒 fail-open——宁可多试一次真服务，不静默丢通路）。`RetrievalPipeline` 持三条独立冷却门（embed/rerank/llm），失败即 `trip(30s)`，冷却期内：语义召回直接返回空（词法/结构路仍在）、rerank 保序回退、LLM rerank 跳过。路径 boost 分支的查询向量化同样受 embed 冷却门保护。
- **弱匹配/降级提示**（`formatter.rs` + `retrieval.rs`）：`RetrievalAudit` 增加 `weak_match`/`semantic_degraded` 两个旁路标记。weak 判定借鉴 BCE `rerankWeakTop`（0.30 阈值）：融合分是归一化 RRF（≤1），exact 召回按 kind 打 0.85-1.0，头部低于 0.30 意味着既无 exact 命中、语义/词法头部也弱。`format_retrieval_with_notes` 在 HEADER 后注入 `Note:` 提示（weak = "may not be implemented in this codebase (could live in a separate repository or service)"；degraded = "semantic index did not participate… keyword/structure matches only"）。HTTP 面与 MCP 面共用。
- **AI 工具目录**：`source_filter.rs` 的 `IGNORED_DIRECTORY_NAMES` 从 29 → 37 项（新增 8 个 AI/编辑器目录）；`workspace.rs` 的 `DEFAULT_IGNORED_DIRS`（MCP 嵌入式扫描）同步补齐，两处同源。
- **enable_thinking**（`openai/llm.rs`）：**不做无条件注入**。官方 OpenAI API 对未知参数直接 400（OpenAI 社区实测报告），BCE 的无条件注入只在其自托管场景成立。改为三态 `TriStateBool`（Auto/True/False，env `LLM_ENABLE_THINKING`）：Auto 模式按 base_url 域名探测已知提供方（siliconflow/dashscope/aliyuncs/qwen/vllm/ollama）才注入；True/False 显式覆盖。`.env.example` 与 `oce init` 模板补注释。

## Alternatives considered

- **BCE 式无条件注入 `enable_thinking: false`**：被否。官方 OpenAI API 严格校验未知参数返回 400，无条件注入会把严格校验的提供方全部打挂；OCE 的 LLM 端点是用户可配的任意 OpenAI 兼容服务，不能假设单一提供方。
- **冷却门用 tokio::sync::Mutex / 原子时间戳**：std Mutex 足够——持锁窗口内无 await，不会跨异步点持有；原子 Instant 在 32 位平台有坑。锁中毒 fail-open 而非 fail-closed：冷却门是优化不是正确性边界。
- **weak 阈值放 RetrievalSettings 可配置**：YAGNI。0.30 的推导（exact kind 分数 0.85+ vs 归一化 RRF 头部）在当前分数体系内自洽，先钉死常量；分数体系变了再谈配置。
- **BCE 的 prefilter / Redis 缓存 / abuse guard / pseudo path 重组**：全部不吸收。prefilter 为 pgvector+解密的线性成本设计，TriviumDB 进程内检索成本结构不同；个人模式无 Redis 无多用户；OCE 客户端不做 `file.vue#chunk2of3` 超大文件切片。

## Consequences

- 收益：外部服务故障时检索延迟从「每次等满超时（最坏 120s×N 通路）」降为「30s 冷却 + 立即降级」；agent 能区分「仓库里没有」和「找到了」；索引不再被 AI 工具目录的规则文件污染；Qwen3 混合模型用户开箱即用不踩空响应坑。
- 代价：冷却期内语义检索确实缺席（30s 窗口）——这正是 degraded 提示存在的原因，两能力配套。enable_thinking 的域名探测表是白名单制，未列出的提供方（如 Nebius）默认不注入，需用户显式 `LLM_ENABLE_THINKING=true`——文档已注明。
- 验证：`cargo test --workspace` 76 通过（新增 cooldown 3 例、formatter notes 4 例、source_filter AI 目录 1 例、llm 三态 2 例、e2e notes 1 例）。检索排序语义零变化——冷却只影响「是否调用外部服务」，不影响任何打分逻辑。
- 后续（另行立项，须 oce-benchmark A/B）：file description enrichment（规则层先行）、related symbols hints、rerank 动态截断 + 近重复抑制、broad mode。改 `embedding_text` 会触发模型指纹 sidecar 的重建语义，须一并设计文本格式版本化。
