//! pgvector 向量引擎集成测试（`#[ignore]` 门控，需 pgvector 扩展的 PG 实例）。
//!
//! 运行方式：
//! ```sh
//! OCE_PG_URL="postgres://oce:oce@localhost:25432/oce" \
//!   cargo test -p oce-infra --test pgvector_integration -- --ignored --nocapture
//! ```
//! 独立 schema 隔离；pgvector 的 vector 列是扩展类型，schema 内建表即可。

use oce_core::search::{
    PathDoc, PathSearchStore, SearchStore, VectorEngine, VectorIndex, VectorStatsSource,
    VectorUpsert,
};
use oce_infra::pgvector::PgVectorStore;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

fn pg_url() -> Option<String> {
    std::env::var("OCE_PG_URL").ok().filter(|s| !s.is_empty())
}

async fn setup(tag: &str) -> Option<(PgPool, PgVectorStore, String)> {
    let url = pg_url()?;
    let schema = format!("oce_test_vec_{}_{}", tag, std::process::id());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .after_connect({
            let schema = schema.clone();
            move |conn, _meta| {
                let schema = schema.clone();
                Box::pin(async move {
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
    // pgvector 扩展装在 public（库级）；表建在测试 schema
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(&pool)
        .await
        .expect("create vector extension");
    let store = PgVectorStore::open(pool.clone(), "test-model dim=8 etext=v1".into())
        .await
        .expect("open pgvector store");
    Some((pool, store, schema))
}

async fn teardown(pool: &PgPool, schema: &str) {
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(pool)
        .await;
    pool.close().await;
}

fn fake_vector(seed: u64, dim: usize) -> Vec<f32> {
    // 确定性伪随机 + 归一化（余弦相似度语义）
    let v: Vec<f32> = (0..dim)
        .map(|i| {
            ((seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(i as u64 * 1442695040888963407))
                >> 33) as f32
                / u32::MAX as f32
                - 0.5
        })
        .collect();
    let norm = v.iter().map(|f| f * f).sum::<f32>().sqrt().max(1e-9);
    v.into_iter().map(|f| f / norm).collect()
}

fn upsert_item(i: usize, blob: &str) -> VectorUpsert {
    VectorUpsert {
        chunk_id: format!("chunk-{i}"),
        content_hash: format!("{i:064x}"),
        blob_name: blob.into(),
        content: format!("fn impl_{i}() {{ /* fixture {i} */ }}"),
        vector: fake_vector(i as u64 + 1, 8),
        path: format!("src/mod_{i}.rs"),
        start_line: 1,
        end_line: 10,
    }
}

#[tokio::test]
#[ignore]
async fn upsert_search_delete_roundtrip() {
    let Some((pool, store, schema)) = setup("roundtrip").await else { return };

    // upsert 幂等：同 chunk_id 二次写入覆盖
    let n = store
        .upsert(vec![upsert_item(1, "b1"), upsert_item(2, "b1"), upsert_item(3, "b2")])
        .await
        .unwrap();
    assert_eq!(n, 3);
    store.upsert(vec![upsert_item(1, "b1")]).await.unwrap();
    assert_eq!(store.node_count(), 3, "同 chunk_id upsert 应覆盖不累加");

    // search：无 scope，按余弦相似度排序
    let hits = store
        .search("q", &fake_vector(2, 8), None, 5, 0.0)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].blob_name, "b1");
    assert!(hits[0].score > 0.9, "同种子向量应高相似度: {}", hits[0].score);

    // scope 过滤：只查 b2
    let hits = store
        .search(
            "q",
            &fake_vector(2, 8),
            Some(&["b2".to_string()]),
            5,
            0.0,
        )
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.blob_name == "b2"));

    // 阈值后过滤
    let hits = store
        .search("q", &fake_vector(2, 8), None, 5, 0.999)
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.score >= 0.999));

    // delete by blob
    store.delete(&["b1".to_string()]).await.unwrap();
    assert_eq!(store.node_count(), 1);
    let hits = store
        .search("q", &fake_vector(2, 8), None, 5, 0.0)
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.blob_name == "b2"));

    teardown(&pool, &schema).await;
}

#[tokio::test]
#[ignore]
async fn path_index_roundtrip() {
    let Some((pool, store, schema)) = setup("paths").await else { return };

    let docs = vec![
        PathDoc {
            path_id: "src/service/user.rs".into(),
            blob_name: "b1".into(),
            path: "src/service/user.rs".into(),
            path_document: "user service module".into(),
            path_vector: fake_vector(1, 8),
        },
        PathDoc {
            path_id: "src/lib.rs".into(),
            blob_name: "b2".into(),
            path: "src/lib.rs".into(),
            path_document: "library root".into(),
            path_vector: fake_vector(2, 8),
        },
    ];
    let n = store.insert(docs).await.unwrap();
    assert_eq!(n, 2);

    // 路径检索：同种子向量最相似
    let results = store
        .search_paths("q", &fake_vector(1, 8), None, 5)
        .await
        .unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].path, "src/service/user.rs");

    // scope 过滤
    let results = store
        .search_paths("q", &fake_vector(1, 8), Some(&["b2".to_string()]), 5)
        .await
        .unwrap();
    assert!(results.iter().all(|r| r.blob_name == "b2"));

    // delete_by_blob_names
    store.delete_by_blob_names(&["b1".to_string()]).await.unwrap();
    let results = store
        .search_paths("q", &fake_vector(1, 8), None, 5)
        .await
        .unwrap();
    assert!(results.iter().all(|r| r.blob_name == "b2"));

    teardown(&pool, &schema).await;
}

#[tokio::test]
#[ignore]
async fn model_fingerprint_fail_closed() {
    let url = pg_url();
    if url.is_none() {
        return;
    }
    let schema = format!("oce_test_vec_fp_{}", std::process::id());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect({
            let schema = schema.clone();
            move |conn, _meta| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}, public"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url.unwrap())
        .await
        .unwrap();
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(&pool)
        .await
        .unwrap();

    // 首次打开：写入指纹
    let store = PgVectorStore::open(pool.clone(), "model-a dim=8 etext=v1".into())
        .await
        .expect("first open");
    store
        .upsert(vec![upsert_item(1, "b1")])
        .await
        .unwrap();
    drop(store);

    // 同指纹：正常打开
    PgVectorStore::open(pool.clone(), "model-a dim=8 etext=v1".into())
        .await
        .expect("same fingerprint");

    // 换模型：fail-closed
    let err = PgVectorStore::open(pool.clone(), "model-b dim=8 etext=v1".into()).await;
    match err {
        Err(msg) => assert!(
            msg.contains("指纹不匹配"),
            "错误信息应说明指纹不匹配: {msg}"
        ),
        Ok(_) => panic!("换模型必须拒绝（fail-closed）"),
    }

    teardown(&pool, &schema).await;
}
