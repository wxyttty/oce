//! 监控旁路的领域侧记录类型（对应 Python `shared/metrics.py` 的 record 类型）。
//! 采集为旁路且非阻塞：sink 失败只跳过，不影响检索主链路。

use crate::search::RetrievalAudit;

/// /admin/stats 读模型（从 infra 迁入 core：PG 实现与 SQLite 实现共享）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MonitoringStats {
    pub api_calls: ApiCallStats,
    pub tokens: Vec<TokenKindStats>,
    pub retrieval: RetrievalStats,
    #[serde(default)]
    pub resource: Option<ResourceSnapshot>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ApiCallStats {
    pub calls: u64,
    pub avg_latency_ms: f64,
    pub error_count: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenKindStats {
    pub kind: String,
    pub model: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RetrievalStats {
    pub count: u64,
    pub empty_count: u64,
}

/// 最新资源快照（/admin/stats 的 resource 字段）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResourceSnapshot {
    pub ts: String,
    pub mem_rss_bytes: u64,
    pub mem_percent: f64,
    pub cpu_percent: f64,
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_data_bytes: u64,
}

/// 监控窗口统计读端口（infra 提供 SQLite/PG 实现）。
#[async_trait::async_trait]
pub trait MonitoringStatsReader: Send + Sync {
    async fn stats(&self, window_hours: u32) -> MonitoringStats;
}

/// 每次外部模型调用一行的 token 消耗记录。
#[derive(Debug, Clone)]
pub struct TokenUsageRecord {
    pub kind: String,
    pub model: String,
    pub credential_id: i64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// 一次 HTTP 请求的调用记录（endpoint 为路由模板路径，/health 豁免）。
#[derive(Debug, Clone)]
pub struct ApiCallRecord {
    pub endpoint: String,
    pub method: String,
    pub status_code: u16,
    pub latency_ms: u64,
    pub error_type: Option<String>,
}

/// 一次资源采样快照（磁盘 / 内存 / CPU）。
#[derive(Debug, Clone)]
pub struct ResourceSampleRecord {
    pub disk_data_bytes: u64,
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub mem_rss_bytes: u64,
    pub mem_percent: f64,
    pub cpu_percent: f64,
}

/// 监控采集端口。实现方的 record_* 必须同步、非阻塞、不抛出。
pub trait MetricsSink: Send + Sync {
    fn record_token_usage(&self, record: TokenUsageRecord);

    /// 检索审计落库（阶段耗时 + 空回）。monitoring 关闭时为空实现。
    fn record_retrieval(&self, record: RetrievalMetricRecord);

    fn record_api_call(&self, record: ApiCallRecord);

    fn record_resource_sample(&self, record: ResourceSampleRecord);
}

/// 一次检索的阶段耗时与结果审计（对应 Python `RetrievalMetricRecord`）。
#[derive(Debug, Clone, Default)]
pub struct RetrievalMetricRecord {
    pub source: String,
    pub scope_size: Option<i64>,
    pub hit_count: i64,
    pub total_ms: i64,
    pub intent: Option<String>,
    pub path_boosted: bool,
    pub query_text: Option<String>,
    pub intent_ms: Option<i64>,
    pub rewrite_ms: Option<i64>,
    pub dense_ms: Option<i64>,
    pub exact_ms: Option<i64>,
    pub fuse_ms: Option<i64>,
    pub rerank_ms: Option<i64>,
    pub llm_rerank_ms: Option<i64>,
    pub select_ms: Option<i64>,
}

impl RetrievalMetricRecord {
    /// 从 audit 构造（字段缺失时 None，与 Python 落库行为一致）。
    /// query_text 默认不落原文（隐私安全）；调用方按 store_query_text 配置覆写。
    pub fn from_audit(
        audit: &RetrievalAudit,
        source: &str,
        hit_count: usize,
        total_ms: i64,
    ) -> Self {
        let stage = |name: &str| audit.stages.get(name).map(|v| *v as i64);
        Self {
            source: source.to_string(),
            scope_size: audit.scope_size.map(|s| s as i64),
            hit_count: hit_count as i64,
            total_ms,
            intent: audit.intent.clone(),
            path_boosted: audit.path_boosted,
            query_text: None,
            intent_ms: stage("intent"),
            rewrite_ms: stage("rewrite"),
            dense_ms: stage("dense"),
            exact_ms: stage("exact"),
            fuse_ms: stage("fuse"),
            rerank_ms: stage("rerank"),
            llm_rerank_ms: stage("llm_rerank"),
            select_ms: stage("select"),
        }
    }
}

/// 空实现（监控关闭时）。
pub struct NoopMetricsSink;

impl MetricsSink for NoopMetricsSink {
    fn record_token_usage(&self, _record: TokenUsageRecord) {}
    fn record_retrieval(&self, _record: RetrievalMetricRecord) {}
    fn record_api_call(&self, _record: ApiCallRecord) {}
    fn record_resource_sample(&self, _record: ResourceSampleRecord) {}
}
