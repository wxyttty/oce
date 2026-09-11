//! 应用服务：跨命令/查询的用例编排。与 Python `application/service.py` 对齐。
//! 传输层只负责 DTO 映射，不编排业务流程。

use oce_core::blob::Blob;
use oce_core::chain::Chain;
use rusqlite;
use oce_core::error::{OceError, OceResult};
use oce_core::formatter::format_retrieval;
use oce_core::indexing::{BlobRepository, IndexingPipeline};
use oce_core::retrieval::RetrievalPipeline;
use oce_core::search::{search_hit_key, VectorIndex, SearchHit, RetrievalAudit};
use oce_core::metrics::{MetricsSink, RetrievalMetricRecord};
use oce_infra::sqlite::chains::classify_blob_status;
use std::sync::Arc;
use std::time::Instant;

/// 上传项（content 为原文，blob_name 由服务端按 path+content 哈希计算）。
#[derive(Debug, Clone)]
pub struct BlobUpload {
    pub path: String,
    pub content: String,
}

/// 与 ACE 客户端一致的内容地址：sha256(path + content)。
pub fn compute_blob_name(path: &str, content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug)]
pub struct BatchUploadResult {
    pub blob_names: Vec<String>,
    pub chunk_count: u64,
    pub embedded_count: usize,
}

#[derive(Debug)]
pub struct RetrievalResult {
    pub hits: Vec<SearchHit>,
    pub formatted_retrieval: String,
    pub elapsed_ms: i64,
}

#[derive(Debug, Default)]
pub struct CheckpointResult {
    pub new_checkpoint_id: String,
}

/// 应用编排：持有索引与检索两条管线 + 仓储。
pub struct RetrievalApplication {
    pub indexing: Arc<IndexingPipeline>,
    pub retrieval: Arc<RetrievalPipeline>,
    pub blob_repo: Arc<oce_infra::sqlite::repos::SqlBlobRepository>,
    pub chain_repo: Arc<oce_infra::sqlite::chains::SqlChainRepository>,
    pub trivium: oce_infra::trivium::TriviumHandle,
    pub metrics: Option<Arc<oce_infra::sqlite::metrics::SqlMetricsSink>>,
}

impl RetrievalApplication {
    pub fn new(
        indexing: Arc<IndexingPipeline>,
        retrieval: Arc<RetrievalPipeline>,
        blob_repo: Arc<oce_infra::sqlite::repos::SqlBlobRepository>,
        chain_repo: Arc<oce_infra::sqlite::chains::SqlChainRepository>,
        trivium: oce_infra::trivium::TriviumHandle,
        metrics: Option<Arc<oce_infra::sqlite::metrics::SqlMetricsSink>>,
    ) -> Self {
        Self {
            indexing,
            retrieval,
            blob_repo,
            chain_repo,
            trivium,
            metrics,
        }
    }

    /// find-missing：未知（未上传）与已上传未索引（非 ready）分类。
    pub async fn find_missing(&self, blob_names: Vec<String>) -> OceResult<(Vec<String>, Vec<String>)> {
        classify_blob_status(&self.blob_repo, blob_names).await
    }

    /// 批量上传：ingest（轻量元数据）→ 同步 embed_pending（个人模式无队列）。
    /// checkpoint_id 非空时把本次 blob 登记进已有链。
    pub async fn batch_upload(
        &self,
        blobs: Vec<BlobUpload>,
        checkpoint_id: Option<&str>,
    ) -> OceResult<BatchUploadResult> {
        let mut names = Vec::with_capacity(blobs.len());
        for blob in &blobs {
            let blob_name = compute_blob_name(&blob.path, &blob.content);
            self.indexing.ingest(&blob_name, &blob.path, &blob.content).await?;
            names.push(blob_name);
        }
        let embedded_count = self.indexing.embed_pending(Some(&names), true).await?;

        let chunk_count = 0; // 异步模式：ingest 只写元数据，客户端轮询 ready
        if let Some(cid) = checkpoint_id {
            if !cid.is_empty() {
                // 可选：把本次 blob 直接登记进已有 checkpoint 链（不隐式创建）
                self.apply_checkpoint_to_chain(cid, &names, &[]).await?;
            }
        }
        Ok(BatchUploadResult {
            blob_names: names,
            chunk_count,
            embedded_count,
        })
    }

    /// 检索：scope 解析（checkpoint/added/deleted）→ 检索管线 → formatter。
    /// 查询路径对 deleted_blobs 无副作用：只做本次检索范围差集，不删服务端数据。
    pub async fn retrieve(
        &self,
        information_request: &str,
        checkpoint_id: Option<&str>,
        added_blobs: &[String],
        deleted_blobs: &[String],
    ) -> OceResult<RetrievalResult> {
        let started = Instant::now();
        let scope = self
            .resolve_scope(checkpoint_id, added_blobs, deleted_blobs)
            .await?;
        let mut audit = RetrievalAudit::default();
        let hits = self
            .retrieval
            .search(information_request, Some(&scope), Some(&mut audit))
            .await;
        let elapsed_ms = started.elapsed().as_millis() as i64;

        // 检索审计旁路落库（monitoring 关闭时为 None，零开销跳过）
        if let Some(metrics) = &self.metrics {
            let record = RetrievalMetricRecord::from_audit(
                &audit,
                "retrieval",
                hits.len(),
                elapsed_ms,
            );
            metrics.record_retrieval(record);
        }

        let formatted = format_retrieval(&hits);
        Ok(RetrievalResult {
            hits,
            formatted_retrieval: formatted,
            elapsed_ms,
        })
    }

    /// scope 解析：(checkpoint 成员 ∪ added) − deleted。
    ///
    /// 全库检索已禁用：checkpoint_id 或 added_blobs 任一有效即可；deleted_blobs 只是
    /// 减法，不构成声明。checkpoint 无效（格式非法或链不存在）直接报错，避免范围
    /// 静默变窄。空集是合法结果（工作集为空，检索返回空而非全库）。
    async fn resolve_scope(
        &self,
        checkpoint_id: Option<&str>,
        added: &[String],
        deleted: &[String],
    ) -> OceResult<Vec<String>> {
        let mut base: Vec<String> = Vec::new();
        if let Some(cid) = checkpoint_id.filter(|s| !s.is_empty()) {
            let Some((chain_id, _version)) = Chain::parse_checkpoint_token(cid) else {
                return Err(OceError::invalid_checkpoint_token(cid));
            };
            let chain = self.chain_repo.get(&chain_id).await?;
            let Some(chain) = chain else {
                return Err(OceError::needs_reset("checkpoint 链不存在（服务端状态丢失）"));
            };
            base.extend(chain.members.into_iter());
        } else if added.is_empty() {
            // 无 checkpoint 也无 added_blobs → 拒绝全库检索
            return Err(OceError::scope_required());
        }
        // added 中尚未嵌入的 blob 同步补嵌（与 Python _prepare_scope 一致）
        if !added.is_empty() {
            self.indexing.embed_pending(Some(added), true).await?;
        }
        let mut scope: Vec<String> = base;
        scope.extend(added.iter().cloned());
        let deleted: std::collections::HashSet<&String> = deleted.iter().collect();
        scope.retain(|m| !deleted.contains(m));
        scope.sort();
        scope.dedup();
        Ok(scope)
    }

    /// checkpoint 命令：新建链或推进已有链，返回 `{chain_id}:{version}` 令牌。
    pub async fn checkpoint(
        &self,
        checkpoint_id: Option<&str>,
        added: &[String],
        deleted: &[String],
    ) -> OceResult<CheckpointResult> {
        match checkpoint_id.filter(|s| !s.is_empty()) {
            None => {
                let members: Vec<String> = added
                    .iter()
                    .filter(|a| !deleted.contains(a))
                    .cloned()
                    .collect();
                let chain = self.chain_repo.create(members).await?;
                self.chain_repo.touch_members(&chain.chain_id).await?;
                Ok(CheckpointResult {
                    new_checkpoint_id: chain.checkpoint_token(),
                })
            }
            Some(token) => {
                let Some((chain_id, _)) = Chain::parse_checkpoint_token(token) else {
                    return Err(OceError::invalid_checkpoint_token(token));
                };
                let version = self
                    .chain_repo
                    .apply_checkpoint(&chain_id, added.to_vec(), deleted.to_vec())
                    .await?;
                let Some(version) = version else {
                    return Err(OceError::needs_reset("checkpoint 链不存在（服务端状态丢失）"));
                };
                self.chain_repo.touch_members(&chain_id).await?;
                Ok(CheckpointResult {
                    new_checkpoint_id: format!("{chain_id}:{version}"),
                })
            }
        }
    }

    /// 推进已有链（batch-upload 可选登记）。链不存在 → NeedsReset（与 Python 一致）。
    async fn apply_checkpoint_to_chain(
        &self,
        token: &str,
        added: &[String],
        deleted: &[String],
    ) -> OceResult<()> {
        let Some((chain_id, _)) = Chain::parse_checkpoint_token(token) else {
            return Err(OceError::invalid_checkpoint_token(token));
        };
        let version = self
            .chain_repo
            .apply_checkpoint(&chain_id, added.to_vec(), deleted.to_vec())
            .await?;
        if version.is_none() {
            return Err(OceError::needs_reset("checkpoint 链不存在（服务端状态丢失）"));
        }
        Ok(())
    }

    /// blob-status：未知 + 未索引 + checkpoint 是否存在。
    pub async fn blob_status(
        &self,
        blob_names: Vec<String>,
        checkpoint_id: Option<&str>,
    ) -> OceResult<(Vec<String>, Vec<String>, bool)> {
        let (unknown, nonindexed) =
            classify_blob_status(&self.blob_repo, blob_names).await?;
        let mut checkpoint_not_found = false;
        if let Some(cid) = checkpoint_id.filter(|s| !s.is_empty()) {
            if let Some((chain_id, _)) = Chain::parse_checkpoint_token(cid) {
                checkpoint_not_found = !self.chain_repo.exists(&chain_id).await?;
            } else {
                checkpoint_not_found = true;
            }
        }
        Ok((unknown, nonindexed, checkpoint_not_found))
    }

    /// GC：过期链删除 + 过期 blob 删除（dry_run 只统计）。
    pub async fn run_gc(
        &self,
        ttl_days: u32,
        dry_run: bool,
        limit: usize,
    ) -> OceResult<GcResult> {
        let expired_chains = self.chain_repo.find_expired(ttl_days).await?;
        let expired_blobs = self.blob_repo.find_expired(ttl_days, limit).await?;
        if dry_run {
            return Ok(GcResult {
                dry_run: true,
                ttl_days,
                expired_chains: expired_chains.len(),
                expired_blobs: expired_blobs.len(),
                deletable_blobs: expired_blobs.len(),
                skipped_inflight: 0,
                deleted_chains: 0,
                deleted_blobs: 0,
            });
        }
        for chain_id in &expired_chains {
            self.chain_repo.delete(chain_id).await?;
        }
        for blob_name in &expired_blobs {
            self.delete_blob(blob_name).await?;
        }
        Ok(GcResult {
            dry_run: false,
            ttl_days,
            expired_chains: expired_chains.len(),
            expired_blobs: expired_blobs.len(),
            deletable_blobs: expired_blobs.len(),
            skipped_inflight: 0,
            deleted_chains: expired_chains.len(),
            deleted_blobs: expired_blobs.len(),
        })
    }

    /// 删除 blob：向量 + 元数据 + 符号 + chunk 孤儿（与 DeleteBlobsCommandHandler 一致）。
    pub async fn delete_blob(&self, blob_name: &str) -> OceResult<()> {
        self.trivium.delete(&[blob_name.to_string()]).await?;
        self.blob_repo.delete(blob_name).await?;
        Ok(())
    }

    /// 查有 staging 但长时间未处理的 pending blob（requeue-stale 用）。
    /// 返回数量；个人模式仅统计，不重复入队（无独立队列）。
    pub async fn find_stale_with_staging(&self, stale_hours: i64, limit: usize) -> OceResult<usize> {
        let db = self.blob_repo.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let cutoff = (chrono::Utc::now()
                    - chrono::Duration::hours(stale_hours))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let mut stmt = conn
                    .prepare(
                        "SELECT b.blob_name FROM blobs b
                         JOIN blob_staging s ON s.blob_name = b.blob_name
                         WHERE b.status = 'pending' AND s.created_at < ?1 LIMIT ?2",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![cutoff, limit as i64], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).count())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }

    /// 队列状态（个人模式无 Redis 队列：enabled=false）。
    pub async fn queue_status(&self) -> OceResult<QueueStatus> {
        let pending = self.blob_repo.find_pending(None).await?;
        Ok(QueueStatus {
            enabled: false,
            main_size: 0,
            inflight: 0,
            db_pending: pending.len(),
        })
    }
}

#[derive(Debug, serde::Serialize)]
pub struct GcResult {
    pub dry_run: bool,
    pub ttl_days: u32,
    pub expired_chains: usize,
    pub expired_blobs: usize,
    pub deletable_blobs: usize,
    pub skipped_inflight: usize,
    pub deleted_chains: usize,
    pub deleted_blobs: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct QueueStatus {
    pub enabled: bool,
    pub main_size: usize,
    pub inflight: usize,
    pub db_pending: usize,
}

/// 便捷：检索命中的去重键（对外暴露给测试）。
pub fn hit_key(hit: &SearchHit) -> (String, String, u32, u32, String) {
    search_hit_key(hit)
}

/// Blob 快照读取（给 status 端点用的最小接口）。
pub async fn blob_snapshot(repo: &dyn BlobRepository, blob_name: &str) -> OceResult<Option<Blob>> {
    repo.get(blob_name).await
}
