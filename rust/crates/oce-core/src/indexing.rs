//! IndexingPipeline — 索引编排。与 Python `domain/services/indexing.py` 语义对齐。
//!
//! 职责：
//! - ingest: 轻量入库（元数据 + staging，切块推给 worker/同步路径），立刻返回
//! - embed_pending: 待嵌入 blob → 切块 → 向量化 → 写回 → Blob 置 ready + 路径索引
//!
//! 关键约束（历史事故修复的保留）：
//! - READY 必须意味着「可被检索」：嵌入开关关闭时保持 pending 且保留 staging，
//!   绝不 mark_ready，避免「有 chunk、零向量」的 blob 被点亮后检索恒空。

use crate::blob::{Blob, BlobStatus};
use crate::chunk::{is_meaningful, Chunk, LocatedChunk};
use crate::chunk::Chunker;
use crate::error::OceResult;
use crate::path_doc::{build_path_document, is_indexable_path};
use crate::search::{Embedder, PathDoc, PathSearchStore, VectorIndex, VectorUpsert};
use crate::source_filter::{is_binary_source, is_ignored_source_path};
use crate::chunk::lang::detect_language;
use async_trait::async_trait;
use rayon::prelude::*;

/// 事件类型常量（与 Python 对齐）。
pub const EVENT_BLOB_CREATED: &str = "blob.created";
pub const EVENT_BLOB_READY: &str = "blob.ready";
pub const EVENT_BLOB_FAILED: &str = "blob.failed";

/// 领域事件。
#[derive(Debug, Clone)]
pub struct DomainEvent {
    pub event_type: String,
    pub data: serde_json::Value,
}

/// Blob/Chunk 元数据的仓储端口（infrastructure 实现；对应 Python repositories）。
#[async_trait]
pub trait BlobRepository: Send + Sync {
    async fn get(&self, blob_name: &str) -> OceResult<Option<Blob>>;
    async fn save(&self, blob: &Blob) -> OceResult<()>;
    async fn delete(&self, blob_name: &str) -> OceResult<()>;
    async fn save_staging(&self, blob_name: &str, content: &str) -> OceResult<()>;
    async fn get_staging(&self, blob_name: &str) -> OceResult<Option<String>>;
    async fn delete_staging(&self, blob_name: &str) -> OceResult<()>;
    /// 取 pending 状态的 blob（限定 blob_names 时只取其中 pending 的）。
    async fn find_pending(&self, blob_names: Option<&[String]>) -> OceResult<Vec<Blob>>;
    /// 按 last_seen 找过期 blob（GC 用）。
    async fn find_expired(&self, ttl_days: u32, batch_size: usize) -> OceResult<Vec<String>>;
    /// 批量存在性检查。
    async fn exists_many(&self, blob_names: &[String]) -> OceResult<std::collections::HashMap<String, bool>>;
    async fn get_many(&self, blob_names: &[String]) -> OceResult<std::collections::HashMap<String, Blob>>;
    /// 取已存在但未嵌入的 chunk 出现位置（LocatedChunk），按 blob 批量。
    async fn find_pending_chunks_for_blobs(&self, blob_names: &[String]) -> OceResult<Vec<LocatedChunk>>;
    /// 保存切块结果（chunk 内容按 content_hash 去重 + blob_chunk 出现位置）。
    async fn save_chunks(&self, blob_name: &str, chunks: &[Chunk]) -> OceResult<()>;
    /// 批量保存多个 blob 的切块结果。默认逐个转发；基础设施可覆写为单事务批量。
    async fn save_chunks_many(&self, batches: &[(String, Vec<Chunk>)]) -> OceResult<()> {
        for (blob_name, chunks) in batches {
            self.save_chunks(blob_name, chunks).await?;
        }
        Ok(())
    }
    /// 批量读取 staging 内容。默认逐个转发；基础设施可覆写为单查询批量。
    async fn get_staging_many(
        &self,
        blob_names: &[String],
    ) -> OceResult<Vec<(String, Option<String>)>> {
        let mut out = Vec::with_capacity(blob_names.len());
        for name in blob_names {
            out.push((name.clone(), self.get_staging(name).await?));
        }
        Ok(out)
    }
    /// 标记 chunk 已嵌入（按 content_hash）。
    async fn mark_embedded(&self, content_hashes: &[String]) -> OceResult<()>;
}

/// 嵌入开关端口（对应 Python settings.embedding.enabled 的运行时读取）。
pub trait EmbeddingGate: Send + Sync {
    fn enabled(&self) -> bool;
}

pub struct IndexingPipeline {
    pub chunker: Box<dyn Chunker>,
    pub embedder: std::sync::Arc<dyn Embedder>,
    pub vector_index: std::sync::Arc<dyn VectorIndex>,
    pub blob_repo: std::sync::Arc<dyn BlobRepository>,
    pub path_store: Option<std::sync::Arc<dyn PathSearchStore>>,
    pub embedding_gate: std::sync::Arc<dyn EmbeddingGate>,
    pub embed_batch_size: usize,
}

impl IndexingPipeline {
    pub fn new(
        chunker: Box<dyn Chunker>,
        embedder: std::sync::Arc<dyn Embedder>,
        vector_index: std::sync::Arc<dyn VectorIndex>,
        blob_repo: std::sync::Arc<dyn BlobRepository>,
        path_store: Option<std::sync::Arc<dyn PathSearchStore>>,
        embedding_gate: std::sync::Arc<dyn EmbeddingGate>,
    ) -> Self {
        Self {
            chunker,
            embedder,
            vector_index,
            blob_repo,
            path_store,
            embedding_gate,
            embed_batch_size: 64,
        }
    }

    /// 轻量入库：只写元数据，切块+嵌入由 embed_pending 完成。
    /// blob_name 由调用方按内容哈希算好传入；客户端轮询 ready 状态。
    pub async fn ingest(&self, blob_name: &str, path: &str, content: &str) -> OceResult<u64> {
        let existing = self.blob_repo.get(blob_name).await?;
        let binary = is_binary_source(content);
        if binary || is_ignored_source_path(path) {
            if existing.as_ref().is_some_and(|b| !b.chunks.is_empty()) {
                self.vector_index.delete(&[blob_name.to_string()]).await?;
                self.blob_repo.delete(blob_name).await?;
            }
            let blob = Blob::new(
                blob_name,
                path,
                BlobStatus::Ready,
                content.len() as u64,
                detect_language(path).map(String::from),
                if binary { "binary" } else { "ignored" },
            );
            self.blob_repo.save(&blob).await?;
            return Ok(0);
        }

        if let Some(existing) = existing {
            if matches!(existing.status, BlobStatus::Pending | BlobStatus::Ready) {
                let mut existing = existing;
                existing.touch();
                self.blob_repo.save(&existing).await?;
                return Ok(0); // 异步模式：不返回 chunk_count，客户端轮询
            }
        }

        let mut blob = Blob::new(
            blob_name,
            path,
            BlobStatus::Pending,
            content.len() as u64,
            detect_language(path).map(String::from),
            "text",
        );
        blob.chunks = vec![]; // worker 负责切块后回填
        self.blob_repo.save(&blob).await?;
        self.blob_repo.save_staging(blob_name, content).await?;
        Ok(0)
    }

    /// 处理待嵌入 blob：切块（如需）→ 向量化 → 写回 → 置 ready。
    /// 返回嵌入条数。路径索引写入失败不影响主索引。
    pub async fn embed_pending(
        &self,
        blob_names: Option<&[String]>,
        mark_failures: bool,
    ) -> OceResult<usize> {
        let blobs = self.blob_repo.find_pending(blob_names).await?;
        if blobs.is_empty() {
            return Ok(0);
        }

        let mut ready_blobs: Vec<Blob> = Vec::new();

        // 第一阶段：补切块（针对 ingest 只写元数据的 blob）。
        // staging 读取与写回走 SQLite（连接互斥，天然串行）；切块是 CPU 密集的
        // tree-sitter 解析，是全量索引的主要耗时——跨 blob 用 rayon 并行，
        // par_iter 保序收集，写回顺序与语义不变。
        let need_chunk_names: Vec<String> = blobs
            .iter()
            .filter(|b| b.chunks.is_empty())
            .map(|b| b.blob_name.clone())
            .collect();
        let staging_map: std::collections::HashMap<String, Option<String>> = if need_chunk_names
            .is_empty()
        {
            std::collections::HashMap::new()
        } else {
            self.blob_repo
                .get_staging_many(&need_chunk_names)
                .await?
                .into_iter()
                .collect()
        };
        let mut to_chunk: Vec<(&Blob, String)> = Vec::new();
        for blob in &blobs {
            if blob.chunks.is_empty() {
                match staging_map.get(&blob.blob_name) {
                    None | Some(None) => {
                        if blob.content_size == 0 {
                            // 空文件没有 staging 内容，无需切块，直接 ready
                            let mut b = blob.clone();
                            b.mark_ready();
                            self.blob_repo.save(&b).await?;
                            ready_blobs.push(b);
                        } else {
                            // staging 不存在（被清理或异常）：标记 error
                            let mut b = blob.clone();
                            b.mark_error("staging content not found");
                            self.blob_repo.save(&b).await?;
                        }
                    }
                    Some(Some(content)) => to_chunk.push((blob, content.clone())),
                }
            }
        }
        let chunked: Vec<(&Blob, Vec<Chunk>)> = to_chunk
            .par_iter()
            .map(|(blob, content)| (*blob, self.chunker.chunk(content, &blob.path)))
            .collect();
        // 空切块与有切块分开处理：有切块的走一次批量事务写回
        let mut write_back: Vec<(String, Vec<Chunk>)> = Vec::new();
        for (blob, chunks) in &chunked {
            if !chunks.is_empty() {
                write_back.push((blob.blob_name.clone(), chunks.clone()));
            }
        }
        if !write_back.is_empty() {
            self.blob_repo.save_chunks_many(&write_back).await?;
        }
        for (blob, chunks) in &chunked {
            if chunks.is_empty() {
                let mut b = (*blob).clone();
                b.mark_ready();
                self.blob_repo.save(&b).await?;
                self.blob_repo.delete_staging(&blob.blob_name).await?;
                ready_blobs.push(b);
            } else {
                let mut b = (*blob).clone();
                b.chunks = chunks.iter().map(|c| c.to_ref()).collect();
                self.blob_repo.save(&b).await?;
            }
        }


        // 嵌入开关关闭：切块已落库，但没有任何向量。保持 pending 且保留 staging，
        // 待开关恢复、blob 重新入队后补嵌。
        if !self.embedding_gate.enabled() {
            return Ok(0);
        }

        // 第二阶段：嵌入
        let names: Vec<String> = blobs.iter().map(|b| b.blob_name.clone()).collect();
        let pending = self.blob_repo.find_pending_chunks_for_blobs(&names).await?;
        let mut embedded = 0usize;

        let result: OceResult<()> = async {
            for batch in pending.chunks(self.embed_batch_size) {
                let texts: Vec<String> = batch.iter().map(|c| embedding_text(c)).collect();
                let vectors = self.embedder.embed_documents(texts).await?;
                if vectors.len() != batch.len() {
                    return Err(crate::error::OceError::new(
                        format!(
                            "Embedding count mismatch: expected {}, got {}",
                            batch.len(),
                            vectors.len()
                        ),
                        "EmbeddingMismatch",
                    ));
                }
                let items: Vec<VectorUpsert> = batch
                    .iter()
                    .zip(vectors.into_iter())
                    .map(|(chunk, vector)| VectorUpsert {
                        chunk_id: chunk.chunk_id(),
                        content_hash: chunk.content_hash.clone(),
                        blob_name: chunk.blob_name.clone(),
                        content: chunk.content.clone(),
                        vector,
                        path: chunk.path.clone(),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                    })
                    .collect();
                self.vector_index.upsert(items).await?;
                self.blob_repo
                    .mark_embedded(&batch.iter().map(|c| c.content_hash.clone()).collect::<Vec<_>>())
                    .await?;
                embedded += batch.len();
            }
            Ok(())
        }
        .await;

        if let Err(exc) = result {
            if mark_failures {
                for blob in &blobs {
                    let mut b = blob.clone();
                    b.mark_error(format!("{exc}"));
                    self.blob_repo.save(&b).await?;
                }
            }
            return Err(exc);
        }

        // 第三阶段：标记 ready + 清理 staging
        for blob in &blobs {
            if blob.status == BlobStatus::Pending {
                let mut b = blob.clone();
                b.mark_ready();
                self.blob_repo.save(&b).await?;
                self.blob_repo.delete_staging(&blob.blob_name).await?;
                ready_blobs.push(b);
            }
        }

        // 路径索引写入失败不应影响主索引（chunk 已嵌入、blob 已 ready），仅记日志
        self.index_paths(&ready_blobs).await;
        Ok(embedded)
    }

    /// 把 ready blob 的路径写入路径索引（文件名查询专用通道）。
    /// 放在 embed_pending 完成后统一批量写入，避免 ingest 阶段多一次 embedding。
    async fn index_paths(&self, blobs: &[Blob]) {
        let Some(path_store) = &self.path_store else {
            return;
        };
        let indexable: Vec<&Blob> = blobs.iter().filter(|b| is_indexable_path(&b.path)).collect();
        if indexable.is_empty() {
            return;
        }
        let docs: Vec<(String, String, String)> = indexable
            .iter()
            .map(|b| (b.blob_name.clone(), b.path.clone(), build_path_document(&b.path)))
            .collect();
        let texts: Vec<String> = docs.iter().map(|(_, _, doc)| doc.clone()).collect();
        match self.embedder.embed_documents(texts).await {
            Ok(vectors) => {
                let path_docs: Vec<PathDoc> = docs
                    .into_iter()
                    .zip(vectors.into_iter())
                    .map(|((blob_name, path, path_document), path_vector)| PathDoc {
                        path_id: format!("path_{blob_name}"),
                        blob_name,
                        path,
                        path_document,
                        path_vector,
                    })
                    .collect();
                let count = path_docs.len();
                match path_store.insert(path_docs).await {
                    Ok(_) => tracing::info!("path index write: {} blobs", count),
                    Err(exc) => tracing::warn!("path index write failed for {} blobs: {}", count, exc),
                }
            }
            Err(exc) => tracing::warn!("path index embed failed: {}", exc),
        }
    }
}

/// 嵌入输入文本（与 Python `LocatedChunk.embedding_text` 一致）。
fn embedding_text(chunk: &LocatedChunk) -> String {
    format!("File: {}\n\n{}", chunk.path, chunk.content)
}

/// 判定内容是否可切块（re-export 便捷用）。
pub fn content_meaningful(content: &str) -> bool {
    is_meaningful(content)
}
