//! 应用层端到端测试：batch_upload → checkpoint → retrieve 全链路（确定性假嵌入器）。

use oce_app::service::{compute_blob_name, BlobUpload};
use oce_core::search::Embedder;
use oce_infra::settings::Settings;
use std::sync::Arc;

/// 确定性嵌入器：字符 4-gram 哈希 → 8 维向量（无网络调用）。
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
impl Embedder for FakeEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> oce_core::error::OceResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }

    async fn embed_query(&self, text: &str) -> oce_core::error::OceResult<Vec<f32>> {
        Ok(self.embed(text))
    }
}

fn test_settings(data_dir: &std::path::Path) -> Settings {
    let mut s = Settings::from_env();
    s.database.url = format!("sqlite:///{}", data_dir.join("test.db").display());
    s.trivium.path = data_dir.join("test.tdb").to_string_lossy().into_owned();
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

async fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oce-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const RUST_SRC: &str = r#"//! Token refresh module.
pub struct TokenRefresher {
    client: String,
}

impl TokenRefresher {
    /// Refresh the access token using the stored refresh token.
    pub async fn refresh_token(&self) -> Result<String, String> {
        let response = self.client.post("/oauth/token").await?;
        Ok(response)
    }
}

pub fn validate_expiry(expires_at: u64) -> bool {
    expires_at > now_seconds()
}

fn now_seconds() -> u64 {
    0
}
"#;

const DOCS: &str = "# Configuration\n\nAll settings live in `config.json`.\nThe refresh interval is controlled by `refresh_interval`.\n\n# Advanced\n\nSee also the token refresh module for programmatic access.\n";

#[tokio::test(flavor = "multi_thread")]
async fn upload_checkpoint_retrieve_e2e() {
    let dir = temp_dir("full").await;
    let settings = test_settings(&dir);
    let container = oce_app::container::Container::build_with_embedder(
        settings,
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await
    .unwrap();
    let app = &container.application;

    // 1. batch_upload（同步索引路径）
    let uploads = vec![
        BlobUpload {
            path: "src/token_refresh.rs".into(),
            content: RUST_SRC.into(),
        },
        BlobUpload {
            path: "docs/config.md".into(),
            content: DOCS.into(),
        },
    ];
    let result = app.batch_upload(uploads.clone(), None).await.unwrap();
    assert_eq!(result.blob_names.len(), 2);
    assert_eq!(
        result.blob_names[0],
        compute_blob_name("src/token_refresh.rs", RUST_SRC)
    );

    // 2. checkpoint：建链
    let checkpoint = app
        .checkpoint(None, &result.blob_names, &[])
        .await
        .unwrap();
    assert!(checkpoint.new_checkpoint_id.contains(':'));

    // 3. find_missing：全部 ready → 无 unknown / 无 nonindexed
    let (unknown, nonindexed) = app
        .find_missing(result.blob_names.clone())
        .await
        .unwrap();
    assert!(unknown.is_empty());
    assert!(nonindexed.is_empty());

    // 4. 检索：refresh_token 符号定位
    let retrieval = app
        .retrieve(
            "refresh_token 函数在哪里定义",
            Some(&checkpoint.new_checkpoint_id),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        retrieval.formatted_retrieval.contains("Path: src/token_refresh.rs"),
        "expected rust source hit, got: {}",
        retrieval.formatted_retrieval
    );
    assert!(retrieval.elapsed_ms >= 0);

    // 5. 未声明工作集 → ScopeRequired
    let err = app.retrieve("anything", None, &[], &[]).await.unwrap_err();
    assert_eq!(err.code, "SCOPE_REQUIRED");

    // 6. 二次上传同内容：幂等（内容寻址）
    let again = app
        .batch_upload(
            vec![BlobUpload {
                path: "src/token_refresh.rs".into(),
                content: RUST_SRC.into(),
            }],
            None,
        )
        .await
        .unwrap();
    assert_eq!(again.blob_names.len(), 1);

    // 7. checkpoint 增量：add + delete
    let checkpoint2 = app
        .checkpoint(
            Some(&checkpoint.new_checkpoint_id),
            &["new-blob".to_string()],
            &[result.blob_names[1].clone()],
        )
        .await
        .unwrap();
    assert_ne!(checkpoint2.new_checkpoint_id, checkpoint.new_checkpoint_id);

    // 8. blob-status
    let (unknown, _nonindexed, checkpoint_not_found) = app
        .blob_status(
            vec!["nonexistent-blob".to_string()],
            Some(&checkpoint2.new_checkpoint_id),
        )
        .await
        .unwrap();
    assert_eq!(unknown, vec!["nonexistent-blob".to_string()]);
    assert!(!checkpoint_not_found);

    // 9. GC dry run
    let gc = app.run_gc(30, true, 100).await.unwrap();
    assert!(gc.dry_run);

    // 清理
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_semantics_edge_cases() {
    let dir = temp_dir("scope").await;
    let settings = test_settings(&dir);
    let container = oce_app::container::Container::build_with_embedder(
        settings,
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await
    .unwrap();
    let app = &container.application;

    // 上传一个 blob 并建链
    let uploads = vec![BlobUpload {
        path: "src/lib.rs".into(),
        content: "pub fn alpha() {}\n".into(),
    }];
    let result = app.batch_upload(uploads, None).await.unwrap();
    let checkpoint = app.checkpoint(None, &result.blob_names, &[]).await.unwrap();

    // 非法 token → INVALID_CHECKPOINT_TOKEN
    let err = app
        .retrieve("q", Some("bad-token"), &[], &[])
        .await
        .unwrap_err();
    assert_eq!(err.code, "INVALID_CHECKPOINT_TOKEN");

    // 合法 token 但链不存在 → NEEDS_RESET（避免范围静默变窄）
    let fake_chain = format!("550e8400-e29b-41d4-a716-446655440000:1");
    let err = app.retrieve("q", Some(&fake_chain), &[], &[]).await.unwrap_err();
    assert_eq!(err.code, "NEEDS_RESET");

    // batch-upload 带不存在的链 → NEEDS_RESET（checkpoint 只推进已有链）
    let err = app
        .batch_upload(
            vec![BlobUpload {
                path: "x.py".into(),
                content: "def beta():\n    pass\n".into(),
            }],
            Some(&fake_chain),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "NEEDS_RESET");

    // 空工作集是合法结果：checkpoint 存在但 added 全被 delete → 空结果而非错误
    let retrieval = app
        .retrieve(
            "alpha",
            Some(&checkpoint.new_checkpoint_id),
            &[],
            &[result.blob_names[0].clone()],
        )
        .await
        .unwrap();
    assert!(retrieval.hits.is_empty());

    // checkpoint 已存在但成员为空 + 无 added → 检索返回空（不报错）
    let empty_chain = app.checkpoint(None, &[], &[]).await.unwrap();
    let retrieval = app
        .retrieve("q", Some(&empty_chain.new_checkpoint_id), &[], &[])
        .await
        .unwrap();
    assert!(retrieval.hits.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn static_embed_provider_end_to_end() {
    let dir = temp_dir("static").await;
    let model_dir = oce_infra::static_embed::testing::write_synthetic_model(&dir.join("m2v"), 8);

    let mut settings = test_settings(&dir);
    settings.embedding.provider = "static".into();
    settings.embedding.static_model = Some(model_dir);
    // 不注入 FakeEmbedder：走真实 StaticEmbedder 路径
    let container = oce_app::container::Container::build_with_embedder(settings, None)
        .await
        .unwrap();
    assert_eq!(container.embed_provider, "static");
    assert_eq!(container.vector_dim, 8);

    let app = &container.application;
    let uploads = vec![BlobUpload {
        path: "src/token.rs".into(),
        content: "pub fn refresh_token() -> u32 {\n    1\n}\n".into(),
    }];
    let result = app.batch_upload(uploads, None).await.unwrap();
    assert_eq!(result.blob_names.len(), 1);
    assert_eq!(result.embedded_count, 1, "static provider must embed inline");

    let checkpoint = app.checkpoint(None, &result.blob_names, &[]).await.unwrap();
    let retrieval = app
        .retrieve(
            "refresh_token",
            Some(&checkpoint.new_checkpoint_id),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        retrieval.formatted_retrieval.contains("Path: src/token.rs"),
        "expected hit via static embeddings, got: {}",
        retrieval.formatted_retrieval
    );

    // openai 语义回归：EMBED_PROVIDER=openai 且无 key/凭据 → 首次嵌入时 ServiceNotReady
    let dir2 = temp_dir("static-openai").await;
    let mut settings2 = test_settings(&dir2);
    settings2.embedding.provider = "openai".into();
    settings2.embedding.api_key = None;
    let container2 = oce_app::container::Container::build_with_embedder(settings2, None)
        .await
        .unwrap();
    assert_eq!(container2.embed_provider, "openai");
    let err = container2
        .application
        .batch_upload(
            vec![BlobUpload {
                path: "x.py".into(),
                content: "def a():\n    pass\n".into(),
            }],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "SERVICE_NOT_READY");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}
