//! SQLite Blob/Staging/Chunk/Symbol 仓储。与 Python `sql_blob_repo.py` +
//! `sql_chunk_repo.py` 语义逐条对齐（UPSERT 列集、孤儿 chunk 清理、staging 幂等写入）。

use crate::sqlite::SqlDb;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use oce_core::blob::{Blob, BlobStatus};
use oce_core::chunk::{Chunk, ChunkRef, LocatedChunk};
use oce_core::error::{OceError, OceResult};
use oce_core::indexing::BlobRepository;
use oce_core::symbol::SymbolExtractor;
use rayon::prelude::*;
use std::collections::HashMap;

pub struct SqlBlobRepository {
    pub db: SqlDb,
}

pub(crate) fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

const BLOB_COLS: &str =
    "blob_name, path, content_size, language, file_type, status, retry_count, last_seen, created_at, error_message";

fn row_to_blob(row: &rusqlite::Row, chunks: Vec<ChunkRef>) -> Result<Blob, rusqlite::Error> {
    Ok(Blob {
        blob_name: row.get::<_, String>(0)?,
        path: row.get::<_, String>(1)?,
        content_size: row.get::<_, i64>(2)? as u64,
        language: row.get::<_, Option<String>>(3)?,
        file_type: row.get::<_, String>(4)?,
        status: BlobStatus::from_str(&row.get::<_, String>(5)?),
        retry_count: row.get::<_, i64>(6)? as u32,
        last_seen: parse_ts(&row.get::<_, String>(7)?),
        created_at: parse_ts(&row.get::<_, String>(8)?),
        error_message: row.get::<_, Option<String>>(9)?,
        chunks,
    })
}

fn load_chunks(conn: &rusqlite::Connection, blob_name: &str) -> Vec<ChunkRef> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT content_hash, start_line, end_line FROM blob_chunks
         WHERE blob_name = ?1 ORDER BY chunk_index",
    ) else {
        return vec![];
    };
    stmt.query_map([blob_name], |row| {
        Ok(ChunkRef {
            content_hash: row.get(0)?,
            start_line: row.get::<_, i64>(1)? as u32,
            end_line: row.get::<_, i64>(2)? as u32,
        })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// 保存 chunks + 符号提取（chunk 表按 content_hash 去重，blob_chunks 记录出现位置）。
/// 与 Python `save_many` → `_save_blob_chunks` + `_extract_and_save_symbols` 等价。
fn insert_chunks_and_symbols(
    tx: &rusqlite::Transaction,
    blob_name: &str,
    chunks: &[Chunk],
) -> Result<(), String> {
    // 符号提取是正则密集的 CPU 工作：块数多时并行预提取，事务内只做批量写入
    let symbol_values = extract_symbols_parallel(chunks);

    // 预编译语句：几千行的批量插入里每行重新 prepare 的开销占大头
    let mut stmt_chunk = tx.prepare_cached(
        "INSERT OR IGNORE INTO chunks (content_hash, content, content_size, chunk_type, embedded)
         VALUES (?1, ?2, ?3, ?4, 0)",
    )
    .map_err(|e| e.to_string())?;
    for chunk in chunks {
        stmt_chunk
            .execute(rusqlite::params![
                chunk.content_hash,
                chunk.content,
                chunk.content.len() as i64,
                chunk.chunk_type,
            ])
            .map_err(|e| e.to_string())?;
    }
    let mut stmt_symbol = tx
        .prepare_cached(
            "INSERT OR IGNORE INTO symbol_occurrences
         (identifier, blob_name, content_hash, kind, start_line, end_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|e| e.to_string())?;
    for (identifier, kind, content_hash, start, end) in &symbol_values {
        stmt_symbol
            .execute(rusqlite::params![
                identifier,
                blob_name,
                content_hash,
                kind,
                start,
                end
            ])
            .map_err(|e| e.to_string())?;
    }
    let mut stmt_blob_chunk = tx.prepare_cached(
        "INSERT OR IGNORE INTO blob_chunks (blob_name, content_hash, start_line, end_line, chunk_index)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .map_err(|e| e.to_string())?;
    for (index, chunk) in chunks.iter().enumerate() {
        stmt_blob_chunk
            .execute(rusqlite::params![
                blob_name,
                chunk.content_hash,
                chunk.start_line as i64,
                chunk.end_line as i64,
                index as i64,
            ])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// save() 路径：对 (content_hash, start, end, content) 批量并行提取符号。
fn extract_symbols_for_refs_parallel(
    refs: &[(String, u32, u32, String)],
) -> Vec<(String, String, String, i64, i64)> {
    let extract = |(hash, start, end, content): &(String, u32, u32, String)| -> Vec<(String, String, String, i64, i64)> {
        SymbolExtractor::extract_symbols(content, *start, *end)
            .into_iter()
            .map(|sym| (sym.identifier, sym.kind, hash.clone(), sym.start_line as i64, sym.end_line as i64))
            .collect()
    };
    if refs.len() >= 8 {
        refs.par_iter().flat_map(extract).collect()
    } else {
        refs.iter().flat_map(extract).collect()
    }
}

/// 符号提取（正则密集）在块数多时用 rayon 并行；少量块顺序执行避免线程开销。
fn extract_symbols_parallel(chunks: &[Chunk]) -> Vec<(String, String, String, i64, i64)> {
    let extract_all = |chunk: &Chunk| -> Vec<(String, String, String, i64, i64)> {
        SymbolExtractor::extract_symbols(&chunk.content, chunk.start_line, chunk.end_line)
            .into_iter()
            .map(|sym| {
                (
                    sym.identifier,
                    sym.kind,
                    chunk.content_hash.clone(),
                    sym.start_line as i64,
                    sym.end_line as i64,
                )
            })
            .collect()
    };
    if chunks.len() >= 8 {
        chunks.par_iter().flat_map(extract_all).collect()
    } else {
        chunks.iter().flat_map(extract_all).collect()
    }
}

impl SqlBlobRepository {
    /// bench 便捷入口：批量写 chunk 内容 + 符号 + 出现位置并标记已嵌入。
    pub async fn save_chunks_and_mark(
        &self,
        located: &[oce_core::chunk::LocatedChunk],
    ) -> OceResult<()> {
        let chunks: Vec<Chunk> = located
            .iter()
            .map(|l| {
                Chunk::new(
                    l.content_hash.clone(),
                    l.path.clone(),
                    l.content.clone(),
                    l.start_line,
                    l.end_line,
                    None,
                )
                .unwrap()
            })
            .collect();
        // 同 blob 分组逐个写入
        let mut by_blob: std::collections::HashMap<String, Vec<Chunk>> =
            std::collections::HashMap::new();
        for (l, chunk) in located.iter().zip(chunks.into_iter()) {
            by_blob.entry(l.blob_name.clone()).or_default().push(chunk);
        }
        let batches: Vec<(String, Vec<Chunk>)> = by_blob.into_iter().collect();
        BlobRepository::save_chunks_many(self, &batches).await?;
        let hashes: Vec<String> = located.iter().map(|l| l.content_hash.clone()).collect();
        self.mark_embedded(&hashes).await
    }
}

#[async_trait]
impl BlobRepository for SqlBlobRepository {
    async fn find_all_blob_names(&self) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT blob_name FROM blobs ORDER BY blob_name")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }


    async fn get(&self, blob_name: &str) -> OceResult<Option<Blob>> {
        let db = self.db.clone();
        let name = blob_name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let chunks = load_chunks(conn, &name);
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {BLOB_COLS} FROM blobs WHERE blob_name = ?1"
                    ))
                    .map_err(|e| e.to_string())?;
                match stmt.query_row([&name], |row| row_to_blob(row, chunks)) {
                    Ok(blob) => Ok(Some(blob)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn save(&self, blob: &Blob) -> OceResult<()> {
        let db = self.db.clone();
        let blob = blob.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                conn.execute(
                    "INSERT INTO blobs (blob_name, path, content_size, language, file_type, status, retry_count, last_seen, created_at, error_message)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(blob_name) DO UPDATE SET
                        path = excluded.path,
                        content_size = excluded.content_size,
                        language = excluded.language,
                        file_type = excluded.file_type,
                        status = excluded.status,
                        retry_count = excluded.retry_count,
                        last_seen = excluded.last_seen,
                        error_message = excluded.error_message",
                    rusqlite::params![
                        blob.blob_name,
                        blob.path,
                        blob.content_size as i64,
                        blob.language,
                        blob.file_type,
                        blob.status.as_str(),
                        blob.retry_count as i64,
                        blob.last_seen.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        blob.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        blob.error_message,
                    ],
                )
                .map_err(|e| e.to_string())?;
                // chunks 非空时同步出现位置 + 符号（Python save_many 语义）
                if !blob.chunks.is_empty() {
                    // blob_chunks 出现位置 + 符号提取（按 chunk_ref 逐条）
                    for (index, chunk_ref) in blob.chunks.iter().enumerate() {
                        conn.execute(
                            "INSERT OR IGNORE INTO blob_chunks (blob_name, content_hash, start_line, end_line, chunk_index)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![
                                blob.blob_name,
                                chunk_ref.content_hash,
                                chunk_ref.start_line as i64,
                                chunk_ref.end_line as i64,
                                index as i64,
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    let contents: Vec<(String, u32, u32, String)> = {
                        let mut stmt2 = conn
                            .prepare("SELECT content FROM chunks WHERE content_hash = ?1")
                            .map_err(|e| e.to_string())?;
                        blob.chunks
                            .iter()
                            .filter_map(|chunk_ref| {
                                stmt2
                                    .query_row([&chunk_ref.content_hash], |r| {
                                        r.get::<_, String>(0)
                                    })
                                    .ok()
                                    .map(|content| {
                                        (
                                            chunk_ref.content_hash.clone(),
                                            chunk_ref.start_line,
                                            chunk_ref.end_line,
                                            content,
                                        )
                                    })
                            })
                            .collect()
                    };
                    let symbol_values = extract_symbols_for_refs_parallel(&contents);
                    for (identifier, kind, content_hash, start, end) in &symbol_values {
                        conn.execute(
                            "INSERT OR IGNORE INTO symbol_occurrences
                             (identifier, blob_name, content_hash, kind, start_line, end_line)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            rusqlite::params![
                                identifier, blob.blob_name, content_hash, kind, start, end
                            ],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn delete(&self, blob_name: &str) -> OceResult<()> {
        let db = self.db.clone();
        let name = blob_name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                // 与 Python delete_many 一致：删除后清理孤儿 chunk 内容
                let mut stmt = conn
                    .prepare("SELECT DISTINCT content_hash FROM blob_chunks WHERE blob_name = ?1")
                    .map_err(|e| e.to_string())?;
                let hashes: Vec<String> = stmt
                    .query_map([&name], |r| r.get(0))
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                conn.execute("DELETE FROM blob_chunks WHERE blob_name = ?1", [&name])
                    .map_err(|e| e.to_string())?;
                conn.execute("DELETE FROM blobs WHERE blob_name = ?1", [&name])
                    .map_err(|e| e.to_string())?;
                conn.execute(
                    "DELETE FROM symbol_occurrences WHERE blob_name = ?1",
                    [&name],
                )
                .map_err(|e| e.to_string())?;
                for hash in &hashes {
                    conn.execute(
                        "DELETE FROM chunks WHERE content_hash = ?1
                         AND NOT EXISTS (SELECT 1 FROM blob_chunks WHERE content_hash = ?1)",
                        [hash],
                    )
                    .map_err(|e| e.to_string())?;
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn save_staging(&self, blob_name: &str, content: &str) -> OceResult<()> {
        let db = self.db.clone();
        let (name, content) = (blob_name.to_string(), content.to_string());
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                // UPSERT 幂等：已存在则跳过
                conn.execute(
                    "INSERT OR IGNORE INTO blob_staging (blob_name, content) VALUES (?1, ?2)",
                    rusqlite::params![name, content],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn get_staging(&self, blob_name: &str) -> OceResult<Option<String>> {
        let db = self.db.clone();
        let name = blob_name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT content FROM blob_staging WHERE blob_name = ?1")
                    .map_err(|e| e.to_string())?;
                match stmt.query_row([&name], |r| r.get::<_, String>(0)) {
                    Ok(content) => Ok(Some(content)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    /// 批量读 staging：单查询替代 N 次 spawn_blocking 往返。
    async fn get_staging_many(
        &self,
        blob_names: &[String],
    ) -> OceResult<Vec<(String, Option<String>)>> {
        if blob_names.is_empty() {
            return Ok(vec![]);
        }
        let db = self.db.clone();
        let names = blob_names.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut out = Vec::with_capacity(names.len());
                let mut stmt = conn
                    .prepare("SELECT content FROM blob_staging WHERE blob_name = ?1")
                    .map_err(|e| e.to_string())?;
                for name in &names {
                    let content = match stmt.query_row([name], |r| r.get::<_, String>(0)) {
                        Ok(c) => Some(c),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(e) => return Err(e.to_string()),
                    };
                    out.push((name.clone(), content));
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn delete_staging(&self, blob_name: &str) -> OceResult<()> {
        let db = self.db.clone();
        let name = blob_name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                conn.execute("DELETE FROM blob_staging WHERE blob_name = ?1", [&name])
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn find_pending(&self, blob_names: Option<&[String]>) -> OceResult<Vec<Blob>> {
        let db = self.db.clone();
        let names: Option<Vec<String>> = blob_names.map(|s| s.to_vec());
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {BLOB_COLS} FROM blobs WHERE status = 'pending' ORDER BY created_at"
                    ))
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |row| row_to_blob(row, vec![]))
                    .map_err(|e| e.to_string())?;
                let mut pending: Vec<Blob> = rows.filter_map(|r| r.ok()).collect();
                if let Some(names) = &names {
                    pending.retain(|b| names.contains(&b.blob_name));
                }
                for b in &mut pending {
                    b.chunks = load_chunks(conn, &b.blob_name);
                }
                Ok(pending)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn find_expired(&self, ttl_days: u32, batch_size: usize) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let threshold = (Utc::now() - Duration::days(ttl_days as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let mut stmt = conn
                    .prepare("SELECT blob_name FROM blobs WHERE last_seen < ?1 LIMIT ?2")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![threshold, batch_size as i64], |r| {
                        r.get(0)
                    })
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn exists_many(&self, blob_names: &[String]) -> OceResult<HashMap<String, bool>> {
        let db = self.db.clone();
        let names: Vec<String> = blob_names.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT 1 FROM blobs WHERE blob_name = ?1")
                    .map_err(|e| e.to_string())?;
                let mut out: HashMap<String, bool> =
                    names.iter().map(|n| (n.clone(), false)).collect();
                for name in &names {
                    if stmt.exists([name]).unwrap_or(false) {
                        out.insert(name.clone(), true);
                    }
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn get_many(&self, blob_names: &[String]) -> OceResult<HashMap<String, Blob>> {
        let db = self.db.clone();
        let names: Vec<String> = blob_names.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut out = HashMap::new();
                for name in &names {
                    let chunks = load_chunks(conn, name);
                    let mut stmt = conn
                        .prepare(&format!(
                            "SELECT {BLOB_COLS} FROM blobs WHERE blob_name = ?1"
                        ))
                        .map_err(|e| e.to_string())?;
                    if let Ok(blob) = stmt.query_row([name], |row| row_to_blob(row, chunks.clone()))
                    {
                        out.insert(name.clone(), blob);
                    }
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn find_pending_chunks_for_blobs(
        &self,
        blob_names: &[String],
    ) -> OceResult<Vec<LocatedChunk>> {
        let db = self.db.clone();
        let names: Vec<String> = blob_names.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                // 与 Python find_pending_for_blobs 一致：JOIN chunk + blob，限 pending
                let mut out: Vec<LocatedChunk> = Vec::new();
                let mut stmt = conn
                    .prepare(
                        "SELECT bc.blob_name, bc.content_hash, b.path, c.content, bc.start_line, bc.end_line
                         FROM blob_chunks bc
                         JOIN chunks c ON c.content_hash = bc.content_hash
                         JOIN blobs b ON b.blob_name = bc.blob_name
                         WHERE b.status = 'pending'
                         ORDER BY bc.blob_name, bc.chunk_index",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(LocatedChunk {
                            blob_name: row.get(0)?,
                            content_hash: row.get(1)?,
                            path: row.get(2)?,
                            content: row.get(3)?,
                            start_line: row.get::<_, i64>(4)? as u32,
                            end_line: row.get::<_, i64>(5)? as u32,
                        })
                    })
                    .map_err(|e| e.to_string())?;
                for row in rows.flatten() {
                    if names.contains(&row.blob_name) {
                        out.push(row);
                    }
                }
                Ok(out)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn save_chunks(&self, blob_name: &str, chunks: &[Chunk]) -> OceResult<()> {
        let db = self.db.clone();
        let (name, chunks) = (blob_name.to_string(), chunks.to_vec());
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                insert_chunks_and_symbols(&tx, &name, &chunks)?;
                tx.commit().map_err(|e| e.to_string())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    /// 批量写回：全部 blob 的 chunk+符号+出现位置合入单个事务。
    /// 全量索引时省掉每 blob 一次事务提交与 spawn_blocking 往返（4.5k blob 实测省数秒）。
    async fn save_chunks_many(&self, batches: &[(String, Vec<Chunk>)]) -> OceResult<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let db = self.db.clone();
        let batches = batches.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                for (name, chunks) in &batches {
                    insert_chunks_and_symbols(&tx, name, chunks)?;
                }
                tx.commit().map_err(|e| e.to_string())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn mark_embedded(&self, content_hashes: &[String]) -> OceResult<()> {
        let db = self.db.clone();
        let hashes: Vec<String> = content_hashes.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("UPDATE chunks SET embedded = 1 WHERE content_hash = ?1")
                    .map_err(|e| e.to_string())?;
                for hash in &hashes {
                    stmt.execute([hash]).map_err(|e| e.to_string())?;
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn list_pending_names(&self) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT blob_name FROM blobs WHERE status = 'pending'")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    async fn find_stale_with_staging(&self, stale_hours: i64, limit: usize) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let cutoff = (chrono::Utc::now() - chrono::Duration::hours(stale_hours))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let mut stmt = conn
                    .prepare(
                        "SELECT b.blob_name FROM blobs b
                         JOIN blob_staging s ON s.blob_name = b.blob_name
                         WHERE b.status = 'pending' AND s.created_at < ?1 LIMIT ?2",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![cutoff, limit as i64], |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect::<Vec<String>>())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }
}
