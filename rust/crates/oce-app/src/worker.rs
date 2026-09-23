//! 后台嵌入 worker。与 Python `application/worker.py` 语义对齐：
//! 消费 blob 名 → embed_pending → ack；失败 retry_count++，超限 mark_error + 清 staging。
//!
//! 队列走 `oce_core::queue::Queue` 端口（进程内 / Redis 同一消费循环）。

use oce_core::indexing::{BlobRepository, IndexingPipeline};
use std::sync::Arc;

pub struct EmbedWorker {
    queue: Arc<dyn oce_core::queue::Queue>,
    indexing: Arc<IndexingPipeline>,
    blob_repo: Arc<dyn BlobRepository>,
    concurrency: usize,
    max_retries: u32,
    running: Arc<std::sync::atomic::AtomicBool>,
    handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl EmbedWorker {
    pub fn new(
        queue: Arc<dyn oce_core::queue::Queue>,
        indexing: Arc<IndexingPipeline>,
        blob_repo: Arc<dyn BlobRepository>,
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

    /// 启动前恢复上次崩溃残留（Redis 语义；进程内恒 0），再拉起 N 个消费协程。
    pub async fn start(&self) {
        if self.is_running() {
            return;
        }
        match self.queue.recover_processing().await {
            Ok(n) if n > 0 => tracing::info!("EmbedWorker: 恢复 {n} 条处理中残留任务"),
            Ok(_) => {}
            Err(e) => tracing::warn!("EmbedWorker: recover_processing 失败: {e}"),
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
    queue: Arc<dyn oce_core::queue::Queue>,
    indexing: Arc<IndexingPipeline>,
    repo: Arc<dyn BlobRepository>,
    running: Arc<std::sync::atomic::AtomicBool>,
    max_retries: u32,
) {
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        // 与 Python dequeue(timeout=5) 一致：超时返回 None 后轻睡重试
        let blob_name = match queue.dequeue(5).await {
            Ok(Some(name)) => name,
            Ok(None) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("worker#{worker_id} dequeue 异常: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let result = indexing
            .embed_pending(Some(&[blob_name.clone()]), false)
            .await;
        match result {
            Ok(_) => {
                let _ = queue.ack(&blob_name).await;
                tracing::debug!(
                    "worker#{worker_id} processed blob {}",
                    &blob_name[..12.min(blob_name.len())]
                );
            }
            Err(exc) => {
                tracing::warn!("worker#{worker_id} process 失败 blob {blob_name}: {exc}");
                let _ = queue.fail(&blob_name).await;
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
                    let _ = queue.enqueue(&blob_name).await; // 重新入队
                }
            }
        }
    }
}
