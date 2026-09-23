//! PostgreSQL 监控 sink：批量缓冲 + 异步落库（与 SQLite 版 `sqlite/metrics.rs` 对齐）。
//! 旁路且非阻塞：采集失败只跳过，不影响检索主链路。

use oce_core::metrics::{
    ApiCallRecord, MetricsSink, MonitoringStats, MonitoringStatsReader, ResourceSampleRecord,
    RetrievalMetricRecord, TokenKindStats, TokenUsageRecord, ApiCallStats, RetrievalStats,
    ResourceSnapshot,
};
use sqlx::PgPool;
use std::sync::Mutex;

/// 批量缓冲 sink：record_* 同步入缓冲，后台任务按间隔 flush。
pub struct PgMetricsSink {
    buffer: Mutex<SinkBuffer>,
    pool: PgPool,
}

#[derive(Default)]
struct SinkBuffer {
    tokens: Vec<TokenUsageRecord>,
    retrievals: Vec<RetrievalMetricRecord>,
    api_calls: Vec<ApiCallRecord>,
    resources: Vec<ResourceSampleRecord>,
}

impl PgMetricsSink {
    pub fn new(pool: PgPool) -> Self {
        Self {
            buffer: Mutex::new(SinkBuffer::default()),
            pool,
        }
    }

    /// 立即落库缓冲内容（后台 flush 任务或 drop 前调用）。
    pub async fn flush(&self) {
        let (tokens, retrievals, api_calls, resources) = {
            let Ok(mut buf) = self.buffer.lock() else {
                return;
            };
            (
                std::mem::take(&mut buf.tokens),
                std::mem::take(&mut buf.retrievals),
                std::mem::take(&mut buf.api_calls),
                std::mem::take(&mut buf.resources),
            )
        };
        if tokens.is_empty()
            && retrievals.is_empty()
            && api_calls.is_empty()
            && resources.is_empty()
        {
            return;
        }
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => return, // 旁路：连接失败静默丢弃本批
        };
        for t in &tokens {
            let _ = sqlx::query(
                "INSERT INTO token_usage_metrics (kind, model, credential_id, prompt_tokens, completion_tokens, total_tokens)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&t.kind)
            .bind(&t.model)
            .bind(t.credential_id)
            .bind(t.prompt_tokens as i64)
            .bind(t.completion_tokens as i64)
            .bind(t.total_tokens as i64)
            .execute(&mut *tx)
            .await;
        }
        for r in &retrievals {
            let _ = sqlx::query(
                "INSERT INTO retrieval_metrics
                 (source, scope_size, hit_count, total_ms, intent, path_boosted, query_text,
                  intent_ms, rewrite_ms, dense_ms, exact_ms, fuse_ms, rerank_ms, llm_rerank_ms, select_ms)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
            )
            .bind(&r.source)
            .bind(r.scope_size)
            .bind(r.hit_count)
            .bind(r.total_ms)
            .bind(&r.intent)
            .bind(r.path_boosted)
            .bind(&r.query_text)
            .bind(r.intent_ms)
            .bind(r.rewrite_ms)
            .bind(r.dense_ms)
            .bind(r.exact_ms)
            .bind(r.fuse_ms)
            .bind(r.rerank_ms)
            .bind(r.llm_rerank_ms)
            .bind(r.select_ms)
            .execute(&mut *tx)
            .await;
        }
        for a in &api_calls {
            let _ = sqlx::query(
                "INSERT INTO api_call_metrics (endpoint, method, status_code, latency_ms, error_type)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&a.endpoint)
            .bind(&a.method)
            .bind(a.status_code as i32)
            .bind(a.latency_ms as i32)
            .bind(&a.error_type)
            .execute(&mut *tx)
            .await;
        }
        for s in &resources {
            let _ = sqlx::query(
                "INSERT INTO resource_samples (disk_data_bytes, disk_free_bytes, disk_total_bytes, mem_rss_bytes, mem_percent, cpu_percent)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(s.disk_data_bytes as i64)
            .bind(s.disk_free_bytes as i64)
            .bind(s.disk_total_bytes as i64)
            .bind(s.mem_rss_bytes as i64)
            .bind(s.mem_percent)
            .bind(s.cpu_percent)
            .execute(&mut *tx)
            .await;
        }
        let _ = tx.commit().await;
    }

    /// 启动周期 flush 任务（对应 Python sink.start()）。
    pub fn spawn_flush_task(self: std::sync::Arc<Self>, interval_seconds: f64) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs_f64(
                interval_seconds.max(1.0),
            ));
            loop {
                ticker.tick().await;
                self.flush().await;
            }
        });
    }

    /// 按窗口统计（对应 /admin/stats 查询）。
    pub async fn stats(&self, window_hours: u32) -> MonitoringStats {
        stats_impl(&self.pool, window_hours).await.unwrap_or_default()
    }

    /// 清理过期监控行（对应 Python MonitoringCleaner）。
    pub async fn cleanup(&self, retention_days: u32) {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(_) => return,
        };
        for table in [
            "api_call_metrics",
            "token_usage_metrics",
            "resource_samples",
            "retrieval_metrics",
        ] {
            let _ = sqlx::query(&format!("DELETE FROM {table} WHERE ts < $1"))
                .bind(cutoff)
                .execute(&mut *tx)
                .await;
        }
        let _ = tx.commit().await;
    }

    /// 启动周期清理任务（对应 Python MonitoringCleaner：按 retention_days 清过期监控行）。
    pub fn spawn_cleanup_task(
        self: std::sync::Arc<Self>,
        retention_days: u32,
        interval_seconds: f64,
    ) {
        let sink = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs_f64(
                interval_seconds.max(60.0),
            ));
            ticker.tick().await; // 首个 tick 立即触发，跳过后再进入周期
            loop {
                ticker.tick().await;
                sink.cleanup(retention_days).await;
            }
        });
    }
}

async fn stats_impl(pool: &PgPool, window_hours: u32) -> Result<MonitoringStats, sqlx::Error> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(window_hours as i64);
    let (api_calls, avg_latency, error_count): (i64, f64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(AVG(latency_ms), 0)::float8, COUNT(*) FILTER (WHERE status_code >= 400)
         FROM api_call_metrics WHERE ts >= $1",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    let tokens: Vec<(String, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT kind, model, COUNT(*), COALESCE(SUM(prompt_tokens),0),
                COALESCE(SUM(completion_tokens),0), COALESCE(SUM(total_tokens),0)
         FROM token_usage_metrics WHERE ts >= $1 GROUP BY kind, model",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let (retrieval_count, empty_count): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE hit_count = 0)
         FROM retrieval_metrics WHERE ts >= $1",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    // 最新资源快照（无样本时 None，对应 Python resource 字段可空）
    let resource: Option<(chrono::DateTime<chrono::Utc>, i64, f64, f64, i64, i64, i64)> =
        sqlx::query_as(
            "SELECT ts, mem_rss_bytes, mem_percent, cpu_percent, disk_free_bytes, disk_total_bytes, disk_data_bytes
             FROM resource_samples ORDER BY ts DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
    Ok(MonitoringStats {
        api_calls: ApiCallStats {
            calls: api_calls.max(0) as u64,
            avg_latency_ms: avg_latency,
            error_count: error_count.max(0) as u64,
        },
        tokens: tokens
            .into_iter()
            .map(|(kind, model, calls, prompt, completion, total)| TokenKindStats {
                kind,
                model,
                calls: calls.max(0) as u64,
                prompt_tokens: prompt.max(0) as u64,
                completion_tokens: completion.max(0) as u64,
                total_tokens: total.max(0) as u64,
            })
            .collect(),
        retrieval: RetrievalStats {
            count: retrieval_count.max(0) as u64,
            empty_count: empty_count.max(0) as u64,
        },
        resource: resource.map(
            |(ts, mem_rss_bytes, mem_percent, cpu_percent, disk_free_bytes, disk_total_bytes, disk_data_bytes)| {
                ResourceSnapshot {
                    ts: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    mem_rss_bytes: mem_rss_bytes.max(0) as u64,
                    mem_percent,
                    cpu_percent,
                    disk_free_bytes: disk_free_bytes.max(0) as u64,
                    disk_total_bytes: disk_total_bytes.max(0) as u64,
                    disk_data_bytes: disk_data_bytes.max(0) as u64,
                }
            },
        ),
    })
}

impl MetricsSink for PgMetricsSink {
    fn record_token_usage(&self, record: TokenUsageRecord) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.tokens.push(record);
        }
    }

    fn record_retrieval(&self, record: RetrievalMetricRecord) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.retrievals.push(record);
        }
    }

    fn record_api_call(&self, record: ApiCallRecord) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.api_calls.push(record);
        }
    }

    fn record_resource_sample(&self, record: ResourceSampleRecord) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.resources.push(record);
        }
    }
}

#[async_trait::async_trait]
impl MonitoringStatsReader for PgMetricsSink {
    async fn stats(&self, window_hours: u32) -> MonitoringStats {
        PgMetricsSink::stats(self, window_hours).await
    }
}

