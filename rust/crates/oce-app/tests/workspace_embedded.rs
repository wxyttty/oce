//! 嵌入式工作区索引器测试：扫描/忽略/增量同步/删除/检索 全周期。

use oce_app::workspace::WorkspaceIndexer;
use oce_infra::settings::Settings;
use std::sync::Arc;

fn temp_ws(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oce-ws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings_for(ws: &std::path::Path) -> Settings {
    let mut s = Settings::from_env();
    s.database.url = format!("sqlite:///{}", ws.join(".oce/oce.db").display());
    s.trivium.path = ws.join(".oce/oce.tdb").to_string_lossy().into_owned();
    s.trivium.dense_dim = 8;
    s.trivium.sync_mode = "off".into();
    s.trivium.auto_build_quiver = false;
    s.embedding.dimensions = 8;
    s.retrieval.inner.intent_classification_enabled = false;
    s.retrieval.inner.query_decomposition_enabled = false;
    s.llm.rerank_enabled = false;
    s.retrieval.inner.query_rewrite_enabled = false;
    s
}

async fn make_indexer(ws: &std::path::Path) -> Arc<WorkspaceIndexer> {
    let container = oce_app::container::Container::build_with_embedder(
        settings_for(ws),
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await
    .unwrap();
    Arc::new(WorkspaceIndexer::new(
        ws.to_path_buf(),
        container.application.indexing.clone(),
        container.application.retrieval.clone(),
        container.application.blob_repo.clone(),
        container.db.clone(),
        container.application.trivium.clone(),
    ))
}

struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let chars: Vec<char> = text.chars().collect();
        for w in chars.windows(4) {
            let s: String = w.iter().collect();
            let h = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                s.hash(&mut h);
                h.finish()
            };
            v[(h as usize) % self.dim] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

#[async_trait::async_trait]
impl oce_core::search::Embedder for FakeEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> oce_core::error::OceResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }
    async fn embed_query(&self, text: &str) -> oce_core::error::OceResult<Vec<f32>> {
        Ok(self.embed(text))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_search_change_delete_cycle() {
    let ws = temp_ws("cycle");
    std::fs::write(ws.join("app.py"), "def hello_world():\n    return 1\n").unwrap();
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/order.rs"), "pub fn place_order() {}\n").unwrap();
    // 应被 .oceignore 排除
    std::fs::write(ws.join(".oceignore"), "generated/\n").unwrap();
    std::fs::create_dir_all(ws.join("generated")).unwrap();
    std::fs::write(ws.join("generated/gen.rs"), "pub fn generated_fn() {}\n").unwrap();

    let indexer = make_indexer(&ws).await;

    // 首次 search 触发全量同步并命中
    let out = indexer.search("hello_world", None).await.unwrap();
    assert!(out.formatted.contains("app.py"), "got: {}", out.formatted);
    assert_eq!(out.synced_files, 2); // app.py + sub/order.rs；generated 被 ignore

    // status
    let status = indexer.status().await.unwrap();
    assert_eq!(status.tracked_files, 2);
    assert_eq!(status.indexed_blobs, 2);

    // 修改文件：mtime 变化 → 增量同步感知
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(ws.join("app.py"), "def hello_world():\n    return 42\n").unwrap();
    let out = indexer.search("return 42", None).await.unwrap();
    assert!(out.formatted.contains("return 42"), "modified content should be re-indexed: {}", out.formatted);

    // 删除文件：下次同步移除
    std::fs::remove_file(ws.join("sub/order.rs")).unwrap();
    let status = indexer.status().await.unwrap();
    assert_eq!(status.tracked_files, 1);
    assert_eq!(status.indexed_blobs, 1);

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test(flavor = "multi_thread")]
async fn reindex_rebuilds_from_scratch() {
    let ws = temp_ws("reindex");
    std::fs::write(ws.join("x.py"), "value = unique_marker_xyz\n").unwrap();
    let indexer = make_indexer(&ws).await;
    indexer.search("unique_marker_xyz", None).await.unwrap();
    let status_before = indexer.status().await.unwrap();
    assert_eq!(status_before.indexed_blobs, 1);

    let report = indexer.reindex().await.unwrap();
    assert_eq!(report.uploaded, 1);
    let status = indexer.status().await.unwrap();
    assert_eq!(status.indexed_blobs, 1);
    assert!(status.vector_nodes > 0);

    let _ = std::fs::remove_dir_all(&ws);
}
