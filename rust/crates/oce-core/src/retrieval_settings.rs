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
}

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
        }
    }
}
