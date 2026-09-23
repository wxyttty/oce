//! PostgreSQL 凭据 admin 存储。与 SQLite 版（`sqlite/credentials.rs`）语义对齐：
//! CRUD/复制/热重载、唯一约束 (kind, model, api_key_hash)、脱敏视图（尾 4 位）。

use async_trait::async_trait;
use oce_core::credentials::{
    hash_key, CredentialAdminStore, CredentialRecord, CredentialUpsert, RuntimeCredential,
};
use oce_core::error::{OceError, OceResult};
use sqlx::PgPool;

fn pg_err(e: sqlx::Error) -> OceError {
    // 唯一约束冲突 (kind, model, api_key_hash) → 409 语义
    if let sqlx::Error::Database(ref db) = e {
        if db.code().as_deref().map(|c| c.ends_with("23505")).unwrap_or(false) {
            return OceError::credential_conflict(None);
        }
    }
    OceError::new(e.to_string(), "PgError")
}

/// FromRow 手写实现（derive 宏在本机 toolchain 产生损坏 dylib，见 repos.rs 注释）。
struct CredentialFullRow {
    id: i32,
    kind: String,
    provider: Option<String>,
    name: String,
    status: String,
    priority: i32,
    endpoint: Option<String>,
    model: Option<String>,
    timeout_seconds: i32,
    rate_limit: Option<i32>,
    note: Option<String>,
    dimensions: Option<i32>,
    max_batch_size: Option<i32>,
    max_batch_chars: Option<i32>,
    max_input_chars: Option<i32>,
    input_overlap_chars: Option<i32>,
    top_n: Option<i32>,
    min_score: Option<f64>,
    tpm_limit: Option<i32>,
    max_candidates: Option<i32>,
    output_top_k: Option<i32>,
    snippet_chars: Option<i32>,
    num_rewrites: Option<i32>,
    api_key: String,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for CredentialFullRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            provider: row.try_get("provider")?,
            name: row.try_get("name")?,
            status: row.try_get("status")?,
            priority: row.try_get("priority")?,
            endpoint: row.try_get("endpoint")?,
            model: row.try_get("model")?,
            timeout_seconds: row.try_get("timeout_seconds")?,
            rate_limit: row.try_get("rate_limit")?,
            note: row.try_get("note")?,
            dimensions: row.try_get("dimensions")?,
            max_batch_size: row.try_get("max_batch_size")?,
            max_batch_chars: row.try_get("max_batch_chars")?,
            max_input_chars: row.try_get("max_input_chars")?,
            input_overlap_chars: row.try_get("input_overlap_chars")?,
            top_n: row.try_get("top_n")?,
            min_score: row.try_get("min_score")?,
            tpm_limit: row.try_get("tpm_limit")?,
            max_candidates: row.try_get("max_candidates")?,
            output_top_k: row.try_get("output_top_k")?,
            snippet_chars: row.try_get("snippet_chars")?,
            num_rewrites: row.try_get("num_rewrites")?,
            api_key: row.try_get("api_key")?,
            last_used_at: row.try_get("last_used_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl CredentialFullRow {
    fn to_record(self) -> CredentialRecord {
        let last4: String = self
            .api_key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let ts = |t: Option<chrono::DateTime<chrono::Utc>>| {
            t.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        };
        CredentialRecord {
            id: self.id as i64,
            kind: self.kind,
            provider: self.provider,
            name: self.name,
            status: self.status,
            priority: self.priority as i64,
            endpoint: self.endpoint,
            model: self.model,
            timeout_seconds: self.timeout_seconds as i64,
            rate_limit: self.rate_limit.map(|v| v as i64),
            note: self.note,
            dimensions: self.dimensions.map(|v| v as i64),
            max_batch_size: self.max_batch_size.map(|v| v as i64),
            max_batch_chars: self.max_batch_chars.map(|v| v as i64),
            max_input_chars: self.max_input_chars.map(|v| v as i64),
            input_overlap_chars: self.input_overlap_chars.map(|v| v as i64),
            top_n: self.top_n.map(|v| v as i64),
            min_score: self.min_score,
            tpm_limit: self.tpm_limit.map(|v| v as i64),
            max_candidates: self.max_candidates.map(|v| v as i64),
            output_top_k: self.output_top_k.map(|v| v as i64),
            snippet_chars: self.snippet_chars.map(|v| v as i64),
            num_rewrites: self.num_rewrites.map(|v| v as i64),
            api_key_last4: last4,
            last_used_at: ts(self.last_used_at),
            created_at: ts(self.created_at),
            updated_at: ts(self.updated_at),
        }
    }
}

const CREDENTIAL_COLS: &str = "id, kind, provider, name, status, priority, endpoint, model, \
     timeout_seconds, rate_limit, note, dimensions, max_batch_size, max_batch_chars, \
     max_input_chars, input_overlap_chars, top_n, min_score, tpm_limit, max_candidates, \
     output_top_k, snippet_chars, num_rewrites, api_key, last_used_at, created_at, updated_at";

/// resolve_active 的行模型（17 列元组超 FromRow 支持上限，用结构体）。
/// FromRow 手写实现（同上）。
struct RuntimeRow {
    id: i32,
    endpoint: String,
    api_key: String,
    model: String,
    timeout_seconds: i32,
    tpm_limit: Option<i32>,
    max_candidates: Option<i32>,
    output_top_k: Option<i32>,
    snippet_chars: Option<i32>,
    num_rewrites: Option<i32>,
    top_n: Option<i32>,
    min_score: Option<f64>,
    dimensions: Option<i32>,
    max_batch_size: Option<i32>,
    max_batch_chars: Option<i32>,
    max_input_chars: Option<i32>,
    input_overlap_chars: Option<i32>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for RuntimeRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            endpoint: row.try_get("endpoint")?,
            api_key: row.try_get("api_key")?,
            model: row.try_get("model")?,
            timeout_seconds: row.try_get("timeout_seconds")?,
            tpm_limit: row.try_get("tpm_limit")?,
            max_candidates: row.try_get("max_candidates")?,
            output_top_k: row.try_get("output_top_k")?,
            snippet_chars: row.try_get("snippet_chars")?,
            num_rewrites: row.try_get("num_rewrites")?,
            top_n: row.try_get("top_n")?,
            min_score: row.try_get("min_score")?,
            dimensions: row.try_get("dimensions")?,
            max_batch_size: row.try_get("max_batch_size")?,
            max_batch_chars: row.try_get("max_batch_chars")?,
            max_input_chars: row.try_get("max_input_chars")?,
            input_overlap_chars: row.try_get("input_overlap_chars")?,
        })
    }
}

impl RuntimeRow {
    fn into_runtime(self) -> RuntimeCredential {
        RuntimeCredential {
            id: self.id as i64,
            endpoint: self.endpoint,
            api_key: self.api_key,
            model: self.model,
            timeout_seconds: self.timeout_seconds as i64,
            tpm_limit: self.tpm_limit.map(|v| v as i64),
            max_candidates: self.max_candidates.map(|v| v as i64),
            output_top_k: self.output_top_k.map(|v| v as i64),
            snippet_chars: self.snippet_chars.map(|v| v as i64),
            num_rewrites: self.num_rewrites.map(|v| v as i64),
            top_n: self.top_n.map(|v| v as i64),
            min_score: self.min_score,
            dimensions: self.dimensions.map(|v| v as i64),
            max_batch_size: self.max_batch_size.map(|v| v as i64),
            max_batch_chars: self.max_batch_chars.map(|v| v as i64),
            max_input_chars: self.max_input_chars.map(|v| v as i64),
            input_overlap_chars: self.input_overlap_chars.map(|v| v as i64),
        }
    }
}

#[derive(Clone)]
pub struct PgCredentialAdminStore {
    pub pool: PgPool,
}

async fn read_one(pool: &PgPool, id: i64) -> OceResult<Option<CredentialRecord>> {
    let row: Option<CredentialFullRow> =
        sqlx::query_as(&format!("SELECT {CREDENTIAL_COLS} FROM model_credentials WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(pg_err)?;
    Ok(row.map(|r| r.to_record()))
}

#[async_trait]
impl CredentialAdminStore for PgCredentialAdminStore {
    async fn list(&self) -> OceResult<Vec<CredentialRecord>> {
        let rows: Vec<CredentialFullRow> = sqlx::query_as(&format!(
            "SELECT {CREDENTIAL_COLS} FROM model_credentials ORDER BY kind, priority, id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(rows.into_iter().map(|r| r.to_record()).collect())
    }

    async fn get(&self, credential_id: i64) -> OceResult<Option<CredentialRecord>> {
        read_one(&self.pool, credential_id).await
    }

    async fn create(&self, data: CredentialUpsert) -> OceResult<CredentialRecord> {
        let hash = hash_key(data.api_key.as_deref().unwrap_or(""));
        let row: CredentialFullRow = sqlx::query_as(&format!(
            "INSERT INTO model_credentials
             (kind, provider, name, api_key, api_key_hash, endpoint, model, status, priority,
              timeout_seconds, rate_limit, note, dimensions, max_batch_size, max_batch_chars,
              max_input_chars, input_overlap_chars, top_n, min_score, tpm_limit, max_candidates,
              output_top_k, snippet_chars, num_rewrites, created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24, now(), now())
             RETURNING {CREDENTIAL_COLS}"
        ))
        .bind(data.kind.clone().unwrap_or_default())
        .bind(&data.provider)
        .bind(data.name.clone().unwrap_or_default())
        .bind(data.api_key.clone().unwrap_or_default())
        .bind(hash)
        .bind(&data.endpoint)
        .bind(&data.model)
        .bind(data.status.clone().unwrap_or_else(|| "active".into()))
        .bind(data.priority.unwrap_or(100))
        .bind(data.timeout_seconds.unwrap_or(30))
        .bind(data.rate_limit)
        .bind(&data.note)
        .bind(data.dimensions)
        .bind(data.max_batch_size)
        .bind(data.max_batch_chars)
        .bind(data.max_input_chars)
        .bind(data.input_overlap_chars)
        .bind(data.top_n)
        .bind(data.min_score)
        .bind(data.tpm_limit)
        .bind(data.max_candidates)
        .bind(data.output_top_k)
        .bind(data.snippet_chars)
        .bind(data.num_rewrites)
        .fetch_one(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(row.to_record())
    }

    async fn update(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>> {
        // 先读旧行拿现有 api_key（保持「提供则刷新 hash」语义）
        let old_key: Option<(String,)> =
            sqlx::query_as("SELECT api_key FROM model_credentials WHERE id = $1")
                .bind(credential_id as i32)
                .fetch_optional(&self.pool)
                .await
                .map_err(pg_err)?;
        let Some((old_key,)) = old_key else {
            return Ok(None);
        };
        let api_key = changes.api_key.clone().unwrap_or(old_key);
        let hash = hash_key(&api_key);
        let n = sqlx::query(
            "UPDATE model_credentials SET
                kind = COALESCE($2, kind), provider = COALESCE($3, provider),
                name = COALESCE($4, name), api_key = $5, api_key_hash = $6,
                endpoint = COALESCE($7, endpoint), model = COALESCE($8, model),
                status = COALESCE($9, status), priority = COALESCE($10, priority),
                timeout_seconds = COALESCE($11, timeout_seconds), rate_limit = COALESCE($12, rate_limit),
                note = COALESCE($13, note), dimensions = COALESCE($14, dimensions),
                max_batch_size = COALESCE($15, max_batch_size), max_batch_chars = COALESCE($16, max_batch_chars),
                max_input_chars = COALESCE($17, max_input_chars), input_overlap_chars = COALESCE($18, input_overlap_chars),
                top_n = COALESCE($19, top_n), min_score = COALESCE($20, min_score),
                tpm_limit = COALESCE($21, tpm_limit), max_candidates = COALESCE($22, max_candidates),
                output_top_k = COALESCE($23, output_top_k), snippet_chars = COALESCE($24, snippet_chars),
                num_rewrites = COALESCE($25, num_rewrites), updated_at = now()
             WHERE id = $1",
        )
        .bind(credential_id as i32)
        .bind(&changes.kind)
        .bind(&changes.provider)
        .bind(&changes.name)
        .bind(&api_key)
        .bind(hash)
        .bind(&changes.endpoint)
        .bind(&changes.model)
        .bind(&changes.status)
        .bind(changes.priority)
        .bind(changes.timeout_seconds)
        .bind(changes.rate_limit)
        .bind(&changes.note)
        .bind(changes.dimensions)
        .bind(changes.max_batch_size)
        .bind(changes.max_batch_chars)
        .bind(changes.max_input_chars)
        .bind(changes.input_overlap_chars)
        .bind(changes.top_n)
        .bind(changes.min_score)
        .bind(changes.tpm_limit)
        .bind(changes.max_candidates)
        .bind(changes.output_top_k)
        .bind(changes.snippet_chars)
        .bind(changes.num_rewrites)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        if n.rows_affected() == 0 {
            return Ok(None);
        }
        read_one(&self.pool, credential_id).await
    }

    async fn delete(&self, credential_id: i64) -> OceResult<bool> {
        let n = sqlx::query("DELETE FROM model_credentials WHERE id = $1")
            .bind(credential_id as i32)
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(n.rows_affected() > 0)
    }

    /// 复制源凭据：None 字段继承源行；省略 api_key 即复用源 key。
    async fn duplicate(
        &self,
        credential_id: i64,
        changes: CredentialUpsert,
    ) -> OceResult<Option<CredentialRecord>> {
        let source = match self.get(credential_id).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let (source_key,): (String,) =
            sqlx::query_as("SELECT api_key FROM model_credentials WHERE id = $1")
                .bind(credential_id as i32)
                .fetch_one(&self.pool)
                .await
                .map_err(pg_err)?;

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
    async fn resolve_active(&self, kind: &str) -> OceResult<Option<RuntimeCredential>> {
        let row: Option<RuntimeRow> = sqlx::query_as(
            "SELECT id, endpoint, api_key, model, timeout_seconds, tpm_limit,
                    max_candidates, output_top_k, snippet_chars, num_rewrites,
                    top_n, min_score, dimensions, max_batch_size, max_batch_chars,
                    max_input_chars, input_overlap_chars
             FROM model_credentials
             WHERE kind = $1 AND status = 'active'
               AND endpoint IS NOT NULL AND model IS NOT NULL
             ORDER BY priority, id LIMIT 1",
        )
        .bind(kind)
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(row.map(|r| r.into_runtime()))
    }
}
