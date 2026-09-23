//! PostgreSQL 元数据层集成测试（`#[ignore]` 门控，需真实 PG 实例）。
//!
//! 运行方式：
//! ```sh
//! OCE_PG_URL="postgres://oce:oce@localhost:25432/oce" \
//!   cargo test -p oce-infra --test pg_integration -- --ignored --nocapture
//! ```
//! 未设置 OCE_PG_URL 时 `--ignored` 过滤后无测试可跑，CI 默认跳过。
//!
//! 每个用例独立 schema（tag + pid 后缀），测试完 DROP SCHEMA CASCADE，
//! 互不污染、可并行。SCHEMA_SQL 全部是裸表名，search_path 指到测试 schema 即生效。

use oce_core::blob::{Blob, BlobStatus};
use oce_core::chain::ChainRepository;
use oce_core::credentials::{CredentialAdminStore, CredentialUpsert};
use oce_core::indexing::BlobRepository;
use oce_core::retrieval::FirstChunkLookup;
use oce_core::search::ExactSearchStore;
use oce_core::chunk::Chunk;
use oce_infra::pg::chains::{PgChainRepository, PgExactStore, PgFirstChunkLookup};
use oce_infra::pg::credentials::PgCredentialAdminStore;
use oce_infra::pg::repos::PgBlobRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

fn pg_url() -> Option<String> {
    std::env::var("OCE_PG_URL").ok().filter(|s| !s.is_empty())
}

/// 独立 schema 连接池（连接级 search_path，池内所有连接生效）。
async fn setup(tag: &str) -> Option<(PgPool, String)> {
    let url = pg_url()?;
    let schema = format!("oce_test_{}_{}", tag, std::process::id());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .after_connect({
            let schema = std::sync::Arc::new(schema.clone());
            move |conn, _meta| {
                let schema = std::sync::Arc::clone(&schema);
                Box::pin(async move {
                    // 连接级 search_path：SCHEMA_SQL 的裸表名会落到该 schema
                    sqlx::query(&format!("SET search_path TO {schema}, public"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .expect("connect pg");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop old schema");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create schema");
    sqlx::raw_sql(oce_infra::pg::SCHEMA_SQL)
        .execute(&pool)
        .await
        .expect("apply schema sql");
    Some((pool, schema))
}

async fn teardown(pool: &PgPool, schema: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(pool)
        .await;
    pool.close().await;
}

fn test_blob(name: &str, path: &str, status: BlobStatus) -> Blob {
    Blob::new(name, path, status, 100, Some("python".into()), "python")
}

/// sha256("content-hash-{i}")——Chunk::new 校验必须是合法 sha256 hex。
fn sha256_of(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn test_chunk(i: usize) -> Chunk {
    let hash = sha256_of(&format!("chunk-{i}"));
    Chunk::new(
        hash,
        format!("src/f{i}.py"),
        format!("def fn_{i}(): pass # fixture {i}"),
        1 + i as u32 * 10,
        9 + i as u32 * 10,
        Some("function".into()),
    )
    .unwrap()
}

// ───────────────────────── blob 仓储

#[tokio::test]
#[ignore]
async fn blob_repo_roundtrip() {
    let Some((pool, schema)) = setup("blob").await else { return };
    let repo = PgBlobRepository { pool: pool.clone() };

    // save（upsert 幂等）+ get
    let blob = test_blob("b1", "src/a.py", BlobStatus::Pending);
    repo.save(&blob).await.unwrap();
    let loaded = repo.get("b1").await.unwrap().expect("saved blob");
    assert_eq!(loaded.path, "src/a.py");
    assert_eq!(loaded.status, BlobStatus::Pending);

    // 二次 save 走 upsert（改路径 + 转 ready）
    let mut updated = blob.clone();
    updated.path = "src/b.py".into();
    updated.status = BlobStatus::Ready;
    repo.save(&updated).await.unwrap();
    let loaded = repo.get("b1").await.unwrap().expect("upserted");
    assert_eq!(loaded.path, "src/b.py");
    assert_eq!(loaded.status, BlobStatus::Ready);

    // save_chunks（含符号提取）+ find_pending_chunks_for_blobs（只看 pending blob）
    let chunks = vec![test_chunk(1), test_chunk(2)];
    repo.save_chunks("b1", &chunks).await.unwrap();
    // b1 已 ready → pending chunks 为空；造一个 pending blob 再验
    repo.save(&test_blob("b2", "src/c.py", BlobStatus::Pending)).await.unwrap();
    let c3 = test_chunk(3);
    repo.save_chunks("b2", &[c3]).await.unwrap();
    let pending: Vec<String> = repo
        .find_pending_chunks_for_blobs(&["b2".to_string()])
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.content_hash)
        .collect();
    assert!(pending.contains(&sha256_of("chunk-3")));

    // find_pending_chunks_for_blobs 按 blob 状态过滤（与 Python 一致），
    // 不看 chunk.embedded：mark_embedded 只影响 chunks 表标记，
    // blob 转 ready 后才不再返回 pending chunks。
    repo.mark_embedded(&[sha256_of("chunk-3")]).await.unwrap();
    let embedded_flag: bool = sqlx::query_scalar(
        "SELECT embedded FROM chunks WHERE content_hash = $1",
    )
    .bind(sha256_of("chunk-3"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(embedded_flag, "mark_embedded 应置位 chunks.embedded");
    // blob 仍 pending → 仍会返回（幂等重嵌语义：按 blob 状态全量重捞）
    let still = repo
        .find_pending_chunks_for_blobs(&["b2".to_string()])
        .await
        .unwrap();
    assert_eq!(still.len(), 1);
    // blob 转 ready 后不再返回
    let mut b2 = repo.get("b2").await.unwrap().unwrap();
    b2.status = BlobStatus::Ready;
    repo.save(&b2).await.unwrap();
    let gone = repo
        .find_pending_chunks_for_blobs(&["b2".to_string()])
        .await
        .unwrap();
    assert!(gone.is_empty());

    // exists_many / get_many（HashMap 返回）
    let exists = repo.exists_many(&["b1".to_string()]).await.unwrap();
    assert_eq!(exists.get("b1"), Some(&true));
    let many = repo.get_many(&["b1".to_string(), "b2".to_string()]).await.unwrap();
    assert_eq!(many.len(), 2);

    // staging：空内容合法（区分"未上传"与"空文件"）→ delete_staging
    repo.save_staging("b2", "").await.unwrap();
    let st = repo.get_staging("b2").await.unwrap();
    assert_eq!(st.as_deref(), Some(""));
    repo.delete_staging("b2").await.unwrap();
    assert!(repo.get_staging("b2").await.unwrap().is_none());

    // delete（单 blob）：级联清理 blob_chunks / staging
    repo.delete("b2").await.unwrap();
    assert!(repo.get("b2").await.unwrap().is_none());

    teardown(&pool, &schema).await;
}

#[tokio::test]
#[ignore]
async fn blob_repo_pending_and_stale() {
    let Some((pool, schema)) = setup("pending").await else { return };
    let repo = PgBlobRepository { pool: pool.clone() };

    repo.save(&test_blob("p1", "x.py", BlobStatus::Pending)).await.unwrap();
    repo.save(&test_blob("p2", "y.py", BlobStatus::Pending)).await.unwrap();
    repo.save(&test_blob("p3", "z.py", BlobStatus::Ready)).await.unwrap();

    // find_pending（无 scope = 全部 pending）
    let pending = repo.find_pending(None).await.unwrap();
    assert_eq!(pending.len(), 2);
    // find_pending（scope 过滤）
    let scoped = repo.find_pending(Some(&["p1".to_string()])).await.unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].blob_name, "p1");
    // list_pending_names
    let names = repo.list_pending_names().await.unwrap();
    assert_eq!(names.len(), 2);
    // find_all_blob_names
    let all = repo.find_all_blob_names().await.unwrap();
    assert_eq!(all.len(), 3);

    // find_stale_with_staging：pending + staging 超过阈值
    repo.save_staging("p1", "content").await.unwrap();
    sqlx::query(
        "UPDATE blob_staging SET created_at = now() - interval '48 hours' WHERE blob_name = 'p1'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let stale = repo.find_stale_with_staging(24, 10).await.unwrap();
    assert_eq!(stale, vec!["p1".to_string()]);
    // 新写入的 staging 不算 stale
    repo.save_staging("p2", "content").await.unwrap();
    let stale = repo.find_stale_with_staging(24, 10).await.unwrap();
    assert_eq!(stale.len(), 1);

    // find_expired：last_seen 拨回 31 天前
    sqlx::query("UPDATE blobs SET last_seen = now() - interval '31 days' WHERE blob_name = 'p3'")
        .execute(&pool)
        .await
        .unwrap();
    let expired = repo.find_expired(30, 100).await.unwrap();
    assert!(expired.contains(&"p3".to_string()));

    teardown(&pool, &schema).await;
}

// ───────────────────────── chain 仓储

#[tokio::test]
#[ignore]
async fn chain_repo_roundtrip() {
    let Some((pool, schema)) = setup("chain").await else { return };
    let repo = PgChainRepository { pool: pool.clone() };
    let blobs = PgBlobRepository { pool: pool.clone() };
    for i in 1..=3 {
        blobs
            .save(&test_blob(&format!("c{i}"), &format!("f{i}.py"), BlobStatus::Ready))
            .await
            .unwrap();
    }

    // create：去重、version=1
    let chain = repo
        .create(vec!["c1".into(), "c2".into(), "c3".into(), "c1".into()])
        .await
        .unwrap();
    let chain_id = chain.chain_id.clone();
    assert_eq!(chain.version, 1);
    assert_eq!(chain.members.len(), 3, "重复成员应去重");
    assert!(repo.exists(&chain_id).await.unwrap());
    let loaded = repo.get(&chain_id).await.unwrap().expect("chain");
    assert_eq!(loaded.members.len(), 3);

    // apply_checkpoint：删 c1 增 c4 → version+1
    blobs.save(&test_blob("c4", "f4.py", BlobStatus::Ready)).await.unwrap();
    let v = repo
        .apply_checkpoint(
            &chain_id,
            vec!["c4".into()],
            vec!["c1".into()],
        )
        .await
        .unwrap()
        .expect("bump");
    assert_eq!(v, 2);
    let loaded = repo.get(&chain_id).await.unwrap().unwrap();
    assert!(loaded.members.contains("c4"));
    assert!(!loaded.members.contains("c1"));

    // apply_checkpoint 不存在的 chain → None
    assert!(repo.apply_checkpoint("nope", vec![], vec![]).await.unwrap().is_none());

    // touch_members：blobs.last_seen 更新为 now
    sqlx::query("UPDATE blobs SET last_seen = now() - interval '10 days' WHERE blob_name = 'c2'")
        .execute(&pool)
        .await
        .unwrap();
    repo.touch_members(&chain_id).await.unwrap();
    let seen: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT last_seen FROM blobs WHERE blob_name = 'c2'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(seen > chrono::Utc::now() - chrono::Duration::minutes(1));

    // find_expired：updated_at 拨回 31 天前
    sqlx::query(&format!(
        "UPDATE chains SET updated_at = now() - interval '31 days' WHERE chain_id = '{chain_id}'"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let expired = repo.find_expired(30).await.unwrap();
    assert_eq!(expired, vec![chain_id.clone()]);

    // delete
    repo.delete(&chain_id).await.unwrap();
    assert!(!repo.exists(&chain_id).await.unwrap());

    teardown(&pool, &schema).await;
}

// ───────────────────────── 凭据管理

#[tokio::test]
#[ignore]
async fn credential_admin_roundtrip() {
    let Some((pool, schema)) = setup("cred").await else { return };
    let store = PgCredentialAdminStore { pool: pool.clone() };

    // create（默认 status=active、priority=100、timeout=30）
    let rec = store
        .create(CredentialUpsert {
            kind: Some("embed".into()),
            name: Some("main".into()),
            endpoint: Some("http://localhost:9999".into()),
            api_key: Some("sk-1234567890".into()),
            model: Some("bge-m3".into()),
            priority: Some(0),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(rec.id > 0);
    assert_eq!(rec.kind, "embed");

    // 唯一约束 (kind, model, api_key_hash) 冲突 → OceError（409 语义）
    let dup = store
        .create(CredentialUpsert {
            kind: Some("embed".into()),
            name: Some("another".into()),
            endpoint: Some("http://localhost:9999".into()),
            api_key: Some("sk-1234567890".into()),
            model: Some("bge-m3".into()),
            ..Default::default()
        })
        .await;
    assert!(dup.is_err(), "同 kind+model+key_hash 应冲突");

    // resolve_active：最小 priority 的 active 记录，运行时取明文
    let rt = store.resolve_active("embed").await.unwrap().expect("active embed");
    assert_eq!(rt.model, "bge-m3");
    assert_eq!(rt.api_key, "sk-1234567890");

    // update（部分更新：None 字段保留旧值）
    let updated = store
        .update(
            rec.id,
            CredentialUpsert {
                model: Some("bge-m3-v2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("updated");
    assert_eq!(updated.model.as_deref(), Some("bge-m3-v2"));
    // api_key 未提交 → 保留旧 key（resolve 仍能取到明文）
    let rt2 = store.resolve_active("embed").await.unwrap().unwrap();
    assert_eq!(rt2.api_key, "sk-1234567890");

    // duplicate：None 字段继承源（含 api_key 复用）；唯一约束是 (kind, model, api_key_hash)，
    // 同 kind+model+key 的完全复制会冲突（与 Python IntegrityError→409 一致），
    // 所以复制必须改 model 或 key 才能落库——这正是"同一把 key 换用途"的语义。
    let copy = store
        .duplicate(
            rec.id,
            CredentialUpsert {
                name: Some("copy".into()),
                model: Some("bge-m3-large".into()),
                priority: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("duplicated");
    assert_eq!(copy.name, "copy");
    assert_eq!(copy.model.as_deref(), Some("bge-m3-large"));
    // 完全复制（不改 model/key）→ 唯一约束冲突
    let same = store
        .duplicate(
            rec.id,
            CredentialUpsert {
                name: Some("same".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(same.is_err(), "同 kind+model+key 复制应冲突");

    // list / get / delete
    let all = store.list().await.unwrap();
    assert_eq!(all.len(), 2);
    let one = store.get(rec.id).await.unwrap().unwrap();
    assert_eq!(one.name, "main");
    assert!(store.delete(rec.id).await.unwrap());
    assert!(store.get(rec.id).await.unwrap().is_none());

    teardown(&pool, &schema).await;
}

// ───────────────────────── 检索辅助 store

#[tokio::test]
#[ignore]
async fn exact_and_lookup_stores() {
    let Some((pool, schema)) = setup("exact").await else { return };
    let blobs = PgBlobRepository { pool: pool.clone() };
    blobs.save(&test_blob("x1", "src/service.py", BlobStatus::Ready)).await.unwrap();
    let c1 = Chunk::new(
        sha256_of("exact-1"),
        "src/service.py",
        "def get_user(user_id): return db.query(User).get(user_id)",
        1,
        1,
        Some("function".into()),
    )
    .unwrap();
    let c2 = Chunk::new(
        sha256_of("exact-2"),
        "src/service.py",
        "class UserService: def get_user(self, uid): ...",
        10,
        12,
        Some("class".into()),
    )
    .unwrap();
    blobs.save_chunks("x1", &[c1, c2]).await.unwrap();

    // FirstChunkLookup：文件首 chunk（start_line 最小）
    let fcl = PgFirstChunkLookup { pool: pool.clone() };
    let firsts = fcl.first_chunks(&["x1".to_string()]).await.unwrap();
    assert_eq!(firsts.len(), 1);
    assert_eq!(firsts[0].start_line, 1);

    // ExactStore：标识符精确召回（scope 内）
    let exact = PgExactStore { pool: pool.clone(), max_scope_blobs: 100 };
    let hits = exact
        .search_exact(&["get_user".to_string()], Some(&["x1".to_string()]), 5)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "get_user 应命中");
    // scope 外无命中
    let none = exact
        .search_exact(&["get_user".to_string()], Some(&["other".to_string()]), 5)
        .await
        .unwrap();
    assert!(none.is_empty());

    teardown(&pool, &schema).await;
}

// ───────────────────────── 监控 sink + 报表 reader

#[tokio::test]
#[ignore]
async fn metrics_sink_and_reports() {
    use oce_core::metrics::{ApiCallRecord, MetricsSink, ResourceSampleRecord, RetrievalMetricRecord, TokenUsageRecord};
    use oce_core::reports::ReportsStore;

    let Some((pool, schema)) = setup("metrics").await else { return };
    let sink = std::sync::Arc::new(oce_infra::pg::metrics::PgMetricsSink::new(pool.clone()));

    // 旁路写入：token / api / retrieval / resource
    sink.record_token_usage(TokenUsageRecord {
        kind: "embed".into(),
        model: "bge-m3".into(),
        credential_id: 1,
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
    });
    sink.record_api_call(ApiCallRecord {
        endpoint: "/api/search".into(),
        method: "POST".into(),
        status_code: 200,
        latency_ms: 42,
        error_type: None,
    });
    sink.record_api_call(ApiCallRecord {
        endpoint: "/api/search".into(),
        method: "POST".into(),
        status_code: 500,
        latency_ms: 900,
        error_type: Some("upstream".into()),
    });
    sink.record_retrieval(RetrievalMetricRecord {
        source: "api".into(),
        scope_size: Some(120),
        hit_count: 8,
        total_ms: 350,
        intent: Some("code".into()),
        path_boosted: true,
        query_text: Some("find user service".into()),
        intent_ms: Some(5),
        rewrite_ms: None,
        dense_ms: Some(100),
        exact_ms: Some(20),
        fuse_ms: Some(10),
        rerank_ms: Some(200),
        llm_rerank_ms: None,
        select_ms: Some(15),
    });
    sink.record_resource_sample(ResourceSampleRecord {
        mem_rss_bytes: 1_000_000,
        mem_percent: 50.0,
        cpu_percent: 25.0,
        disk_data_bytes: 2_000_000,
        disk_free_bytes: 3_000_000,
        disk_total_bytes: 10_000_000,
    });
    sink.flush().await;

    // stats（/admin/stats 视角）
    let stats = sink.stats(1).await;
    assert_eq!(stats.api_calls.calls, 2);
    assert_eq!(stats.api_calls.error_count, 1);
    assert_eq!(stats.retrieval.count, 1);
    assert_eq!(stats.tokens.len(), 1);
    assert_eq!(stats.tokens[0].total_tokens, 150);
    assert!(stats.resource.is_some());

    // 报表 reader：api_calls / tokens / retrieval / storage
    let reader = oce_infra::pg::reports::PgReportsReader::new(pool.clone(), None, None, 8);
    let api = reader.api_calls(1, "hour").await.unwrap();
    assert_eq!(api.buckets.len(), 1);
    assert_eq!(api.buckets[0].count, 2);
    assert_eq!(api.buckets[0].error_count, 1);
    let tokens_report = reader.tokens(1, "hour").await.unwrap();
    assert_eq!(tokens_report.tokens_total, 150);
    assert_eq!(tokens_report.models.len(), 1);
    let retrieval_report = reader.retrieval(1, "hour").await.unwrap();
    assert_eq!(retrieval_report.buckets.len(), 1);
    assert_eq!(retrieval_report.buckets[0].count, 1);
    assert!(!retrieval_report.stages.is_empty(), "dense/rerank 等阶段应出现");
    let slow = reader.slow_queries(1, 10).await.unwrap();
    assert_eq!(slow.len(), 1);
    assert_eq!(slow[0].total_ms, 350);
    let empty = reader.empty_queries(1, 10).await.unwrap();
    assert!(empty.is_empty(), "hit_count=8 不在 empty 列表");
    let storage = reader.storage().await.unwrap();
    assert_eq!(storage.dialect, "postgres");
    assert!(!storage.tables.is_empty());
    let inventory = reader.index_inventory().await.unwrap();
    assert_eq!(inventory.blob_total, 0);

    // cleanup：清窗口外数据
    sqlx::query("UPDATE api_call_metrics SET ts = now() - interval '48 hours'")
        .execute(&pool)
        .await
        .unwrap();
    sink.cleanup(1).await;
    let stats = sink.stats(1).await;
    assert_eq!(stats.api_calls.calls, 0, "48h 前的记录应被清理");

    teardown(&pool, &schema).await;
}
