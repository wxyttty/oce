//! 进程内任务队列（个人模式；实现 `oce_core::queue::Queue`）。
//!
//! 与 Python `application/queue.py` 协议逐方法对齐：
//! - enqueue 幂等：inflight 名单去重，防重复上传累积幽灵消息
//! - dequeue(timeout)：tokio timeout 包 mpsc recv
//! - ack/fail：从 inflight 名单摘除（进程内无处理中队列，消费即取出）
//! - recover_processing 恒 0（进程内无崩溃残留）
//! - inflight_set：GC 跳过在飞项 / reset 对账去重的关键

use oce_core::error::{OceError, OceResult};
use oce_core::queue::Queue;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct InProcessQueue {
    tx: mpsc::UnboundedSender<String>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>>,
    /// 在飞名单（已入队未 ack/fail）；enqueue 插入，ack/fail/purge 移除
    inflight: Arc<tokio::sync::Mutex<HashSet<String>>>,
}

impl InProcessQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
            inflight: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
        }
    }
}

impl Default for InProcessQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Queue for InProcessQueue {
    async fn enqueue(&self, blob_name: &str) -> OceResult<()> {
        let mut inflight = self.inflight.lock().await;
        if inflight.insert(blob_name.to_string()) {
            let _ = self.tx.send(blob_name.to_string());
        }
        Ok(())
    }

    async fn dequeue(&self, timeout_secs: u64) -> OceResult<Option<String>> {
        let mut rx = self.rx.lock().await;
        let name = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            rx.recv(),
        )
        .await
        .map_err(|e: tokio::time::error::Elapsed| OceError::new(e.to_string(), "QueueTimeout"))?;
        Ok(name)
    }

    async fn ack(&self, blob_name: &str) -> OceResult<()> {
        self.inflight.lock().await.remove(blob_name);
        Ok(())
    }

    async fn fail(&self, blob_name: &str) -> OceResult<()> {
        self.inflight.lock().await.remove(blob_name);
        Ok(())
    }

    async fn size(&self) -> OceResult<u64> {
        // 主队列长度：inflight 名单含处理中，mpsc 无长度查询——用 inflight 近似主队列
        // （进程内消费即取出，主队列与 inflight 差异只在消费瞬间）
        Ok(self.inflight.lock().await.len() as u64)
    }

    async fn recover_processing(&self) -> OceResult<u64> {
        Ok(0) // 进程内无崩溃残留
    }

    async fn inflight_set(&self) -> OceResult<HashSet<String>> {
        Ok(self.inflight.lock().await.clone())
    }

    async fn purge(&self) -> OceResult<u64> {
        let mut inflight = self.inflight.lock().await;
        let n = inflight.len() as u64;
        inflight.clear();
        Ok(n)
    }

    async fn retain(&self, blob_names: &HashSet<String>) -> OceResult<u64> {
        let mut inflight = self.inflight.lock().await;
        let before = inflight.len();
        inflight.retain(|name| blob_names.contains(name));
        Ok((before - inflight.len()) as u64)
    }
}
