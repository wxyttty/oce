//! PostgreSQL 报表只读聚合（与 SQLite 版 `sqlite/reports.rs` 对齐）。
//!
//! 约定不变：SQL 只做窗口过滤与基础聚合；分桶（hour/day 截断）与分位数在 Rust 侧
//! 计算（跨方言可移植）；报表是旁路只读路径，任何单表查询失败降级为空结果。

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use oce_core::error::{OceError, OceResult};
use oce_core::reports::{
    ApiCallBucket, ApiCallsReport, CountStat, CredentialTokenStat, DataFileStat, EndpointStat,
    ErrorStat, IndexInventoryReport, IntentStat, ModelTokenStat, ResourceBucket,
    ResourcesReport, RetrievalBucket, RetrievalQueryDetail, RetrievalReport, ReportsStore,
    ScopeBucketStat, StageStat, StorageReport, TableSpaceStat, TokenBucket, TokensReport,
    VectorStoreStat,
};
use sqlx::PgPool;
use std::collections::HashMap;

fn pg_err(e: sqlx::Error) -> OceError {
    OceError::new(e.to_string(), "PgError")
}

/// 排序数组的最近邻分位（与 Python _percentile / SQLite 版一致）。
fn percentile(sorted: &[i64], p: u32) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p as f64 / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn check_bucket(bucket: &str) -> Result<(), String> {
    if bucket != "hour" && bucket != "day" {
        return Err(format!("bucket 必须是 'hour' 或 'day'，收到: {bucket:?}"));
    }
    Ok(())
}

/// 时间戳截断到分桶边界。ts 为 RFC3339 字符串（落库格式统一）。
fn bucket_ts(ts: &str, bucket: &str) -> String {
    use chrono::Timelike;
    let dt = DateTime::parse_from_rfc3339(ts)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap());
    let truncated = if bucket == "day" {
        dt.date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|t| Utc.from_utc_datetime(&t))
            .unwrap()
    } else {
        dt.date_naive()
            .and_hms_opt(dt.time().hour(), 0, 0)
            .map(|t| Utc.from_utc_datetime(&t))
            .unwrap()
    };
    truncated.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn cutoff(window_hours: u32) -> chrono::DateTime<Utc> {
    Utc::now() - Duration::hours(window_hours as i64)
}

fn scope_label(scope_size: Option<i64>) -> &'static str {
    match scope_size {
        None => "unknown",
        Some(n) if n <= 100 => "1-100",
        Some(n) if n <= 1000 => "101-1000",
        Some(n) if n <= 10000 => "1001-10000",
        Some(_) => ">10000",
    }
}

/// 检索管线阶段名（对应 retrieval_metrics 的 <stage>_ms 列）。
const STAGE_NAMES: [&str; 8] = [
    "intent",
    "rewrite",
    "dense",
    "exact",
    "fuse",
    "rerank",
    "llm_rerank",
    "select",
];

/// storage() 关心的全部业务表。
const SPACE_TABLES: [&str; 12] = [
    "model_credentials",
    "blobs",
    "blob_staging",
    "chunks",
    "blob_chunks",
    "chains",
    "chain_members",
    "symbol_occurrences",
    "api_call_metrics",
    "token_usage_metrics",
    "resource_samples",
    "retrieval_metrics",
];

/// 递归累加目录内文件字节数；不可读的文件跳过。
fn dir_size_bytes(path: &std::path::Path) -> i64 {
    fn walk(dir: &std::path::Path, total: &mut i64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_file() => {
                    if let Ok(md) = entry.metadata() {
                        *total += md.len() as i64;
                    }
                }
                Ok(t) if t.is_dir() => walk(&entry.path(), total),
                _ => {}
            }
        }
    }
    let mut total = 0;
    walk(path, &mut total);
    total
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// 报表聚合 reader（PG 实现）。
pub struct PgReportsReader {
    pool: PgPool,
    /// 数据目录（storage/resources 报表用）
    data_dir: Option<String>,
    /// 向量库统计注入（保持本模块纯 SQL；异常在调用侧降级）
    vector_stats: Option<std::sync::Arc<dyn Fn() -> VectorStoreStat + Send + Sync>>,
    /// 向量维度（est_bytes 估算用）
    vector_dim: usize,
}

impl PgReportsReader {
    pub fn new(
        pool: PgPool,
        data_dir: Option<String>,
        vector_stats: Option<std::sync::Arc<dyn Fn() -> VectorStoreStat + Send + Sync>>,
        vector_dim: usize,
    ) -> Self {
        Self {
            pool,
            data_dir,
            vector_stats,
            vector_dim,
        }
    }

    /// 旁路读：任何失败降级为默认值，报表端点不 5xx。
    async fn read<T: Default + Send + 'static>(
        &self,
        f: impl FnOnce(&PgPool) -> Result<T, String> + Send + 'static,
    ) -> T {
        let pool = self.pool.clone();
        let result = tokio::task::spawn_blocking(move || f(&pool))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        result.unwrap_or_default()
    }

    // ───────────────────────── API 健康

    pub async fn api_calls(
        &self,
        window_hours: u32,
        bucket: &str,
    ) -> Result<ApiCallsReport, String> {
        check_bucket(bucket)?;
        let cutoff = cutoff(window_hours);
        let rows: Vec<(chrono::DateTime<Utc>, String, String, i32, i32, Option<String>)> =
            sqlx::query_as(
                "SELECT ts, endpoint, method, status_code, latency_ms, error_type
                 FROM api_call_metrics WHERE ts >= $1",
            )
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(ts, endpoint, method, status, latency, error_type)| {
                (
                    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    endpoint,
                    method,
                    status as i64,
                    latency as i64,
                    error_type,
                )
            })
            .collect();

        let mut by_bucket: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        let mut by_endpoint: HashMap<(String, String), Vec<(i64, i64)>> = HashMap::new();
        let mut by_error: HashMap<(i64, Option<String>), Vec<String>> = HashMap::new();
        for (ts, endpoint, method, status, latency, error_type) in &rows {
            by_bucket
                .entry(bucket_ts(ts, bucket))
                .or_default()
                .push((*latency, *status));
            by_endpoint
                .entry((endpoint.clone(), method.clone()))
                .or_default()
                .push((*latency, *status));
            if *status >= 400 {
                by_error
                    .entry((*status, error_type.clone()))
                    .or_default()
                    .push(ts.clone());
            }
        }

        let mut buckets: Vec<ApiCallBucket> = by_bucket
            .into_iter()
            .map(|(ts, samples)| {
                let mut latencies: Vec<i64> = samples.iter().map(|(l, _)| *l).collect();
                latencies.sort_unstable();
                let count = latencies.len();
                ApiCallBucket {
                    ts,
                    count: count as u64,
                    error_count: samples.iter().filter(|(_, s)| *s >= 500).count() as u64,
                    avg_latency_ms: round2(latencies.iter().sum::<i64>() as f64 / count as f64),
                    p50_latency_ms: percentile(&latencies, 50),
                    p95_latency_ms: percentile(&latencies, 95),
                    max_latency_ms: *latencies.last().unwrap_or(&0),
                }
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut endpoints: Vec<EndpointStat> = by_endpoint
            .into_iter()
            .map(|((endpoint, method), samples)| {
                let mut latencies: Vec<i64> = samples.iter().map(|(l, _)| *l).collect();
                latencies.sort_unstable();
                let count = latencies.len();
                let error_count = samples.iter().filter(|(_, s)| *s >= 500).count();
                EndpointStat {
                    endpoint,
                    method,
                    count: count as u64,
                    error_count: error_count as u64,
                    error_rate: round4(error_count as f64 / count as f64),
                    avg_latency_ms: round2(latencies.iter().sum::<i64>() as f64 / count as f64),
                    p95_latency_ms: percentile(&latencies, 95),
                }
            })
            .collect();
        endpoints.sort_by(|a, b| b.count.cmp(&a.count));
        endpoints.truncate(50);

        let mut errors: Vec<ErrorStat> = by_error
            .into_iter()
            .map(|((status_code, error_type), ts_list)| ErrorStat {
                status_code,
                error_type,
                count: ts_list.len() as u64,
                last_ts: ts_list.into_iter().max(),
            })
            .collect();
        errors.sort_by(|a, b| b.count.cmp(&a.count));
        errors.truncate(50);

        Ok(ApiCallsReport {
            window_hours,
            bucket: bucket.to_string(),
            buckets,
            endpoints,
            errors,
        })
    }

    // ───────────────────────── 检索质量

    pub async fn retrieval(
        &self,
        window_hours: u32,
        bucket: &str,
    ) -> Result<RetrievalReport, String> {
        check_bucket(bucket)?;
        let cutoff = cutoff(window_hours);
        let rows: Vec<(
            chrono::DateTime<Utc>,
            Option<i32>,
            i32,
            i32,
            Option<String>,
            bool,
            Option<i32>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
        )> = sqlx::query_as(
            "SELECT ts, scope_size, hit_count, total_ms, intent, path_boosted,
                    intent_ms, rewrite_ms, dense_ms, exact_ms, fuse_ms, rerank_ms, llm_rerank_ms, select_ms
             FROM retrieval_metrics WHERE ts >= $1",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let rows: Vec<_> = rows
            .into_iter()
            .map(
                |(ts, scope_size, hit_count, total_ms, intent, path_boosted, i1, i2, i3, i4, i5, i6, i7, i8)| {
                    (
                        ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        scope_size.map(|v| v as i64),
                        hit_count as i64,
                        total_ms as i64,
                        intent,
                        path_boosted,
                        [i1, i2, i3, i4, i5, i6, i7, i8].map(|v| v.map(|x| x as i64)),
                    )
                },
            )
            .collect();

        let mut by_bucket: HashMap<String, Vec<&(String, Option<i64>, i64, i64, Option<String>, bool, [Option<i64>; 8])>> =
            HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.0, bucket))
                .or_default()
                .push(row);
        }
        let mut buckets: Vec<RetrievalBucket> = by_bucket
            .into_iter()
            .map(|(ts, samples)| {
                let count = samples.len();
                let empty_count = samples.iter().filter(|r| r.2 == 0).count();
                let mut totals: Vec<i64> = samples.iter().map(|r| r.3).collect();
                totals.sort_unstable();
                RetrievalBucket {
                    ts,
                    count: count as u64,
                    empty_count: empty_count as u64,
                    empty_rate: round4(empty_count as f64 / count as f64),
                    avg_hit_count: round2(samples.iter().map(|r| r.2).sum::<i64>() as f64 / count as f64),
                    avg_total_ms: round2(samples.iter().map(|r| r.3).sum::<i64>() as f64 / count as f64),
                    p95_total_ms: percentile(&totals, 95),
                }
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut stages: Vec<StageStat> = STAGE_NAMES
            .iter()
            .enumerate()
            .map(|(i, stage)| {
                let samples: Vec<i64> = rows.iter().filter_map(|r| r.6[i]).collect();
                let count = samples.len();
                if count == 0 {
                    return StageStat {
                        stage: stage.to_string(),
                        count: 0,
                        avg_ms: 0.0,
                        p95_ms: 0,
                        max_ms: 0,
                    };
                }
                let mut sorted = samples.clone();
                sorted.sort_unstable();
                StageStat {
                    stage: stage.to_string(),
                    count: count as u64,
                    avg_ms: round2(samples.iter().sum::<i64>() as f64 / count as f64),
                    p95_ms: percentile(&sorted, 95),
                    max_ms: *sorted.last().unwrap_or(&0),
                }
            })
            .collect();
        stages.retain(|s| s.count > 0);

        let mut by_intent: HashMap<Option<String>, Vec<&(String, Option<i64>, i64, i64, Option<String>, bool, [Option<i64>; 8])>> =
            HashMap::new();
        for row in &rows {
            by_intent.entry(row.4.clone()).or_default().push(row);
        }
        let mut intents: Vec<IntentStat> = by_intent
            .into_iter()
            .map(|(intent, samples)| {
                let count = samples.len();
                let empty_count = samples.iter().filter(|r| r.2 == 0).count();
                IntentStat {
                    intent,
                    count: count as u64,
                    empty_count: empty_count as u64,
                    empty_rate: round4(empty_count as f64 / count as f64),
                    avg_total_ms: round2(samples.iter().map(|r| r.3).sum::<i64>() as f64 / count as f64),
                    path_boosted_count: samples.iter().filter(|r| r.5).count() as u64,
                }
            })
            .collect();
        intents.sort_by(|a, b| b.count.cmp(&a.count));

        let mut by_scope: HashMap<&str, Vec<&(String, Option<i64>, i64, i64, Option<String>, bool, [Option<i64>; 8])>> =
            HashMap::new();
        for row in &rows {
            by_scope.entry(scope_label(row.1)).or_default().push(row);
        }
        let mut scopes: Vec<ScopeBucketStat> = by_scope
            .into_iter()
            .map(|(label, samples)| {
                let count = samples.len();
                let empty_count = samples.iter().filter(|r| r.2 == 0).count();
                let mut totals: Vec<i64> = samples.iter().map(|r| r.3).collect();
                totals.sort_unstable();
                ScopeBucketStat {
                    label: label.to_string(),
                    count: count as u64,
                    empty_rate: round4(empty_count as f64 / count as f64),
                    p95_total_ms: percentile(&totals, 95),
                }
            })
            .collect();
        scopes.sort_by(|a, b| b.count.cmp(&a.count));

        Ok(RetrievalReport {
            window_hours,
            bucket: bucket.to_string(),
            buckets,
            stages,
            intents,
            scopes,
        })
    }

    pub async fn slow_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> Result<Vec<RetrievalQueryDetail>, String> {
        let cutoff = cutoff(window_hours);
        let rows: Vec<(
            chrono::DateTime<Utc>,
            String,
            Option<String>,
            i32,
            i32,
            Option<i32>,
            Option<String>,
            bool,
        )> = sqlx::query_as(
            "SELECT ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted
             FROM retrieval_metrics WHERE ts >= $1 ORDER BY total_ms DESC LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted)| {
                RetrievalQueryDetail {
                    ts: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    source,
                    query_text,
                    total_ms: total_ms as i64,
                    hit_count: hit_count as i64,
                    scope_size: scope_size.map(|v| v as i64),
                    intent,
                    path_boosted,
                }
            })
            .collect())
    }

    pub async fn empty_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> Result<Vec<RetrievalQueryDetail>, String> {
        let cutoff = cutoff(window_hours);
        let rows: Vec<(
            chrono::DateTime<Utc>,
            String,
            Option<String>,
            i32,
            i32,
            Option<i32>,
            Option<String>,
            bool,
        )> = sqlx::query_as(
            "SELECT ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted
             FROM retrieval_metrics WHERE ts >= $1 AND hit_count = 0 ORDER BY ts DESC LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted)| {
                RetrievalQueryDetail {
                    ts: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    source,
                    query_text,
                    total_ms: total_ms as i64,
                    hit_count: hit_count as i64,
                    scope_size: scope_size.map(|v| v as i64),
                    intent,
                    path_boosted,
                }
            })
            .collect())
    }

    // ───────────────────────── Token 用量

    pub async fn tokens(&self, window_hours: u32, bucket: &str) -> Result<TokensReport, String> {
        check_bucket(bucket)?;
        let cutoff = cutoff(window_hours);
        let rows: Vec<(chrono::DateTime<Utc>, String, String, Option<i32>, i32, i32, i32)> =
            sqlx::query_as(
                "SELECT ts, kind, model, credential_id, prompt_tokens, completion_tokens, total_tokens
                 FROM token_usage_metrics WHERE ts >= $1",
            )
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let rows: Vec<_> = rows
            .into_iter()
            .map(
                |(ts, kind, model, credential_id, prompt, completion, total)| {
                    (
                        ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        kind,
                        model,
                        credential_id.map(|v| v as i64),
                        prompt as i64,
                        completion as i64,
                        total as i64,
                    )
                },
            )
            .collect();

        let mut by_bucket: HashMap<String, Vec<&(String, String, String, Option<i64>, i64, i64, i64)>> =
            HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.0, bucket))
                .or_default()
                .push(row);
        }
        let mut buckets: Vec<TokenBucket> = by_bucket
            .into_iter()
            .map(|(ts, samples)| TokenBucket {
                ts,
                kind: samples
                    .iter()
                    .map(|r| r.1.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join("+"),
                calls: samples.len() as u64,
                prompt_tokens: samples.iter().map(|r| r.4).sum::<i64>().max(0) as u64,
                completion_tokens: samples.iter().map(|r| r.5).sum::<i64>().max(0) as u64,
                total_tokens: samples.iter().map(|r| r.6).sum::<i64>().max(0) as u64,
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut by_model: HashMap<(String, String), Vec<&(String, String, String, Option<i64>, i64, i64, i64)>> =
            HashMap::new();
        for row in &rows {
            by_model
                .entry((row.2.clone(), row.1.clone()))
                .or_default()
                .push(row);
        }
        let mut models: Vec<ModelTokenStat> = by_model
            .into_iter()
            .map(|((model, kind), samples)| {
                let count = samples.len();
                ModelTokenStat {
                    model,
                    kind,
                    calls: count as u64,
                    prompt_tokens: samples.iter().map(|r| r.4).sum::<i64>().max(0) as u64,
                    completion_tokens: samples.iter().map(|r| r.5).sum::<i64>().max(0) as u64,
                    total_tokens: samples.iter().map(|r| r.6).sum::<i64>().max(0) as u64,
                    avg_tokens_per_call: round2(
                        samples.iter().map(|r| r.6).sum::<i64>() as f64 / count as f64,
                    ),
                }
            })
            .collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        let mut by_credential: HashMap<Option<i64>, Vec<&(String, String, String, Option<i64>, i64, i64, i64)>> =
            HashMap::new();
        for row in &rows {
            by_credential.entry(row.3).or_default().push(row);
        }
        let mut credentials: Vec<CredentialTokenStat> = by_credential
            .into_iter()
            .map(|(credential_id, samples)| CredentialTokenStat {
                credential_id,
                calls: samples.len() as u64,
                total_tokens: samples.iter().map(|r| r.6).sum::<i64>().max(0) as u64,
            })
            .collect();
        credentials.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        let tokens_total: u64 = buckets.iter().map(|b| b.total_tokens).sum();
        Ok(TokensReport {
            window_hours,
            bucket: bucket.to_string(),
            buckets,
            models,
            credentials,
            tokens_total,
        })
    }

    // ───────────────────────── 索引资产

    pub async fn index_inventory(&self) -> Result<IndexInventoryReport, String> {
        let report = self
            .read(|pool| {
                let rt = tokio::runtime::Handle::current();
                let count = |sql: &'static str| -> i64 {
                    rt.block_on(async {
                        sqlx::query_scalar::<_, i64>(sql)
                            .fetch_one(pool)
                            .await
                            .unwrap_or(0)
                    })
                };
                let group = |sql: &'static str| -> Vec<CountStat> {
                    rt.block_on(async {
                        sqlx::query_as::<_, (Option<String>, i64)>(sql)
                            .fetch_all(pool)
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(key, c)| CountStat {
                                key: key.unwrap_or_else(|| "unknown".into()),
                                count: c.max(0) as u64,
                            })
                            .collect()
                    })
                };
                let blob_row: (i64, i64) = rt
                    .block_on(async {
                        sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(content_size), 0) FROM blobs")
                            .fetch_one(pool)
                            .await
                    })
                    .unwrap_or((0, 0));
                let chunk_row: (i64, i64) = rt
                    .block_on(async {
                        sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(content_size), 0) FROM chunks")
                            .fetch_one(pool)
                            .await
                    })
                    .unwrap_or((0, 0));
                let now = Utc::now();
                let stale = |days: i64| -> u64 {
                    let threshold = now - Duration::days(days);
                    rt.block_on(async {
                        sqlx::query_scalar::<_, i64>(
                            "SELECT COUNT(*) FROM chains WHERE updated_at < $1",
                        )
                        .bind(threshold)
                        .fetch_one(pool)
                        .await
                        .unwrap_or(0)
                    })
                    .max(0) as u64
                };
                let mut blob_by_language =
                    group("SELECT language, COUNT(*) FROM blobs GROUP BY language");
                blob_by_language.sort_by(|a, b| b.count.cmp(&a.count));
                blob_by_language.truncate(30);
                Ok(IndexInventoryReport {
                    blob_total: blob_row.0.max(0) as u64,
                    blob_by_status: group("SELECT status, COUNT(*) FROM blobs GROUP BY status"),
                    blob_by_language,
                    blob_retrying: count("SELECT COUNT(*) FROM blobs WHERE retry_count > 0")
                        .max(0) as u64,
                    blob_content_bytes: blob_row.1,
                    chunk_total: chunk_row.0.max(0) as u64,
                    chunk_pending_embed: count("SELECT COUNT(*) FROM chunks WHERE embedded = false")
                        .max(0) as u64,
                    chunk_by_type: group("SELECT chunk_type, COUNT(*) FROM chunks GROUP BY chunk_type"),
                    chunk_content_bytes: chunk_row.1,
                    blob_chunk_links: count("SELECT COUNT(*) FROM blob_chunks").max(0) as u64,
                    symbol_total: count("SELECT COUNT(*) FROM symbol_occurrences").max(0) as u64,
                    symbol_by_kind: group(
                        "SELECT kind, COUNT(*) FROM symbol_occurrences GROUP BY kind",
                    ),
                    chain_total: count("SELECT COUNT(*) FROM chains").max(0) as u64,
                    chain_stale_7d: stale(7),
                    chain_stale_30d: stale(30),
                    staging_rows: count("SELECT COUNT(*) FROM blob_staging").max(0) as u64,
                })
            })
            .await;
        Ok(report)
    }

    // ───────────────────────── 资源容量

    pub async fn resources(
        &self,
        window_hours: u32,
        bucket: &str,
    ) -> Result<ResourcesReport, String> {
        check_bucket(bucket)?;
        let cutoff = cutoff(window_hours);
        let rows: Vec<(chrono::DateTime<Utc>, f64, f64, i64, i64, i64)> = sqlx::query_as(
            "SELECT ts, cpu_percent, mem_percent, mem_rss_bytes, disk_data_bytes, disk_free_bytes
             FROM resource_samples WHERE ts >= $1 ORDER BY ts ASC",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(ts, cpu, mem_percent, mem_rss, disk_data, disk_free)| {
                (
                    ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    cpu,
                    mem_percent,
                    mem_rss,
                    disk_data,
                    disk_free,
                )
            })
            .collect();
        let latest: Option<(i64, i64)> = sqlx::query_as(
            "SELECT disk_total_bytes, disk_free_bytes FROM resource_samples ORDER BY ts DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut by_bucket: HashMap<String, Vec<&(String, f64, f64, i64, i64, i64)>> = HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.0, bucket))
                .or_default()
                .push(row);
        }
        let mut buckets: Vec<ResourceBucket> = by_bucket
            .into_iter()
            .map(|(ts, samples)| {
                let count = samples.len();
                let last = samples[count - 1];
                ResourceBucket {
                    ts,
                    avg_cpu_percent: round2(samples.iter().map(|r| r.1).sum::<f64>() / count as f64),
                    max_cpu_percent: samples.iter().map(|r| r.1).fold(f64::MIN, f64::max),
                    avg_mem_percent: round2(samples.iter().map(|r| r.2).sum::<f64>() / count as f64),
                    max_mem_rss_bytes: samples.iter().map(|r| r.3).max().unwrap_or(0),
                    disk_data_bytes: last.4,
                    disk_free_bytes: last.5,
                }
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut growth_per_day = 0.0;
        if rows.len() >= 2 {
            let (first, last) = (&rows[0], &rows[rows.len() - 1]);
            if let (Ok(t0), Ok(t1)) = (
                DateTime::parse_from_rfc3339(&first.0),
                DateTime::parse_from_rfc3339(&last.0),
            ) {
                let elapsed = (t1 - t0).num_seconds();
                if elapsed >= 3600 {
                    growth_per_day =
                        round2((last.4 - first.4) as f64 / (elapsed as f64 / 86400.0));
                }
            }
        }
        let days_until_full = if growth_per_day > 0.0 {
            latest.map(|(_, free)| round1(free as f64 / growth_per_day))
        } else {
            None
        };

        Ok(ResourcesReport {
            window_hours,
            bucket: bucket.to_string(),
            buckets,
            disk_total_bytes: latest.map(|(t, _)| t).unwrap_or(0),
            disk_growth_bytes_per_day: growth_per_day,
            disk_days_until_full: days_until_full,
        })
    }

    // ───────────────────────── 空间占用

    pub async fn storage(&self) -> Result<StorageReport, String> {
        let tables: Vec<TableSpaceStat> = self
            .read(|pool| {
                let rt = tokio::runtime::Handle::current();
                Ok::<Vec<TableSpaceStat>, String>(
                    SPACE_TABLES
                        .iter()
                        .map(|name| {
                            let rows: i64 = rt
                                .block_on(async {
                                    sqlx::query_scalar::<_, i64>(&format!(
                                        "SELECT COUNT(*) FROM {name}"
                                    ))
                                    .fetch_one(pool)
                                    .await
                                })
                                .unwrap_or(0);
                            // pg_total_relation_size 含索引/TOAST，比 SQLite dbstat 更准
                            let bytes: i64 = rt
                                .block_on(async {
                                    sqlx::query_scalar::<_, i64>(
                                        "SELECT pg_total_relation_size($1::regclass)",
                                    )
                                    .bind(*name)
                                    .fetch_one(pool)
                                    .await
                                })
                                .unwrap_or(0);
                            TableSpaceStat {
                                table: name.to_string(),
                                bytes,
                                rows,
                                approximate: false,
                            }
                        })
                        .collect(),
                )
            })
            .await;

        // 文件系统统计：任何异常降级为空结果，绝不让 storage() 抛错
        let (data_dir, data_files, data_dir_total) = match &self.data_dir {
            Some(dir) => {
                let root = std::path::Path::new(dir);
                if root.is_dir() {
                    let mut files: Vec<DataFileStat> = std::fs::read_dir(root)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|entry| {
                            let bytes =
                                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                    dir_size_bytes(&entry.path())
                                } else {
                                    entry.metadata().map(|m| m.len() as i64).unwrap_or(0)
                                };
                            DataFileStat {
                                name: entry.file_name().to_string_lossy().into_owned(),
                                bytes,
                            }
                        })
                        .collect();
                    files.sort_by(|a, b| b.bytes.cmp(&a.bytes));
                    let total = files.iter().map(|f| f.bytes).sum();
                    (Some(dir.clone()), files, total)
                } else {
                    (None, Vec::new(), 0)
                }
            }
            None => (None, Vec::new(), 0),
        };

        // 向量库统计走注入的 provider；异常降级为 unavailable
        let vector = self.vector_stats.as_ref().map(|f| {
            let mut stat = f();
            if stat.error.is_none() {
                for c in &mut stat.collections {
                    c.est_bytes = c.rows as i64 * self.vector_dim as i64 * 4;
                }
            }
            stat
        });

        Ok(StorageReport {
            dialect: "postgres".into(),
            total_table_bytes: tables.iter().map(|t| t.bytes).sum(),
            tables,
            data_dir,
            data_files,
            data_dir_total_bytes: data_dir_total,
            vector,
        })
    }
}

#[async_trait]
impl ReportsStore for PgReportsReader {
    async fn api_calls(&self, window_hours: u32, bucket: &str) -> OceResult<ApiCallsReport> {
        PgReportsReader::api_calls(self, window_hours, bucket)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn retrieval(&self, window_hours: u32, bucket: &str) -> OceResult<RetrievalReport> {
        PgReportsReader::retrieval(self, window_hours, bucket)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn slow_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> OceResult<Vec<RetrievalQueryDetail>> {
        PgReportsReader::slow_queries(self, window_hours, limit)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn empty_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> OceResult<Vec<RetrievalQueryDetail>> {
        PgReportsReader::empty_queries(self, window_hours, limit)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn tokens(&self, window_hours: u32, bucket: &str) -> OceResult<TokensReport> {
        PgReportsReader::tokens(self, window_hours, bucket)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn index_inventory(&self) -> OceResult<IndexInventoryReport> {
        PgReportsReader::index_inventory(self)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn resources(&self, window_hours: u32, bucket: &str) -> OceResult<ResourcesReport> {
        PgReportsReader::resources(self, window_hours, bucket)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }

    async fn storage(&self) -> OceResult<StorageReport> {
        PgReportsReader::storage(self)
            .await
            .map_err(|e| OceError::new(e, "ReportError"))
    }
}
