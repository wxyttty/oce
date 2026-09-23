//! 报表读模型 DTO + 端口（对应 Python `shared/reports_read.py`）。
//!
//! 约定（与 Python 版一致）：
//! - SQL 只做窗口过滤与基础聚合；分桶（hour/day 截断）与分位数在 Rust 侧计算，
//!   避免依赖方言专有函数（跨 SQLite/PG 可移植性）；
//! - 报表是旁路只读路径，绝不写库、不影响检索主链路；
//! - 任何单表查询失败降级为空结果，不让报表端点 5xx。

use crate::error::OceResult;
use async_trait::async_trait;

// ───────────────────────────────────────────────────────── 读模型 DTO

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ApiCallBucket {
    pub ts: String,
    pub count: u64,
    pub error_count: u64,
    pub avg_latency_ms: f64,
    pub p50_latency_ms: i64,
    pub p95_latency_ms: i64,
    pub max_latency_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct EndpointStat {
    pub endpoint: String,
    pub method: String,
    pub count: u64,
    pub error_count: u64,
    pub error_rate: f64,
    pub avg_latency_ms: f64,
    pub p95_latency_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ErrorStat {
    pub status_code: i64,
    #[serde(default)]
    pub error_type: Option<String>,
    pub count: u64,
    #[serde(default)]
    pub last_ts: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiCallsReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<ApiCallBucket>,
    #[serde(default)]
    pub endpoints: Vec<EndpointStat>,
    #[serde(default)]
    pub errors: Vec<ErrorStat>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct RetrievalBucket {
    pub ts: String,
    pub count: u64,
    pub empty_count: u64,
    pub empty_rate: f64,
    pub avg_hit_count: f64,
    pub avg_total_ms: f64,
    pub p95_total_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct StageStat {
    pub stage: String,
    pub count: u64,
    pub avg_ms: f64,
    pub p95_ms: i64,
    pub max_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct IntentStat {
    #[serde(default)]
    pub intent: Option<String>,
    pub count: u64,
    pub empty_count: u64,
    pub empty_rate: f64,
    pub avg_total_ms: f64,
    pub path_boosted_count: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ScopeBucketStat {
    pub label: String,
    pub count: u64,
    pub empty_rate: f64,
    pub p95_total_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RetrievalReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<RetrievalBucket>,
    #[serde(default)]
    pub stages: Vec<StageStat>,
    #[serde(default)]
    pub intents: Vec<IntentStat>,
    #[serde(default)]
    pub scopes: Vec<ScopeBucketStat>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct RetrievalQueryDetail {
    pub ts: String,
    pub source: String,
    #[serde(default)]
    pub query_text: Option<String>,
    pub total_ms: i64,
    pub hit_count: i64,
    #[serde(default)]
    pub scope_size: Option<i64>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub path_boosted: bool,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TokenBucket {
    pub ts: String,
    pub kind: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ModelTokenStat {
    pub model: String,
    pub kind: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub avg_tokens_per_call: f64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct CredentialTokenStat {
    #[serde(default)]
    pub credential_id: Option<i64>,
    pub calls: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokensReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<TokenBucket>,
    #[serde(default)]
    pub models: Vec<ModelTokenStat>,
    #[serde(default)]
    pub credentials: Vec<CredentialTokenStat>,
    pub tokens_total: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct CountStat {
    pub key: String,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct IndexInventoryReport {
    pub blob_total: u64,
    #[serde(default)]
    pub blob_by_status: Vec<CountStat>,
    #[serde(default)]
    pub blob_by_language: Vec<CountStat>,
    pub blob_retrying: u64,
    pub blob_content_bytes: i64,
    pub chunk_total: u64,
    pub chunk_pending_embed: u64,
    #[serde(default)]
    pub chunk_by_type: Vec<CountStat>,
    pub chunk_content_bytes: i64,
    pub blob_chunk_links: u64,
    pub symbol_total: u64,
    #[serde(default)]
    pub symbol_by_kind: Vec<CountStat>,
    pub chain_total: u64,
    pub chain_stale_7d: u64,
    pub chain_stale_30d: u64,
    pub staging_rows: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ResourceBucket {
    pub ts: String,
    pub avg_cpu_percent: f64,
    pub max_cpu_percent: f64,
    pub avg_mem_percent: f64,
    pub max_mem_rss_bytes: i64,
    pub disk_data_bytes: i64,
    pub disk_free_bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResourcesReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<ResourceBucket>,
    pub disk_total_bytes: i64,
    pub disk_growth_bytes_per_day: f64,
    #[serde(default)]
    pub disk_days_until_full: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TableSpaceStat {
    pub table: String,
    pub bytes: i64,
    pub rows: i64,
    pub approximate: bool,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct DataFileStat {
    pub name: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct VectorCollectionStat {
    pub name: String,
    pub rows: u64,
    pub est_bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorStoreStat {
    pub mode: String,
    #[serde(default)]
    pub collections: Vec<VectorCollectionStat>,
    pub file_bytes: i64,
    #[serde(default)]
    pub error: Option<String>,
}

impl Default for VectorStoreStat {
    fn default() -> Self {
        Self {
            mode: "unavailable".into(),
            collections: Vec::new(),
            file_bytes: 0,
            error: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct StorageReport {
    pub dialect: String,
    pub total_table_bytes: i64,
    #[serde(default)]
    pub tables: Vec<TableSpaceStat>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub data_files: Vec<DataFileStat>,
    pub data_dir_total_bytes: i64,
    #[serde(default)]
    pub vector: Option<VectorStoreStat>,
}

// ───────────────────────────────────────────────────────── 端口

/// 报表聚合读端口（infra 提供 SQLite/PG 实现；全部旁路降级语义）。
#[async_trait]
pub trait ReportsStore: Send + Sync {
    async fn api_calls(&self, window_hours: u32, bucket: &str) -> OceResult<ApiCallsReport>;
    async fn retrieval(&self, window_hours: u32, bucket: &str) -> OceResult<RetrievalReport>;
    async fn slow_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> OceResult<Vec<RetrievalQueryDetail>>;
    async fn empty_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> OceResult<Vec<RetrievalQueryDetail>>;
    async fn tokens(&self, window_hours: u32, bucket: &str) -> OceResult<TokensReport>;
    async fn index_inventory(&self) -> OceResult<IndexInventoryReport>;
    async fn resources(&self, window_hours: u32, bucket: &str) -> OceResult<ResourcesReport>;
    async fn storage(&self) -> OceResult<StorageReport>;
}
