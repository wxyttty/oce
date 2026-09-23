//! ACE 兼容 HTTP DTO。字段名与错误语义与 Python `api/schemas.py` 一致。

use serde::{Deserialize, Serialize};

/// Python 客户端以 JSON null 表达「空 checkpoint」（pydantic 有 none_to_empty_string
/// 验证器）；对齐语义：null 反序列化为默认值而非 422。
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
pub struct FindMissingRequest {
    #[serde(default)]
    pub mem_object_names: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FindMissingResponse {
    #[serde(default)]
    pub unknown_memory_names: Vec<String>,
    #[serde(default)]
    pub nonindexed_blob_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct BlobInput {
    pub content: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct BatchUploadRequest {
    #[serde(default)]
    pub blobs: Vec<BlobInput>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub checkpoint_id: String,
}

#[derive(Debug, Serialize)]
pub struct BatchUploadResponse {
    #[serde(default)]
    pub blob_names: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReloadCredentialsResponse {
    pub reloaded: bool,
    #[serde(default)]
    pub pool_size: usize,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BlobsPayload {
    #[serde(default, deserialize_with = "null_to_default")]
    pub checkpoint_id: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub added_blobs: Vec<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub deleted_blobs: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CodebaseRetrievalRequest {
    pub information_request: String,
    #[serde(default)]
    pub blobs: BlobsPayload,
    #[serde(default)]
    pub chat_history: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CodebaseRetrievalResponse {
    pub formatted_retrieval: String,
    pub codebase_retrieval_elapsed_ms: i64,
}

#[derive(Debug, Deserialize)]
pub struct CheckpointBlobsRequest {
    #[serde(default)]
    pub blobs: BlobsPayload,
}

#[derive(Debug, Serialize)]
pub struct CheckpointBlobsResponse {
    pub new_checkpoint_id: String,
}

#[derive(Debug, Deserialize)]
pub struct BlobStatusRequest {
    #[serde(default)]
    pub blobs: BlobsPayload,
}

#[derive(Debug, Serialize)]
pub struct BlobStatusResponse {
    #[serde(default)]
    pub unknown_blob_names: Vec<String>,
    #[serde(default)]
    pub nonindexed_blob_names: Vec<String>,
    #[serde(default)]
    pub checkpoint_not_found: bool,
}

// ── Admin DTO ──

#[derive(Debug, Serialize)]
pub struct CredentialResponse {
    pub id: i64,
    pub kind: String,
    pub provider: Option<String>,
    pub name: String,
    pub status: String,
    pub priority: i64,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub timeout_seconds: i64,
    pub rate_limit: Option<i64>,
    pub note: Option<String>,
    pub dimensions: Option<i64>,
    pub max_batch_size: Option<i64>,
    pub max_batch_chars: Option<i64>,
    pub max_input_chars: Option<i64>,
    pub input_overlap_chars: Option<i64>,
    pub top_n: Option<i64>,
    pub min_score: Option<f64>,
    pub tpm_limit: Option<i64>,
    pub max_candidates: Option<i64>,
    pub output_top_k: Option<i64>,
    pub snippet_chars: Option<i64>,
    pub num_rewrites: Option<i64>,
    pub api_key_last4: String,
    pub last_used_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CredentialListResponse {
    #[serde(default)]
    pub credentials: Vec<CredentialResponse>,
}

pub type CredentialCreateRequest = oce_core::credentials::CredentialUpsert;
pub type CredentialUpdateRequest = oce_core::credentials::CredentialUpsert;
pub type CredentialDuplicateRequest = oce_core::credentials::CredentialUpsert;

#[derive(Debug, Serialize)]
pub struct QueueStatusResponse {
    pub enabled: bool,
    #[serde(default)]
    pub main_size: usize,
    #[serde(default)]
    pub inflight: usize,
    #[serde(default)]
    pub db_pending: usize,
}

#[derive(Debug, Deserialize)]
pub struct QueueResetRequest {
    #[serde(default = "default_reset_mode")]
    pub mode: String,
    #[serde(default = "default_true")]
    pub requeue: bool,
}

fn default_reset_mode() -> String {
    "sync".into()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct QueueResetResponse {
    #[serde(default)]
    pub removed: usize,
    #[serde(default)]
    pub requeued: usize,
    #[serde(default)]
    pub queue_size: usize,
    #[serde(default)]
    pub db_pending: usize,
}

#[derive(Debug, Deserialize)]
pub struct RequeueStaleRequest {
    #[serde(default = "default_stale_hours")]
    pub stale_hours: i64,
    #[serde(default = "default_requeue_limit")]
    pub limit: usize,
}

fn default_stale_hours() -> i64 {
    24
}

fn default_requeue_limit() -> usize {
    100
}

#[derive(Debug, Serialize)]
pub struct RequeueStaleResponse {
    #[serde(default)]
    pub requeued_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct GcRequest {
    #[serde(default = "default_ttl_days")]
    pub ttl_days: u32,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default = "default_gc_limit")]
    pub limit: usize,
}

fn default_ttl_days() -> u32 {
    30
}

fn default_gc_limit() -> usize {
    1000
}

#[derive(Debug, Serialize)]
pub struct GcResponse {
    pub dry_run: bool,
    pub ttl_days: u32,
    #[serde(default)]
    pub expired_chains: usize,
    #[serde(default)]
    pub expired_blobs: usize,
    #[serde(default)]
    pub deletable_blobs: usize,
    #[serde(default)]
    pub skipped_inflight: usize,
    #[serde(default)]
    pub deleted_chains: usize,
    #[serde(default)]
    pub deleted_blobs: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct MonitoringStatsResponse {
    pub window_hours: u32,
    pub api_calls: ApiCallStatsResponse,
    #[serde(default)]
    pub tokens: Vec<TokenKindStatsResponse>,
    #[serde(default)]
    pub tokens_total: u64,
    pub retrieval: RetrievalStatsResponse,
    #[serde(default)]
    pub resource: Option<ResourceSnapshotResponse>,
}

#[derive(Debug, Serialize, Default)]
pub struct ApiCallStatsResponse {
    pub count: u64,
    #[serde(default)]
    pub error_count: u64,
    #[serde(default)]
    pub avg_latency_ms: f64,
    #[serde(default)]
    pub p50_latency_ms: u64,
    #[serde(default)]
    pub p95_latency_ms: u64,
    #[serde(default)]
    pub max_latency_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct TokenKindStatsResponse {
    pub kind: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Serialize, Default)]
pub struct RetrievalStatsResponse {
    pub count: u64,
    #[serde(default)]
    pub empty_count: u64,
    #[serde(default)]
    pub empty_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct ResourceSnapshotResponse {
    pub ts: Option<String>,
    pub mem_rss_bytes: u64,
    pub mem_percent: f64,
    pub cpu_percent: f64,
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_data_bytes: u64,
}
