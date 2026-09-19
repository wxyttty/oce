//! 凭据 admin 存储 + 凭据解析查询（对应 Python `credential_admin_store.py` +
//! `credential_embedder._resolve_config`）。
//!
//! 明文只落 model_credentials 表；响应与日志一律只暴露尾 4 位。

use crate::sqlite::SqlDb;
use oce_core::error::{OceError, OceResult};
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

pub fn hash_key(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    hex::encode(digest)
}

const CREDENTIAL_COLS: &str = "id, kind, provider, name, status, priority, endpoint, model, timeout_seconds, rate_limit, note, dimensions, max_batch_size, max_batch_chars, max_input_chars, input_overlap_chars, top_n, min_score, tpm_limit, max_candidates, output_top_k, snippet_chars, num_rewrites, api_key, last_used_at, created_at, updated_at";

fn sql_err(e: rusqlite::Error) -> OceError {
    OceError::new(e.to_string(), "SqliteError")
}

fn row_to_record(row: &rusqlite::Row) -> Result<CredentialRecord, rusqlite::Error> {
    let api_key: String = row.get(23)?;
    let last4: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(CredentialRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        provider: row.get(2)?,
        name: row.get(3)?,
        status: row.get(4)?,
        priority: row.get(5)?,
        endpoint: row.get(6)?,
        model: row.get(7)?,
        timeout_seconds: row.get(8)?,
        rate_limit: row.get(9)?,
        note: row.get(10)?,
        dimensions: row.get(11)?,
        max_batch_size: row.get(12)?,
        max_batch_chars: row.get(13)?,
        max_input_chars: row.get(14)?,
        input_overlap_chars: row.get(15)?,
        top_n: row.get(16)?,
        min_score: row.get(17)?,
        tpm_limit: row.get(18)?,
        max_candidates: row.get(19)?,
        output_top_k: row.get(20)?,
        snippet_chars: row.get(21)?,
        num_rewrites: row.get(22)?,
        api_key_last4: last4,
        last_used_at: row.get(24)?,
        created_at: row.get(25)?,
        updated_at: row.get(26)?,
    })
}

fn read_one(
    conn: &mut rusqlite::Connection,
    id: i64,
) -> Result<Option<CredentialRecord>, OceError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {CREDENTIAL_COLS} FROM model_credentials WHERE id = ?1"
        ))
        .map_err(sql_err)?;
    match stmt.query_row([id], row_to_record) {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(sql_err(e)),
    }
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

/// 凭据 admin CRUD（对应 Python `SqlCredentialAdminStore`）。
#[derive(Clone)]
pub struct SqlCredentialAdminStore {
    pub db: SqlDb,
}

impl SqlCredentialAdminStore {
    pub async fn list(&self) -> OceResult<Vec<CredentialRecord>> {
        let db = self.db.clone();
        crate::run_sql_oce(db, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {CREDENTIAL_COLS} FROM model_credentials
                     ORDER BY kind, priority, id"
                ))
                .map_err(sql_err)?;
            let rows = stmt.query_map([], row_to_record).map_err(sql_err)?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .await
    }

    pub async fn get(&self, credential_id: i64) -> OceResult<Option<CredentialRecord>> {
        let db = self.db.clone();
        crate::run_sql_oce(db, move |conn| read_one(conn, credential_id)).await
    }

    pub async fn create(&self, data: CredentialUpsert) -> OceResult<CredentialRecord> {
        let db = self.db.clone();
        let data = data.clone();
        crate::run_sql_oce(db, move |conn| {
            let now = crate::sqlite::repos::utc_now_iso();
            let hash = hash_key(data.api_key.as_deref().unwrap_or(""));
            conn.execute(
                "INSERT INTO model_credentials
                 (kind, provider, name, api_key, api_key_hash, endpoint, model, status, priority,
                  timeout_seconds, rate_limit, note, dimensions, max_batch_size, max_batch_chars,
                  max_input_chars, input_overlap_chars, top_n, min_score, tpm_limit, max_candidates,
                  output_top_k, snippet_chars, num_rewrites, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?25)",
                rusqlite::params![
                    data.kind.clone().unwrap_or_default(),
                    data.provider, data.name.clone().unwrap_or_default(),
                    data.api_key.clone().unwrap_or_default(), hash, data.endpoint,
                    data.model, data.status.clone().unwrap_or_else(|| "active".into()),
                    data.priority.unwrap_or(100), data.timeout_seconds.unwrap_or(30),
                    data.rate_limit, data.note, data.dimensions, data.max_batch_size,
                    data.max_batch_chars, data.max_input_chars, data.input_overlap_chars,
                    data.top_n, data.min_score, data.tpm_limit, data.max_candidates,
                    data.output_top_k, data.snippet_chars, data.num_rewrites, now,
                ],
            )
            .map_err(|e| {
                if e.to_string().contains("UNIQUE") {
                    // 唯一约束冲突 (kind, model, api_key_hash)
                    OceError::credential_conflict(None)
                } else {
                    OceError::new(e.to_string(), "SqliteError")
                }
            })?;
            let id = conn.last_insert_rowid();
            read_one(conn, id)?.ok_or_else(|| OceError::new("credential vanished", "SqliteError"))
        })
        .await
    }

    pub async fn update(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>> {
        let db = self.db.clone();
        crate::run_sql_oce(db, move |conn| {
            // 先读旧行拿现有 api_key（保持「提供则刷新 hash」语义）
            let old_api_key: Option<String> = {
                let mut stmt = conn
                    .prepare("SELECT api_key FROM model_credentials WHERE id = ?1")
                    .map_err(sql_err)?;
                stmt.query_row([credential_id], |r| r.get(0)).ok()
            };
            let Some(old_key) = old_api_key else { return Ok(None) };
            let api_key = changes.api_key.clone().unwrap_or(old_key);
            let hash = hash_key(&api_key);
            let n = conn
                .execute(
                    "UPDATE model_credentials SET
                        kind = COALESCE(?2, kind), provider = COALESCE(?3, provider),
                        name = COALESCE(?4, name), api_key = ?5, api_key_hash = ?6,
                        endpoint = COALESCE(?7, endpoint), model = COALESCE(?8, model),
                        status = COALESCE(?9, status), priority = COALESCE(?10, priority),
                        timeout_seconds = COALESCE(?11, timeout_seconds), rate_limit = COALESCE(?12, rate_limit),
                        note = COALESCE(?13, note), dimensions = COALESCE(?14, dimensions),
                        max_batch_size = COALESCE(?15, max_batch_size), max_batch_chars = COALESCE(?16, max_batch_chars),
                        max_input_chars = COALESCE(?17, max_input_chars), input_overlap_chars = COALESCE(?18, input_overlap_chars),
                        top_n = COALESCE(?19, top_n), min_score = COALESCE(?20, min_score),
                        tpm_limit = COALESCE(?21, tpm_limit), max_candidates = COALESCE(?22, max_candidates),
                        output_top_k = COALESCE(?23, output_top_k), snippet_chars = COALESCE(?24, snippet_chars),
                        num_rewrites = COALESCE(?25, num_rewrites), updated_at = ?26
                     WHERE id = ?1",
                    rusqlite::params![
                        credential_id, changes.kind, changes.provider, changes.name,
                        api_key, hash, changes.endpoint, changes.model, changes.status,
                        changes.priority, changes.timeout_seconds, changes.rate_limit, changes.note,
                        changes.dimensions, changes.max_batch_size, changes.max_batch_chars,
                        changes.max_input_chars, changes.input_overlap_chars, changes.top_n,
                        changes.min_score, changes.tpm_limit, changes.max_candidates,
                        changes.output_top_k, changes.snippet_chars, changes.num_rewrites,
                        crate::sqlite::repos::utc_now_iso(),
                    ],
                )
                .map_err(|e| {
                    if e.to_string().contains("UNIQUE") {
                        OceError::credential_conflict(None)
                    } else {
                        OceError::new(e.to_string(), "SqliteError")
                    }
                })?;
            if n == 0 {
                return Ok(None);
            }
            read_one(conn, credential_id)
        })
        .await
    }

    pub async fn delete(&self, credential_id: i64) -> OceResult<bool> {
        let db = self.db.clone();
        crate::run_sql_oce(db, move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM model_credentials WHERE id = ?1",
                    [credential_id],
                )
                .map_err(sql_err)?;
            Ok(n > 0)
        })
        .await
    }

    /// 复制源凭据：None 字段继承源行；省略 api_key 即复用源 key。
    pub async fn duplicate(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>> {
        let source = match self.get(credential_id).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let db = self.db.clone();
        let source_key = crate::run_sql_oce(db, move |conn| {
            let mut stmt = conn
                .prepare("SELECT api_key FROM model_credentials WHERE id = ?1")
                .map_err(sql_err)?;
            match stmt.query_row([credential_id], |r| r.get::<_, String>(0)) {
                Ok(k) => Ok(k),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    Err(OceError::new("credential not found", "NotFound"))
                }
                Err(e) => Err(sql_err(e)),
            }
        })
        .await?;

        let merged = CredentialUpsert {
            kind: changes.kind.or(Some(source.kind)),
            name: changes.name.or(Some(source.name)),
            api_key: changes.api_key.or(Some(source_key)),
            provider: changes.provider.or(source.provider),
            status: changes.status.or(Some(source.status)),
            priority: changes.priority.or(Some(source.priority)),
            endpoint: changes.endpoint.or(source.endpoint),
            model: changes.model.or(source.model),
            timeout_seconds: changes.timeout_seconds.or(Some(source.timeout_seconds)),
            rate_limit: changes.rate_limit.or(source.rate_limit),
            note: changes.note.or(source.note),
            dimensions: changes.dimensions.or(source.dimensions),
            max_batch_size: changes.max_batch_size.or(source.max_batch_size),
            max_batch_chars: changes.max_batch_chars.or(source.max_batch_chars),
            max_input_chars: changes.max_input_chars.or(source.max_input_chars),
            input_overlap_chars: changes.input_overlap_chars.or(source.input_overlap_chars),
            top_n: changes.top_n.or(source.top_n),
            min_score: changes.min_score.or(source.min_score),
            tpm_limit: changes.tpm_limit.or(source.tpm_limit),
            max_candidates: changes.max_candidates.or(source.max_candidates),
            output_top_k: changes.output_top_k.or(source.output_top_k),
            snippet_chars: changes.snippet_chars.or(source.snippet_chars),
            num_rewrites: changes.num_rewrites.or(source.num_rewrites),
        };
        self.create(merged).await.map(Some)
    }

    /// 解析 kind 专属运行时凭据：kind + active + endpoint/model 非空，
    /// 按 (priority, id) 升序取第一条；取不到返回 None（调用方回落 env）。
    pub async fn resolve_active(&self, kind: &str) -> OceResult<Option<RuntimeCredential>> {
        let db = self.db.clone();
        let kind = kind.to_string();
        crate::run_sql_oce(db, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, endpoint, api_key, model, timeout_seconds, tpm_limit,
                            max_candidates, output_top_k, snippet_chars, num_rewrites,
                            top_n, min_score, dimensions, max_batch_size, max_batch_chars,
                            max_input_chars, input_overlap_chars
                     FROM model_credentials
                     WHERE kind = ?1 AND status = 'active'
                       AND endpoint IS NOT NULL AND model IS NOT NULL
                     ORDER BY priority, id LIMIT 1",
                )
                .map_err(sql_err)?;
            match stmt.query_row([&kind], |row| {
                Ok(RuntimeCredential {
                    id: row.get(0)?,
                    endpoint: row.get(1)?,
                    api_key: row.get(2)?,
                    model: row.get(3)?,
                    timeout_seconds: row.get(4)?,
                    tpm_limit: row.get(5)?,
                    max_candidates: row.get(6)?,
                    output_top_k: row.get(7)?,
                    snippet_chars: row.get(8)?,
                    num_rewrites: row.get(9)?,
                    top_n: row.get(10)?,
                    min_score: row.get(11)?,
                    dimensions: row.get(12)?,
                    max_batch_size: row.get(13)?,
                    max_batch_chars: row.get(14)?,
                    max_input_chars: row.get(15)?,
                    input_overlap_chars: row.get(16)?,
                })
            }) {
                Ok(r) => Ok(Some(r)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(sql_err(e)),
            }
        })
        .await
    }
}
