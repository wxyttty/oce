//! SQLite 连接与 schema。与 Python 版（StaticPool + WAL + busy_timeout=30s）对齐：
//! 个人模式单连接、WAL 并发读；schema 与 alembic head 一致。

pub mod chains;
pub mod credentials;
pub mod metrics;
pub mod repos;

use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 单连接 SQLite 封装（阻塞调用经 spawn_blocking 隔离）。
#[derive(Clone)]
pub struct SqlDb {
    conn: Arc<Mutex<Connection>>,
}

impl SqlDb {
    pub fn open(path: &str) -> Result<Self, String> {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        // 与 sqlite_adapter.py 的 PRAGMA 一致
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA_SQL).map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 在连接上执行阻塞操作（内部持锁）。
    pub fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T, String>) -> Result<T, String> {
        let mut guard = self.conn.lock().map_err(|_| "sqlite lock poisoned".to_string())?;
        f(&mut guard)
    }

    /// 取连接锁（OceError 语义的调用方使用）。
    pub fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Schema：与 alembic head（models.py）一致。服务模式（PostgreSQL）走 alembic，
/// SQLite 个人库由此建表。
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS model_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    provider TEXT,
    name TEXT NOT NULL,
    api_key TEXT NOT NULL,
    api_key_hash TEXT NOT NULL,
    endpoint TEXT,
    model TEXT,
    status TEXT NOT NULL DEFAULT 'active',
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
    min_score REAL,
    tpm_limit INTEGER,
    max_candidates INTEGER,
    output_top_k INTEGER,
    snippet_chars INTEGER,
    num_rewrites INTEGER,
    last_used_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CONSTRAINT uq_model_credentials_kind_model_key UNIQUE (kind, model, api_key_hash)
);
CREATE INDEX IF NOT EXISTS idx_model_credentials_kind_status_priority
    ON model_credentials (kind, status, priority);

CREATE TABLE IF NOT EXISTS blobs (
    blob_name TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    content_size INTEGER NOT NULL,
    language TEXT,
    file_type TEXT NOT NULL DEFAULT 'text',
    status TEXT NOT NULL DEFAULT 'pending',
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_seen TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    error_message TEXT
);
CREATE INDEX IF NOT EXISTS ix_blobs_status ON blobs (status);
CREATE INDEX IF NOT EXISTS ix_blobs_last_seen ON blobs (last_seen);
CREATE INDEX IF NOT EXISTS ix_blobs_language ON blobs (language);
CREATE INDEX IF NOT EXISTS ix_blobs_retry_count ON blobs (retry_count);

CREATE TABLE IF NOT EXISTS blob_staging (
    blob_name TEXT PRIMARY KEY REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS ix_blob_staging_created_at ON blob_staging (created_at);

CREATE TABLE IF NOT EXISTS chunks (
    content_hash TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    content_size INTEGER NOT NULL,
    chunk_type TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    embedded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ix_chunks_chunk_type ON chunks (chunk_type);
CREATE INDEX IF NOT EXISTS ix_chunks_embedded ON chunks (embedded);

CREATE TABLE IF NOT EXISTS blob_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    blob_name TEXT NOT NULL REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content_hash TEXT NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    CONSTRAINT uq_blob_chunks_span UNIQUE (blob_name, content_hash, start_line, end_line)
);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_blob_name ON blob_chunks (blob_name);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_content_hash ON blob_chunks (content_hash);
CREATE INDEX IF NOT EXISTS ix_blob_chunks_blob_index ON blob_chunks (blob_name, chunk_index);

CREATE TABLE IF NOT EXISTS chains (
    chain_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL DEFAULT 1,
    description TEXT,
    total_blobs INTEGER NOT NULL DEFAULT 0,
    total_chunks INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS ix_chains_updated_at ON chains (updated_at);

CREATE TABLE IF NOT EXISTS chain_members (
    chain_id TEXT NOT NULL REFERENCES chains(chain_id) ON DELETE CASCADE,
    blob_name TEXT NOT NULL,
    PRIMARY KEY (chain_id, blob_name)
);
CREATE INDEX IF NOT EXISTS ix_chain_members_blob_name ON chain_members (blob_name);

CREATE TABLE IF NOT EXISTS symbol_occurrences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    identifier TEXT NOT NULL,
    blob_name TEXT NOT NULL REFERENCES blobs(blob_name) ON DELETE CASCADE,
    content_hash TEXT NOT NULL REFERENCES chunks(content_hash) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    CONSTRAINT uq_symbol_occurrences_key UNIQUE (identifier, blob_name, content_hash, kind)
);
CREATE INDEX IF NOT EXISTS idx_so_identifier ON symbol_occurrences (identifier);
CREATE INDEX IF NOT EXISTS idx_so_blob_name ON symbol_occurrences (blob_name);
CREATE INDEX IF NOT EXISTS idx_so_identifier_kind ON symbol_occurrences (identifier, kind);
CREATE INDEX IF NOT EXISTS idx_so_content_hash ON symbol_occurrences (content_hash);

CREATE TABLE IF NOT EXISTS api_call_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    endpoint TEXT NOT NULL,
    method TEXT NOT NULL,
    status_code INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    error_type TEXT
);
CREATE INDEX IF NOT EXISTS ix_api_call_metrics_ts ON api_call_metrics (ts);
CREATE INDEX IF NOT EXISTS ix_api_call_metrics_endpoint ON api_call_metrics (endpoint);

CREATE TABLE IF NOT EXISTS token_usage_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    kind TEXT NOT NULL,
    model TEXT NOT NULL,
    credential_id INTEGER,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_ts ON token_usage_metrics (ts);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_kind ON token_usage_metrics (kind);
CREATE INDEX IF NOT EXISTS ix_token_usage_metrics_credential_id ON token_usage_metrics (credential_id);

CREATE TABLE IF NOT EXISTS resource_samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    disk_data_bytes INTEGER NOT NULL,
    disk_free_bytes INTEGER NOT NULL,
    disk_total_bytes INTEGER NOT NULL,
    mem_rss_bytes INTEGER NOT NULL,
    mem_percent REAL NOT NULL,
    cpu_percent REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_resource_samples_ts ON resource_samples (ts);

-- 工作区嵌入式 MCP 模式的登记表（HTTP 模式不使用；独立于 Python 共享 schema 的扩展表）
CREATE TABLE IF NOT EXISTS workspace_files (
    path TEXT PRIMARY KEY,
    blob_name TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS retrieval_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    source TEXT NOT NULL,
    scope_size INTEGER,
    hit_count INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    intent TEXT,
    path_boosted INTEGER NOT NULL DEFAULT 0,
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
