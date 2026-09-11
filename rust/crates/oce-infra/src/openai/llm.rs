//! OpenAI 兼容 LLM 客户端（chat completions）+ TPM 滑窗限流 + API rerank 客户端。
//! 与 Python `openai_compatible_client.py` / `rate_limiter.py` / `openai_reranker.py` 对齐。

use oce_core::error::{OceError, OceResult};
use reqwest::Client;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 用量回调：(credential_id, kind, model, prompt_tokens, completion_tokens)。
pub type UsageCallback = Arc<dyn Fn(i64, &str, &str, i64, i64) + Send + Sync>;

/// 粗估 token 数，宁可高估以免触发 429。
/// 中日韩字符约 1 token/字，其余约 3 字符/token。
pub fn estimate_tokens(text: &str) -> i64 {
    let mut cjk = 0i64;
    let mut total = 0i64;
    for ch in text.chars() {
        total += 1;
        if ('\u{4e00}'..='\u{9fff}').contains(&ch) || ('\u{3040}'..='\u{30ff}').contains(&ch) {
            cjk += 1;
        }
    }
    let other = total - cjk;
    cjk + other / 3 + 1
}

/// 滑动窗口 TPM 限流器（对应 Python TokenRateLimiter）。
pub struct TokenRateLimiter {
    budget: i64,
    window: std::time::Duration,
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    events: VecDeque<(std::time::Instant, i64)>,
    used: i64,
}

impl TokenRateLimiter {
    pub fn new(tokens_per_minute: i64) -> Self {
        Self {
            budget: (tokens_per_minute as f64 * 0.9) as i64,
            window: std::time::Duration::from_secs(60),
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// 申请额度，不足则等待窗口滑动。返回累计等待秒数。
    pub async fn acquire(&self, tokens: i64) -> i64 {
        let need = tokens.clamp(1, self.budget);
        let mut waited: i64 = 0;
        loop {
            let sleep_for = {
                let mut state = self.state.lock().await;
                let now = std::time::Instant::now();
                // 滑出窗口的记账出队
                while let Some((ts, _)) = state.events.front() {
                    if now.duration_since(*ts) >= self.window {
                        let (_, tokens) = state.events.pop_front().unwrap();
                        state.used -= tokens;
                    } else {
                        break;
                    }
                }
                if state.used + need <= self.budget {
                    state.events.push_back((now, need));
                    state.used += need;
                    return waited;
                }
                // 最早一笔记账滑出窗口后才可能腾出额度
                match state.events.front() {
                    Some((ts, _)) => {
                        let elapsed = now.duration_since(*ts);
                        self.window.saturating_sub(elapsed)
                    }
                    None => std::time::Duration::from_millis(50),
                }
            };
            let sleep_ms = sleep_for.as_millis().max(50) as u64;
            waited += sleep_ms as i64;
            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
        }
    }
}

const OUTPUT_TOKEN_ALLOWANCE: i64 = 256;
const MAX_ATTEMPTS: u32 = 5;
const RETRY_BACKOFF_SECONDS: f64 = 20.0;
/// 504/网关超时重试前的额外等待：网关冷启动或上游拥塞时立即重试只会再吃一个 504
const GATEWAY_RETRY_BACKOFF_SECONDS: f64 = 30.0;

/// OpenAI 兼容 chat 客户端（rerank / rewrite / intent 共用）。
pub struct OpenAILlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    tpm_limiter: Option<TokenRateLimiter>,
    on_usage: Option<UsageCallback>,
    credential_id: i64,
}

#[allow(clippy::too_many_arguments)]
impl OpenAILlmClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        timeout_seconds: f64,
        proxy: Option<&str>,
        tpm_limit: i64,
        on_usage: Option<UsageCallback>,
        credential_id: i64,
    ) -> Result<Self, String> {
        let mut builder = Client::builder().timeout(std::time::Duration::from_secs_f64(timeout_seconds.max(1.0)));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| e.to_string())?);
        }
        Ok(Self {
            http: builder.build().map_err(|e| e.to_string())?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            tpm_limiter: if tpm_limit > 0 { Some(TokenRateLimiter::new(tpm_limit)) } else { None },
            on_usage,
            credential_id,
        })
    }

    /// chat 一次，返回首条 message 内容（reasoning 兜底）。
    pub async fn chat(
        &self,
        messages: &[serde_json::Value],
        model: &str,
        temperature: f32,
        max_tokens: Option<i64>,
    ) -> OceResult<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut payload = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
        });
        if let Some(mt) = max_tokens {
            payload["max_tokens"] = json!(mt);
        }

        let estimated: i64 = messages
            .iter()
            .map(|m| estimate_tokens(m["content"].as_str().unwrap_or("")))
            .sum::<i64>()
            + OUTPUT_TOKEN_ALLOWANCE;

        for attempt in 1..=MAX_ATTEMPTS {
            if let Some(limiter) = &self.tpm_limiter {
                let waited = limiter.acquire(estimated).await;
                if waited > 0 {
                    tracing::debug!("TPM limiter delayed request by {waited}ms");
                }
            }
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&payload)
                .send()
                .await
                .map_err(|e| OceError::new(format!("llm request: {e}"), "LlmError"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_ATTEMPTS {
                tracing::warn!("LLM 429, retry {attempt}/{MAX_ATTEMPTS} after {RETRY_BACKOFF_SECONDS}s");
                tokio::time::sleep(std::time::Duration::from_secs_f64(RETRY_BACKOFF_SECONDS)).await;
                continue;
            }
            // 网关类超时（504 Gateway Timeout / 502 / 503）重试：长 prompt 非流式
            // 请求在拥塞网关上会间歇性 504，重试通常能过；指数退避避免连续撞墙
            if (status == reqwest::StatusCode::GATEWAY_TIMEOUT
                || status == reqwest::StatusCode::BAD_GATEWAY
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE)
                && attempt < MAX_ATTEMPTS
            {
                let backoff = GATEWAY_RETRY_BACKOFF_SECONDS * (1 << (attempt - 1)) as f64;
                tracing::warn!(
                    "LLM {status} (gateway), retry {attempt}/{MAX_ATTEMPTS} after {backoff:.0}s"
                );
                tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
                continue;
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(OceError::new(format!("LLM API error: {status} {body}"), "LlmError"));
            }
            let data: serde_json::Value =
                resp.json().await.map_err(|e| OceError::new(e.to_string(), "LlmError"))?;
            let mut content = data["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if content.is_empty() {
                content = data["choices"][0]["message"]["reasoning"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
            }
            if let Some(cb) = &self.on_usage {
                let prompt = data["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
                let completion = data["usage"]["completion_tokens"].as_i64().unwrap_or(0);
                if prompt != 0 || completion != 0 {
                    cb(self.credential_id, "llm", model, prompt, completion);
                }
            }
            return Ok(content);
        }
        Err(OceError::new("LLM chat exhausted retries without a response", "LlmError"))
    }
}

/// SiliconFlow/Cohere 风格 rerank 客户端（对应 Python OpenAIReranker）。
pub struct OpenAIReranker {
    http: Client,
    endpoint: String,
    api_key: String,
    model: String,
    top_n: usize,
    min_score: f32,
    on_usage: Option<UsageCallback>,
    credential_id: i64,
}

impl OpenAIReranker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: &str,
        api_key: &str,
        model: &str,
        top_n: usize,
        min_score: f32,
        timeout_seconds: f64,
        on_usage: Option<UsageCallback>,
        credential_id: i64,
    ) -> Result<Self, String> {
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs_f64(timeout_seconds.max(1.0)))
                .build()
                .map_err(|e| e.to_string())?,
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            top_n,
            min_score,
            on_usage,
            credential_id,
        })
    }

    /// 重排：documents 由 query+content 组成；返回 (hit, score) 按 relevance 降序，
    /// 低于 min_score 的被过滤。
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
    ) -> OceResult<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(vec![]);
        }
        let top_n = top_n.unwrap_or(self.top_n);
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": self.model,
                "query": query,
                "documents": documents,
                "top_n": top_n,
            }))
            .send()
            .await
            .map_err(|e| OceError::new(format!("rerank request: {e}"), "RerankError"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OceError::new(format!("rerank API {status}: {body}"), "RerankError"));
        }
        let data: serde_json::Value =
            resp.json().await.map_err(|e| OceError::new(e.to_string(), "RerankError"))?;
        let mut results: Vec<(usize, f32)> = data["results"]
            .as_array()
            .ok_or_else(|| OceError::new("rerank missing results", "RerankError"))?
            .iter()
            .filter_map(|item| {
                Some((
                    item["index"].as_u64()? as usize,
                    item["relevance_score"].as_f64()? as f32,
                ))
            })
            .filter(|(_, score)| *score >= self.min_score)
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_n);
        if let Some(cb) = &self.on_usage {
            let prompt = data["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
            let completion = data["usage"]["completion_tokens"].as_i64().unwrap_or(0);
            cb(self.credential_id, "rerank", &self.model, prompt, completion);
        }
        Ok(results)
    }
}
