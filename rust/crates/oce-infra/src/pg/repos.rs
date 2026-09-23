//! PostgreSQL Blob/Staging/Chunk/Symbol 仓储。与 SQLite 版（`sqlite/repos.rs`）
//! 语义逐条对齐（UPSERT 列集、孤儿 chunk 清理、staging 幂等写入、批量事务）。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oce_core::blob::{Blob, BlobStatus};
use oce_core::chunk::{Chunk, ChunkRef, LocatedChunk};
use oce_core::error::{OceError, OceResult};
use oce_core::indexing::BlobRepository;
use rayon::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;

pub struct PgBlobRepository {
    pub pool: PgPool,
}

fn pg_err(e: sqlx::Error) -> OceError {
    OceError::new(e.to_string(), "PgError")
}

/// 行 → 领域 Blob（chunks 由调用方按需加载）。
/// FromRow 手写实现：sqlx 的 derive 宏在本机 toolchain 下产生损坏的
/// proc-macro dylib（mis-aligned LINKEDIT），运行时查询不需要宏特性。
struct BlobRow {
    blob_name: String,
    path: String,
    content_size: i32,
    language: Option<String>,
    file_type: String,
    status: String,
    retry_count: i32,
    last_seen: DateTime<Utc>,
    created_at: DateTime<Utc>,
    error_message: Option<String>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for BlobRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            blob_name: row.try_get("blob_name")?,
            path: row.try_get("path")?,
            content_size: row.try_get("content_size")?,
            language: row.try_get("language")?,
            file_type: row.try_get("file_type")?,
            status: row.try_get("status")?,
            retry_count: row.try_get("retry_count")?,
            last_seen: row.try_get("last_seen")?,
            created_at: row.try_get("created_at")?,
            error_message: row.try_get("error_message")?,
        })
    }
}

impl From<BlobRow> for Blob {
    fn from(row: BlobRow) -> Self {
        Blob {
            blob_name: row.blob_name,
            path: row.path,
            status: BlobStatus::from_str(&row.status),
            chunks: vec![],
            content_size: row.content_size.max(0) as u64,
            language: row.language,
            file_type: row.file_type,
            retry_count: row.retry_count.max(0) as u32,
            error_message: row.error_message,
            last_seen: row.last_seen,
            created_at: row.created_at,
        }
    }
}

const BLOB_COLS: &str =
    "blob_name, path, content_size, language, file_type, status, retry_count, last_seen, created_at, error_message";

async fn load_chunks(pool: &PgPool, blob_name: &str) -> OceResult<Vec<ChunkRef>> {
    let rows: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT content_hash, start_line, end_line FROM blob_chunks
         WHERE blob_name = $1 ORDER BY chunk_index",
    )
    .bind(blob_name)
    .fetch_all(pool)
    .await
    .map_err(pg_err)?;
    Ok(rows
        .into_iter()
        .map(|(content_hash, start_line, end_line)| ChunkRef {
            content_hash,
            start_line: start_line.max(0) as u32,
            end_line: end_line.max(0) as u32,
        })
        .collect())
}

/// 符号预提取（与 SQLite 版 extract_symbols_parallel 同构：≥8 块并行）。
fn extract_symbols_parallel(chunks: &[Chunk]) -> Vec<(String, String, String, i64, i64)> {
    let extract_all = |chunk: &Chunk| {
        oce_core::symbol::SymbolExtractor::extract_symbols(
            &chunk.content,
            chunk.start_line,
            chunk.end_line,
        )
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
        .collect::<Vec<_>>()
    };
    if chunks.len() >= 8 {
        chunks.par_iter().flat_map(extract_all).collect()
    } else {
        chunks.iter().flat_map(extract_all).collect()
    }
}

/// 单事务批量写 chunks + symbols + blob_chunks（与 SQLite insert_chunks_and_symbols 对齐）。
async fn insert_chunks_and_symbols(
    tx: &mut sqlx::PgConnection,
    blob_name: &str,
    chunks: &[Chunk],
) -> OceResult<()> {
    // 符号提取是正则密集的 CPU 工作：事务外并行预提取，事务内只做批量写入。
    // UNNEST 批量：逐条 INSERT 的网络往返是 PG 后端 stage1 的主要瓶颈
    // （~1ms RTT × 数百条 = 数百 ms/批，实测比 SQLite 慢 10-100 倍）。
    let symbol_values = extract_symbols_parallel(chunks);

    let hashes: Vec<String> = chunks.iter().map(|c| c.content_hash.clone()).collect();
    let contents: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let sizes: Vec<i64> = chunks.iter().map(|c| c.content.len() as i64).collect();
    let types: Vec<Option<String>> = chunks.iter().map(|c| c.chunk_type.clone()).collect();
    sqlx::query(
        "INSERT INTO chunks (content_hash, content, content_size, chunk_type, embedded)
         SELECT h, c, s, t, false FROM UNNEST($1::text[], $2::text[], $3::int8[], $4::text[])
         AS t0(h, c, s, t)
         ON CONFLICT (content_hash) DO NOTHING",
    )
    .bind(&hashes)
    .bind(&contents)
    .bind(&sizes)
    .bind(&types)
    .execute(&mut *tx)
    .await
    .map_err(pg_err)?;

    if !symbol_values.is_empty() {
        let sym_ids: Vec<String> = symbol_values.iter().map(|s| s.0.clone()).collect();
        let sym_kinds: Vec<String> = symbol_values.iter().map(|s| s.1.clone()).collect();
        let sym_hashes: Vec<String> = symbol_values.iter().map(|s| s.2.clone()).collect();
        let sym_blob: Vec<String> = symbol_values.iter().map(|_| blob_name.to_string()).collect();
        let sym_starts: Vec<i32> = symbol_values.iter().map(|s| s.3 as i32).collect();
        let sym_ends: Vec<i32> = symbol_values.iter().map(|s| s.4 as i32).collect();
        sqlx::query(
            "INSERT INTO symbol_occurrences
             (identifier, blob_name, content_hash, kind, start_line, end_line)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::int4[], $6::int4[])
             ON CONFLICT DO NOTHING",
        )
        .bind(&sym_ids)
        .bind(&sym_blob)
        .bind(&sym_hashes)
        .bind(&sym_kinds)
        .bind(&sym_starts)
        .bind(&sym_ends)
        .execute(&mut *tx)
        .await
        .map_err(pg_err)?;
    }

    let bc_names: Vec<String> = chunks.iter().map(|_| blob_name.to_string()).collect();
    let bc_starts: Vec<i32> = chunks.iter().map(|c| c.start_line as i32).collect();
    let bc_ends: Vec<i32> = chunks.iter().map(|c| c.end_line as i32).collect();
    let bc_indexes: Vec<i32> = (0..chunks.len() as i32).collect();
    sqlx::query(
        "INSERT INTO blob_chunks (blob_name, content_hash, start_line, end_line, chunk_index)
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::int4[], $4::int4[], $5::int4[])
         ON CONFLICT DO NOTHING",
    )
    .bind(&bc_names)
    .bind(&hashes)
    .bind(&bc_starts)
    .bind(&bc_ends)
    .bind(&bc_indexes)
    .execute(&mut *tx)
    .await
    .map_err(pg_err)?;
    Ok(())
}

#[async_trait]
impl BlobRepository for PgBlobRepository {
    async fn get(&self, blob_name: &str) -> OceResult<Option<Blob>> {
        let row: Option<BlobRow> = sqlx::query_as(&format!(
            "SELECT {BLOB_COLS} FROM blobs WHERE blob_name = $1"
        ))
        .bind(blob_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?;
        match row {
            None => Ok(None),
            Some(row) => {
                let chunks = load_chunks(&self.pool, blob_name).await?;
                let mut blob: Blob = row.into();
                blob.chunks = chunks;
                Ok(Some(blob))
            }
        }
    }

    async fn save(&self, blob: &Blob) -> OceResult<()> {
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query(
            "INSERT INTO blobs (blob_name, path, content_size, language, file_type, status, retry_count, last_seen, created_at, error_message)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (blob_name) DO UPDATE SET
                path = excluded.path,
                content_size = excluded.content_size,
                language = excluded.language,
                file_type = excluded.file_type,
                status = excluded.status,
                retry_count = excluded.retry_count,
                last_seen = excluded.last_seen,
                error_message = excluded.error_message",
        )
        .bind(&blob.blob_name)
        .bind(&blob.path)
        .bind(blob.content_size as i64)
        .bind(&blob.language)
        .bind(&blob.file_type)
        .bind(blob.status.as_str())
        .bind(blob.retry_count as i64)
        .bind(blob.last_seen)
        .bind(blob.created_at)
        .bind(&blob.error_message)
        .execute(&mut *tx)
        .await
        .map_err(pg_err)?;
        // chunks 非空时同步出现位置 + 符号（Python save_many 语义）。
        // 批量 UNNEST：PG 网络往返 ~1ms/次，逐条 INSERT 在 32-blob 批次下
        // 产生数百次 RTT（stage1 实测比 SQLite 慢 10-100 倍的主因）。
        if !blob.chunks.is_empty() {
            let names: Vec<String> = blob
                .chunks
                .iter()
                .map(|_| blob.blob_name.clone())
                .collect();
            let hashes: Vec<String> =
                blob.chunks.iter().map(|c| c.content_hash.clone()).collect();
            let starts: Vec<i32> =
                blob.chunks.iter().map(|c| c.start_line as i32).collect();
            let ends: Vec<i32> = blob.chunks.iter().map(|c| c.end_line as i32).collect();
            let indexes: Vec<i32> = (0..blob.chunks.len() as i32).collect();
            sqlx::query(
                "INSERT INTO blob_chunks (blob_name, content_hash, start_line, end_line, chunk_index)
                 SELECT * FROM UNNEST($1::text[], $2::text[], $3::int4[], $4::int4[], $5::int4[])
                 ON CONFLICT DO NOTHING",
            )
            .bind(&names)
            .bind(&hashes)
            .bind(&starts)
            .bind(&ends)
            .bind(&indexes)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
            let contents: Vec<(String, String)> = sqlx::query_as(
                "SELECT content_hash, content FROM chunks WHERE content_hash = ANY($1)",
            )
            .bind(blob.chunks.iter().map(|c| c.content_hash.clone()).collect::<Vec<_>>())
            .fetch_all(&mut *tx)
            .await
            .map_err(pg_err)?;
            let by_hash: HashMap<&str, &str> = contents
                .iter()
                .map(|(h, c)| (h.as_str(), c.as_str()))
                .collect();
            let symbol_values: Vec<(String, String, String, i64, i64)> = blob
                .chunks
                .iter()
                .filter_map(|cr| {
                    by_hash.get(cr.content_hash.as_str()).map(|content| {
                        (
                            cr.content_hash.clone(),
                            cr.start_line,
                            cr.end_line,
                            content.to_string(),
                        )
                    })
                })
                .flat_map(|(hash, start, end, content)| {
                    oce_core::symbol::SymbolExtractor::extract_symbols(&content, start, end)
                        .into_iter()
                        .map(move |sym| {
                            (sym.identifier, sym.kind, hash.clone(), sym.start_line as i64, sym.end_line as i64)
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            if !symbol_values.is_empty() {
                let sym_ids: Vec<String> =
                    symbol_values.iter().map(|s| s.0.clone()).collect();
                let sym_kinds: Vec<String> =
                    symbol_values.iter().map(|s| s.1.clone()).collect();
                let sym_hashes: Vec<String> =
                    symbol_values.iter().map(|s| s.2.clone()).collect();
                let sym_blob: Vec<String> = symbol_values
                    .iter()
                    .map(|_| blob.blob_name.clone())
                    .collect();
                let sym_starts: Vec<i32> =
                    symbol_values.iter().map(|s| s.3 as i32).collect();
                let sym_ends: Vec<i32> =
                    symbol_values.iter().map(|s| s.4 as i32).collect();
                sqlx::query(
                    "INSERT INTO symbol_occurrences
                     (identifier, blob_name, content_hash, kind, start_line, end_line)
                     SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::int4[], $6::int4[])
                     ON CONFLICT DO NOTHING",
                )
                .bind(&sym_ids)
                .bind(&sym_blob)
                .bind(&sym_hashes)
                .bind(&sym_kinds)
                .bind(&sym_starts)
                .bind(&sym_ends)
                .execute(&mut *tx)
                .await
                .map_err(pg_err)?;
            }
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(())
    }

    async fn delete(&self, blob_name: &str) -> OceResult<()> {
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        // 与 Python delete_many 一致：删除后清理孤儿 chunk 内容
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT content_hash FROM blob_chunks WHERE blob_name = $1",
        )
        .bind(blob_name)
        .fetch_all(&mut *tx)
        .await
        .map_err(pg_err)?;
        sqlx::query("DELETE FROM blob_chunks WHERE blob_name = $1")
            .bind(blob_name)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        sqlx::query("DELETE FROM blobs WHERE blob_name = $1")
            .bind(blob_name)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        if !hashes.is_empty() {
            sqlx::query(
                "DELETE FROM chunks WHERE content_hash = ANY($1)
                 AND NOT EXISTS (
                     SELECT 1 FROM blob_chunks bc WHERE bc.content_hash = chunks.content_hash
                 )",
            )
            .bind(hashes)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(())
    }

    async fn save_staging(&self, blob_name: &str, content: &str) -> OceResult<()> {
        // UPSERT 幂等：已存在则跳过（与 Python on_conflict_do_nothing 一致）
        sqlx::query(
            "INSERT INTO blob_staging (blob_name, content) VALUES ($1, $2)
             ON CONFLICT (blob_name) DO NOTHING",
        )
        .bind(blob_name)
        .bind(content)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(())
    }

    async fn get_staging(&self, blob_name: &str) -> OceResult<Option<String>> {
        let content: Option<(String,)> = sqlx::query_as(
            "SELECT content FROM blob_staging WHERE blob_name = $1",
        )
        .bind(blob_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(content.map(|(c,)| c))
    }

    async fn delete_staging(&self, blob_name: &str) -> OceResult<()> {
        sqlx::query("DELETE FROM blob_staging WHERE blob_name = $1")
            .bind(blob_name)
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    async fn find_pending(&self, blob_names: Option<&[String]>) -> OceResult<Vec<Blob>> {
        let rows: Vec<BlobRow> = match blob_names {
            Some(names) if names.is_empty() => return Ok(vec![]),
            Some(names) => {
                sqlx::query_as(&format!(
                    "SELECT {BLOB_COLS} FROM blobs
                     WHERE status = 'pending' AND blob_name = ANY($1)
                     ORDER BY created_at"
                ))
                .bind(names.to_vec())
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?
            }
            None => {
                sqlx::query_as(&format!(
                    "SELECT {BLOB_COLS} FROM blobs WHERE status = 'pending' ORDER BY created_at"
                ))
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?
            }
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut blob: Blob = row.into();
            blob.chunks = load_chunks(&self.pool, &blob.blob_name).await?;
            out.push(blob);
        }
        Ok(out)
    }

    async fn find_expired(&self, ttl_days: u32, batch_size: usize) -> OceResult<Vec<String>> {
        let threshold = Utc::now() - chrono::Duration::days(ttl_days as i64);
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT blob_name FROM blobs WHERE last_seen < $1 LIMIT $2",
        )
        .bind(threshold)
        .bind(batch_size as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(names)
    }

    async fn exists_many(&self, blob_names: &[String]) -> OceResult<HashMap<String, bool>> {
        if blob_names.is_empty() {
            return Ok(HashMap::new());
        }
        let found: Vec<String> = sqlx::query_scalar(
            "SELECT blob_name FROM blobs WHERE blob_name = ANY($1)",
        )
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        let found: std::collections::HashSet<String> = found.into_iter().collect();
        Ok(blob_names
            .iter()
            .map(|n| (n.clone(), found.contains(n)))
            .collect())
    }

    async fn get_many(&self, blob_names: &[String]) -> OceResult<HashMap<String, Blob>> {
        if blob_names.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<BlobRow> = sqlx::query_as(&format!(
            "SELECT {BLOB_COLS} FROM blobs WHERE blob_name = ANY($1)"
        ))
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let chunks = load_chunks(&self.pool, &row.blob_name).await?;
            let mut blob: Blob = row.into();
            blob.chunks = chunks;
            out.insert(blob.blob_name.clone(), blob);
        }
        Ok(out)
    }

    async fn find_pending_chunks_for_blobs(
        &self,
        blob_names: &[String],
    ) -> OceResult<Vec<LocatedChunk>> {
        if blob_names.is_empty() {
            return Ok(vec![]);
        }
        // 与 Python find_pending_for_blobs 一致：JOIN chunk + blob，限 pending
        let rows: Vec<(String, String, String, String, i32, i32)> = sqlx::query_as(
            "SELECT bc.blob_name, bc.content_hash, b.path, c.content, bc.start_line, bc.end_line
             FROM blob_chunks bc
             JOIN chunks c ON c.content_hash = bc.content_hash
             JOIN blobs b ON b.blob_name = bc.blob_name
             WHERE b.status = 'pending' AND bc.blob_name = ANY($1)
             ORDER BY bc.blob_name, bc.chunk_index",
        )
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(rows
            .into_iter()
            .map(|(blob_name, content_hash, path, content, start_line, end_line)| LocatedChunk {
                blob_name,
                content_hash,
                path,
                content,
                start_line: start_line.max(0) as u32,
                end_line: end_line.max(0) as u32,
            })
            .collect())
    }

    async fn save_chunks(&self, blob_name: &str, chunks: &[Chunk]) -> OceResult<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        insert_chunks_and_symbols(&mut tx, blob_name, chunks).await?;
        tx.commit().await.map_err(pg_err)?;
        Ok(())
    }

    /// 批量保存多个 blob 的切块结果：单事务（SQLite 版逐 blob 转发，PG 版合并）。
    async fn save_chunks_many(&self, batches: &[(String, Vec<Chunk>)]) -> OceResult<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        for (blob_name, chunks) in batches {
            insert_chunks_and_symbols(&mut tx, blob_name, chunks).await?;
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(())
    }

    /// 批量读取 staging：单查询。
    async fn get_staging_many(
        &self,
        blob_names: &[String],
    ) -> OceResult<Vec<(String, Option<String>)>> {
        if blob_names.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT blob_name, content FROM blob_staging WHERE blob_name = ANY($1)",
        )
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        let by_name: HashMap<String, String> = rows.into_iter().collect();
        Ok(blob_names
            .iter()
            .map(|n| (n.clone(), by_name.get(n).cloned()))
            .collect())
    }

    async fn mark_embedded(&self, content_hashes: &[String]) -> OceResult<()> {
        if content_hashes.is_empty() {
            return Ok(());
        }
        sqlx::query("UPDATE chunks SET embedded = true WHERE content_hash = ANY($1)")
            .bind(content_hashes.to_vec())
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    async fn find_all_blob_names(&self) -> OceResult<Vec<String>> {
        let names: Vec<String> =
            sqlx::query_scalar("SELECT blob_name FROM blobs ORDER BY blob_name")
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?;
        Ok(names)
    }

    async fn list_pending_names(&self) -> OceResult<Vec<String>> {
        let names: Vec<String> =
            sqlx::query_scalar("SELECT blob_name FROM blobs WHERE status = 'pending'")
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?;
        Ok(names)
    }

    async fn find_stale_with_staging(&self, stale_hours: i64, limit: usize) -> OceResult<Vec<String>> {
        let cutoff = Utc::now() - chrono::Duration::hours(stale_hours);
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT b.blob_name FROM blobs b
             JOIN blob_staging s ON s.blob_name = b.blob_name
             WHERE b.status = 'pending' AND s.created_at < $1 LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(names)
    }
}
