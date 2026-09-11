//! 监控 sink：批量缓冲 + 异步落库（对应 Python `sql_metrics_sink.py`）。
//! 旁路且非阻塞：采集失败只跳过，不影响检索主链路。

use crate::sqlite::SqlDb;
use oce_core::metrics::{MetricsSink, RetrievalMetricRecord, TokenUsageRecord};
use std::sync::Mutex;

/// 批量缓冲 sink：record_* 同步入缓冲，后台任务按间隔 flush。
pub struct SqlMetricsSink {
    buffer: Mutex<SinkBuffer>,
    db: SqlDb,
}

#[derive(Default)]
struct SinkBuffer {
    tokens: Vec<TokenUsageRecord>,
    retrievals: Vec<RetrievalMetricRecord>,
}

impl SqlMetricsSink {
    pub fn new(db: SqlDb) -> Self {
        Self {
            buffer: Mutex::new(SinkBuffer::default()),
            db,
        }
    }

    /// 立即落库缓冲内容（后台 flush 任务或 drop 前调用）。
    pub async fn flush(&self) {
        let (tokens, retrievals) = {
            let Ok(mut buf) = self.buffer.lock() else { return };
            (
                std::mem::take(&mut buf.tokens),
                std::mem::take(&mut buf.retrievals),
            )
        };
        if tokens.is_empty() && retrievals.is_empty() {
            return;
        }
        let db = self.db.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = db.with_conn(|conn| {
                for t in &tokens {
                    let _ = conn.execute(
                        "INSERT INTO token_usage_metrics (kind, model, credential_id, prompt_tokens, completion_tokens, total_tokens)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![t.kind, t.model, t.credential_id, t.prompt_tokens as i64, t.completion_tokens as i64, t.total_tokens as i64],
                    );
                }
                for r in &retrievals {
                    let _ = conn.execute(
                        "INSERT INTO retrieval_metrics
                         (source, scope_size, hit_count, total_ms, intent, path_boosted, query_text,
                          intent_ms, rewrite_ms, dense_ms, exact_ms, fuse_ms, rerank_ms, llm_rerank_ms, select_ms)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                        rusqlite::params![
                            r.source, r.scope_size, r.hit_count, r.total_ms, r.intent,
                            r.path_boosted as i64, r.query_text,
                            r.intent_ms, r.rewrite_ms, r.dense_ms, r.exact_ms, r.fuse_ms,
                            r.rerank_ms, r.llm_rerank_ms, r.select_ms,
                        ],
                    );
                }
                Ok(())
            });
        })
        .await;
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
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let cutoff = (chrono::Utc::now()
                    - chrono::Duration::hours(window_hours as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let api_calls: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM api_call_metrics WHERE ts >= ?1",
                        [&cutoff],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let avg_latency: f64 = conn
                    .query_row(
                        "SELECT COALESCE(AVG(latency_ms), 0) FROM api_call_metrics WHERE ts >= ?1",
                        [&cutoff],
                        |r| r.get(0),
                    )
                    .unwrap_or(0.0);
                let error_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM api_call_metrics WHERE ts >= ?1 AND status_code >= 400",
                        [&cutoff],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let mut stmt = conn
                    .prepare(
                        "SELECT kind, model, COUNT(*), COALESCE(SUM(prompt_tokens),0),
                                COALESCE(SUM(completion_tokens),0), COALESCE(SUM(total_tokens),0)
                         FROM token_usage_metrics WHERE ts >= ?1 GROUP BY kind, model",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([&cutoff], |r| {
                        Ok(TokenKindStats {
                            kind: r.get(0)?,
                            model: r.get(1)?,
                            calls: r.get::<_, i64>(2)? as u64,
                            prompt_tokens: r.get::<_, i64>(3)? as u64,
                            completion_tokens: r.get::<_, i64>(4)? as u64,
                            total_tokens: r.get::<_, i64>(5)? as u64,
                        })
                    })
                    .map_err(|e| e.to_string())?;
                let tokens: Vec<TokenKindStats> = rows.filter_map(|r| r.ok()).collect();
                let retrieval_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM retrieval_metrics WHERE ts >= ?1",
                        [&cutoff],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let empty_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM retrieval_metrics WHERE ts >= ?1 AND hit_count = 0",
                        [&cutoff],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                Ok(MonitoringStats {
                    api_calls: ApiCallStats {
                        calls: api_calls as u64,
                        avg_latency_ms: avg_latency,
                        error_count: error_count as u64,
                    },
                    tokens,
                    retrieval: RetrievalStats {
                        count: retrieval_count as u64,
                        empty_count: empty_count as u64,
                    },
                })
            })
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        result.unwrap_or_default()
    }

    /// 清理过期监控行（对应 Python MonitoringCleaner）。
    pub async fn cleanup(&self, retention_days: u32) {
        let db = self.db.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = db.with_conn(|conn| {
                let cutoff = (chrono::Utc::now()
                    - chrono::Duration::days(retention_days as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                for table in [
                    "api_call_metrics",
                    "token_usage_metrics",
                    "resource_samples",
                    "retrieval_metrics",
                ] {
                    let _ = conn.execute(&format!("DELETE FROM {table} WHERE ts < ?1"), [&cutoff]);
                }
                Ok(())
            });
        })
        .await;
    }

    /// 记录一次 API 调用（对应 ApiCallMetricModel）。
    pub async fn record_api_call(&self, endpoint: &str, method: &str, status_code: u16, latency_ms: u64, error_type: Option<String>) {
        let db = self.db.clone();
        let (endpoint, method) = (endpoint.to_string(), method.to_string());
        let _ = tokio::task::spawn_blocking(move || {
            let _ = db.with_conn(|conn| {
                let _ = conn.execute(
                    "INSERT INTO api_call_metrics (endpoint, method, status_code, latency_ms, error_type)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![endpoint, method, status_code as i64, latency_ms as i64, error_type],
                );
                Ok(())
            });
        })
        .await;
    }
}

impl MetricsSink for SqlMetricsSink {
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
}

/// /admin/stats 读模型。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MonitoringStats {
    pub api_calls: ApiCallStats,
    pub tokens: Vec<TokenKindStats>,
    pub retrieval: RetrievalStats,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ApiCallStats {
    pub calls: u64,
    pub avg_latency_ms: f64,
    pub error_count: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenKindStats {
    pub kind: String,
    pub model: String,
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RetrievalStats {
    pub count: u64,
    pub empty_count: u64,
}
