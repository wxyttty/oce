//! axum 路由。ACE 兼容数据面（Bearer API key）+ Admin 运维面（独立 admin key）。
//! 错误语义与 FastAPI 版一致：401 带 OpenAI 风格 error 体；4xx/5xx 为 {"detail": ...}。

use crate::schemas::*;
use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use oce_app::service::{BlobUpload, RetrievalApplication};
use oce_core::error::OceError;
use oce_core::reports::{
    ApiCallsReport, IndexInventoryReport, ResourcesReport, RetrievalReport, StorageReport,
    TokensReport,
};
use serde_json::json;
use std::sync::Arc;

/// 共享应用状态。
#[derive(Clone)]
pub struct AppState {
    pub application: Arc<RetrievalApplication>,
    pub container: Arc<oce_app::container::Container>,
}

/// 鉴权错误 → 401（与 Python `_unauthorized` 一致）。
fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": null,
                "code": "invalid_api_key",
            }
        })),
    )
        .into_response()
}

/// Bearer key 提取 + 常量时间比较（对应 verify_api_key / verify_admin_key）。
fn extract_bearer(headers: &HeaderMap) -> Result<String, Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| unauthorized("You didn't provide an API key."))?;
    if !auth.starts_with("Bearer ") {
        return Err(unauthorized(
            "Invalid API key format. Expected 'Bearer <key>'",
        ));
    }
    Ok(auth["Bearer ".len()..].to_string())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub fn verify_api_key(headers: &HeaderMap, state: &AppState) -> Result<(), Response> {
    let key = extract_bearer(headers)?;
    if !constant_time_eq(&key, &state.container.settings.api_key) {
        return Err(unauthorized("Invalid API key provided"));
    }
    Ok(())
}

pub fn verify_admin_key(headers: &HeaderMap, state: &AppState) -> Result<(), Response> {
    let key = extract_bearer(headers)?;
    if !constant_time_eq(&key, state.container.settings.effective_admin_key()) {
        return Err(unauthorized("Invalid admin key provided"));
    }
    Ok(())
}

/// OceError → HTTP（与 Python router.py 的映射一致）。
fn error_response(exc: &OceError) -> Response {
    match exc.code.as_str() {
        "INVALID_CHECKPOINT_TOKEN" | "SCOPE_REQUIRED" => (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": exc.message})),
        )
            .into_response(),
        "NEEDS_RESET" => {
            (StatusCode::NOT_FOUND, Json(json!({"detail": exc.message}))).into_response()
        }
        "SERVICE_NOT_READY" => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Retry-After", "0")],
            Json(json!({"detail": exc.message})),
        )
            .into_response(),
        "CREDENTIAL_CONFLICT" => {
            (StatusCode::CONFLICT, Json(json!({"detail": exc.message}))).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": exc.message})),
        )
            .into_response(),
    }
}

type ApiResult<T> = Result<Json<T>, Response>;

pub fn router(state: AppState) -> Router {
    Router::new()
        // 数据面（ACE 兼容）
        .route("/find-missing", post(find_missing))
        .route("/batch-upload", post(batch_upload))
        .route("/agents/codebase-retrieval", post(codebase_retrieval))
        .route("/checkpoint-blobs", post(checkpoint_blobs))
        .route("/agents/blob-status", post(blob_status))
        // Admin 运维面
        .route(
            "/admin/credentials",
            get(admin_list_credentials).post(admin_create_credential),
        )
        .route(
            "/admin/credentials/{credential_id}",
            axum::routing::patch(admin_update_credential).delete(admin_delete_credential),
        )
        .route(
            "/admin/credentials/{credential_id}/duplicate",
            post(admin_duplicate_credential),
        )
        .route("/admin/credentials/reload", post(admin_reload_credentials))
        .route("/admin/queue", get(admin_queue_status))
        .route("/admin/queue/reset", post(admin_queue_reset))
        .route("/admin/queue/requeue-stale", post(admin_requeue_stale))
        .route("/admin/gc", post(admin_gc))
        .route("/admin/stats", get(admin_stats))
        // 报表（只读旁路）
        .route("/admin/reports/api-calls", get(report_api_calls))
        .route("/admin/reports/retrieval", get(report_retrieval))
        .route(
            "/admin/reports/retrieval/slow-queries",
            get(report_slow_queries),
        )
        .route(
            "/admin/reports/retrieval/empty-queries",
            get(report_empty_queries),
        )
        .route("/admin/reports/tokens", get(report_tokens))
        .route(
            "/admin/reports/index-inventory",
            get(report_index_inventory),
        )
        .route("/admin/reports/resources", get(report_resources))
        .route("/admin/reports/storage", get(report_storage))
        // Meta
        .route("/health", get(health))
        .route("/version", get(version))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api_call_metrics_middleware,
        ))
        .with_state(state)
        // FastAPI/uvicorn 对 JSON body 无默认上限；ace-client 单批内容 ≤2MB，
        // JSON 转义（\n→\\n 等）与协议包裹会放大体积，取 64MB 留足余量
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// HTTP 调用监控中间件（对应 Python ApiCallMetricsMiddleware）。
/// 旁路：采集失败只跳过；monitoring 关闭（sink 未装配）时直接放行；/health 豁免。
async fn api_call_metrics_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    const EXEMPT: [&str; 1] = ["/health"];
    let path = req.uri().path().to_string();
    if EXEMPT.contains(&path.as_str()) {
        return next.run(req).await;
    }
    let method = req.method().to_string();
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    if let Some(metrics) = &state.container.application.metrics {
        metrics.record_api_call(oce_core::metrics::ApiCallRecord {
            // 路由模板不可得时退化为请求路径（axum 0.8 无 route 模板暴露；
            // 个人模式端点少，路径即模板，无动态段）
            endpoint: path.chars().take(128).collect(),
            method,
            status_code: status,
            latency_ms: started.elapsed().as_millis() as u64,
            error_type: if status >= 500 {
                Some("http_error".into())
            } else {
                None
            },
        });
    }
    response
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn version(State(_state): State<AppState>) -> Json<serde_json::Value> {
    // 公开无需鉴权（与 Python 版一致），供客户端做兼容性检查
    Json(json!({
        "name": "oce",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": "rust",
    }))
}

// ── 数据面 ──

async fn find_missing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<FindMissingRequest>,
) -> ApiResult<FindMissingResponse> {
    verify_api_key(&headers, &state)?;
    let (unknown, nonindexed) = state
        .application
        .find_missing(req.mem_object_names)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(FindMissingResponse {
        unknown_memory_names: unknown,
        nonindexed_blob_names: nonindexed,
    }))
}

async fn batch_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchUploadRequest>,
) -> ApiResult<BatchUploadResponse> {
    verify_api_key(&headers, &state)?;
    // 内存硬限制：超限时拒绝新的上传/嵌入请求
    if let Err(e) = oce_infra::memory_guard::check() {
        tracing::warn!("batch_upload rejected: {e}");
        return Err(error_response(&OceError::new(e, "MemoryLimitExceeded")));
    }
    let uploads: Vec<BlobUpload> = req
        .blobs
        .into_iter()
        .map(|b| BlobUpload {
            path: b.path,
            content: b.content,
        })
        .collect();
    let checkpoint_id = if req.checkpoint_id.is_empty() {
        None
    } else {
        Some(req.checkpoint_id)
    };
    let result = state
        .application
        .batch_upload(uploads, checkpoint_id.as_deref())
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(BatchUploadResponse {
        blob_names: result.blob_names,
    }))
}

async fn codebase_retrieval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CodebaseRetrievalRequest>,
) -> ApiResult<CodebaseRetrievalResponse> {
    verify_api_key(&headers, &state)?;
    let payload = req.blobs;
    let result = state
        .application
        .retrieve(
            &req.information_request,
            Some(payload.checkpoint_id.as_str()).filter(|s| !s.is_empty()),
            &payload.added_blobs,
            &payload.deleted_blobs,
        )
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(CodebaseRetrievalResponse {
        formatted_retrieval: result.formatted_retrieval,
        codebase_retrieval_elapsed_ms: result.elapsed_ms,
    }))
}

async fn checkpoint_blobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckpointBlobsRequest>,
) -> ApiResult<CheckpointBlobsResponse> {
    verify_api_key(&headers, &state)?;
    let payload = req.blobs;
    let result = state
        .application
        .checkpoint(
            Some(payload.checkpoint_id.as_str()).filter(|s| !s.is_empty()),
            &payload.added_blobs,
            &payload.deleted_blobs,
        )
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(CheckpointBlobsResponse {
        new_checkpoint_id: result.new_checkpoint_id,
    }))
}

async fn blob_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BlobStatusRequest>,
) -> ApiResult<BlobStatusResponse> {
    verify_api_key(&headers, &state)?;
    let (unknown, nonindexed, checkpoint_not_found) = state
        .application
        .blob_status(
            req.blobs.added_blobs,
            Some(req.blobs.checkpoint_id.as_str()).filter(|s| !s.is_empty()),
        )
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(BlobStatusResponse {
        unknown_blob_names: unknown,
        nonindexed_blob_names: nonindexed,
        checkpoint_not_found,
    }))
}

// ── Admin 面 ──

fn credential_to_response(
    r: oce_core::credentials::CredentialRecord,
) -> CredentialResponse {
    CredentialResponse {
        id: r.id,
        kind: r.kind,
        provider: r.provider,
        name: r.name,
        status: r.status,
        priority: r.priority,
        endpoint: r.endpoint,
        model: r.model,
        timeout_seconds: r.timeout_seconds,
        rate_limit: r.rate_limit,
        note: r.note,
        dimensions: r.dimensions,
        max_batch_size: r.max_batch_size,
        max_batch_chars: r.max_batch_chars,
        max_input_chars: r.max_input_chars,
        input_overlap_chars: r.input_overlap_chars,
        top_n: r.top_n,
        min_score: r.min_score,
        tpm_limit: r.tpm_limit,
        max_candidates: r.max_candidates,
        output_top_k: r.output_top_k,
        snippet_chars: r.snippet_chars,
        num_rewrites: r.num_rewrites,
        api_key_last4: r.api_key_last4,
        last_used_at: r.last_used_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

async fn admin_list_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<CredentialListResponse> {
    verify_admin_key(&headers, &state)?;
    let records = state
        .container
        .credential_admin
        .list()
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(CredentialListResponse {
        credentials: records.into_iter().map(credential_to_response).collect(),
    }))
}

async fn admin_create_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CredentialCreateRequest>,
) -> Result<(StatusCode, Json<CredentialResponse>), Response> {
    verify_admin_key(&headers, &state)?;
    // pydantic 必填字段语义：缺 kind/name/api_key → 422
    if req.kind.as_deref().unwrap_or("").is_empty()
        || req.name.as_deref().unwrap_or("").is_empty()
        || req.api_key.as_deref().unwrap_or("").is_empty()
    {
        return Err(validation_error("kind, name and api_key are required"));
    }
    let record = state
        .container
        .credential_admin
        .create(req)
        .await
        .map_err(|e| error_response(&e))?;
    Ok((StatusCode::CREATED, Json(credential_to_response(record))))
}

async fn admin_update_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(credential_id): axum::extract::Path<i64>,
    Json(req): Json<CredentialUpdateRequest>,
) -> ApiResult<CredentialResponse> {
    verify_admin_key(&headers, &state)?;
    let record = state
        .container
        .credential_admin
        .update(credential_id, req)
        .await
        .map_err(|e| error_response(&e))?;
    match record {
        Some(r) => Ok(Json(credential_to_response(r))),
        None => Err(not_found("credential not found")),
    }
}

async fn admin_delete_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(credential_id): axum::extract::Path<i64>,
) -> Result<StatusCode, Response> {
    verify_admin_key(&headers, &state)?;
    let deleted = state
        .container
        .credential_admin
        .delete(credential_id)
        .await
        .map_err(|e| error_response(&e))?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("credential not found"))
    }
}

async fn admin_duplicate_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(credential_id): axum::extract::Path<i64>,
    Json(req): Json<CredentialDuplicateRequest>,
) -> Result<(StatusCode, Json<CredentialResponse>), Response> {
    verify_admin_key(&headers, &state)?;
    let record = state
        .container
        .credential_admin
        .duplicate(credential_id, req)
        .await
        .map_err(|e| error_response(&e))?;
    match record {
        Some(r) => Ok((StatusCode::CREATED, Json(credential_to_response(r)))),
        None => Err(not_found("credential not found")),
    }
}

async fn admin_reload_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<ReloadCredentialsResponse> {
    verify_admin_key(&headers, &state)?;
    let (reloaded, pool_size, reason) = state.container.reload_credentials().await;
    Ok(Json(ReloadCredentialsResponse {
        reloaded,
        pool_size,
        reason,
    }))
}

async fn admin_queue_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<QueueStatusResponse> {
    verify_admin_key(&headers, &state)?;
    let status = state
        .application
        .queue_status()
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(QueueStatusResponse {
        enabled: status.enabled,
        main_size: status.main_size,
        inflight: status.inflight,
        db_pending: status.db_pending,
    }))
}

async fn admin_queue_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QueueResetRequest>,
) -> ApiResult<QueueResetResponse> {
    verify_admin_key(&headers, &state)?;
    // 与 DB pending 对齐（sync 剔除无效项 / purge 清空重投）；无队列时返回全零
    let result = state
        .application
        .reset_queue(&req.mode, req.requeue)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(QueueResetResponse {
        removed: result.removed,
        requeued: result.requeued,
        queue_size: result.queue_size,
        db_pending: result.db_pending,
    }))
}

async fn admin_requeue_stale(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RequeueStaleRequest>,
) -> ApiResult<RequeueStaleResponse> {
    verify_admin_key(&headers, &state)?;
    // 查有 staging 但长时间未处理的 pending blob（与 Python find_stale_with_staging 一致）
    let stale = state
        .application
        .requeue_stale(req.stale_hours, req.limit)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(RequeueStaleResponse {
        requeued_count: stale,
    }))
}

async fn admin_gc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GcRequest>,
) -> ApiResult<GcResponse> {
    verify_admin_key(&headers, &state)?;
    let result = state
        .application
        .run_gc(req.ttl_days, req.dry_run, req.limit)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(GcResponse {
        dry_run: result.dry_run,
        ttl_days: result.ttl_days,
        expired_chains: result.expired_chains,
        expired_blobs: result.expired_blobs,
        deletable_blobs: result.deletable_blobs,
        skipped_inflight: result.skipped_inflight,
        deleted_chains: result.deleted_chains,
        deleted_blobs: result.deleted_blobs,
    }))
}

async fn admin_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<MonitoringStatsResponse> {
    verify_admin_key(&headers, &state)?;
    let window_hours: u32 = params
        .get("window_hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let Some(stats_reader) = &state.container.stats_reader else {
        return Ok(Json(MonitoringStatsResponse {
            window_hours,
            ..Default::default()
        }));
    };
    let stats = stats_reader.stats(window_hours).await;
    let tokens_total: u64 = stats.tokens.iter().map(|t| t.total_tokens).sum();
    let empty_rate = if stats.retrieval.count > 0 {
        stats.retrieval.empty_count as f64 / stats.retrieval.count as f64
    } else {
        0.0
    };
    let resource = stats.resource.map(|r| ResourceSnapshotResponse {
        ts: Some(r.ts),
        mem_rss_bytes: r.mem_rss_bytes,
        mem_percent: r.mem_percent,
        cpu_percent: r.cpu_percent,
        disk_free_bytes: r.disk_free_bytes,
        disk_total_bytes: r.disk_total_bytes,
        disk_data_bytes: r.disk_data_bytes,
    });
    Ok(Json(MonitoringStatsResponse {
        window_hours,
        api_calls: ApiCallStatsResponse {
            count: stats.api_calls.calls,
            error_count: stats.api_calls.error_count,
            avg_latency_ms: stats.api_calls.avg_latency_ms,
            ..Default::default()
        },
        tokens: stats
            .tokens
            .into_iter()
            .map(|t| TokenKindStatsResponse {
                kind: t.kind,
                calls: t.calls,
                prompt_tokens: t.prompt_tokens,
                completion_tokens: t.completion_tokens,
                total_tokens: t.total_tokens,
            })
            .collect(),
        tokens_total,
        retrieval: RetrievalStatsResponse {
            count: stats.retrieval.count,
            empty_count: stats.retrieval.empty_count,
            empty_rate,
        },
        resource,
    }))
}

// ───────────────────────────────────────────────────────── 报表端点
// 入参在路由层收敛（与 Python 版一致）：窗口 1..720h、明细 1..500 行、分桶仅 hour/day。

fn clamp_window(window_hours: u32) -> u32 {
    window_hours.clamp(1, 720)
}

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, 500)
}

fn validate_bucket(bucket: &str) -> Result<String, Response> {
    if bucket != "hour" && bucket != "day" {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "bucket must be 'hour' or 'day'"})),
        )
            .into_response());
    }
    Ok(bucket.to_string())
}

/// 报表参数提取：window_hours（默认 24）、bucket（默认 hour）。
fn report_params(params: &std::collections::HashMap<String, String>) -> (u32, String) {
    let window_hours = params
        .get("window_hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let bucket = params
        .get("bucket")
        .cloned()
        .unwrap_or_else(|| "hour".into());
    (window_hours, bucket)
}

async fn report_api_calls(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ApiCallsReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let (window_hours, bucket) = report_params(&params);
    let bucket = validate_bucket(&bucket)?;
    let report = state.container.reports.as_ref()
        .api_calls(clamp_window(window_hours), &bucket)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

async fn report_retrieval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<RetrievalReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let (window_hours, bucket) = report_params(&params);
    let bucket = validate_bucket(&bucket)?;
    let report = state.container.reports.as_ref()
        .retrieval(clamp_window(window_hours), &bucket)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

async fn report_slow_queries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    verify_admin_key(&headers, &state)?;
    let window_hours: u32 = params
        .get("window_hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let limit: u32 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let items = state.container.reports.as_ref()
        .slow_queries(clamp_window(window_hours), clamp_limit(limit))
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(serde_json::json!({
        "window_hours": clamp_window(window_hours),
        "items": items,
    })))
}

async fn report_empty_queries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    verify_admin_key(&headers, &state)?;
    let window_hours: u32 = params
        .get("window_hours")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let limit: u32 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let items = state.container.reports.as_ref()
        .empty_queries(clamp_window(window_hours), clamp_limit(limit))
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(serde_json::json!({
        "window_hours": clamp_window(window_hours),
        "items": items,
    })))
}

async fn report_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<TokensReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let (window_hours, bucket) = report_params(&params);
    let bucket = validate_bucket(&bucket)?;
    let report = state.container.reports.as_ref()
        .tokens(clamp_window(window_hours), &bucket)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

async fn report_index_inventory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<IndexInventoryReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let report = state.container.reports.as_ref()
        .index_inventory()
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

async fn report_resources(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ResourcesReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let (window_hours, bucket) = report_params(&params);
    let bucket = validate_bucket(&bucket)?;
    let report = state.container.reports.as_ref()
        .resources(clamp_window(window_hours), &bucket)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

async fn report_storage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StorageReport>, Response> {
    verify_admin_key(&headers, &state)?;
    let report = state.container.reports.as_ref()
        .storage()
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(report))
}

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": message}))).into_response()
}

/// pydantic 422 形状：{"detail": [{"loc": ..., "msg": ..., "type": ...}]}
/// 简化为字符串列表首项，保持 422 状态码与 detail 包裹结构。
fn validation_error(message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{"msg": message, "type": "missing"}]})),
    )
        .into_response()
}
