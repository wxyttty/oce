//! 报表只读聚合（对应 Python `report_store.py` + `reports_read.py`，仅个人模式 SQLite）。
//!
//! 约定（与 Python 版一致）：
//! - SQL 只做窗口过滤与基础聚合；分桶（hour/day 截断）与分位数在 Rust 侧计算，
//!   避免依赖方言专有函数（Python 版为跨 SQLite/PG 可移植性如此设计，Rust 版沿用）；
//! - 报表是旁路只读路径，绝不写库、不影响检索主链路；
//! - 任何单表查询失败降级为空结果（unwrap_or_default），不让报表端点 5xx。

use crate::sqlite::SqlDb;
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::collections::HashMap;

// ───────────────────────────────────────────────────────── 读模型 DTO

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ApiCallBucket {
    pub ts: String,
    pub count: u64,
    pub error_count: u64,
    pub avg_latency_ms: f64,
    pub p50_latency_ms: i64,
    pub p95_latency_ms: i64,
    pub max_latency_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct EndpointStat {
    pub endpoint: String,
    pub method: String,
    pub count: u64,
    pub error_count: u64,
    pub error_rate: f64,
    pub avg_latency_ms: f64,
    pub p95_latency_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ErrorStat {
    pub status_code: i64,
    #[serde(default)]
    pub error_type: Option<String>,
    pub count: u64,
    #[serde(default)]
    pub last_ts: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiCallsReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<ApiCallBucket>,
    #[serde(default)]
    pub endpoints: Vec<EndpointStat>,
    #[serde(default)]
    pub errors: Vec<ErrorStat>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct RetrievalBucket {
    pub ts: String,
    pub count: u64,
    pub empty_count: u64,
    pub empty_rate: f64,
    pub avg_hit_count: f64,
    pub avg_total_ms: f64,
    pub p95_total_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct StageStat {
    pub stage: String,
    pub count: u64,
    pub avg_ms: f64,
    pub p95_ms: i64,
    pub max_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct IntentStat {
    #[serde(default)]
    pub intent: Option<String>,
    pub count: u64,
    pub empty_count: u64,
    pub empty_rate: f64,
    pub avg_total_ms: f64,
    pub path_boosted_count: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ScopeBucketStat {
    pub label: String,
    pub count: u64,
    pub empty_rate: f64,
    pub p95_total_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RetrievalReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<RetrievalBucket>,
    #[serde(default)]
    pub stages: Vec<StageStat>,
    #[serde(default)]
    pub intents: Vec<IntentStat>,
    #[serde(default)]
    pub scopes: Vec<ScopeBucketStat>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct RetrievalQueryDetail {
    pub ts: String,
    pub source: String,
    #[serde(default)]
    pub query_text: Option<String>,
    pub total_ms: i64,
    pub hit_count: i64,
    #[serde(default)]
    pub scope_size: Option<i64>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub path_boosted: bool,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TokenBucket {
    pub ts: String,
    pub kind: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ModelTokenStat {
    pub model: String,
    pub kind: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub avg_tokens_per_call: f64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct CredentialTokenStat {
    #[serde(default)]
    pub credential_id: Option<i64>,
    pub calls: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokensReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<TokenBucket>,
    #[serde(default)]
    pub models: Vec<ModelTokenStat>,
    #[serde(default)]
    pub credentials: Vec<CredentialTokenStat>,
    pub tokens_total: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct CountStat {
    pub key: String,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct IndexInventoryReport {
    pub blob_total: u64,
    #[serde(default)]
    pub blob_by_status: Vec<CountStat>,
    #[serde(default)]
    pub blob_by_language: Vec<CountStat>,
    pub blob_retrying: u64,
    pub blob_content_bytes: i64,
    pub chunk_total: u64,
    pub chunk_pending_embed: u64,
    #[serde(default)]
    pub chunk_by_type: Vec<CountStat>,
    pub chunk_content_bytes: i64,
    pub blob_chunk_links: u64,
    pub symbol_total: u64,
    #[serde(default)]
    pub symbol_by_kind: Vec<CountStat>,
    pub chain_total: u64,
    pub chain_stale_7d: u64,
    pub chain_stale_30d: u64,
    pub staging_rows: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ResourceBucket {
    pub ts: String,
    pub avg_cpu_percent: f64,
    pub max_cpu_percent: f64,
    pub avg_mem_percent: f64,
    pub max_mem_rss_bytes: i64,
    pub disk_data_bytes: i64,
    pub disk_free_bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResourcesReport {
    pub window_hours: u32,
    pub bucket: String,
    #[serde(default)]
    pub buckets: Vec<ResourceBucket>,
    pub disk_total_bytes: i64,
    pub disk_growth_bytes_per_day: f64,
    #[serde(default)]
    pub disk_days_until_full: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TableSpaceStat {
    pub table: String,
    pub bytes: i64,
    pub rows: i64,
    pub approximate: bool,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct DataFileStat {
    pub name: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct VectorCollectionStat {
    pub name: String,
    pub rows: u64,
    pub est_bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VectorStoreStat {
    pub mode: String,
    #[serde(default)]
    pub collections: Vec<VectorCollectionStat>,
    pub file_bytes: i64,
    #[serde(default)]
    pub error: Option<String>,
}

impl Default for VectorStoreStat {
    fn default() -> Self {
        Self {
            mode: "unavailable".into(),
            collections: Vec::new(),
            file_bytes: 0,
            error: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct StorageReport {
    pub dialect: String,
    pub total_table_bytes: i64,
    #[serde(default)]
    pub tables: Vec<TableSpaceStat>,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub data_files: Vec<DataFileStat>,
    pub data_dir_total_bytes: i64,
    #[serde(default)]
    pub vector: Option<VectorStoreStat>,
}

// ───────────────────────────────────────────────────────── 工具函数

/// 排序数组的最近邻分位（与 Python _percentile 一致）。
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

/// 时间戳截断到分桶边界。ts 为 RFC3339 字符串（SQLite 落库格式）。
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

fn cutoff(window_hours: u32) -> String {
    (Utc::now() - Duration::hours(window_hours as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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

const SCOPE_LABELS: [&str; 5] = ["1-100", "101-1000", "1001-10000", ">10000", "unknown"];

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

// ───────────────────────────────────────────────────────── reader

/// 报表聚合 reader（个人模式 SQLite 实现）。
pub struct ReportsReader {
    db: SqlDb,
    /// 数据目录（storage/resources 报表用；SQLite 相对路径时 None）
    data_dir: Option<String>,
    /// 向量库统计注入（保持本模块纯 SQL；异常在调用侧降级）
    vector_stats: Option<std::sync::Arc<dyn Fn() -> VectorStoreStat + Send + Sync>>,
    /// 向量维度（est_bytes 估算用）
    vector_dim: usize,
}

impl ReportsReader {
    pub fn new(
        db: SqlDb,
        data_dir: Option<String>,
        vector_stats: Option<std::sync::Arc<dyn Fn() -> VectorStoreStat + Send + Sync>>,
        vector_dim: usize,
    ) -> Self {
        Self {
            db,
            data_dir,
            vector_stats,
            vector_dim,
        }
    }

    /// 旁路读：任何失败降级为默认值，报表端点不 5xx。闭包移入 spawn_blocking，
    /// 捕获变量（cutoff/limit）需 clone 进闭包。返回值已解包（非 Result）。
    async fn read<T: Default + Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, String> + Send + 'static,
    ) -> T {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || db.with_conn(|conn| f(&*conn)))
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

        // (ts, endpoint, method, status, latency, error_type)
        #[derive(Clone)]
        struct Row {
            ts: String,
            endpoint: String,
            method: String,
            status: i64,
            latency: i64,
            error_type: Option<String>,
        }
        let rows: Vec<Row> = self
            .read(move |conn| {
                let cutoff = cutoff.clone();
                let mut stmt = conn
                    .prepare(
                        "SELECT ts, endpoint, method, status_code, latency_ms, error_type
                     FROM api_call_metrics WHERE ts >= ?1",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([&cutoff], |r| {
                        Ok(Row {
                            ts: r.get(0)?,
                            endpoint: r.get(1)?,
                            method: r.get(2)?,
                            status: r.get(3)?,
                            latency: r.get(4)?,
                            error_type: r.get(5)?,
                        })
                    })
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
            .await;

        let mut by_bucket: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        let mut by_endpoint: HashMap<(String, String), Vec<(i64, i64)>> = HashMap::new();
        let mut by_error: HashMap<(i64, Option<String>), Vec<String>> = HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.ts, bucket))
                .or_default()
                .push((row.latency, row.status));
            by_endpoint
                .entry((row.endpoint.clone(), row.method.clone()))
                .or_default()
                .push((row.latency, row.status));
            if row.status >= 400 {
                by_error
                    .entry((row.status, row.error_type.clone()))
                    .or_default()
                    .push(row.ts.clone());
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
                last_ts: ts_list.into_max(),
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

        struct Row {
            ts: String,
            scope_size: Option<i64>,
            hit_count: i64,
            total_ms: i64,
            intent: Option<String>,
            path_boosted: bool,
            stages: [Option<i64>; 8],
        }
        let rows: Vec<Row> = self.read(move |conn| {
            let cutoff = cutoff.clone();
            let mut stmt = conn
                .prepare(
                    "SELECT ts, scope_size, hit_count, total_ms, intent, path_boosted,
                            intent_ms, rewrite_ms, dense_ms, exact_ms, fuse_ms, rerank_ms, llm_rerank_ms, select_ms
                     FROM retrieval_metrics WHERE ts >= ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&cutoff], |r| {
                    Ok(Row {
                        ts: r.get(0)?,
                        scope_size: r.get(1)?,
                        hit_count: r.get(2)?,
                        total_ms: r.get(3)?,
                        intent: r.get(4)?,
                        path_boosted: r.get::<_, i64>(5)? != 0,
                        stages: [
                            r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?,
                            r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?,
                        ],
                    })
                })
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }).await;

        let mut by_bucket: HashMap<String, Vec<(i64, i64)>> = HashMap::new(); // (hit, total)
        let mut stage_samples: HashMap<&'static str, Vec<i64>> = HashMap::new();
        let mut by_intent: HashMap<Option<String>, Vec<(i64, i64, bool)>> = HashMap::new();
        let mut by_scope: HashMap<&'static str, Vec<(i64, i64)>> = HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.ts, bucket))
                .or_default()
                .push((row.hit_count, row.total_ms));
            by_intent.entry(row.intent.clone()).or_default().push((
                row.hit_count,
                row.total_ms,
                row.path_boosted,
            ));
            by_scope
                .entry(scope_label(row.scope_size))
                .or_default()
                .push((row.hit_count, row.total_ms));
            for (i, stage) in STAGE_NAMES.iter().enumerate() {
                if let Some(v) = row.stages[i] {
                    stage_samples.entry(stage).or_default().push(v);
                }
            }
        }

        let mut buckets: Vec<RetrievalBucket> = by_bucket
            .into_iter()
            .map(|(ts, samples)| {
                let mut totals: Vec<i64> = samples.iter().map(|(_, t)| *t).collect();
                totals.sort_unstable();
                let count = samples.len();
                let empty = samples.iter().filter(|(h, _)| *h == 0).count();
                RetrievalBucket {
                    ts,
                    count: count as u64,
                    empty_count: empty as u64,
                    empty_rate: round4(empty as f64 / count as f64),
                    avg_hit_count: round2(
                        samples.iter().map(|(h, _)| *h).sum::<i64>() as f64 / count as f64,
                    ),
                    avg_total_ms: round2(totals.iter().sum::<i64>() as f64 / count as f64),
                    p95_total_ms: percentile(&totals, 95),
                }
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut stages: Vec<StageStat> = stage_samples
            .into_iter()
            .map(|(stage, mut samples)| {
                samples.sort_unstable();
                StageStat {
                    stage: stage.to_string(),
                    count: samples.len() as u64,
                    avg_ms: round2(samples.iter().sum::<i64>() as f64 / samples.len() as f64),
                    p95_ms: percentile(&samples, 95),
                    max_ms: *samples.last().unwrap_or(&0),
                }
            })
            .collect();
        stages.sort_by(|a, b| a.stage.cmp(&b.stage));

        let mut intents: Vec<IntentStat> = by_intent
            .into_iter()
            .map(|(intent, samples)| {
                let count = samples.len();
                let empty = samples.iter().filter(|(h, _, _)| *h == 0).count();
                IntentStat {
                    intent,
                    count: count as u64,
                    empty_count: empty as u64,
                    empty_rate: round4(empty as f64 / count as f64),
                    avg_total_ms: round2(
                        samples.iter().map(|(_, t, _)| *t).sum::<i64>() as f64 / count as f64,
                    ),
                    path_boosted_count: samples.iter().filter(|(_, _, b)| *b).count() as u64,
                }
            })
            .collect();
        intents.sort_by(|a, b| b.count.cmp(&a.count));

        let scopes: Vec<ScopeBucketStat> = SCOPE_LABELS
            .iter()
            .filter_map(|label| {
                let samples = by_scope.get(*label)?;
                let mut totals: Vec<i64> = samples.iter().map(|(_, t)| *t).collect();
                totals.sort_unstable();
                let count = samples.len();
                Some(ScopeBucketStat {
                    label: label.to_string(),
                    count: count as u64,
                    empty_rate: round4(
                        samples.iter().filter(|(h, _)| *h == 0).count() as f64 / count as f64,
                    ),
                    p95_total_ms: percentile(&totals, 95),
                })
            })
            .collect();

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
        let items = self.read(move |conn| {
            let (cutoff, limit) = (cutoff.clone(), limit);
            let mut stmt = conn
                .prepare(
                    "SELECT ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted
                     FROM retrieval_metrics WHERE ts >= ?1 ORDER BY total_ms DESC LIMIT ?2",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![cutoff, limit as i64], query_detail_row)
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }).await;
        Ok(items)
    }

    pub async fn empty_queries(
        &self,
        window_hours: u32,
        limit: u32,
    ) -> Result<Vec<RetrievalQueryDetail>, String> {
        let cutoff = cutoff(window_hours);
        let items = self.read(move |conn| {
            let (cutoff, limit) = (cutoff.clone(), limit);
            let mut stmt = conn
                .prepare(
                    "SELECT ts, source, query_text, total_ms, hit_count, scope_size, intent, path_boosted
                     FROM retrieval_metrics WHERE ts >= ?1 AND hit_count = 0 ORDER BY ts DESC LIMIT ?2",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![cutoff, limit as i64], query_detail_row)
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }).await;
        Ok(items)
    }

    // ───────────────────────── Token 用量

    pub async fn tokens(&self, window_hours: u32, bucket: &str) -> Result<TokensReport, String> {
        check_bucket(bucket)?;
        let cutoff = cutoff(window_hours);

        struct Row {
            ts: String,
            kind: String,
            model: String,
            credential_id: Option<i64>,
            prompt: i64,
            completion: i64,
            total: i64,
        }
        let rows: Vec<Row> = self.read(move |conn| {
            let cutoff = cutoff.clone();
            let mut stmt = conn
                .prepare(
                    "SELECT ts, kind, model, credential_id, prompt_tokens, completion_tokens, total_tokens
                     FROM token_usage_metrics WHERE ts >= ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&cutoff], |r| {
                    Ok(Row {
                        ts: r.get(0)?,
                        kind: r.get(1)?,
                        model: r.get(2)?,
                        credential_id: r.get(3)?,
                        prompt: r.get(4)?,
                        completion: r.get(5)?,
                        total: r.get(6)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }).await;

        let mut by_bucket: HashMap<(String, String), Vec<(i64, i64, i64)>> = HashMap::new();
        let mut by_model: HashMap<(String, String), Vec<(i64, i64, i64)>> = HashMap::new();
        let mut by_credential: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
        for row in &rows {
            let sums = (row.prompt, row.completion, row.total);
            by_bucket
                .entry((bucket_ts(&row.ts, bucket), row.kind.clone()))
                .or_default()
                .push(sums);
            by_model
                .entry((row.model.clone(), row.kind.clone()))
                .or_default()
                .push(sums);
            by_credential
                .entry(row.credential_id)
                .or_default()
                .push(row.total);
        }

        let mut buckets: Vec<TokenBucket> = by_bucket
            .into_iter()
            .map(|((ts, kind), samples)| TokenBucket {
                ts,
                kind,
                calls: samples.len() as u64,
                prompt_tokens: samples.iter().map(|(p, _, _)| *p).sum::<i64>() as u64,
                completion_tokens: samples.iter().map(|(_, c, _)| *c).sum::<i64>() as u64,
                total_tokens: samples.iter().map(|(_, _, t)| *t).sum::<i64>() as u64,
            })
            .collect();
        buckets.sort_by(|a, b| (&a.ts, &a.kind).cmp(&(&b.ts, &b.kind)));

        let mut models: Vec<ModelTokenStat> = by_model
            .into_iter()
            .map(|((model, kind), samples)| {
                let calls = samples.len();
                let total_tokens: i64 = samples.iter().map(|(_, _, t)| *t).sum();
                ModelTokenStat {
                    model,
                    kind,
                    calls: calls as u64,
                    prompt_tokens: samples.iter().map(|(p, _, _)| *p).sum::<i64>() as u64,
                    completion_tokens: samples.iter().map(|(_, c, _)| *c).sum::<i64>() as u64,
                    total_tokens: total_tokens as u64,
                    avg_tokens_per_call: round2(total_tokens as f64 / calls as f64),
                }
            })
            .collect();
        models.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

        let mut credentials: Vec<CredentialTokenStat> = by_credential
            .into_iter()
            .map(|(credential_id, totals)| CredentialTokenStat {
                credential_id,
                calls: totals.len() as u64,
                total_tokens: totals.iter().sum::<i64>() as u64,
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
            .read(|conn| {
                let count =
                    |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
                let blob_row: (i64, i64) = conn
                    .query_row(
                        "SELECT COUNT(*), COALESCE(SUM(content_size), 0) FROM blobs",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap_or((0, 0));
                let group = |sql: &str| -> Vec<CountStat> {
                    let Ok(mut stmt) = conn.prepare(sql) else {
                        return Vec::new();
                    };
                    let Ok(rows) = stmt.query_map([], |r| {
                        Ok(CountStat {
                            key: r
                                .get::<_, Option<String>>(0)?
                                .unwrap_or_else(|| "unknown".into()),
                            count: r.get::<_, i64>(1)?.max(0) as u64,
                        })
                    }) else {
                        return Vec::new();
                    };
                    rows.filter_map(|r| r.ok()).collect()
                };
                let now = Utc::now();
                let stale = |days: i64| -> u64 {
                    let threshold = (now - Duration::days(days))
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    conn.query_row(
                        "SELECT COUNT(*) FROM chains WHERE updated_at < ?1",
                        [&threshold],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                    .max(0) as u64
                };
                let chunk_row: (i64, i64) = conn
                    .query_row(
                        "SELECT COUNT(*), COALESCE(SUM(content_size), 0) FROM chunks",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap_or((0, 0));
                Ok(IndexInventoryReport {
                    blob_total: blob_row.0.max(0) as u64,
                    blob_by_status: group("SELECT status, COUNT(*) FROM blobs GROUP BY status"),
                    blob_by_language: {
                        let mut v = group("SELECT language, COUNT(*) FROM blobs GROUP BY language");
                        v.sort_by(|a, b| b.count.cmp(&a.count));
                        v.truncate(30);
                        v
                    },
                    blob_retrying: count("SELECT COUNT(*) FROM blobs WHERE retry_count > 0").max(0)
                        as u64,
                    blob_content_bytes: blob_row.1,
                    chunk_total: chunk_row.0.max(0) as u64,
                    chunk_pending_embed: count("SELECT COUNT(*) FROM chunks WHERE embedded = 0")
                        .max(0) as u64,
                    chunk_by_type: group(
                        "SELECT chunk_type, COUNT(*) FROM chunks GROUP BY chunk_type",
                    ),
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

        struct Row {
            ts: String,
            cpu: f64,
            mem_percent: f64,
            mem_rss: i64,
            disk_data: i64,
            disk_free: i64,
        }
        let rows: Vec<Row> = self.read(move |conn| {
            let cutoff = cutoff.clone();
            let mut stmt = conn
                .prepare(
                    "SELECT ts, cpu_percent, mem_percent, mem_rss_bytes, disk_data_bytes, disk_free_bytes
                     FROM resource_samples WHERE ts >= ?1 ORDER BY ts ASC",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&cutoff], |r| {
                    Ok(Row {
                        ts: r.get(0)?,
                        cpu: r.get(1)?,
                        mem_percent: r.get(2)?,
                        mem_rss: r.get(3)?,
                        disk_data: r.get(4)?,
                        disk_free: r.get(5)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        }).await;
        let latest: Option<(i64, i64)> = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT disk_total_bytes, disk_free_bytes FROM resource_samples ORDER BY ts DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok())
        }).await;

        let mut by_bucket: HashMap<String, Vec<&Row>> = HashMap::new();
        for row in &rows {
            by_bucket
                .entry(bucket_ts(&row.ts, bucket))
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
                    avg_cpu_percent: round2(
                        samples.iter().map(|r| r.cpu).sum::<f64>() / count as f64,
                    ),
                    max_cpu_percent: samples.iter().map(|r| r.cpu).fold(f64::MIN, f64::max),
                    avg_mem_percent: round2(
                        samples.iter().map(|r| r.mem_percent).sum::<f64>() / count as f64,
                    ),
                    max_mem_rss_bytes: samples.iter().map(|r| r.mem_rss).max().unwrap_or(0),
                    disk_data_bytes: last.disk_data,
                    disk_free_bytes: last.disk_free,
                }
            })
            .collect();
        buckets.sort_by(|a, b| a.ts.cmp(&b.ts));

        let mut growth_per_day = 0.0;
        if rows.len() >= 2 {
            let (first, last) = (&rows[0], &rows[rows.len() - 1]);
            if let (Ok(t0), Ok(t1)) = (
                DateTime::parse_from_rfc3339(&first.ts),
                DateTime::parse_from_rfc3339(&last.ts),
            ) {
                let elapsed = (t1 - t0).num_seconds();
                if elapsed >= 3600 {
                    growth_per_day = round2(
                        (last.disk_data - first.disk_data) as f64 / (elapsed as f64 / 86400.0),
                    );
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
            .read(|conn| {
                Ok(SPACE_TABLES
                    .iter()
                    .map(|name| {
                        let rows: i64 = conn
                            .query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |r| r.get(0))
                            .unwrap_or(0);
                        // dbstat 虚表可能未编译进 bundled SQLite；失败降级为估算标记
                        let dbstat: Option<Option<i64>> = conn
                            .query_row(
                                "SELECT SUM(pgsize) FROM dbstat WHERE name = ?1",
                                [name],
                                |r| r.get(0),
                            )
                            .ok();
                        match dbstat {
                            Some(bytes) => TableSpaceStat {
                                table: name.to_string(),
                                bytes: bytes.unwrap_or(0),
                                rows,
                                approximate: false,
                            },
                            None => TableSpaceStat {
                                table: name.to_string(),
                                bytes: 0,
                                rows,
                                approximate: true,
                            },
                        }
                    })
                    .collect())
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
                            let bytes = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
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
            dialect: "sqlite".into(),
            total_table_bytes: tables.iter().map(|t| t.bytes).sum(),
            tables,
            data_dir,
            data_files,
            data_dir_total_bytes: data_dir_total,
            vector,
        })
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

trait IntoMax {
    fn into_max(self) -> Option<String>;
}

impl IntoMax for Vec<String> {
    fn into_max(mut self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            self.sort();
            self.pop()
        }
    }
}

fn query_detail_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RetrievalQueryDetail> {
    Ok(RetrievalQueryDetail {
        ts: r.get(0)?,
        source: r.get(1)?,
        query_text: r.get(2)?,
        total_ms: r.get(3)?,
        hit_count: r.get(4)?,
        scope_size: r.get(5)?,
        intent: r.get(6)?,
        path_boosted: r.get::<_, i64>(7)? != 0,
    })
}
