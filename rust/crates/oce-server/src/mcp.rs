//! MCP stdio 服务器：单进程嵌入式模式（对齐 semble_rs 的使用形态）。
//!
//! `oce mcp --workspace .` 一个进程内嵌全部引擎（SQLite + TriviumDB + 静态嵌入器），
//! 工作区扫描/忽略/增量索引内联完成。协议面：initialize / tools/list / tools/call / ping。
//! 日志一律走 stderr——stdout 只承载 JSON-RPC 消息。

use oce_app::workspace::WorkspaceIndexer;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

/// HOME 目录（与 main.rs 的 serve 默认数据目录同源）。
fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// 工具定义（tools/list 载荷）。
fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "oce_search",
                "description": "在当前工作区的语义代码索引中检索。先用中文业务描述或英文字符串命名提问均可；返回按相关性排序的代码片段（含路径与行号）。首次调用会自动扫描并索引整个工作区。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "检索请求（自然语言或代码标识符）"},
                        "top_k": {"type": "integer", "description": "可选：返回片段数上限"}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "oce_status",
                "description": "查看工作区索引状态：已跟踪文件数、已索引 blob 数、向量节点数。",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "oce_reindex",
                "description": "强制全量重建索引（修改 .gitignore/.oceignore 或更换嵌入模型后使用）。",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

/// 单个 JSON-RPC 请求 → 响应（notification 返回 None）。独立成函数便于测试。
pub async fn handle_message(state: &Option<Arc<WorkspaceIndexer>>, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let is_notification = id.is_none();

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": msg
                .pointer("/params/protocolVersion")
                .cloned()
                .unwrap_or(json!("2024-11-05")),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "oce",
                "version": env!("CARGO_PKG_VERSION"),
                "engine": "rust-embedded"
            }
        })),
        "notifications/initialized" | "notifications/cancelled" => {
            // notification：无需响应
            return None;
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tool_definitions()),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
            let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            call_tool(state, name, &args).await
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };

    if is_notification {
        return None;
    }
    let id = id.unwrap_or(Value::Null);
    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

async fn call_tool(
    state: &Option<Arc<WorkspaceIndexer>>,
    name: &str,
    args: &Value,
) -> Result<Value, (i64, String)> {
    let Some(indexer) = state else {
        return Err((-32002, "workspace not initialized".into()));
    };
    type ToolOutcome = Result<Value, String>;
    let outcome: ToolOutcome = match name {
        "oce_search" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string();
            if query.trim().is_empty() {
                Err("query is required".into())
            } else {
                match indexer.search(&query, None).await {
                    Ok(o) => Ok(json!({
                        "hit_count": o.hit_count,
                        "indexed_blobs": o.indexed_blobs,
                        "formatted_retrieval": o.formatted,
                    })),
                    Err(e) => Err(e.message),
                }
            }
        }
        "oce_status" => match indexer.status().await {
            Ok(s) => serde_json::to_value(s)
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.message),
        },
        "oce_reindex" => match indexer.reindex().await {
            Ok(r) => Ok(json!({
                "scanned": r.scanned,
                "uploaded": r.uploaded,
                "deleted": r.deleted,
                "elapsed_ms": r.elapsed_ms,
            })),
            Err(e) => Err(e.message),
        },
        other => Err(format!("unknown tool: {other}")),
    };
    match outcome {
        Ok(mut value) => {
            // MCP tools/call 结果必须是 content 数组
            if let Some(obj) = value.as_object_mut() {
                let text = serde_json::to_string_pretty(obj).unwrap_or_default();
                *obj = json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false
                })
                .as_object()
                .unwrap()
                .clone();
            }
            Ok(value)
        }
        Err(message) => {
            // 工具级错误：MCP 约定以 isError 结果返回，而非 JSON-RPC error
            Ok(json!({
                "content": [{"type": "text", "text": message}],
                "isError": true
            }))
        }
    }
}

/// MCP stdio 主循环：stdin 逐行读 JSON-RPC，stdout 写响应。
pub async fn run_stdio(workspace: PathBuf) -> Result<(), String> {
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;
    // 与 HTTP 服务共用同一数据目录（~/.oce/data 或 OCE_DATA_DIR）：
    // 嵌入式与 serve 只是接入方式不同，索引本体不应分裂成两套。
    // workspace_files 表按 path 主键隔离各工作区的登记，检索时用
    // 当前工作区的 blob 名单做 scope，不会串到其它项目的向量。
    let data_dir = std::env::var("OCE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_home().join(".oce").join("data")
        });

    // 与 serve 同源加载 data_dir/.env（嵌入模型/凭据等配置不应因接入方式不同而漂移）
    let env_path = data_dir.join(".env");
    if env_path.exists() {
        let _ = dotenvy::from_path(&env_path);
    }

    let mut settings = oce_infra::settings::Settings::from_env();
    settings.database.url = format!("sqlite:///{}", data_dir.join("oce.db").display());
    settings.trivium.path = data_dir.join("oce.tdb").to_string_lossy().into_owned();
    settings.worker.enabled = false;
    // MCP 模式无 HTTP 面：监控 sink 关闭（无 flush 任务写库）
    settings.monitoring.enabled = false;

    let container = oce_app::container::Container::build(settings).await?;
    let indexer = Arc::new(WorkspaceIndexer::new(
        workspace.clone(),
        container.application.indexing.clone(),
        container.application.retrieval.clone(),
        container.application.blob_repo.clone(),
        container.db.clone(),
        container.application.trivium.clone(),
    ));
    let state: Option<Arc<WorkspaceIndexer>> = Some(indexer);

    // 日志与一切非协议输出进 stderr
    eprintln!(
        "oce mcp: ready (workspace={}, data_dir={})",
        workspace.display(),
        data_dir.display()
    );

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = stdin.lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": format!("parse error: {e}")}})
                );
                let _ = stdout.flush();
                continue;
            }
        };
        if let Some(response) = handle_message(&state, &msg).await {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
    Ok(())
}
