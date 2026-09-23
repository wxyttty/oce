//! 凭据 admin 存储端口（对应 Python `credential_admin_store.py` 协议面）。
//!
//! 明文只落 model_credentials 表；`CredentialRecord` 视图只暴露尾 4 位。
//! DTO 定义在 core（infra 的 SQLite 实现与未来的 PG 实现共享）。

use crate::error::OceResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 凭据视图（脱敏，对应 Python `CredentialRecord`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
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

/// 创建/更新请求。更新语义：None 表示不改（api_key 提供则同步刷新 hash）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialUpsert {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i64>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub timeout_seconds: Option<i64>,
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
}

/// 运行时凭据（内部使用，含明文 key；响应与日志只暴露尾 4 位）。
#[derive(Debug, Clone)]
pub struct RuntimeCredential {
    pub id: i64,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub timeout_seconds: i64,
    pub tpm_limit: Option<i64>,
    pub max_candidates: Option<i64>,
    pub output_top_k: Option<i64>,
    pub snippet_chars: Option<i64>,
    pub num_rewrites: Option<i64>,
    pub top_n: Option<i64>,
    pub min_score: Option<f64>,
    /// embed 专属参数（其它 kind 恒为 None）
    pub dimensions: Option<i64>,
    pub max_batch_size: Option<i64>,
    pub max_batch_chars: Option<i64>,
    pub max_input_chars: Option<i64>,
    pub input_overlap_chars: Option<i64>,
}

/// api_key 的去重哈希（唯一约束 (kind, model, api_key_hash) 的第三列）。
pub fn hash_key(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    hex::encode(digest)
}

/// 凭据 admin CRUD + 运行时解析端口。
#[async_trait]
pub trait CredentialAdminStore: Send + Sync {
    async fn list(&self) -> OceResult<Vec<CredentialRecord>>;
    async fn get(&self, credential_id: i64) -> OceResult<Option<CredentialRecord>>;
    async fn create(&self, data: CredentialUpsert) -> OceResult<CredentialRecord>;
    /// 更新；目标不存在返回 None（404 语义）。
    async fn update(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>>;
    async fn delete(&self, credential_id: i64) -> OceResult<bool>;
    /// 复制源凭据：None 字段继承源行；省略 api_key 即复用源 key。
    async fn duplicate(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>>;
    /// 解析 kind 专属运行时凭据：kind + active + 最小 priority；取不到返回 None
    /// （调用方回落 env）。
    async fn resolve_active(&self, kind: &str) -> OceResult<Option<RuntimeCredential>>;
}
