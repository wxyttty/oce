//! RedisQueue 集成测试（`#[ignore]` 门控，需真实 Redis 实例）。
//!
//! 运行方式：
//! ```sh
//! OCE_REDIS_URL="redis://:pass@localhost:26379/0" \
//!   cargo test -p oce-infra --test redis_queue_integration -- --ignored --nocapture
//! ```
//! 每个用例独立队列名（tag + pid），测试完 FLUSHDB 级清理（只删本队列三键）。

use oce_core::queue::Queue;
use oce_infra::redis_queue::RedisQueue;

fn redis_url() -> Option<String> {
    std::env::var("OCE_REDIS_URL").ok().filter(|s| !s.is_empty())
}

async fn setup(tag: &str) -> Option<RedisQueue> {
    let url = redis_url()?;
    let name = format!("oce_test_queue_{}_{}", tag, std::process::id());
    let q = RedisQueue::connect(&url, &name).await.expect("connect redis");
    // 清残留
    q.purge().await.expect("clean start");
    Some(q)
}

#[tokio::test]
#[ignore]
async fn enqueue_dedup_and_dequeue() {
    let Some(q) = setup("basic").await else { return };

    // enqueue 幂等：同一 blob 反复投递只留一份（幽灵消息防御）
    q.enqueue("blob-a").await.unwrap();
    q.enqueue("blob-a").await.unwrap();
    q.enqueue("blob-a").await.unwrap();
    assert_eq!(q.size().await.unwrap(), 1, "重复 enqueue 应去重");

    // 不同 blob 正常入队
    q.enqueue("blob-b").await.unwrap();
    assert_eq!(q.size().await.unwrap(), 2);

    // inflight_set 覆盖主队列（尚未消费）
    let inflight = q.inflight_set().await.unwrap();
    assert!(inflight.contains("blob-a") && inflight.contains("blob-b"));

    // dequeue：LPUSH 入 → 尾部出（FIFO）
    let item = q.dequeue(1).await.unwrap();
    assert_eq!(item.as_deref(), Some("blob-a"), "先入先出");
    // 取走后仍在飞（处理中），主队列减一
    assert_eq!(q.size().await.unwrap(), 1);
    let inflight = q.inflight_set().await.unwrap();
    assert!(inflight.contains("blob-a"), "处理中仍属在飞");

    // ack：摘除处理中 + pending
    q.ack("blob-a").await.unwrap();
    let inflight = q.inflight_set().await.unwrap();
    assert!(!inflight.contains("blob-a"));

    // 超时返回 None
    let none = q.dequeue(1).await.unwrap();
    assert_eq!(none.as_deref(), Some("blob-b"));
    let timeout = q.dequeue(1).await.unwrap();
    assert!(timeout.is_none(), "空队列 1s 超时应返回 None");

    q.purge().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn fail_and_requeue() {
    let Some(q) = setup("fail").await else { return };

    q.enqueue("blob-x").await.unwrap();
    let item = q.dequeue(1).await.unwrap().unwrap();
    assert_eq!(item, "blob-x");

    // fail：清理在飞状态（处理中 + pending）
    q.fail("blob-x").await.unwrap();
    assert!(q.inflight_set().await.unwrap().is_empty());

    // 重新 enqueue（worker 重试路径）：可再次入队
    q.enqueue("blob-x").await.unwrap();
    assert_eq!(q.size().await.unwrap(), 1);

    q.purge().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn recover_processing_rebuilds() {
    let Some(q) = setup("recover").await else { return };

    // 模拟崩溃残留：直接把消息塞进处理中队列 + pending 哨兵缺失
    // （绕过 enqueue，模拟旧版数据 / 异常状态）
    let mut conn = dedicated(&q).await;
    let name = queue_name_of(&q);
    let _: () = redis::cmd("LPUSH")
        .arg(format!("{name}:processing"))
        .arg("stale-1")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("LPUSH")
        .arg(format!("{name}:processing"))
        .arg("stale-2")
        .query_async(&mut conn)
        .await
        .unwrap();

    // recover_processing：处理中残留回流主队列 + 重建 pending 哨兵
    let n = q.recover_processing().await.unwrap();
    assert_eq!(n, 2, "应恢复 2 条残留");
    assert_eq!(q.size().await.unwrap(), 2);
    let inflight = q.inflight_set().await.unwrap();
    assert!(inflight.contains("stale-1") && inflight.contains("stale-2"));

    // 消费一条 + ack，再 recover 应为 0
    let item = q.dequeue(1).await.unwrap().unwrap();
    q.ack(&item).await.unwrap();
    let n = q.recover_processing().await.unwrap();
    assert_eq!(n, 0);

    q.purge().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn purge_and_retain() {
    let Some(q) = setup("purge").await else { return };

    for name in ["a", "b", "c", "d"] {
        q.enqueue(name).await.unwrap();
    }
    // 取走一条进处理中（purge 计数含处理中）
    let taken = q.dequeue(1).await.unwrap().unwrap();
    assert_eq!(taken, "a");

    // purge：清三个键，返回 4（主队列 3 + 处理中 1）
    let removed = q.purge().await.unwrap();
    assert_eq!(removed, 4);
    assert_eq!(q.size().await.unwrap(), 0);
    assert!(q.inflight_set().await.unwrap().is_empty());

    // retain：重建后只保留 keep-set
    for name in ["x", "y", "z"] {
        q.enqueue(name).await.unwrap();
    }
    let mut keep = std::collections::HashSet::new();
    keep.insert("y".to_string());
    let removed = q.retain(&keep).await.unwrap();
    assert_eq!(removed, 2, "x/z 被剔除");
    assert_eq!(q.size().await.unwrap(), 1);
    let item = q.dequeue(1).await.unwrap();
    assert_eq!(item.as_deref(), Some("y"));

    // retain 无剔除时返回 0（不重写）
    let removed = q.retain(&keep).await.unwrap();
    assert_eq!(removed, 0);

    q.purge().await.unwrap();
}

// ───────────────────────── 测试辅助

/// 专用连接（绕过 Queue 端口直接操作 Redis 原始键，模拟异常状态）。
async fn dedicated(_q: &RedisQueue) -> redis::aio::MultiplexedConnection {
    let url = redis_url().unwrap();
    let client = redis::Client::open(url.as_str()).unwrap();
    client.get_multiplexed_async_connection().await.unwrap()
}

/// 队列名从 RedisQueue 拿不到（私有字段），测试里用固定约定重建。
/// setup() 的 name 格式是 oce_test_queue_{tag}_{pid}，这里直接查 Redis 现有键。
fn queue_name_of(_q: &RedisQueue) -> String {
    // recover_processing 测试的队列名在 setup("recover") 时确定；
    // 简化处理：直接用测试内一致的命名约定。
    format!("oce_test_queue_recover_{}", std::process::id())
}
