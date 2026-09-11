//! API 契约测试：路由路径、鉴权、错误语义、响应字段与 Python 版一致。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use oce_app::container::Container;
use oce_infra::settings::Settings;
use serde_json::{json, Value};
use tower::ServiceExt;

/// 确定性嵌入器（同 oce-app e2e 测试）。
struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
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

async fn app(tag: &str) -> Router {
    let dir = std::env::temp_dir().join(format!("oce-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut settings = Settings::from_env();
    settings.database.url = format!("sqlite:///{}", dir.join("t.db").display());
    settings.trivium.path = dir.join("t.tdb").to_string_lossy().into_owned();
    settings.trivium.dense_dim = 8;
    settings.trivium.sync_mode = "off".into();
    settings.trivium.auto_build_quiver = false;
    settings.embedding.dimensions = 8;
    settings.embedding.api_key = Some("test-key".into());
    settings.retrieval.inner.intent_classification_enabled = false;
    settings.retrieval.inner.query_decomposition_enabled = false;
    settings.llm.rerank_enabled = false;
    settings.retrieval.inner.query_rewrite_enabled = false;
    let container = Container::build_with_embedder(settings, Some(std::sync::Arc::new(FakeEmbedder { dim: 8 })))
        .await
        .unwrap();
    let state = AppState {
        application: container.application.clone(),
        container,
    };
    make_router(state)
}

/// 通过 axum oneshot 调用（server crate 内嵌模块复用路由构建）。
async fn call(app: Router, method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match key {
        Some(k) => builder.header("authorization", format!("Bearer {k}")),
        None => builder,
    };
    let builder = if body.is_some() {
        builder.header("content-type", "application/json")
    } else {
        builder
    };
    let req = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

/// 路由构建复用入口（避免测试重复定义路由）。
use oce_server::routes::{router as make_router, AppState};

const CONTENT: &str = "pub fn parse_config() -> u32 {\n    42\n}\n";

#[tokio::test(flavor = "multi_thread")]
async fn health_and_version_public() {
    let router = app("meta").await;
    let (status, body) = call(router, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let router = app("meta2").await;
    let (status, body) = call(router, "GET", "/version", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "oce");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_required_with_fastapi_error_shape() {
    let router = app("auth").await;
    // 无 key → 401 + OpenAI 风格 error 体
    let (status, body) = call(router.clone(), "POST", "/find-missing", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    // 错误 key → 401
    let (status, _) = call(router.clone(), "POST", "/find-missing", Some("wrong"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 正确 key（默认 sk-opencontextengine）→ 200
    let (status, body) = call(router, "POST", "/find-missing", Some("sk-opencontextengine"), Some(json!({"mem_object_names": ["x"]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["unknown_memory_names"].is_array());
    assert!(body["nonindexed_blob_names"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_upload_and_retrieval_contract() {
    let router = app("data").await;
    let key = "sk-opencontextengine";

    // upload
    let (status, body) = call(
        router.clone(),
        "POST",
        "/batch-upload",
        Some(key),
        Some(json!({"blobs": [{"path": "src/lib.rs", "content": CONTENT}], "checkpoint_id": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(body["blob_names"].as_array().unwrap().len() == 1);
    let blob_name = body["blob_names"][0].as_str().unwrap().to_string();

    // checkpoint
    let (status, body) = call(
        router.clone(),
        "POST",
        "/checkpoint-blobs",
        Some(key),
        Some(json!({"blobs": {"added_blobs": [blob_name]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let checkpoint_id = body["new_checkpoint_id"].as_str().unwrap().to_string();

    // retrieval
    let (status, body) = call(
        router.clone(),
        "POST",
        "/agents/codebase-retrieval",
        Some(key),
        Some(json!({
            "information_request": "parse_config 定义",
            "blobs": {"checkpoint_id": checkpoint_id},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(body["formatted_retrieval"].as_str().unwrap().contains("The following code sections were retrieved:"));
    assert!(body["codebase_retrieval_elapsed_ms"].is_i64());

    // retrieval 无 scope → 400 SCOPE_REQUIRED
    let (status, body) = call(
        router.clone(),
        "POST",
        "/agents/codebase-retrieval",
        Some(key),
        Some(json!({"information_request": "anything"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["detail"].as_str().unwrap().contains("工作集"));

    // 非法 checkpoint token → 400
    let (status, _) = call(
        router.clone(),
        "POST",
        "/checkpoint-blobs",
        Some(key),
        Some(json!({"blobs": {"checkpoint_id": "bad-token"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // blob-status
    let (status, body) = call(
        router,
        "POST",
        "/agents/blob-status",
        Some(key),
        Some(json!({"blobs": {"added_blobs": ["missing"], "checkpoint_id": checkpoint_id}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unknown_blob_names"][0], "missing");
    assert_eq!(body["checkpoint_not_found"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_endpoints_auth_and_crud() {
    let router = app("admin").await;
    let admin_key = "sk-opencontextengine"; // ADMIN_API_KEY 空回落 API_KEY

    // 数据面 key 不等于 admin 语义分离——此处同一 key（回落），应放行
    let (status, body) = call(router.clone(), "GET", "/admin/credentials", Some(admin_key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["credentials"].is_array());

    // 创建凭据
    let (status, body) = call(
        router.clone(),
        "POST",
        "/admin/credentials",
        Some(admin_key),
        Some(json!({
            "kind": "embed",
            "name": "main",
            "api_key": "sk-test-123456",
            "endpoint": "https://api.example.com/v1/embeddings",
            "model": "test-model",
            "dimensions": 8,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert_eq!(body["api_key_last4"], "3456"); // 脱敏：只暴露尾 4 位
    let cred_id = body["id"].as_i64().unwrap();

    // 重复创建同 kind+model+key → 409
    let (status, body) = call(
        router.clone(),
        "POST",
        "/admin/credentials",
        Some(admin_key),
        Some(json!({
            "kind": "embed",
            "name": "dup",
            "api_key": "sk-test-123456",
            "endpoint": "https://api.example.com/v1/embeddings",
            "model": "test-model",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body={body}");

    // 更新
    let (status, body) = call(
        router.clone(),
        "PATCH",
        &format!("/admin/credentials/{cred_id}"),
        Some(admin_key),
        Some(json!({"priority": 50})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["priority"], 50);

    // duplicate
    let (status, body) = call(
        router.clone(),
        "POST",
        &format!("/admin/credentials/{cred_id}/duplicate"),
        Some(admin_key),
        Some(json!({"kind": "rerank", "model": "rerank-model"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert_eq!(body["kind"], "rerank");

    // 删除
    let (status, _) = call(router.clone(), "DELETE", &format!("/admin/credentials/{cred_id}"), Some(admin_key), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // GC
    let (status, body) = call(router.clone(), "POST", "/admin/gc", Some(admin_key), Some(json!({"ttl_days": 30, "dry_run": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dry_run"], true);

    // stats
    let (status, body) = call(router, "GET", "/admin/stats?window_hours=24", Some(admin_key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["window_hours"], 24);
}
