//! 任务队列端口（对应 Python `application/queue.py`）。
//!
//! 语义（与 RedisQueue 逐方法对齐）：
//! - enqueue 幂等：在飞（主队列 ∪ 处理中）内同一 blob_name 至多一份，防幽灵消息
//! - dequeue 阻塞取出并移入处理中；ack/fail 摘除在飞状态
//! - recover_processing 启动时恢复上次崩溃残留
//! - inflight_set 供 GC 跳过在飞项、reset 对账去重

use crate::error::OceResult;
use async_trait::async_trait;

/// 任务队列协议（进程内 / Redis / DB 多种实现）。
#[async_trait]
pub trait Queue: Send + Sync {
    /// 投递待处理 blob（幂等，去重防幽灵消息）。
    async fn enqueue(&self, blob_name: &str) -> OceResult<()>;

    /// 阻塞取一个 blob_name，超时返回 None。
    async fn dequeue(&self, timeout_secs: u64) -> OceResult<Option<String>>;

    /// 确认完成：从处理中队列移除。
    async fn ack(&self, blob_name: &str) -> OceResult<()>;

    /// 失败：从处理中队列移除（重试逻辑由 DB 层处理）。
    async fn fail(&self, blob_name: &str) -> OceResult<()>;

    /// 主队列待处理条数。
    async fn size(&self) -> OceResult<u64>;

    /// 启动时恢复处理中队列残留，返回恢复条数。
    async fn recover_processing(&self) -> OceResult<u64>;

    /// 在飞 blob_name 集合（主队列 ∪ 处理中）。
    async fn inflight_set(&self) -> OceResult<std::collections::HashSet<String>>;

    /// 清空队列全部状态，返回清除的在飞条数。
    async fn purge(&self) -> OceResult<u64>;

    /// 只保留给定 blob_name，其余全部剔除，返回剔除条数。
    async fn retain(&self, blob_names: &std::collections::HashSet<String>) -> OceResult<u64>;
}
