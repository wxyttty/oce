//! TriviumDB 存储层集成测试：节点模型 + payload 过滤 + upsert 幂等 + 删除。

use oce_core::search::{PathDoc, PathSearchStore, SearchStore, VectorIndex, VectorUpsert};
use oce_infra::settings::TriviumSettings;
use oce_infra::trivium::TriviumStore;

fn temp_tdb(tag: &str) -> TriviumSettings {
    let dir = std::env::temp_dir().join(format!("oce-trivium-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TriviumSettings {
        fts_lexical: "off".into(),
        path: dir.join("test.tdb").to_string_lossy().into_owned(),
        sync_mode: "off".into(),
        storage_mode: "rom".into(),
        dense_dim: 8,
        auto_build_quiver: false,
        text_hybrid: false,
                text_boost: 0.8,
        expand_depth: 0,
    }
}

fn fake_vector(seed: u64, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|i| {
            ((seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(i as u64 * 1442695040888963407))
                >> 33) as f32
                / u32::MAX as f32
                - 0.5
        })
        .collect()
}

fn upsert(blob: &str, hash: &str, seed: u64, content: &str) -> VectorUpsert {
    VectorUpsert {
        chunk_id: format!("{blob}-{hash}-1-2"),
        content_hash: hash.to_string(),
        blob_name: blob.to_string(),
        content: content.to_string(),
        vector: fake_vector(seed, 8),
        path: format!("src/{blob}.rs"),
        start_line: 1,
        end_line: 2,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_search_delete_roundtrip() {
    let settings = temp_tdb("roundtrip");
    let store = TriviumStore::open(settings.clone()).unwrap();

    store
        .upsert(vec![
            upsert("blob-a", "hash1", 1, "fn parse_config() {}"),
            upsert("blob-a", "hash2", 2, "fn run_worker() {}"),
            upsert("blob-b", "hash3", 3, "class Widget {}"),
        ])
        .await
        .unwrap();
    assert_eq!(store.node_count(), 3);

    // 幂等 upsert：同 chunk_id 重放不新增节点
    store
        .upsert(vec![upsert("blob-a", "hash1", 1, "fn parse_config() {}")])
        .await
        .unwrap();
    assert_eq!(store.node_count(), 3);

    // blob 过滤检索
    let qv = fake_vector(1, 8);
    let hits = store
        .search("query", &qv, Some(&["blob-a".to_string()]), 10, 0.0)
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.blob_name == "blob-a"));
    // 命中载荷逐字回读
    let top = &hits[0];
    assert!(!top.content.is_empty());
    assert_eq!(top.path, "src/blob-a.rs");
    assert_eq!(top.start_line, 1);
    assert_eq!(top.end_line, 2);

    // 无过滤检索覆盖全部 blob
    let all = store.search("query", &qv, None, 10, 0.0).await.unwrap();
    assert_eq!(all.len(), 3);

    // 按 blob 删除
    store.delete(&["blob-a".to_string()]).await.unwrap();
    assert_eq!(store.node_count(), 1);
    let rest = store.search("query", &qv, None, 10, 0.0).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].blob_name, "blob-b");

    // 重开文件：数据持久化
    drop(store);
    let reopened = TriviumStore::open(settings).unwrap();
    assert_eq!(reopened.node_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn path_index_roundtrip() {
    let settings = temp_tdb("path");
    let store = TriviumStore::open(settings).unwrap();

    store
        .insert(vec![
            PathDoc {
                path_id: "path_blob-a".into(),
                blob_name: "blob-a".into(),
                path: "src/token_refresh.rs".into(),
                path_document: "src/token_refresh.rs token_refresh.rs token_refresh".into(),
                path_vector: fake_vector(10, 8),
            },
            PathDoc {
                path_id: "path_blob-b".into(),
                blob_name: "blob-b".into(),
                path: "docs/guide.md".into(),
                path_document: "docs/guide.md guide.md guide".into(),
                path_vector: fake_vector(20, 8),
            },
        ])
        .await
        .unwrap();

    let results = store
        .search_paths(
            "query",
            &fake_vector(10, 8),
            Some(&["blob-a".to_string()]),
            20,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].blob_name, "blob-a");
    assert!(results[0].score > 0.9);

    store
        .delete_by_blob_names(&["blob-a".to_string()])
        .await
        .unwrap();
    let rest = store
        .search_paths("query", &fake_vector(10, 8), None, 20)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].path, "docs/guide.md");
}

#[tokio::test(flavor = "multi_thread")]
async fn hybrid_text_recall_surfaces_lexical_match() {
    // 场景：中文内容块 A 的向量与查询不相似，但查询文本与 A 的内容词法匹配；
    // BM25 混合检索应把 A 拉回结果（纯向量检索会漏掉它）。
    let mut settings = temp_tdb("hybrid");
    settings.text_hybrid = true;
    let store = TriviumStore::open(settings.clone()).unwrap();

    let v_query = fake_vector(1, 8);
    let v_unrelated = fake_vector(999, 8); // 与查询向量无相似
    store
        .upsert(vec![
            VectorUpsert {
                chunk_id: "chunk-a".into(),
                content_hash: "hash-a".into(),
                blob_name: "blob-a".into(),
                content: "期号计算与期数生成逻辑 三明代发调拨单打印期号显示".into(),
                vector: v_unrelated.clone(),
                path: "src/a.rs".into(),
                start_line: 1,
                end_line: 2,
            },
            VectorUpsert {
                chunk_id: "chunk-b".into(),
                content_hash: "hash-b".into(),
                blob_name: "blob-b".into(),
                content: "plain english config text".into(),
                vector: v_query.clone(), // 向量上完全匹配查询
                path: "src/b.rs".into(),
                start_line: 1,
                end_line: 2,
            },
        ])
        .await
        .unwrap();

    // 纯向量检索（text_hybrid 关闭的同构库）：只有 B
    {
        let mut s_plain = settings.clone();
        s_plain.text_hybrid = false;
        s_plain.path = settings.path.replace("hybrid", "hybrid-plain");
        let plain = TriviumStore::open(s_plain).unwrap();
        plain
            .upsert(vec![VectorUpsert {
                chunk_id: "chunk-a".into(),
                content_hash: "hash-a".into(),
                blob_name: "blob-a".into(),
                content: "期号计算与期数生成逻辑 三明代发调拨单打印期号显示".into(),
                vector: v_unrelated.clone(),
                path: "src/a.rs".into(),
                start_line: 1,
                end_line: 2,
            }])
            .await
            .unwrap();
    }

    // 混合检索：查询文本「期号计算」应把 A 拉进结果
    let hits = store
        .search("期号计算", &v_query, None, 10, -1.0)
        .await
        .unwrap();
    let contents: Vec<&str> = hits.iter().map(|h| h.content.as_str()).collect();
    assert!(
        contents.iter().any(|c| c.contains("期号")),
        "hybrid search should surface lexical match, got: {contents:?}"
    );
}
