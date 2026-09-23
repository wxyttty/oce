//! PostgreSQL Chain 仓储 + 精确标识符召回 + 首个 chunk 查询。
//! 与 SQLite 版（`sqlite/chains.rs`）语义对齐。

use async_trait::async_trait;
use chrono::Utc;
use oce_core::chain::{Chain, ChainRepository};
use oce_core::error::{OceError, OceResult};
use sqlx::PgPool;
use std::collections::HashSet;

fn pg_err(e: sqlx::Error) -> OceError {
    OceError::new(e.to_string(), "PgError")
}

pub struct PgChainRepository {
    pub pool: PgPool,
}

async fn load_members(pool: &PgPool, chain_id: &str) -> OceResult<HashSet<String>> {
    let names: Vec<String> =
        sqlx::query_scalar("SELECT blob_name FROM chain_members WHERE chain_id = $1")
            .bind(chain_id)
            .fetch_all(pool)
            .await
            .map_err(pg_err)?;
    Ok(names.into_iter().collect())
}

#[async_trait]
impl ChainRepository for PgChainRepository {
    async fn get(&self, chain_id: &str) -> OceResult<Option<Chain>> {
        let row: Option<(String, i32)> =
            sqlx::query_as("SELECT chain_id, version FROM chains WHERE chain_id = $1")
                .bind(chain_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(pg_err)?;
        match row {
            None => Ok(None),
            Some((chain_id, version)) => {
                let members = load_members(&self.pool, &chain_id).await?;
                Ok(Some(Chain {
                    chain_id,
                    version: version.max(0) as u32,
                    members,
                }))
            }
        }
    }

    async fn exists(&self, chain_id: &str) -> OceResult<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chains WHERE chain_id = $1")
            .bind(chain_id)
            .fetch_one(&self.pool)
            .await
            .map_err(pg_err)?;
        Ok(n > 0)
    }

    /// 创建新链：chain_id = uuid4 hex（无连字符，与 Python 一致）。
    async fn create(&self, members: Vec<String>) -> OceResult<Chain> {
        let mut unique = members;
        unique.sort();
        unique.dedup();
        let chain_id = oce_core::chain::new_chain_id_hex();
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query(
            "INSERT INTO chains (chain_id, version, total_blobs, created_at, updated_at)
             VALUES ($1, 1, $2, now(), now())",
        )
        .bind(&chain_id)
        .bind(unique.len() as i32)
        .execute(&mut *tx)
        .await
        .map_err(pg_err)?;
        for name in &unique {
            sqlx::query(
                "INSERT INTO chain_members (chain_id, blob_name) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&chain_id)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(Chain {
            chain_id,
            version: 1,
            members: unique.into_iter().collect(),
        })
    }

    /// 应用 checkpoint：members − deleted ∪ added，version += 1。链不存在返回 None。
    async fn apply_checkpoint(
        &self,
        chain_id: &str,
        added: Vec<String>,
        deleted: Vec<String>,
    ) -> OceResult<Option<u32>> {
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        let current: Option<i32> =
            sqlx::query_scalar("SELECT version FROM chains WHERE chain_id = $1 FOR UPDATE")
                .bind(chain_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(pg_err)?;
        let Some(current) = current else {
            tx.rollback().await.map_err(pg_err)?;
            return Ok(None);
        };
        let mut unique_deleted = deleted;
        unique_deleted.sort();
        unique_deleted.dedup();
        if !unique_deleted.is_empty() {
            sqlx::query("DELETE FROM chain_members WHERE chain_id = $1 AND blob_name = ANY($2)")
                .bind(chain_id)
                .bind(unique_deleted)
                .execute(&mut *tx)
                .await
                .map_err(pg_err)?;
        }
        let mut unique_added = added;
        unique_added.sort();
        unique_added.dedup();
        for name in &unique_added {
            sqlx::query(
                "INSERT INTO chain_members (chain_id, blob_name) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(chain_id)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chain_members WHERE chain_id = $1")
                .bind(chain_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(pg_err)?;
        let new_version = current + 1;
        sqlx::query(
            "UPDATE chains SET version = $2, total_blobs = $3, updated_at = now()
             WHERE chain_id = $1",
        )
        .bind(chain_id)
        .bind(new_version)
        .bind(count as i32)
        .execute(&mut *tx)
        .await
        .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
        Ok(Some(new_version as u32))
    }

    /// checkpoint 后 touch 链内全部 blob 的 last_seen。
    async fn touch_members(&self, chain_id: &str) -> OceResult<()> {
        sqlx::query(
            "UPDATE blobs SET last_seen = now()
             WHERE blob_name IN (
                 SELECT blob_name FROM chain_members WHERE chain_id = $1
             )",
        )
        .bind(chain_id)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(())
    }

    async fn delete(&self, chain_id: &str) -> OceResult<()> {
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query("DELETE FROM chain_members WHERE chain_id = $1")
            .bind(chain_id)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        sqlx::query("DELETE FROM chains WHERE chain_id = $1")
            .bind(chain_id)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
        Ok(())
    }

    async fn find_expired(&self, ttl_days: u32) -> OceResult<Vec<String>> {
        let threshold = Utc::now() - chrono::Duration::days(ttl_days as i64);
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT chain_id FROM chains WHERE updated_at < $1",
        )
        .bind(threshold)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;
        Ok(ids)
    }
}

/// 精确标识符召回（对应 Python `SymbolSearchStore`）。
/// 从 symbol_occurrences 倒排索引查询，按 kind 分配优先级分数
/// （endpoint 1.0 / definition 0.95 / 其余 0.85），按 content_hash 去重。
pub struct PgExactStore {
    pub pool: PgPool,
    pub max_scope_blobs: usize,
}

fn score_by_kind(kind: &str) -> f32 {
    match kind {
        "endpoint" => 1.0,
        "definition" => 0.95,
        _ => 0.85,
    }
}

#[async_trait]
impl oce_core::search::ExactSearchStore for PgExactStore {
    async fn search_exact(
        &self,
        identifiers: &[String],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<oce_core::search::SearchHit>> {
        let mut identifiers: Vec<String> = identifiers.to_vec();
        identifiers.dedup();
        identifiers.retain(|s| !s.is_empty());
        if identifiers.is_empty() || top_k == 0 {
            return Ok(vec![]);
        }
        // 前置检查：scope 必须合理（与 SQLite 版一致）
        if let Some(scope) = allowed_blob_names {
            if self.max_scope_blobs > 0 && scope.len() > self.max_scope_blobs {
                return Ok(vec![]);
            }
        }
        let limit = top_k.max(1) * 20;
        let mut sql = String::from(
            "SELECT so.content_hash, so.identifier, so.kind, so.blob_name,
                    b.path, c.content, bc.start_line, bc.end_line
             FROM symbol_occurrences so
             JOIN chunks c ON so.content_hash = c.content_hash
             JOIN blobs b ON so.blob_name = b.blob_name
             JOIN blob_chunks bc
               ON bc.content_hash = so.content_hash AND bc.blob_name = so.blob_name
             WHERE so.identifier = ANY($1)",
        );
        let scope: Option<Vec<String>> = allowed_blob_names.map(|s| s.to_vec());
        if scope.is_some() {
            sql.push_str(" AND so.blob_name = ANY($2)");
        }
        sql.push_str(" LIMIT ");
        sql.push_str(&limit.to_string());

        let mut query =
            sqlx::query_as::<_, (String, String, String, String, String, String, i32, i32)>(&sql)
                .bind(identifiers);
        if let Some(scope) = &scope {
            query = query.bind(scope);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(pg_err)?;
        let mut hits: Vec<oce_core::search::SearchHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (content_hash, _identifier, kind, blob_name, path, content, start, end) in rows {
            if !seen.insert(content_hash.clone()) {
                continue;
            }
            let score = score_by_kind(&kind);
            hits.push(oce_core::search::SearchHit {
                blob_name,
                path,
                content,
                score,
                content_hash,
                start_line: start.max(0) as u32,
                end_line: end.max(0) as u32,
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    /// related symbols hints 的定义查询：identifier → (kind, path, fanout)。
    /// 与 SQLite 版同构（窗口函数 fanout、endpoint 优先、LIMIT 500）。
    async fn find_definitions(
        &self,
        identifiers: &[String],
        allowed_blob_names: Option<&[String]>,
    ) -> OceResult<Vec<oce_core::related::SymbolDefinition>> {
        let mut identifiers: Vec<String> = identifiers.to_vec();
        identifiers.dedup();
        identifiers.retain(|s| !s.is_empty());
        if identifiers.is_empty() {
            return Ok(vec![]);
        }
        if let Some(scope) = allowed_blob_names {
            if self.max_scope_blobs > 0 && scope.len() > self.max_scope_blobs {
                return Ok(vec![]);
            }
        }
        let mut sql = String::from(
            "SELECT so.identifier, so.kind, b.path,
                    COUNT(*) OVER (PARTITION BY so.identifier) AS fanout
             FROM (SELECT DISTINCT identifier, blob_name, kind FROM symbol_occurrences) so
             JOIN blobs b ON so.blob_name = b.blob_name
             WHERE so.identifier = ANY($1)",
        );
        let scope: Option<Vec<String>> = allowed_blob_names.map(|s| s.to_vec());
        if scope.is_some() {
            sql.push_str(" AND so.blob_name = ANY($2)");
        }
        sql.push_str(
            " ORDER BY so.identifier, CASE so.kind WHEN 'endpoint' THEN 0 ELSE 1 END, fanout LIMIT 500",
        );
        let mut query =
            sqlx::query_as::<_, (String, String, String, i64)>(&sql).bind(identifiers);
        if let Some(scope) = &scope {
            query = query.bind(scope);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(pg_err)?;
        Ok(rows
            .into_iter()
            .map(|(identifier, kind, path, fanout)| oce_core::related::SymbolDefinition {
                identifier,
                kind,
                path,
                file_fanout: fanout.max(0) as usize,
            })
            .collect())
    }
}

/// 首个 chunk 查询（LLM 重排候选内容用）。
pub struct PgFirstChunkLookup {
    pub pool: PgPool,
}

#[async_trait]
impl oce_core::retrieval::FirstChunkLookup for PgFirstChunkLookup {
    async fn first_chunks(
        &self,
        blob_names: &[String],
    ) -> Result<Vec<oce_core::search::SearchHit>, String> {
        if blob_names.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(String, String, i32, i32, String, String)> = sqlx::query_as(
            "SELECT DISTINCT ON (b.blob_name)
                    c.content_hash, c.content, bc.start_line, bc.end_line, b.blob_name, b.path
             FROM blob_chunks bc
             JOIN chunks c ON c.content_hash = bc.content_hash
             JOIN blobs b ON b.blob_name = bc.blob_name
             WHERE b.blob_name = ANY($1)
             ORDER BY b.blob_name, bc.start_line",
        )
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(content_hash, content, start_line, end_line, blob_name, path)| {
                oce_core::search::SearchHit {
                    content_hash,
                    content,
                    start_line: start_line.max(0) as u32,
                    end_line: end_line.max(0) as u32,
                    blob_name,
                    path,
                    score: 0.0, // 由调用方填路径分
                }
            })
            .collect())
    }
}

/// span 合并/补全的行文本重构（RETRIEVAL_SPAN_MERGE_ENABLED）。
pub struct PgFileContentLookup {
    pub pool: PgPool,
}

#[async_trait]
impl oce_core::retrieval::FileContentLookup for PgFileContentLookup {
    async fn blob_lines(
        &self,
        blob_names: &[String],
    ) -> Result<Vec<(String, Vec<(u32, String)>)>, String> {
        if blob_names.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(String, String, i32)> = sqlx::query_as(
            "SELECT bc.blob_name, c.content, bc.start_line
             FROM blob_chunks bc
             JOIN chunks c ON c.content_hash = bc.content_hash
             WHERE bc.blob_name = ANY($1)
             ORDER BY bc.blob_name, bc.chunk_index",
        )
        .bind(blob_names.to_vec())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let mut by_blob: std::collections::HashMap<String, Vec<(u32, String)>> =
            std::collections::HashMap::new();
        for (blob_name, content, start_line) in rows {
            let start = start_line.max(0) as u32;
            let entry = by_blob.entry(blob_name).or_default();
            for (offset, line) in content.lines().enumerate() {
                entry.push((start + offset as u32, line.to_string()));
            }
        }
        Ok(by_blob.into_iter().collect())
    }
}
