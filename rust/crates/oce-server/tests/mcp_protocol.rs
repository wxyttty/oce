//! MCP stdio 协议测试：initialize / tools/list / tools/call / 错误语义。

use oce_app::container::Container;
use oce_app::workspace::WorkspaceIndexer;
use oce_infra::settings::Settings;
use serde_json::{json, Value};
use std::sync::Arc;

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

async fn indexer_for(tag: &str) -> Arc<WorkspaceIndexer> {
    let dir = std::env::temp_dir().join(format!("oce-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.py"), "def greet_user():\n    return 'hi'\n").unwrap();

    let mut settings = Settings::from_env();
    settings.database.url = format!("sqlite:///{}", dir.join(".oce/oce.db").display());
    settings.trivium.path = dir.join(".oce/oce.tdb").to_string_lossy().into_owned();
    settings.trivium.dense_dim = 8;
    settings.trivium.sync_mode = "off".into();
    settings.trivium.auto_build_quiver = false;
    settings.embedding.dimensions = 8;
    settings.retrieval.inner.intent_classification_enabled = false;
    settings.retrieval.inner.query_decomposition_enabled = false;
    settings.llm.rerank_enabled = false;
    settings.retrieval.inner.query_rewrite_enabled = false;
    let container = Container::build_with_embedder(settings, Some(Arc::new(FakeEmbedder { dim: 8 })))
        .await
        .unwrap();
    Arc::new(WorkspaceIndexer::new(
        dir,
        container.application.indexing.clone(),
        container.application.retrieval.clone(),
        container.application.blob_repo.clone(),
        container.db.clone(),
        container.application.trivium.clone(),
    ))
}

/// tools/call 结果里的文本载荷解析回 JSON。
fn content_text(response: &Value) -> Value {
    let text = response
        .pointer("/result/content/0/text")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_and_tools_list() {
    let state: Option<Arc<WorkspaceIndexer>> = None;
    let init = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
    )
    .await
    .unwrap();
    assert_eq!(init.pointer("/result/serverInfo/name"), Some(&json!("oce")));
    assert_eq!(
        init.pointer("/result/protocolVersion"),
        Some(&json!("2024-11-05"))
    );

    // notification 无响应
    let none = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert!(none.is_none());

    let tools = oce_server::mcp::handle_message(&state, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .await
        .unwrap();
    let names: Vec<&str> = tools
        .pointer("/result/tools")
        .and_then(|t| t.as_array())
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["oce_search", "oce_status", "oce_reindex"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_is_jsonrpc_error() {
    let state: Option<Arc<WorkspaceIndexer>> = None;
    let resp = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","id":9,"method":"resources/list"}),
    )
    .await
    .unwrap();
    assert_eq!(resp.pointer("/error/code"), Some(&json!(-32601)));
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_search_roundtrip() {
    let indexer = indexer_for("call").await;
    let state = Some(indexer.clone());

    // 首次 search 触发全量同步并命中
    let resp = oce_server::mcp::handle_message(
        &state,
        &json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"oce_search","arguments":{"query":"greet_user"}}
        }),
    )
    .await
    .unwrap();
    let payload = content_text(&resp);
    assert!(payload["hit_count"].as_u64().unwrap() >= 1, "payload={payload}");
    assert!(payload["formatted_retrieval"]
        .as_str()
        .unwrap()
        .contains("app.py"));

    // oce_status
    let resp = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"oce_status","arguments":{}}}),
    )
    .await
    .unwrap();
    let payload = content_text(&resp);
    assert_eq!(payload["indexed_blobs"].as_u64(), Some(1));

    // 缺 query → isError 结果
    let resp = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"oce_search","arguments":{}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp.pointer("/result/isError"), Some(&json!(true)));

    // 未知工具 → isError 结果
    let resp = oce_server::mcp::handle_message(
        &state,
        &json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp.pointer("/result/isError"), Some(&json!(true)));
}
