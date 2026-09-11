//! TriviumDB 向量存储（个人模式引擎，替代 Milvus Lite）。
//!
//! 节点模型（单一 `oce.tdb` 文件）：
//! - `kind="chunk"`：chunk 出现位置节点。vector = embedding；payload 携带
//!   chunk_id/content_hash/blob_name/path/start_line/end_line/content。
//!   upsert 语义按 chunk_id 幂等（与 Python Milvus collection 一致）。
//! - `kind="path"`：路径索引节点（文件名查询专用通道），vector = 路径文档 embedding。
//!
//! 检索全部带 `payload_filter kind` 前置过滤；节点 ID 由 chunk_id 的 sha256 前 8 字节
//! 派生（碰撞时线性探测 salt，读回时校验 payload 一致）。

use crate::settings::TriviumSettings;
use async_trait::async_trait;
use oce_core::error::{OceError, OceResult};
use oce_core::search::{
    PathDoc, PathSearchResult, PathSearchStore, SearchHit, SearchStore, VectorIndex, VectorUpsert,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use triviumdb::database::{Config, SearchConfig, StorageMode};
use triviumdb::filter::Filter;
use triviumdb::storage::wal::SyncMode;
use triviumdb::{Database, NodeId};

/// chunk / path 节点类型标记。
const KIND_CHUNK: &str = "chunk";
const KIND_PATH: &str = "path";

pub struct TriviumStore {
    db: RwLock<Database<f32>>,
    settings: TriviumSettings,
    text_hybrid: bool,
    /// SA-PPR 图扩散深度（0=关闭；需节点间边才有意义）
    expand_depth: usize,
}

impl TriviumStore {
    fn w(&self) -> std::sync::RwLockWriteGuard<'_, Database<f32>> {
        self.db.write().unwrap_or_else(|e| e.into_inner())
    }

    fn r(&self) -> std::sync::RwLockReadGuard<'_, Database<f32>> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }
}

impl TriviumStore {
    /// 打开（或创建）向量库。
    pub fn open(settings: TriviumSettings) -> Result<Self, String> {
        let storage_mode = match settings.storage_mode.as_str() {
            "mmap" => StorageMode::Mmap,
            _ => StorageMode::Rom,
        };
        let sync_mode = match settings.sync_mode.as_str() {
            "full" => SyncMode::Full,
            "off" => SyncMode::Off,
            _ => SyncMode::Normal,
        };
        let db = Database::<f32>::open_with_config(
            &settings.path,
            Config {
                dim: settings.dense_dim,
                storage_mode,
                sync_mode,
                auto_build_quiver: settings.auto_build_quiver,
                // 重启后恢复 BM25 稀疏索引（.tdb.text sidecar），否则混合检索静默失效
                load_text_index: settings.text_hybrid,
                ..Default::default()
            },
        )
        .map_err(|e| format!("open triviumdb {}: {e}", settings.path))?;
        let text_hybrid = settings.text_hybrid;
        let expand_depth = settings.expand_depth;
        let store = Self {
            db: RwLock::new(db),
            settings,
            text_hybrid,
            expand_depth,
        };
        // kind/blob_name/identifier 属性索引：O(1) 前置过滤
        Ok(store)
    }

    pub fn node_count(&self) -> usize {
        self.r().node_count()
    }

    /// 引擎实际维度（来自文件元数据，非配置值）。
    pub fn dim(&self) -> usize {
        self.r().dim()
    }

    /// 批量导入期间降低 WAL 同步级别（bulk load 优化，落库后恢复）。
    pub fn begin_bulk_load(&self) {
        let _ = self.w().set_sync_mode(SyncMode::Off);
    }

    pub fn end_bulk_load(&self) -> Result<(), String> {
        let sync = match self.settings.sync_mode.as_str() {
            "full" => SyncMode::Full,
            "off" => SyncMode::Off,
            _ => SyncMode::Normal,
        };
        let mut db = self.w();
        // 批量导入后编译 BM25/AC 稀疏索引（混合检索的查询侧依赖它）
        if self.text_hybrid {
            db.build_text_index().map_err(|e| e.to_string())?;
        }
        db.set_sync_mode(sync).map_err(|e| e.to_string())?;
        db.flush().map_err(|e| e.to_string())
    }

    pub fn flush(&self) -> Result<(), String> {
        self.w().flush().map_err(|e| e.to_string())
    }

    /// 精确暴力检索（benchmark 基线；kind 过滤在应用层做）。
    pub fn search_exact_hits_sync(
        &self,
        query_vector: &[f32],
        top_k: usize,
    ) -> Result<Vec<(SearchHit, f32)>, String> {
        let db = self.r();
        let hits = db
            .search_exact(query_vector, top_k)
            .map_err(|e| e.to_string())?;
        Ok(hits.into_iter().filter_map(payload_to_hit).collect())
    }

    /// chunk_id → 稳定 u64 节点 ID（sha256 前 8 字节 BE）。
    fn node_id_for(chunk_id: &str) -> NodeId {
        let digest = Sha256::digest(chunk_id.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(bytes)
    }

    /// upsert 单个 chunk 节点。ID 冲突且 payload 属于其它 chunk 时线性探测。
    fn upsert_chunk_sync(&self, item: &VectorUpsert) -> Result<(), String> {
        let payload = json!({
            "kind": KIND_CHUNK,
            "chunk_id": item.chunk_id,
            "content_hash": item.content_hash,
            "blob_name": item.blob_name,
            "path": item.path,
            "start_line": item.start_line,
            "end_line": item.end_line,
            "content": item.content,
        });
        let mut db = self.w();
        let mut salt = 0u32;
        loop {
            let id = if salt == 0 {
                Self::node_id_for(&item.chunk_id)
            } else {
                Self::node_id_for(&format!("{}#{}", item.chunk_id, salt))
            };
            match db.get_payload(id) {
                Some(existing) => {
                    if existing.get("chunk_id").and_then(|v| v.as_str()) == Some(item.chunk_id.as_str())
                    {
                        // 幂等重放：覆盖向量与 payload
                        db.update_vector(id, &item.vector).map_err(|e| e.to_string())?;
                        db.update_payload(id, payload.clone()).map_err(|e| e.to_string())?;
                        if self.text_hybrid {
                            db.index_text(id, &item.content).map_err(|e| e.to_string())?;
                        }
                        return Ok(());
                    }
                    // 罕见碰撞：换 salt 继续
                    salt = salt.checked_add(1).ok_or("node id space exhausted")?;
                }
                None => {
                    db.insert_with_id(id, &item.vector, payload.clone())
                        .map_err(|e| e.to_string())?;
                    if self.text_hybrid {
                        db.index_text(id, &item.content).map_err(|e| e.to_string())?;
                    }
                    return Ok(());
                }
            }
        }
    }

    /// 为同 blob 的 chunk 节点建立双向边（label "same_file"）。
    /// SA-PPR 扩散沿边传播分数：一条 chunk 命中时，同文件兄弟 chunk 进入
    /// 候选池 —— 文件级召回的引擎侧实现。幂等：重复 link 覆盖同边。
    fn link_same_file_edges_sync(&self, blob_name: &str) -> Result<(), String> {
        let tql = format!("FIND {{kind: \"{KIND_CHUNK}\", blob_name: \"{blob_name}\"}} RETURN *");
        let rows = {
            let db = self.r();
            db.tql_nodes(&tql).map_err(|e| e.to_string())?
        };
        let mut line_ids: Vec<(u32, NodeId)> = Vec::new();
        for row in &rows {
            for (_key, node) in row {
                // node.payload 是 serde_json::Value（非 Option）
                let start = node
                    .payload
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                line_ids.push((start, node.id));
            }
        }
        if line_ids.len() < 2 {
            return Ok(());
        }
        line_ids.sort();
        line_ids.dedup_by_key(|(start, _)| *start);
        let mut db = self.w();
        for pair in line_ids.windows(2) {
            let (a, b) = (pair[0].1, pair[1].1);
            db.link(a, b, "same_file", 1.0).map_err(|e| e.to_string())?;
            db.link(b, a, "same_file", 1.0).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 按 blob 删除 chunk 节点：Hash 索引定位 + 逐节点删除。
    fn delete_blob_sync(&self, blob_name: &str) -> Result<usize, String> {
        let ids = self.ids_by_kind_blob(KIND_CHUNK, blob_name)?;
        let mut db = self.w();
        let mut deleted = 0;
        for id in ids {
            if db.delete(id).is_ok() {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// 按 kind + blob_name 过滤取全部节点 ID（TQL FIND，属性索引加速）。
    fn ids_by_kind_blob(&self, kind: &str, blob_name: &str) -> Result<Vec<NodeId>, String> {
        let db = self.r();
        let tql = format!(
            "FIND {{kind: \"{kind}\", blob_name: \"{blob_name}\"}} RETURN *"
        );
        let rows = db.tql_nodes(&tql).map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .flat_map(|row| row.into_values().map(|node| node.id))
            .collect())
    }

    /// 向量检索（kind 过滤）。阻塞调用，由外层 spawn_blocking 隔离。
    fn search_sync(
        &self,
        query_text: Option<&str>,
        query_vector: &[f32],
        kind: &str,
        blob_filter: Option<&[String]>,
        top_k: usize,
        min_score: f32,
        force_brute_force: bool,
    ) -> Result<Vec<(SearchHit, f32)>, String> {
        let filter = if let Some(names) = blob_filter {
            Filter::And(vec![
                Filter::Eq("kind".into(), json!(kind)),
                Filter::In("blob_name".into(), names.iter().map(|n| json!(n)).collect()),
            ])
        } else {
            Filter::Eq("kind".into(), json!(kind))
        };
        // 与 Python 版对齐：引擎不做相似度下限过滤（Milvus 语义），阈值由调用方后过滤
        let _ = min_score;
        let config = SearchConfig {
            top_k,
            expand_depth: self.expand_depth, // SA-PPR 图扩散（同文件边 + 扩散 = 文件级召回）
            min_score: -1.0,
            payload_filter: Some(filter),
            enable_advanced_pipeline: false,
            // BM25 稀疏 + dense 加权 RRF 融合（CJK 2-gram 分词，中文词法兜底）
            enable_text_hybrid_search: self.text_hybrid,
            text_boost: 1.5,
            force_brute_force,
            ..Default::default()
        };
        // 概念投影只作用于概念型查询（Feature/Overview/Compound）的词法通道：
        // PATH/SYMBOL 查询的 BM25 词法是精确信号，灌入别名 token 反而淹没目标
        // （实测 flask file_exact 类 -10 分的根因）。
        let enriched = query_text
            .filter(|_| kind == KIND_CHUNK)
            .filter(|q| {
                matches!(
                    oce_core::classifier::classify_query_intent(q),
                    oce_core::classifier::Intent::Feature
                        | oce_core::classifier::Intent::Overview
                        | oce_core::classifier::Intent::Compound
                )
            })
            .map(oce_core::lexical::enrich_lexical_query);
        let db = self.r();
        let hits = db
            .search_hybrid(enriched.as_deref().or(query_text), Some(query_vector), &config)
            .map_err(|e| e.to_string())?;
        Ok(hits.into_iter().filter_map(payload_to_hit).collect())
    }

    /// 路径文档 upsert：path_id 确定性派生节点 ID。
    fn upsert_path_sync(&self, doc: &PathDoc) -> Result<(), String> {
        let payload = json!({
            "kind": KIND_PATH,
            "blob_name": doc.blob_name,
            "path": doc.path,
            "path_id": doc.path_id,
        });
        let mut db = self.w();
        let id = Self::node_id_for(&doc.path_id);
        match db.get_payload(id) {
            Some(existing)
                if existing.get("path_id").and_then(|v| v.as_str())
                    == Some(doc.path_id.as_str()) =>
            {
                db.update_vector(id, &doc.path_vector).map_err(|e| e.to_string())?;
                db.update_payload(id, payload.clone()).map_err(|e| e.to_string())?;
                if self.text_hybrid {
                    db.index_text(id, &doc.path_document).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            Some(_) => {
                let salted = Self::node_id_for(&format!("{}#dup", doc.path_id));
                db.insert_with_id(salted, &doc.path_vector, payload.clone())
                    .map_err(|e| e.to_string())?;
                if self.text_hybrid {
                    db.index_text(salted, &doc.path_document).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            None => {
                db.insert_with_id(id, &doc.path_vector, payload.clone())
                    .map_err(|e| e.to_string())?;
                if self.text_hybrid {
                    db.index_text(id, &doc.path_document).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }

    fn delete_paths_sync(&self, blob_names: &[String]) -> Result<usize, String> {
        let mut ids = Vec::new();
        for name in blob_names {
            ids.extend(self.ids_by_kind_blob(KIND_PATH, name)?);
        }
        let mut db = self.w();
        let mut deleted = 0;
        for id in ids {
            if db.delete(id).is_ok() {
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

/// TriviumDB SearchHit → oce SearchHit（path 节点缺 content/start/end 时给默认值）。
fn payload_to_hit(hit: triviumdb::node::SearchHit) -> Option<(SearchHit, f32)> {
    let payload = hit.payload;
    let path = payload.get("path")?.as_str()?.to_string();
    let content = payload
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let blob_name = payload.get("blob_name")?.as_str()?.to_string();
    let content_hash = payload
        .get("content_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let start_line = payload
        .get("start_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;
    let end_line = payload
        .get("end_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;
    Some((
        SearchHit {
            blob_name,
            path,
            content,
            score: hit.score,
            content_hash,
            start_line,
            end_line,
        },
        hit.score,
    ))
}

/// 把阻塞的 TriviumDB 调用移出异步 worker 线程。
/// 注意：要求多线程 tokio runtime（`#[tokio::main]` 默认即多线程）。
fn blocking<T>(f: impl FnOnce() -> Result<T, String>) -> OceResult<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| f().map_err(|m| OceError::new(m, "TriviumError")))
    } else {
        f().map_err(|m| OceError::new(m, "TriviumError"))
    }
}

#[async_trait]
impl SearchStore for TriviumStore {
    async fn search(
        &self,
        query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
        vector_threshold: f32,
    ) -> OceResult<Vec<SearchHit>> {
        let qv = query_vector.to_vec();
        let blob_filter: Option<Vec<String>> = allowed_blob_names.map(|s| s.to_vec());
        blocking(|| {
            self.search_sync(Some(query), &qv, KIND_CHUNK, blob_filter.as_deref(), top_k, vector_threshold, false)
                .map(|hits| {
                    // 后过滤（与 Python `score < vector_threshold: continue` 一致）
                    hits.into_iter()
                        .filter(|(hit, _)| hit.score >= vector_threshold)
                        .map(|(hit, _)| hit)
                        .collect()
                })
        })
    }
}

#[async_trait]
impl VectorIndex for TriviumStore {
    async fn upsert(&self, items: Vec<VectorUpsert>) -> OceResult<u64> {
        blocking(|| {
            self.begin_bulk_load();
            let mut n = 0usize;
            for item in &items {
                self.upsert_chunk_sync(item)?;
                n += 1;
            }
            self.end_bulk_load()?;
            // 同文件边：批量去重后逐 blob 建链（扩散召回通道）
            if self.expand_depth > 0 {
                let mut blobs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                for item in &items {
                    blobs.insert(item.blob_name.clone());
                }
                for blob in blobs {
                    self.link_same_file_edges_sync(&blob)?;
                }
            }
            Ok(n as u64)
        })
    }

    async fn delete(&self, blob_names: &[String]) -> OceResult<()> {
        blocking(|| {
            for name in blob_names {
                self.delete_blob_sync(name)?;
            }
            self.flush()?;
            Ok(())
        })
    }
}

#[async_trait]
impl PathSearchStore for TriviumStore {
    async fn search_paths(
        &self,
        query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<PathSearchResult>> {
        let qv = query_vector.to_vec();
        let blob_filter: Option<Vec<String>> = allowed_blob_names.map(|s| s.to_vec());
        blocking(|| {
            self.search_sync(Some(query), &qv, KIND_PATH, blob_filter.as_deref(), top_k, 0.0, false)
                .map(|hits| {
                    hits.into_iter()
                        .map(|(hit, score)| PathSearchResult {
                            path: hit.path,
                            blob_name: hit.blob_name,
                            score,
                        })
                        .collect()
                })
        })
    }

    async fn insert(&self, path_docs: Vec<PathDoc>) -> OceResult<u64> {
        blocking(|| {
            let mut n = 0;
            for doc in &path_docs {
                self.upsert_path_sync(doc)?;
                n += 1;
            }
            self.flush()?;
            Ok(n as u64)
        })
    }

    async fn delete_by_blob_names(&self, blob_names: &[String]) -> OceResult<()> {
        blocking(|| {
            self.delete_paths_sync(blob_names)?;
            self.flush()?;
            Ok(())
        })
    }
}

/// 共享句柄：`Arc<TriviumStore>` 别名。trait 已直接实现在 TriviumStore 上，
/// Arc 自动向 `Arc<dyn SearchStore/VectorIndex/PathSearchStore>` 强转。
pub type TriviumHandle = Arc<TriviumStore>;
