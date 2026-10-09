//! 检索配置值对象（对应 Python `RetrievalSettings`）。
//! 环境变量解析在 infra 层完成；core 只持有值。

/// 检索配置。
#[derive(Debug, Clone)]
pub struct RetrievalSettings {
    /// 向量召回条数
    pub default_top_k: usize,
    /// Milvus dense 相似度过滤阈值；默认不预过滤
    pub vector_threshold: f32,
    /// 最终返回条数
    pub final_select_k: usize,
    /// 多查询结果融合平滑常数
    pub rrf_k: usize,
    /// 最终置信度门槛
    pub confidence_floor: f32,
    /// SQL 精确标识符召回允许的最大 blob scope；0 表示禁用
    pub exact_max_scope_blobs: usize,
    /// SQL 精确标识符召回超时；超时后回退向量检索
    pub exact_timeout_seconds: f32,
    /// 是否分解多句检索请求
    pub query_decomposition_enabled: bool,
    /// 原查询和子查询总数上限
    pub query_max_queries: usize,
    /// 子查询最少字符数
    pub query_min_facet_chars: usize,
    /// 子查询融合权重
    pub query_facet_weight: f32,
    /// 单查询模式下覆盖 default_top_k
    pub per_query_top_k: usize,
    /// 单文件最多返回片段数
    pub max_chunks_per_path: usize,
    /// 返回代码总字符预算（硬限制）
    pub max_context_chars: usize,
    /// 同文件片段重叠抑制阈值
    pub overlap_threshold: f32,
    /// 是否启用 LLM 查询改写
    pub query_rewrite_enabled: bool,
    /// 路径分数与内容分数同为 COSINE 量纲，加权相加而非替换
    pub path_boost_weight: f32,
    /// 是否启用查询意图分类（LLM-based）
    pub intent_classification_enabled: bool,
    /// 是否启用 related symbols hints（输出层追加，不动排序；默认关）
    pub related_symbols_enabled: bool,
    /// rerank 悬崖截断（RETRIEVAL_RERANK_CUTOFF_ENABLED，默认关）：仅作用于
    /// API rerank（已被 LLM 重排取代、默认关）的端点校准分——head×0.35 /
    /// 绝对下限 0.10 双线，最少保留 6 条。LLM 重排只返回顺序无校准分，
    /// 其截断语义由 prompt 的 "output fewer — do not pad" 天然承担。
    pub rerank_cutoff_enabled: bool,
    /// broad regime（RETRIEVAL_BROAD_MODE_ENABLED，默认关）：架构/概览探索查询
    /// 的宽窗口（20 hits / per-path 2）+ manifest prior + 长摘录骨架化。
    /// 触发 = LLM intent Overview ∨ 启发式架构词表（见 `broad` 模块）。
    pub broad_mode_enabled: bool,
    /// 元目录降权（RETRIEVAL_META_DIR_PENALTY_ENABLED，默认关）：.github 等
    /// CI/模板基础设施目录 ×0.5，查询点名 CI/工作流时豁免。仅主检索路生效，
    /// 路径增强路保持文档中立。
    pub meta_dir_penalty_enabled: bool,
    /// 相邻 span 合并 + 小片段补全（RETRIEVAL_SPAN_MERGE_ENABLED，默认关）：
    /// 同文件相距 ≤2 行的选中片段合并成连续段、<6 行片段两侧各补 3 行，
    /// 内容从 chunk 行重构（缺行回退原样）；select 池加宽 8 条供合并缩窗
    /// 后回填（semble M1）。
    pub span_merge_enabled: bool,
    /// rerank 候选池上限（RETRIEVAL_RERANK_POOL_K，默认 0=不截断）：
    /// 向量召回 default_top_k 与 rerank 候选池解耦——大池保融合质量，
    /// 小池让 reranker 集中在嵌入头部候选上（实测 24 池比 120 池 +2.8 分）。
    pub rerank_pool_k: usize,
    /// 选择器改用「边际覆盖增益 + 成本归一」贪心
    /// （`RETRIEVAL_MARGINAL_COVERAGE_ENABLED`，默认关；OCE cascade 的目标函数形状）。
    /// 关时逐字沿用两趟固定顺序填充。
    pub marginal_coverage_enabled: bool,
    /// facet 亲和度 soft-max 温度（`RETRIEVAL_FACET_TEMPERATURE`，默认 0.06）：
    /// 逐列减自身最大值后按该温度取指数，越小越强调"该列最强"。
    pub facet_temperature: f32,
    /// 精确召回独占候选的保留名额（`RETRIEVAL_RESERVED_CANDIDATE_SLOTS`，默认 0=关闭）：
    /// >0 时把 CALL_CHAIN 的手工 1/3 规则推广到所有意图，精确/图候选不被
    /// 语义融合的 `default_top_k` 平截挤掉。
    pub reserved_candidate_slots: usize,
    /// 符号注解 + 短函数 bundle（`RETRIEVAL_CONTEXT_BUNDLE_ENABLED`，默认关）：
    /// 打开时按 `symbol_occurrences` 的 definition 行给候选标注符号名，供选择器
    /// 按符号整体取舍；关时零查询、零行为变化。
    pub context_bundle_enabled: bool,
    /// bundle 整组字符上限（`RETRIEVAL_BUNDLE_MAX_CHARS`，默认 768）：同符号成员
    /// 总字符不超过它时作为一个 action 整体取舍，超过则退化为成员逐个取舍。
    pub bundle_max_chars: usize,
    /// 图扩展召回（`RETRIEVAL_GRAPH_EXPANSION_ENABLED`，默认关）：从锚点正文现算
    /// 静态调用关系，把被调符号的**跨文件**定义作为一路低权重候选送进融合。
    pub graph_expansion_enabled: bool,
    /// 图这一路在 RRF 里的权重（`RETRIEVAL_GRAPH_WEIGHT`，默认 0.1）。
    pub graph_weight: f32,
    /// 图扩展最多引入多少个邻居（`RETRIEVAL_GRAPH_MAX_NODES`，默认 16）。
    pub graph_max_nodes: usize,
    /// hub 抑制：定义文件扇出超过它的通用符号不进图（`RETRIEVAL_GRAPH_FANOUT_CAP`，默认 15）。
    pub graph_fanout_cap: usize,
    /// 查询期子窗口切分（`RETRIEVAL_SPAN_WINDOW_LINES`，默认 0=关闭）：
    /// 召回后把超长 chunk 按空行（语句边界代理）切成 ≤该行数的子窗口，
    /// 子窗口作为独立候选流经 rerank/融合/选择/渲染。行级覆盖实测：
    /// cAST chunk 平均 45 行但最大 232 行，答案埋在 chunk 中部时 reranker
    /// 打分被无关前缀稀释；子窗口把答案推到候选头部，逼近对手
    /// 「每定义一个 unit」的打分精度而不需要重建索引（向量仍 chunk 级，
    /// 召回已饱和，粒度只影响打分与打包）。
    pub span_window_lines: usize,
    /// 选择器 rank 分离权重（`RETRIEVAL_SELECTOR_RANK_WEIGHT`，默认 0=关）：
    /// rerank 分数饱和（多个 0.9+ 候选几乎同分）时，用组内 rank 的指数
    /// 衰减 exp(-rank/8) 打破平局——排名靠前的优先入席。对方 EvidenceEngine
    /// value() 的同款机制（0.5*relevance + 0.5*exp(-rank/8)），替代逆成本
    /// 奖励做「性价比」区分。>0 时 gain 混入 rank 项。
    pub selector_rank_weight: f32,
}

/// 图扩展的种子取样：每路召回只取头部这么多条做关系抽取。
/// 种子是"已召回内容"，取头部既够用又避免对整个候选池跑正则。
pub const GRAPH_SEED_PER_LIST: usize = 5;

impl Default for RetrievalSettings {
    fn default() -> Self {
        Self {
            // 召回预算实测（nollm 全量、确定性）：50→120 在 flask +4.3 分 / cc +0.3，
            // 200 饱和。TriviumDB 内存检索下加深池几乎零成本（Python/Milvus 默认 50）。
            default_top_k: 120,
            vector_threshold: 0.0,
            final_select_k: 10,
            rrf_k: 60,
            confidence_floor: 0.0,
            exact_max_scope_blobs: 2_000,
            exact_timeout_seconds: 2.0,
            query_decomposition_enabled: true,
            query_max_queries: 4,
            query_min_facet_chars: 8,
            query_facet_weight: 0.75,
            // 多查询（分解/改写变体）模式下每路召回量：与 default_top_k 同理加深，
            // 每路 20 会把变体的价值截断在浅池里。
            per_query_top_k: 60,
            max_chunks_per_path: 2,
            max_context_chars: 32_000,
            overlap_threshold: 0.6,
            query_rewrite_enabled: false,
            path_boost_weight: 0.5,
            intent_classification_enabled: true,
            related_symbols_enabled: false,
            rerank_cutoff_enabled: false,
            broad_mode_enabled: false,
            meta_dir_penalty_enabled: false,
            span_merge_enabled: false,
            rerank_pool_k: 0,
            marginal_coverage_enabled: false,
            facet_temperature: 0.06,
            reserved_candidate_slots: 0,
            context_bundle_enabled: false,
            bundle_max_chars: 768,
            graph_expansion_enabled: false,
            graph_weight: 0.1,
            graph_max_nodes: 16,
            graph_fanout_cap: 15,
            span_window_lines: 0,
            selector_rank_weight: 0.0,
        }
    }
}
