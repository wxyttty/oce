//! RedisQueue — 可靠的异步任务队列（Python `infrastructure/queue/redis_queue.py` 逐方法移植）。
//!
//! 键布局
//! ------
//! - `{name}`            主队列（LIST，LPUSH 入 / BLMOVE 出）
//! - `{name}:processing` 处理中队列（worker 取走暂存，ack 后删；崩溃残留可恢复）
//! - `{name}:pending`    在飞哨兵 SET（主队列 ∪ 处理中 的去重索引，O(1) 入队判重）
//!
//! 可靠性
//! ------
//! BLMOVE 原子地「主队列出 → 处理中入」，worker 崩在处理中途时消息不丢；
//! ack 才从处理中删。失败时 fail 清理当前在飞状态，worker 更新 DB retry_count 后
//! 按重试上限决定是否重新 enqueue。
//!
//! 幽灵消息防御
//! ------------
//! batch_upload 客户端反复上传同一文件会反复 enqueue —— 无脑 LPUSH 累加曾酿成
//! 「149K 队列消息 vs 5K 真实未就绪」事故。enqueue 走 Lua 原子脚本
//! `SADD pending → 新加才 LPUSH`，保证 (主队列 ∪ 处理中) 内同一 blob_name 至多
//! 一份。ack / fail 时 SREM；未达重试上限时 worker 再次 enqueue。
//!
//! 连接模型
//! --------
//! dequeue 是秒级阻塞调用，必须独占连接（MultiplexedConnection 上阻塞会把
//! 同一连接上的其他命令一起挂住）。每个 worker 拿自己的专用连接；管理面
//! （enqueue/ack/fail/size/…）走共享的 connection-manager。

use oce_core::error::{OceError, OceResult};
use oce_core::queue::Queue;
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::collections::HashSet;

/// Lua 脚本：SADD pending 新加成功才 LPUSH 主队列；返回 1=新加，0=已在飞。
/// 与 Python `_ENQUEUE_DEDUP_LUA` 逐字符一致（幽灵消息防御是真实事故的修复，不得改动）。
const ENQUEUE_DEDUP_LUA: &str = r#"
local added = redis.call('SADD', KEYS[1], ARGV[1])
if added == 1 then
    redis.call('LPUSH', KEYS[2], ARGV[1])
end
return added
"#;

fn redis_err(e: redis::RedisError) -> OceError {
    OceError::new(e.to_string(), "RedisError")
}

pub struct RedisQueue {
    /// 管理面共享连接（非阻塞命令）。
    conn: ConnectionManager,
    /// 原始 URL：worker 专用阻塞连接按需重建（ConnectionManager 无 client 访问器）。
    url: String,
    /// 队列名（三个键的前缀）。
    name: String,
}

impl RedisQueue {
    /// 建立共享连接（connection-manager：断线自动重连）。
    pub async fn connect(url: &str, name: &str) -> Result<Self, String> {
        let client = redis::Client::open(url).map_err(|e| format!("redis url: {e}"))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| format!("redis connect: {e}"))?;
        Ok(Self {
            conn,
            url: url.to_string(),
            name: name.to_string(),
        })
    }

    /// 为 worker 开一条专用阻塞连接（dequeue 用）。
    /// 连接信息从共享 client 克隆，避免重复解析 URL / 建池。
    pub async fn dedicated_connection(&self) -> Result<redis::aio::MultiplexedConnection, String> {
        let client = redis::Client::open(self.url.as_str()).map_err(|e| format!("redis url: {e}"))?;
        client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| format!("redis connect: {e}"))
    }

    fn main_key(&self) -> &str {
        &self.name
    }

    fn processing_key(&self) -> String {
        format!("{}:processing", self.name)
    }

    fn pending_key(&self) -> String {
        format!("{}:pending", self.name)
    }

    /// 阻塞取一个 blob_name，原子移入处理中队列。超时返回 None。
    /// 不动 pending SET：blob 从主队列移到处理中仍属于「在飞」状态。
    ///
    /// Python 用 BRPOPLPUSH（Redis 6.2 起废弃）；等价语义是
    /// BLMOVE src dst RIGHT LEFT timeout（从尾部弹出、压入目标头部）。
    async fn dequeue_on(
        conn: &mut impl redis::aio::ConnectionLike,
        main: &str,
        processing: &str,
        timeout_secs: u64,
    ) -> OceResult<Option<String>> {
        let res: Option<String> = redis::cmd("BLMOVE")
            .arg(main)
            .arg(processing)
            .arg("RIGHT")
            .arg("LEFT")
            .arg(timeout_secs)
            .query_async(conn)
            .await
            .map_err(redis_err)?;
        Ok(res)
    }
}

#[async_trait]
impl Queue for RedisQueue {
    /// 投递待处理 blob：在飞 SET 去重，幽灵消息防御。
    async fn enqueue(&self, blob_name: &str) -> OceResult<()> {
        let script = redis::Script::new(ENQUEUE_DEDUP_LUA);
        let mut conn = self.conn.clone();
        script
            .key(self.pending_key())
            .key(self.main_key())
            .arg(blob_name)
            .invoke_async::<()>(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok(())
    }

    /// 阻塞取一个 blob_name，超时返回 None。
    /// 走专用连接（见 EmbedWorker 装配），这里只保留接口完整性——
    /// 直接在共享连接上阻塞会挂住其他命令，调用方不应使用本方法。
    async fn dequeue(&self, timeout_secs: u64) -> OceResult<Option<String>> {
        let mut conn = self.conn.clone();
        Self::dequeue_on(
            &mut conn,
            self.main_key(),
            &self.processing_key(),
            timeout_secs,
        )
        .await
    }

    /// 确认完成：从处理中队列移除 + 摘 pending。
    async fn ack(&self, blob_name: &str) -> OceResult<()> {
        let mut conn = self.conn.clone();
        let _: () = redis::pipe()
            .lrem(self.processing_key(), 1, blob_name)
            .srem(self.pending_key(), blob_name)
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok(())
    }

    /// 失败：从处理中队列移除 + 摘 pending。
    /// worker 提交 DB retry_count 后可再次 enqueue；Redis 只清理本次在飞状态。
    async fn fail(&self, blob_name: &str) -> OceResult<()> {
        let mut conn = self.conn.clone();
        let _: () = redis::pipe()
            .lrem(self.processing_key(), 1, blob_name)
            .srem(self.pending_key(), blob_name)
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok(())
    }

    /// 主队列待处理条数。
    async fn size(&self) -> OceResult<u64> {
        let mut conn = self.conn.clone();
        let n: usize = conn.llen(self.main_key()).await.map_err(redis_err)?;
        Ok(n as u64)
    }

    /// 启动时把处理中队列残留（上次崩溃遗留）重新入主队列。返回恢复条数。
    /// 顺带重建 pending 哨兵 SET：以 (主队列 ∪ 处理中) 为权威覆盖，
    /// 老数据 / 异常残留都能修正。
    async fn recover_processing(&self) -> OceResult<u64> {
        let mut conn = self.conn.clone();
        // RPOPLPUSH 逐条回流（无阻塞版，处理中残留通常少量）
        let mut n: u64 = 0;
        loop {
            let item: Option<String> = redis::cmd("RPOPLPUSH")
                .arg(self.processing_key())
                .arg(self.main_key())
                .query_async(&mut conn)
                .await
                .map_err(redis_err)?;
            match item {
                Some(_) => n += 1,
                None => break,
            }
        }

        // 重建 pending：以 (主队列 ∪ 处理中) 为权威
        let (main, processing): (Vec<String>, Vec<String>) = redis::pipe()
            .lrange(self.main_key(), 0, -1)
            .lrange(self.processing_key(), 0, -1)
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        let all_inflight: HashSet<String> = main.into_iter().chain(processing).collect();
        let mut pipe = redis::pipe();
        pipe.del(self.pending_key());
        if !all_inflight.is_empty() {
            pipe.sadd(
                self.pending_key(),
                all_inflight.iter().map(String::as_str).collect::<Vec<_>>(),
            );
        }
        let _: () = pipe
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok(n)
    }

    /// 已在飞的 blob_name 集合（主队列 + 处理中）。
    /// 直接读 pending 哨兵 SET，给 requeue 自愈做去重。
    async fn inflight_set(&self) -> OceResult<HashSet<String>> {
        let mut conn = self.conn.clone();
        let members: HashSet<String> = conn
            .smembers(self.pending_key())
            .await
            .map_err(redis_err)?;
        Ok(members)
    }

    /// 删除三个键，返回清除前主队列 + 处理中的条数。
    /// pending 哨兵一并删除：留着它会让这些 blob_name 永远无法重新入队。
    async fn purge(&self) -> OceResult<u64> {
        let mut conn = self.conn.clone();
        let (main_len, processing_len): (usize, usize) = redis::pipe()
            .llen(self.main_key())
            .llen(self.processing_key())
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        let _: () = redis::pipe()
            .del(self.main_key())
            .del(self.processing_key())
            .del(self.pending_key())
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok((main_len + processing_len) as u64)
    }

    /// 按 blob_names 重建主队列与哨兵，返回剔除条数。
    ///
    /// 逐条 LREM 在数万条队列上是 O(n·m)，所以整表读出后在内存里过滤再重写。
    /// DELETE + RPUSH 之间队列短暂为空，因此要求调用时 worker 已停：否则
    /// worker 可能在空窗期取空、或读到重写前的旧序列。
    async fn retain(&self, blob_names: &HashSet<String>) -> OceResult<u64> {
        let mut conn = self.conn.clone();
        let (main_items, processing_items): (Vec<String>, Vec<String>) = redis::pipe()
            .lrange(self.main_key(), 0, -1)
            .lrange(self.processing_key(), 0, -1)
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;

        // 主队列 RPUSH 回填时保持原顺序：BLMOVE 从尾部取，
        // LRANGE 的头部就是最后被消费的一端。
        let kept_main: Vec<&String> =
            main_items.iter().filter(|i| blob_names.contains(*i)).collect();
        let kept_processing: Vec<&String> = processing_items
            .iter()
            .filter(|i| blob_names.contains(*i))
            .collect();
        let removed = (main_items.len() - kept_main.len())
            + (processing_items.len() - kept_processing.len());
        if removed == 0 {
            return Ok(0);
        }

        let surviving: HashSet<&String> = kept_main.iter().chain(kept_processing.iter()).copied().collect();
        let mut pipe = redis::pipe();
        pipe.del(self.main_key());
        if !kept_main.is_empty() {
            pipe.rpush(
                self.main_key(),
                kept_main.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            );
        }
        pipe.del(self.processing_key());
        if !kept_processing.is_empty() {
            pipe.rpush(
                self.processing_key(),
                kept_processing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
            );
        }
        pipe.del(self.pending_key());
        if !surviving.is_empty() {
            pipe.sadd(
                self.pending_key(),
                surviving.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            );
        }
        let _: () = pipe
            .query_async(&mut conn)
            .await
            .map_err(redis_err)?;
        Ok(removed as u64)
    }
}
