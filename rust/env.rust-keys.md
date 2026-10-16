# Rust 版专属环境变量（Python 版忽略）
#
# 这些键不在 Python Settings 元数据里，根 .env.example 由
# scripts/generate_env_template.py 自动生成、不含它们，故独立维护于此。
# 完整说明见 rust/README.md。

# ---- TriviumDB（向量库） ----
# TRIVIUM_TEXT_HYBRID=true                # BM25+dense RRF 混合（标识符门控后 +4.4 分）
# TRIVIUM_TEXT_BOOST=0.3                  # BM25 融合权重（0.3 实测最优；1.5 会 -20 分）
# TRIVIUM_EXPAND_DEPTH=0                  # 图扩散深度（1 会 -33 分，保持 0）

# ---- 嵌入 provider ----
# EMBED_PROVIDER=auto                     # auto|openai|static|local（local=candle CPU）
# EMBED_STATIC_MODEL=minishlab/potion-base-8M    # static provider 的模型
# EMBED_LOCAL_MODEL=Qwen/Qwen3-Embedding-0.6B    # local provider 的 candle 模型
# EMBED_LOCAL_DTYPE=f32                   # local 精度 f32|f16|bf16

# ---- 选择器 rank 分离 [实验，A/B 通过] ----
# rerank 分数饱和（多个 0.9+ 候选几乎同分）时，用组内 rank 的指数衰减
# exp(-rank/8) 打破平局——排名靠前者优先入席（对方 EvidenceEngine value() 同款）。
# 2026-10-09 行级覆盖 A/B（click/httpx/zod 60 题，同日单变量）：
# cov@4000 94.07% → 95.14%（+1.07）；0=关，0.5=推荐值。
# RETRIEVAL_SELECTOR_RANK_WEIGHT=0.5

# ==================== Rust 版实验开关（Python 版未实现）====================
# 以下为 Rust 版检索实验开关。旧 .env.example 曾把它们混排在 Python 模板里
# （超前文档——Python settings.py 从未实现这些键），现集中于此。

# --- 文件描述注入 [实验] ---
# [开关] 规则层文件描述（清单/配置/入口文件名→英文桥接句，README 首段）
# 注入 embedding_text 与 rerank 文档，弥合「中文查询 vs TOML/CSS 内容零词法重叠」。
# 默认关（A/B 实验开关）；开启后模型指纹变为 etext=v2，旧索引启动时
# fail-closed 提示重建（静态嵌入零成本；API 嵌入需重嵌，flask 230 blob/
# cc-switch 1031 blob 量级实测秒级到几十秒）。
# RETRIEVAL_FILE_DESC_ENABLED=false

# --- related symbols hints [实验] ---
# [开关] 输出层追加 <related_symbols> 块：选中内容引用但定义未入窗的
# 符号（含定义路径），agent 可据此跟进检索（grep leads）。不动排序、
# 不改结果集，评测评分不受影响。默认关。
# RETRIEVAL_RELATED_SYMBOLS_ENABLED=false

# --- rerank 悬崖截断 [实验] ---
# [开关] 仅作用于 API 重排（RERANK_ENABLED，已被 LLM 重排取代、默认关）
# 的端点校准分：head×0.35 与绝对下限 0.10 双线以下的弱尾部丢弃，最少
# 保留 6 条。LLM 重排只返回顺序无校准分，不受此开关影响（其截断语义
# 由 prompt 的 "output fewer — do not pad" 天然承担）。默认关。
# RETRIEVAL_RERANK_CUTOFF_ENABLED=false

# --- broad mode（架构/概览探索查询专用 regime）[实验] ---
# [开关] 触发 = LLM 意图判为 Overview ∨ 启发式架构词表（架构/技术栈/
# 部署/framework/architecture…中英双语；裸「配置」刻意缺席）。regime 差异：
# 宽窗口 20 hits、单文件最多 2 片（覆盖优先于深度）、清单文件结构先验
# （package.json/pyproject.toml/Dockerfile… 作为一路额外融合）、超 28 行
# 摘录骨架化（头 12 行 + 查询词命中行，省略段标注真实行号区间）。
# 默认关。
# RETRIEVAL_BROAD_MODE_ENABLED=false

# --- 元目录降权 [实验] ---
# [开关] .github/.gitlab/.circleci 等 CI/模板基础设施目录 ×0.5 降权（对普通
# 代码问题是纯噪声）；查询明确点名 CI/工作流（github/action/工作流/流水线…）
# 时豁免。仅主检索路生效，路径增强定位查询不受影响。默认关。
# RETRIEVAL_META_DIR_PENALTY_ENABLED=false

# --- 相邻 span 合并 + 小片段补全 [实验] ---
# [开关] 同文件相距 ≤2 行的选中片段合并成一个连续段，<6 行的片段两侧各补
# 3 行上下文；内容从 chunk 行重构，覆盖有洞时回退原样（行号永不撒谎）。
# select 池加宽 8 条，合并缩窗后由次优排名回填。默认关。
# RETRIEVAL_SPAN_MERGE_ENABLED=false

# --- 边际覆盖选择器 [实验，A/B 未通过] ---
# [开关] 选择器改用「边际覆盖增益 + 成本归一」贪心：多 facet 共同满足的片段
# 优先于单点高分，长片段不能靠体量吃预算。输出顺序仍回到分数序。
# 2026-10-06 双仓 nollm A/B：flask −15.03 / cc-switch −13.99（不达标），默认关；
# 调参作为独立实验（计划 STEP-7b），若不通过则删除该分支。
# RETRIEVAL_MARGINAL_COVERAGE_ENABLED=false
# facet 亲和度 soft-max 温度（逐列减自身最大值后取指数）
# RETRIEVAL_FACET_TEMPERATURE=0.06

# --- 精确召回保留名额 [实验，A/B 未通过] ---
# [开关] >0 时把 CALL_CHAIN 的手工 1/3 规则推广到所有意图：精确独占候选按固定
# 名额进入候选池，不被语义融合的 default_top_k 平截。
# 2026-10-06 双仓 nollm A/B：flask +1.51 / cc-switch −4.52（不达双仓门槛），默认关。
# RETRIEVAL_RESERVED_CANDIDATE_SLOTS=0

# --- 符号注解 + 短函数 bundle [实验] ---
# [开关] 打开时按 symbol_occurrences 的 definition 行给候选标注符号名，供选择器
# 按符号整体取舍（短函数 bundle / 符号级实体聚合）。关时零查询、零行为变化。
# 2026-10-06 实测：机制生效（单查询 hits=133 → annotated=75 → entities=60 → bundles=75），
# 但双仓主基准与 graph 集 all_expected@10（14/30）全部 +0.00——bundle 只补同文件同符号的
# 兄弟片段，而实测缺口是跨文件的。达回归门槛但零收益，保持默认关。
# RETRIEVAL_CONTEXT_BUNDLE_ENABLED=false
# bundle 整组字符上限（超过则退化为成员逐个取舍）
# RETRIEVAL_BUNDLE_MAX_CHARS=768

# --- 图扩展召回 [实验，A/B 显著正收益，默认关] ---
# [开关] 从各路召回头部锚点现算静态调用关系（1 跳），把被调符号的**跨文件**定义
# 作为一路低权重候选送进 RRF；只在 reference / call_chain 意图下启用。
# 2026-10-06 双仓 nollm 实测（100 题/仓）：flask 99.52 → 107.23（+7.71）、
# cc-switch 65.77 → 75.82（+10.05），Top-1 42→47 / 30→37，无单类下跌；
# 两次独立跑逐位相同。graph 集 all_expected@10 14/30 → 15/30（w=0.5）。
# **默认仍关的理由是延迟预算而非效果**：median 4ms→30ms（flask）、12ms→70.5ms
# （cc-switch），每次检索多两次 SQL。离线/批处理/质量优先场景可开。
# RETRIEVAL_GRAPH_EXPANSION_ENABLED=false
# 图这一路在 RRF 里的权重（0.1 与 0.5 实测同向、量级相当）
# RETRIEVAL_GRAPH_WEIGHT=0.1
# 最多引入多少邻居 / hub 抑制：定义文件扇出超过它的通用符号不进图
# RETRIEVAL_GRAPH_MAX_NODES=16
# RETRIEVAL_GRAPH_FANOUT_CAP=15
