//! 后台嵌入 worker（进程内队列）。与 Python `application/worker.py` 语义对齐：
//! 消费 blob 名 → embed_pending → ack；失败 retry_count++，超限 mark_error + 清 staging。
//!
//! 个人模式默认关闭（同步索引）；WORKER_ENABLED=true 时启动 N 个并发消费协程。

use oce_core::indexing::{BlobRepository, IndexingPipeline};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 进程内队列（替代 Redis；服务模式的 Redis 队列留待后续阶段）。
#[derive(Clone)]
pub struct InProcessQueue {
    tx: mpsc::UnboundedSender<String>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>,
    inflight: Arc<std::sync::atomic::AtomicUsize>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl InProcessQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn enqueue(&self, blob_name: &str) {
        if self.tx.send(blob_name.to_string()).is_ok() {
            self.queued
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn dequeue(&self) -> Option<String> {
        let name = self.rx.lock().await.recv().await;
        if name.is_some() {
            self.queued
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        name
    }

    pub fn size(&self) -> usize {
        // tokio UnboundedSender 无长度查询；用 queued 计数器近似
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct EmbedWorker {
    queue: Arc<InProcessQueue>,
    indexing: Arc<IndexingPipeline>,
    blob_repo: Arc<oce_infra::sqlite::repos::SqlBlobRepository>,
    concurrency: usize,
    max_retries: u32,
    running: Arc<std::sync::atomic::AtomicBool>,
    handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl EmbedWorker {
    pub fn new(
        queue: Arc<InProcessQueue>,
        indexing: Arc<IndexingPipeline>,
        blob_repo: Arc<oce_infra::sqlite::repos::SqlBlobRepository>,
        concurrency: usize,
        max_retries: u32,
    ) -> Self {
        Self {
            queue,
            indexing,
            blob_repo,
            concurrency: concurrency.max(1),
            max_retries,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handles: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 启动 N 个消费协程。
    pub async fn start(&self) {
        if self.is_running() {
            return;
        }
        self.running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut handles = self.handles.lock().await;
        for worker_id in 0..self.concurrency {
            let queue = self.queue.clone();
            let indexing = self.indexing.clone();
            let repo = self.blob_repo.clone();
            let running = self.running.clone();
            let max_retries = self.max_retries;
            handles.push(tokio::spawn(async move {
                worker_loop(worker_id, queue, indexing, repo, running, max_retries).await;
            }));
        }
    }

    /// 停止消费（在途消息处理完自然退出）。
    pub async fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mut handles = self.handles.lock().await;
        for h in handles.drain(..) {
            h.abort();
        }
    }
}

async fn worker_loop(
    worker_id: usize,
    queue: Arc<InProcessQueue>,
    indexing: Arc<IndexingPipeline>,
    repo: Arc<oce_infra::sqlite::repos::SqlBlobRepository>,
    running: Arc<std::sync::atomic::AtomicBool>,
    max_retries: u32,
) {
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        let Some(blob_name) = queue.dequeue().await else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        };
        queue
            .inflight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = indexing
            .embed_pending(Some(&[blob_name.clone()]), false)
            .await;
        match result {
            Ok(_) => {
                tracing::debug!(
                    "worker#{worker_id} processed blob {}",
                    &blob_name[..12.min(blob_name.len())]
                );
            }
            Err(exc) => {
                tracing::warn!("worker#{worker_id} process 失败 blob {blob_name}: {exc}");
                // DB 层重试：超限 mark_error + 删 staging；未超限保留 staging 供重试
                let mut exceeded = false;
                if let Ok(Some(mut blob)) = repo.get(&blob_name).await {
                    blob.retry_count += 1;
                    if blob.retry_count > max_retries {
                        blob.mark_error(format!("{exc}"));
                        exceeded = true;
                    }
                    let _ = repo.save(&blob).await;
                    if exceeded {
                        let _ = repo.delete_staging(&blob_name).await;
                        tracing::error!(
                            "worker#{worker_id} blob {blob_name} 重试超限 → error, staging 已清理"
                        );
                    }
                }
                if !exceeded {
                    queue.enqueue(&blob_name); // 重新入队
                }
            }
        }
        queue
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}
