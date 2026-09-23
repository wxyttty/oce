//! SQLite FTS5 词法混合路（个人模式）。
//!
//! 服务模式 pgvector tsvector 路的验证结论（+2.8~3.3 分）复刻：
//! bm25 检索与 dense 做 RRF(k=60) 融合。FTS5 的 bm25() 是真 BM25
//! （k1/b 可调，含词频与文档长度归一），且无 trivium 引擎的 AC 前缀
//! 展开噪声——词匹配信号干净。
//!
//! 中文说明：unicode61 分词器把连续汉字当一个 token，中文查询词匹配
//! 不上（自然跳过词法路）——与 pgvector `simple` 配置行为一致，
//! gated 模式（仅代码标识符）不受影响。

use oce_core::error::{OceError, OceResult};

/// FTS5 词法检索句柄（持有 SQLite 连接池引用）。
pub struct FtsLexical {
    db: crate::sqlite::SqlDb,
}

/// bm25 命中（content_hash, blob_name, content, 正分——越大越好）。
/// content 冗余携带：词法补位命中无需回表 JOIN。
pub struct LexicalHit {
    pub content_hash: String,
    pub blob_name: String,
    pub content: String,
    pub score: f32,
}

impl FtsLexical {
    pub fn new(db: crate::sqlite::SqlDb) -> Self {
        Self { db }
    }

    /// bm25 检索（scope 过滤可选）。FTS5 MATCH 语法：空格为 AND。
    /// 返回按 bm25 降序（分数升序取负）的命中列表。
    pub fn search(
        &self,
        tsquery: &str,
        allowed_blob_names: Option<&[String]>,
        limit: usize,
    ) -> OceResult<Vec<LexicalHit>> {
        use rusqlite::params_from_iter;
        let scope_names: Vec<String> = allowed_blob_names.map(|s| s.to_vec()).unwrap_or_default();
        let has_scope = !scope_names.is_empty();
        // scope 占位符从 ?2 开始
        let placeholders = if has_scope {
            format!(
                " AND blob_name IN ({})",
                (0..scope_names.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT content_hash, blob_name, content, -bm25(chunk_fts) AS score
             FROM chunk_fts
             WHERE chunk_fts MATCH ?1{placeholders}
             ORDER BY score DESC
             LIMIT {limit}"
        );
        let result = self.db.with_conn(move |conn| {
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let mut rows = if has_scope {
                let mut params: Vec<Box<dyn rusqlite::ToSql>> =
                    vec![Box::new(tsquery.to_string())];
                for n in &scope_names {
                    params.push(Box::new(n.clone()));
                }
                stmt.query(params_from_iter(params))
                    .map_err(|e| e.to_string())?
            } else {
                stmt.query([tsquery]).map_err(|e| e.to_string())?
            };
            let mut hits = Vec::new();
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                hits.push(LexicalHit {
                    content_hash: r.get(0).map_err(|e| e.to_string())?,
                    blob_name: r.get(1).map_err(|e| e.to_string())?,
                    content: r.get(2).map_err(|e| e.to_string())?,
                    score: r.get::<_, f64>(3).map_err(|e| e.to_string())? as f32,
                });
            }
            Ok(hits)
        });
        // with_conn 闭包返回 Result<Vec<LexicalHit>, String>
        match result {
            Ok(hits) => Ok(hits),
            Err(e) => Err(OceError::new(e, "FtsLexicalError")),
        }
    }
}
