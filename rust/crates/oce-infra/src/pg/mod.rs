//! PostgreSQL 元数据存储（服务模式）。实现与 SQLite 版相同的端口：
//! `BlobRepository` / `ChainRepository` / `CredentialAdminStore` / `MetricsSink` /
//! `MonitoringStatsReader` / `ReportsStore`。
//!
//! Schema 与 alembic head（`models.py`）逐列一致：既有 Python 建的 PG 库可直接挂载；
//! 空库由 `SCHEMA_SQL` 幂等建表（`CREATE TABLE IF NOT EXISTS`）。
//!
//! 方言差异（相对 SQLite 版）：
//! - `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`
//! - `INSERT ... ON CONFLICT DO UPDATE`（upsert 语义同 Python pg_insert）
//! - 时间戳用原生 `TIMESTAMPTZ`（与 SQLAlchemy `DateTime(timezone=True)` 对齐），
//!   Rust 侧 chrono `DateTime<Utc>` 直接绑定
//! - `strftime('%Y-%m-%dT%H:%M:%fZ','now')` → `now()`
//!
//! 报表聚合约定不变：SQL 只做窗口过滤与基础聚合；分桶与分位数在 Rust 侧计算
//! （与 Python 版跨方言可移植性设计一致）。

pub mod chains;
pub mod credentials;
pub mod metrics;
pub mod repos;
pub mod reports;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// 打开连接池并确保 schema 存在（幂等）。
pub async fn open_pool(url: &str, max_connections: u32) -> Result<PgPool, String> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections.max(1))
        .acquire_timeout(std::time::Duration::from_secs(5))
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET timezone TO 'UTC'").execute(conn).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .map_err(|e| format!("connect postgres: {e}"))?;
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(&pool)
        .await
        .map_err(|e| format!("apply schema: {e}"))?;
    Ok(pool)
}

/// Schema：与 alembic head 一致（服务模式由 compose/手动 alembic 维护版本表；
/// 这里幂等建表兜底空库，不与 alembic 冲突——已存在的表不会被改动）。
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS blobs (
    blob_name VARCHAR(64) PRIMARY KEY,
    path VARCHAR(1024) NOT NULL,
    content_size INTEGER NOT NULL,
    language VARCHAR(32),
    file_type VARCHAR(16) NOT NULL,
    status VARCHAR(16) NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    error_message TEXT
);
CREATE INDEX IF NOT EXISTS ix_blobs_status ON blobs (status);
CREATE INDEX IF NOT EXISTS ix_blobs_last_seen ON blobs (last_seen);
CREATE INDEX IF NOT EXISTS ix_blobs_language ON blobs (language);
CREATE INDEX IF NOT EXISTS ix_blobs_retry_count ON blobs (retry_count);

CREATE TABLE IF NOT EXISTS blob_staging (
    blob_name VARCHAR(64) PRIMARY KEY REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ix_blob_staging_created_at ON blob_staging (created_at);

CREATE TABLE IF NOT EXISTS chunks (
    content_hash VARCHAR(64) PRIMARY KEY,
    content TEXT NOT NULL,
    content_size INTEGER NOT NULL,
    chunk_type VARCHAR(32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    embedded BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS ix_chunks_chunk_type ON chunks (chunk_type);
CREATE INDEX IF NOT EXISTS ix_chunks_embedded ON chunks (embedded);

CREATE TABLE IF NOT EXISTS blob_chunks (
    id BIGSERIAL PRIMARY KEY,
    blob_name VARCHAR(64) NOT NULL REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content_hash VARCHAR(64) NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    CONSTRAINT uq_blob_chunks_span UNIQUE (blob_name, content_hash, start_line, end_line)
);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_blob_name ON blob_chunks (blob_name);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_content_hash ON blob_chunks (content_hash);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_blob_index ON blob_chunks (blob_name, chunk_index);

CREATE TABLE IF NOT EXISTS chains (
    chain_id VARCHAR(64) PRIMARY KEY,
    version INTEGER NOT NULL DEFAULT 1,
    description VARCHAR(512),
    total_blobs INTEGER NOT NULL DEFAULT 0,
    total_chunks INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ix_chains_updated_at ON chains (updated_at);

CREATE TABLE IF NOT EXISTS chain_members (
    chain_id VARCHAR(64) NOT NULL REFERENCES chains(chain_id) ON DELETE CASCADE,
    blob_name VARCHAR(64) NOT NULL,
    PRIMARY KEY (chain_id, blob_name)
);
CREATE INDEX IF NOT EXISTS ix_chain_members_blob_name ON chain_members (blob_name);

CREATE TABLE IF NOT EXISTS symbol_occurrences (
    id BIGSERIAL PRIMARY KEY,
    identifier VARCHAR(256) NOT NULL,
    blob_name VARCHAR(64) NOT NULL REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content_hash VARCHAR(64) NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    kind VARCHAR(16) NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT uq_symbol_occurrences_key UNIQUE (identifier, blob_name, content_hash, kind)
);
CREATE INDEX IF NOT EXISTS idx_so_identifier ON symbol_occurrences (identifier);
CREATE INDEX IF NOT EXISTS idx_so_blob_name ON symbol_occurrences (blob_name);
CREATE INDEX IF NOT EXISTS idx_so_identifier_kind ON symbol_occurrences (identifier, kind);
CREATE INDEX IF NOT EXISTS idx_so_content_hash ON symbol_occurrences (content_hash);

CREATE TABLE IF NOT EXISTS model_credentials (
    id SERIAL PRIMARY KEY,
    kind VARCHAR(16) NOT NULL,
    provider VARCHAR(64),
    name VARCHAR(128) NOT NULL,
    api_key VARCHAR(512) NOT NULL,
    api_key_hash VARCHAR(64) NOT NULL,
    endpoint VARCHAR(512),
    model VARCHAR(128),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    priority INTEGER NOT NULL DEFAULT 100,
    timeout_seconds INTEGER NOT NULL DEFAULT 30,
    rate_limit INTEGER,
    note TEXT,
    dimensions INTEGER,
    max_batch_size INTEGER,
    max_batch_chars INTEGER,
    max_input_chars INTEGER,
    input_overlap_chars INTEGER,
    top_n INTEGER,
    min_score DOUBLE PRECISION,
    tpm_limit INTEGER,
    max_candidates INTEGER,
    output_top_k INTEGER,
    snippet_chars INTEGER,
    num_rewrites INTEGER,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT uq_model_credentials_kind_model_key UNIQUE (kind, model, api_key_hash)
);
CREATE INDEX IF NOT EXISTS idx_model_credentials_kind_status_priority
    ON model_credentials (kind, status, priority);

CREATE TABLE IF NOT EXISTS api_call_metrics (
    id BIGSERIAL PRIMARY KEY,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    endpoint VARCHAR(128) NOT NULL,
    method VARCHAR(8) NOT NULL,
    status_code INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    error_type VARCHAR(64)
);
CREATE INDEX IF NOT EXISTS ix_api_call_metrics_ts ON api_call_metrics (ts);
CREATE INDEX IF NOT EXISTS ix_api_call_metrics_endpoint ON api_call_metrics (endpoint);

CREATE TABLE IF NOT EXISTS token_usage_metrics (
    id BIGSERIAL PRIMARY KEY,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind VARCHAR(16) NOT NULL,
    model VARCHAR(128) NOT NULL,
    credential_id INTEGER,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_ts ON token_usage_metrics (ts);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_kind ON token_usage_metrics (kind);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_credential_id ON token_usage_metrics (credential_id);

CREATE TABLE IF NOT EXISTS resource_samples (
    id BIGSERIAL PRIMARY KEY,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    disk_data_bytes BIGINT NOT NULL,
    disk_free_bytes BIGINT NOT NULL,
    disk_total_bytes BIGINT NOT NULL,
    mem_rss_bytes BIGINT NOT NULL,
    mem_percent DOUBLE PRECISION NOT NULL,
    cpu_percent DOUBLE PRECISION NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_resource_samples_ts ON resource_samples (ts);

CREATE TABLE IF NOT EXISTS retrieval_metrics (
    id BIGSERIAL PRIMARY KEY,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    source VARCHAR(32) NOT NULL,
    scope_size INTEGER,
    hit_count INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    intent VARCHAR(32),
    path_boosted BOOLEAN NOT NULL DEFAULT false,
    query_text TEXT,
    intent_ms INTEGER,
    rewrite_ms INTEGER,
    dense_ms INTEGER,
    exact_ms INTEGER,
    fuse_ms INTEGER,
    rerank_ms INTEGER,
    llm_rerank_ms INTEGER,
    select_ms INTEGER
);
CREATE INDEX IF NOT EXISTS ix_retrieval_metrics_ts ON retrieval_metrics (ts);
CREATE INDEX IF NOT EXISTS ix_retrieval_metrics_source ON retrieval_metrics (source);
CREATE INDEX IF NOT EXISTS ix_retrieval_metrics_hit_count ON retrieval_metrics (hit_count);
"#;
