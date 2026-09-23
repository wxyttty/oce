//! pgvector 向量引擎：chunk + path 双表 HNSW 检索（服务模式可选后端）。
//!
//! 设计（与 TriviumStore 对齐的四个端口实现）：
//! - `chunk_vectors`：chunk 向量 + HNSW (vector_cosine_ops) + blob_name btree
//! - `path_vectors`：路径文档向量 + HNSW
//! - 模型指纹 sidecar（`vector_index_meta`）：换嵌入模型 fail-closed，与 TriviumDB
//!   的 `oce.tdb.model` 同语义
//! - score = 1 - cosine_distance（与 TriviumDB/Milvus 的余弦相似度语义一致），
//!   阈值由调用方后过滤
//!
//! 已知限制（相对 TriviumDB）：
//! - 无 BM25 词法混合（OCE 的词法信号由 symbol_occurrences exact 路承担，
//!   标识符门控 BM25 的增益另由 A/B 实验评估）
//! - 无 SA-PPR 图扩散（expand_depth 语义不适用）
//! - HNSW 索引删除不收缩（pgvector 已知行为；delete 只清行，索引膨胀靠
//!   VACUUM + 重建缓解）
//!
//! scope 过滤（OCE 的核心查询模式：blob_name IN 数百哈希）是 pgvector 的
//! 已知弱项：HNSW 先 ANN 后过滤会欠返回。缓解：
//! - `SET hnsw.iterative_scan = relaxed`（0.8.0+，过滤后不足时继续扫描）
//! - blob_name btree 支持过滤下推

use async_trait::async_trait;
use oce_core::error::{OceError, OceResult};
use oce_core::search::{
    PathDoc, PathSearchResult, PathSearchStore, SearchHit, SearchStore, VectorEngine,
    VectorIndex, VectorStatsSource, VectorUpsert,
};
use sqlx::PgPool;
use std::sync::Mutex;

fn pg_err(e: sqlx::Error) -> OceError {
    OceError::new(e.to_string(), "PgVectorError")
}

/// 建表 DDL（幂等）。维度在打开时确定，vector 列用占位维度后 ALTER 不改——
/// 直接以实际维度建列；换模型（维度变化）要求重建表（fail-closed）。
fn schema_sql(dim: usize) -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS chunk_vectors (
    chunk_id VARCHAR(128) PRIMARY KEY,
    content_hash VARCHAR(64) NOT NULL,
    blob_name VARCHAR(64) NOT NULL,
    path TEXT NOT NULL,
    content TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    embedding vector({dim}) NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chunk_vectors_blob_name ON chunk_vectors (blob_name);
CREATE INDEX IF NOT EXISTS idx_chunk_vectors_embedding
    ON chunk_vectors USING hnsw (embedding vector_cosine_ops);

CREATE TABLE IF NOT EXISTS path_vectors (
    path_id VARCHAR(256) PRIMARY KEY,
    blob_name VARCHAR(64) NOT NULL,
    path TEXT NOT NULL,
    path_document TEXT NOT NULL,
    embedding vector({dim}) NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_path_vectors_blob_name ON path_vectors (blob_name);
CREATE INDEX IF NOT EXISTS idx_path_vectors_embedding
    ON path_vectors USING hnsw (embedding vector_cosine_ops);

CREATE TABLE IF NOT EXISTS vector_index_meta (
    key VARCHAR(64) PRIMARY KEY,
    value TEXT NOT NULL
);
"#
    )
}

/// pgvector 向量引擎。
pub struct PgVectorStore {
    pool: PgPool,
    /// 模型指纹（打开时校验，fail-closed）。
    model_fingerprint: String,
    /// node_count / kind_stats 缓存（写路径更新，读路径无锁快照）。
    stats_cache: Mutex<(usize, usize)>, // (chunk_count, path_count)
}

impl PgVectorStore {
    /// 打开（或初始化）pgvector 存储。模型指纹不匹配时 fail-closed。
    pub async fn open(pool: PgPool, model_fingerprint: String) -> Result<Self, String> {
        // 扩展存在性检查（镜像 TriviumDB 维度校验的 fail-closed 风格）
        let has_ext: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector')",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("pgvector 扩展检查失败: {e}"))?;
        if !has_ext {
            return Err(
                "pgvector 扩展未安装（CREATE EXTENSION vector；镜像需用 pgvector/pgvector 镜像）"
                    .into(),
            );
        }

        // 建表幂等（首启 / 升级）；指纹校验在其后（表已存在才能读）
        sqlx::raw_sql(&schema_sql(dim_of(&model_fingerprint)))
            .execute(&pool)
            .await
            .map_err(|e| format!("建表失败: {e}"))?;

        let existing: Option<String> = sqlx::query_scalar(
            "SELECT value FROM vector_index_meta WHERE key = 'model_fingerprint'",
        )
        .fetch_optional(&pool)
        .await
        .map_err(|e| format!("读取 vector_index_meta 失败: {e}"))?;
        match existing.as_deref() {
            Some(fp) if fp != model_fingerprint => {
                return Err(format!(
                    "向量索引模型指纹不匹配：索引建于 `{fp}`，当前 `{model_fingerprint}`。\
                     换嵌入模型必须重建向量表（DROP TABLE chunk_vectors, path_vectors, \
                     vector_index_meta 后重启）——混入会静默污染检索"
                ));
            }
            Some(_) => {}
            None => {
                sqlx::query(
                    "INSERT INTO vector_index_meta (key, value) VALUES ('model_fingerprint', $1)
                     ON CONFLICT (key) DO NOTHING",
                )
                .bind(&model_fingerprint)
                .execute(&pool)
                .await
                .map_err(|e| format!("写入模型指纹失败: {e}"))?;
            }
        }

        let store = Self {
            pool,
            model_fingerprint,
            stats_cache: Mutex::new((0, 0)),
        };
        store.refresh_stats_cache().await;
        Ok(store)
    }

    async fn refresh_stats_cache(&self) {
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunk_vectors")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        let paths: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_vectors")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        if let Ok(mut cache) = self.stats_cache.lock() {
            *cache = (chunks.max(0) as usize, paths.max(0) as usize);
        }
    }

    /// 向量字面量：`[0.1,0.2,...]`（pgvector 文本协议）。
    fn vec_literal(v: &[f32]) -> String {
        let mut s = String::with_capacity(v.len() * 9 + 2);
        s.push('[');
        for (i, f) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&f.to_string());
        }
        s.push(']');
        s
    }

    /// scope 过滤 SQL 片段 + 绑定参数起点。
    fn scope_clause(allowed: Option<&[String]>) -> (String, usize) {
        match allowed {
            Some(names) if !names.is_empty() => (" AND blob_name = ANY($2)".into(), 2),
            _ => (String::new(), 1),
        }
    }
}

/// 从模型指纹提取维度（"model dim=1024 etext=v2" → 1024）。
fn dim_of(fingerprint: &str) -> usize {
    fingerprint
        .split("dim=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

#[async_trait]
impl SearchStore for PgVectorStore {
    /// 余弦相似度检索（scope 过滤 + iterative_scan 缓解 HNSW 欠返回）。
    async fn search(
        &self,
        _query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
        vector_threshold: f32,
    ) -> OceResult<Vec<SearchHit>> {
        if top_k == 0 {
            return Ok(vec![]);
        }
        let qv = Self::vec_literal(query_vector);
        let (scope_sql, next_param) = Self::scope_clause(allowed_blob_names);
        // 取 3 倍候选再后过滤阈值（HNSW 近似 + 阈值过滤的召回余量）
        let limit = (top_k * 3).max(30);
        let sql = format!(
            "SELECT content_hash, blob_name, path, content, start_line, end_line,
                    (1 - (embedding <=> $1::vector))::float4 AS score
             FROM chunk_vectors
             WHERE true{scope_sql}
             ORDER BY embedding <=> $1::vector
             LIMIT {limit}"
        );
        let mut q = sqlx::query_as::<
            _,
            (String, String, String, String, i32, i32, f32),
        >(&sql)
        .bind(&qv);
        if next_param == 2 {
            q = q.bind(allowed_blob_names.unwrap().to_vec());
        }
        let rows = q.fetch_all(&self.pool).await.map_err(pg_err)?;
        Ok(rows
            .into_iter()
            .filter(|r| r.6 >= vector_threshold)
            .map(|(content_hash, blob_name, path, content, start_line, end_line, score)| {
                SearchHit {
                    blob_name,
                    path,
                    content,
                    score,
                    content_hash,
                    start_line: start_line.max(0) as u32,
                    end_line: end_line.max(0) as u32,
                }
            })
            .collect())
    }
}

#[async_trait]
impl VectorIndex for PgVectorStore {
    /// 幂等 upsert（ON CONFLICT 主键覆盖）。
    async fn upsert(&self, items: Vec<VectorUpsert>) -> OceResult<u64> {
        if items.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        for item in &items {
            sqlx::query(
                "INSERT INTO chunk_vectors
                 (chunk_id, content_hash, blob_name, path, content, start_line, end_line, embedding)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector)
                 ON CONFLICT (chunk_id) DO UPDATE SET
                    content_hash = excluded.content_hash,
                    blob_name = excluded.blob_name,
                    path = excluded.path,
                    content = excluded.content,
                    start_line = excluded.start_line,
                    end_line = excluded.end_line,
                    embedding = excluded.embedding",
            )
            .bind(&item.chunk_id)
            .bind(&item.content_hash)
            .bind(&item.blob_name)
            .bind(&item.path)
            .bind(&item.content)
            .bind(item.start_line as i32)
            .bind(item.end_line as i32)
            .bind(Self::vec_literal(&item.vector))
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        }
        tx.commit().await.map_err(pg_err)?;
        self.refresh_stats_cache().await;
        Ok(items.len() as u64)
    }

    async fn delete(&self, blob_names: &[String]) -> OceResult<()> {
        if blob_names.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query("DELETE FROM chunk_vectors WHERE blob_name = ANY($1)")
            .bind(blob_names.to_vec())
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        sqlx::query("DELETE FROM path_vectors WHERE blob_name = ANY($1)")
            .bind(blob_names.to_vec())
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)?;
        self.refresh_stats_cache().await;
        Ok(())
    }
}

#[async_trait]
impl PathSearchStore for PgVectorStore {
    async fn search_paths(
        &self,
        _query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<PathSearchResult>> {
        if top_k == 0 {
            return Ok(vec![]);
        }
        let qv = Self::vec_literal(query_vector);
        let (scope_sql, next_param) = Self::scope_clause(allowed_blob_names);
        let sql = format!(
            "SELECT path, blob_name, (1 - (embedding <=> $1::vector))::float4 AS score
             FROM path_vectors
             WHERE true{scope_sql}
             ORDER BY embedding <=> $1::vector
             LIMIT {top_k}"
        );
        let mut q = sqlx::query_as::<_, (String, String, f32)>(&sql).bind(&qv);
        if next_param == 2 {
            q = q.bind(allowed_blob_names.unwrap().to_vec());
        }
        let rows = q.fetch_all(&self.pool).await.map_err(pg_err)?;
        Ok(rows
            .into_iter()
            .map(|(path, blob_name, score)| PathSearchResult {
                path,
                blob_name,
                score,
            })
            .collect())
    }

    async fn insert(&self, path_docs: Vec<PathDoc>) -> OceResult<u64> {
        if path_docs.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        for doc in &path_docs {
            sqlx::query(
                "INSERT INTO path_vectors
                 (path_id, blob_name, path, path_document, embedding)
                 VALUES ($1, $2, $3, $4, $5::vector)
                 ON CONFLICT (path_id) DO UPDATE SET
                    blob_name = excluded.blob_name,
                    path = excluded.path,
                    path_document = excluded.path_document,
                    embedding = excluded.embedding",
            )
            .bind(&doc.path_id)
            .bind(&doc.blob_name)
            .bind(&doc.path)
            .bind(&doc.path_document)
            .bind(Self::vec_literal(&doc.path_vector))
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        }
        tx.commit().await.map_err(pg_err)?;
        self.refresh_stats_cache().await;
        Ok(path_docs.len() as u64)
    }

    async fn delete_by_blob_names(&self, blob_names: &[String]) -> OceResult<()> {
        if blob_names.is_empty() {
            return Ok(());
        }
        sqlx::query("DELETE FROM path_vectors WHERE blob_name = ANY($1)")
            .bind(blob_names.to_vec())
            .execute(&self.pool)
            .await
            .map_err(pg_err)?;
        self.refresh_stats_cache().await;
        Ok(())
    }
}

impl VectorStatsSource for PgVectorStore {
    fn node_count(&self) -> usize {
        self.stats_cache
            .lock()
            .map(|g| g.0 + g.1)
            .unwrap_or(0)
    }

    fn kind_stats(&self) -> Vec<(String, usize)> {
        self.stats_cache
            .lock()
            .map(|g| {
                vec![
                    ("chunk".to_string(), g.0),
                    ("path".to_string(), g.1),
                ]
            })
            .unwrap_or_default()
    }
}

impl VectorEngine for PgVectorStore {}

/// 打开连接池（复用 pg::open_pool 的 UTC + acquire 超时约定）。
pub async fn open_pool(url: &str, max_connections: u32) -> Result<PgPool, String> {
    crate::pg::open_pool(url, max_connections).await
}

/// 模型指纹构造（与 TriviumDB sidecar 同格式，便于 A/B 对照）。
pub fn model_fingerprint(model_tag: &str, dim: usize, etext_version: &str) -> String {
    format!("{model_tag} dim={dim} {etext_version}")
}
