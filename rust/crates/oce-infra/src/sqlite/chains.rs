//! Chain 仓储 + 精确标识符召回（SymbolSearchStore 语义）+ 首个 chunk 查询。
//! 与 Python `sql_chain_repo.py` / `symbol_search_store.py` 对齐。

use crate::sqlite::repos::SqlBlobRepository;
use crate::sqlite::SqlDb;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use oce_core::chain::Chain;
use oce_core::indexing::BlobRepository;
use oce_core::error::{OceError, OceResult};
use oce_core::search::{search_hit_key, ExactSearchStore, SearchHit};
use std::collections::{HashMap, HashSet};

/// Chain 仓储（对应 Python `SqlChainRepository`）。
pub struct SqlChainRepository {
    pub db: SqlDb,
}

impl SqlChainRepository {
    fn row_to_chain(conn: &rusqlite::Connection, row: &rusqlite::Row) -> Result<Chain, rusqlite::Error> {
        let chain_id: String = row.get(0)?;
        let version: i64 = row.get(1)?;
        let mut members = HashSet::new();
        let mut stmt = conn
            .prepare("SELECT blob_name FROM chain_members WHERE chain_id = ?1")?;
        let rows = stmt.query_map([&chain_id], |r| r.get::<_, String>(0))?;
        for m in rows.flatten() {
            members.insert(m);
        }
        Ok(Chain {
            chain_id,
            version: version as u32,
            members,
        })
    }

    pub async fn get(&self, chain_id: &str) -> OceResult<Option<Chain>> {
        let db = self.db.clone();
        let id = chain_id.to_string();
        run(db, move |conn| {
            let mut stmt = conn
                .prepare("SELECT chain_id, version FROM chains WHERE chain_id = ?1")
                .map_err(|e| e.to_string())?;
            match stmt.query_row([&id], |row| Self::row_to_chain(conn, row)) {
                Ok(chain) => Ok(Some(chain)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        })
        .await
    }

    pub async fn exists(&self, chain_id: &str) -> OceResult<bool> {
        let db = self.db.clone();
        let id = chain_id.to_string();
        run(db, move |conn| {
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM chains WHERE chain_id = ?1")
                .map_err(|e| e.to_string())?;
            let count: i64 = stmt.query_row([&id], |r| r.get(0)).map_err(|e| e.to_string())?;
            Ok(count > 0)
        })
        .await
    }

    /// 创建新链：chain_id = uuid4 hex（无连字符，与 Python 一致）。
    pub async fn create(&self, members: Vec<String>) -> OceResult<Chain> {
        let db = self.db.clone();
        let mut unique: Vec<String> = members;
        unique.sort();
        unique.dedup();
        let chain_id = oce_core::chain::new_chain_id_hex();
        let now = crate::sqlite::repos::utc_now_iso();
        run(db, move |conn| {
            conn.execute(
                "INSERT INTO chains (chain_id, version, total_blobs, created_at, updated_at)
                 VALUES (?1, 1, ?2, ?3, ?3)",
                rusqlite::params![chain_id, unique.len() as i64, now],
            )
            .map_err(|e| e.to_string())?;
            insert_members(conn, &chain_id, &unique)?;
            Ok(Chain {
                chain_id: chain_id.clone(),
                version: 1,
                members: unique.into_iter().collect(),
            })
        })
        .await
    }

    /// 应用 checkpoint：members − deleted ∪ added，version += 1。链不存在返回 None。
    pub async fn apply_checkpoint(
        &self,
        chain_id: &str,
        added: Vec<String>,
        deleted: Vec<String>,
    ) -> OceResult<Option<u32>> {
        let db = self.db.clone();
        let id = chain_id.to_string();
        run(db, move |conn| {
            let current: Option<i64> = {
                let mut stmt = conn
                    .prepare("SELECT version FROM chains WHERE chain_id = ?1")
                    .map_err(|e| e.to_string())?;
                match stmt.query_row([&id], |r| r.get(0)) {
                    Ok(v) => Some(v),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(e) => return Err(e.to_string()),
                }
            };
            let Some(current) = current else { return Ok(None) };
            for name in &deleted {
                conn.execute(
                    "DELETE FROM chain_members WHERE chain_id = ?1 AND blob_name = ?2",
                    rusqlite::params![id, name],
                )
                .map_err(|e| e.to_string())?;
            }
            insert_members(conn, &id, &added)?;
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM chain_members WHERE chain_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let new_version = current + 1;
            conn.execute(
                "UPDATE chains SET version = ?2, total_blobs = ?3, updated_at = ?4 WHERE chain_id = ?1",
                rusqlite::params![
                    id,
                    new_version,
                    count,
                    crate::sqlite::repos::utc_now_iso()
                ],
            )
            .map_err(|e| e.to_string())?;
            Ok(Some(new_version as u32))
        })
        .await
    }

    /// checkpoint 后 touch 链内全部 blob 的 last_seen。
    pub async fn touch_members(&self, chain_id: &str) -> OceResult<()> {
        let db = self.db.clone();
        let id = chain_id.to_string();
        run(db, move |conn| {
            conn.execute(
                "UPDATE blobs SET last_seen = ?1
                 WHERE blob_name IN (SELECT blob_name FROM chain_members WHERE chain_id = ?2)",
                rusqlite::params![crate::sqlite::repos::utc_now_iso(), id],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
    }

    pub async fn delete(&self, chain_id: &str) -> OceResult<()> {
        let db = self.db.clone();
        let id = chain_id.to_string();
        run(db, move |conn| {
            conn.execute("DELETE FROM chain_members WHERE chain_id = ?1", [&id])
                .map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM chains WHERE chain_id = ?1", [&id])
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
    }

    pub async fn find_expired(&self, ttl_days: u32) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        run(db, move |conn| {
            let threshold = (Utc::now() - Duration::days(ttl_days as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let mut stmt = conn
                .prepare("SELECT chain_id FROM chains WHERE updated_at < ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&threshold], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .await
    }
}

fn insert_members(
    conn: &rusqlite::Connection,
    chain_id: &str,
    members: &[String],
) -> Result<(), String> {
    let mut unique: Vec<&String> = members.iter().collect();
    unique.sort();
    unique.dedup();
    for name in unique {
        conn.execute(
            "INSERT OR IGNORE INTO chain_members (chain_id, blob_name) VALUES (?1, ?2)",
            rusqlite::params![chain_id, name],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 阻塞 SQL 的统一包装（spawn_blocking）。
async fn run<T: Send + 'static>(
    db: SqlDb,
    f: impl FnOnce(&mut rusqlite::Connection) -> Result<T, String> + Send + 'static,
) -> OceResult<T> {
    let result = tokio::task::spawn_blocking(move || db.with_conn(f))
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
    result.map_err(|m| OceError::new(m, "SqliteError"))
}

/// 精确标识符召回（对应 Python `SymbolSearchStore`）。
///
/// 从 symbol_occurrences 倒排索引查询，按 kind 分配优先级分数
/// （endpoint 1.0 / definition 0.95 / 其余 0.85），按 content_hash 去重。
pub struct SqlExactStore {
    pub db: SqlDb,
    pub max_scope_blobs: usize,
}

#[async_trait]
impl ExactSearchStore for SqlExactStore {
    async fn search_exact(
        &self,
        identifiers: &[String],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<SearchHit>> {
        let mut identifiers: Vec<String> = identifiers.to_vec();
        identifiers.dedup();
        identifiers.retain(|s| !s.is_empty());
        if identifiers.is_empty() || top_k == 0 {
            return Ok(vec![]);
        }
        // 前置检查：scope 必须合理
        if let Some(scope) = allowed_blob_names {
            if self.max_scope_blobs > 0 && scope.len() > self.max_scope_blobs {
                return Ok(vec![]);
            }
        }

        let db = self.db.clone();
        let idents = identifiers.clone();
        let scope: Option<Vec<String>> = allowed_blob_names.map(|s| s.to_vec());
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                // 查询 symbol_occurrences JOIN chunk/blob/blob_chunk
                let limit = top_k.max(1) * 20;
                let mut sql = format!(
                    "SELECT so.content_hash, so.identifier, so.kind, so.blob_name,
                            b.path, c.content, bc.start_line, bc.end_line
                     FROM symbol_occurrences so
                     JOIN chunks c ON so.content_hash = c.content_hash
                     JOIN blobs b ON so.blob_name = b.blob_name
                     JOIN blob_chunks bc
                       ON bc.content_hash = so.content_hash AND bc.blob_name = so.blob_name
                     WHERE so.identifier IN ({placeholders})",
                    placeholders = vec!["?"; idents.len()].join(", ")
                );
                if let Some(scope) = &scope {
                    sql.push_str(&format!(
                        " AND so.blob_name IN ({})",
                        vec!["?"; scope.len()].join(", ")
                    ));
                }
                sql.push_str(" LIMIT ");
                sql.push_str(&limit.to_string());

                let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                    idents.iter().map(|i| Box::new(i.clone()) as Box<dyn rusqlite::ToSql>).collect();
                if let Some(scope) = &scope {
                    params.extend(scope.iter().map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>));
                }
                let params_ref: Vec<&dyn rusqlite::ToSql> =
                    params.iter().map(|p| p.as_ref() as &dyn rusqlite::ToSql).collect();

                let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params_ref.as_slice(), |row| {
                        Ok((
                            row.get::<_, String>(0)?,     // content_hash
                            row.get::<_, String>(2)?,     // kind
                            row.get::<_, String>(3)?,     // blob_name
                            row.get::<_, String>(4)?,     // path
                            row.get::<_, String>(5)?,     // content
                            row.get::<_, i64>(6)? as u32, // start
                            row.get::<_, i64>(7)? as u32, // end
                        ))
                    })
                    .map_err(|e| e.to_string())?;

                let mut hits: Vec<SearchHit> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                for (content_hash, kind, blob_name, path, content, start, end) in rows.flatten() {
                    if !seen.insert(content_hash.clone()) {
                        continue;
                    }
                    let score = score_by_kind(&kind);
                    hits.push(SearchHit {
                        blob_name,
                        path,
                        content,
                        score,
                        content_hash,
                        start_line: start,
                        end_line: end,
                    });
                }
                hits.sort_by(|a, b| {
                    b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
                });
                hits.truncate(top_k);
                Ok(hits)
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }
}

fn score_by_kind(kind: &str) -> f32 {
    match kind {
        "endpoint" => 1.0,
        "definition" => 0.95,
        _ => 0.85,
    }
}

/// 首个 chunk 查询（对应 Python `_fetch_content_for_paths` 的 SQL 直查）。
pub struct SqlFirstChunkLookup {
    pub db: SqlDb,
}

#[async_trait]
impl oce_core::retrieval::FirstChunkLookup for SqlFirstChunkLookup {
    async fn first_chunks(&self, blob_names: &[String]) -> Result<Vec<SearchHit>, String> {
        let db = self.db.clone();
        let names: Vec<String> = blob_names.to_vec();
        tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut hits = Vec::new();
                for name in &names {
                    let mut stmt = conn.prepare(
                        "SELECT c.content_hash, c.content, bc.start_line, bc.end_line, b.blob_name, b.path
                         FROM blob_chunks bc
                         JOIN chunks c ON c.content_hash = bc.content_hash
                         JOIN blobs b ON b.blob_name = bc.blob_name
                         WHERE b.blob_name = ?1
                         ORDER BY bc.start_line LIMIT 1",
                    )
                    .map_err(|e| e.to_string())?;
                    if let Ok(row) = stmt.query_row([name], |row| {
                        Ok(SearchHit {
                            content_hash: row.get(0)?,
                            content: row.get(1)?,
                            start_line: row.get::<_, i64>(2)? as u32,
                            end_line: row.get::<_, i64>(3)? as u32,
                            blob_name: row.get(4)?,
                            path: row.get(5)?,
                            score: 0.0, // 由调用方填路径分
                        })
                    }) {
                        hits.push(row);
                    }
                }
                let _ = search_hit_key; // 保持 import 一致性
                Ok(hits)
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// `_classify` 的 blob 状态分类（find_missing / blob-status 共用）。
pub async fn classify_blob_status(
    repo: &SqlBlobRepository,
    blob_names: Vec<String>,
) -> OceResult<(Vec<String>, Vec<String>)> {
    if blob_names.is_empty() {
        return Ok((vec![], vec![]));
    }
    let exists = repo.exists_many(&blob_names).await?;
    let unknown: Vec<String> = blob_names
        .iter()
        .filter(|n| !exists.get(*n).copied().unwrap_or(false))
        .cloned()
        .collect();
    let existing: Vec<String> = blob_names
        .iter()
        .filter(|n| exists.get(*n).copied().unwrap_or(false))
        .cloned()
        .collect();
    let blobs = repo.get_many(&existing).await?;
    let nonindexed: Vec<String> = existing
        .iter()
        .filter(|n| blobs.get(*n).map(|b| !b.is_ready()).unwrap_or(true))
        .cloned()
        .collect();
    Ok((unknown, nonindexed))
}

/// chunk 内容查询（跨 chunk/blob 联表）——LLM 重排等需要内容时使用。
pub async fn content_map(
    db: &SqlDb,
    content_hashes: &[String],
) -> OceResult<HashMap<String, String>> {
    let db = db.clone();
    let hashes: Vec<String> = content_hashes.to_vec();
    run(db, move |conn| {
        let mut out = HashMap::new();
        let mut stmt = conn
            .prepare("SELECT content_hash, content FROM chunks WHERE content_hash = ?1")
            .map_err(|e| e.to_string())?;
        for hash in &hashes {
            if let Ok(content) = stmt.query_row([hash], |r| r.get::<_, String>(1)) {
                out.insert(hash.clone(), content);
            }
        }
        Ok(out)
    })
    .await
}
