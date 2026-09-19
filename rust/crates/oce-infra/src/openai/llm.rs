//! OpenAI 兼容 LLM 客户端（chat completions）+ TPM 滑窗限流 + API rerank 客户端。
//! 与 Python `openai_compatible_client.py` / `rate_limiter.py` / `openai_reranker.py` 对齐。

use futures::StreamExt;
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

/// enable_thinking 参数的三态：Auto = 按提供方域名探测，True/False = 显式覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriStateBool {
    #[default]
    Auto,
    True,
    False,
}

impl TriStateBool {
    /// 环境变量解析：""/"auto" → Auto，"1"/"true" → True，"0"/"false" → False。
    pub fn from_env(key: &str) -> Self {
        match std::env::var(key).ok().filter(|v| !v.is_empty()) {
            None => Self::Auto,
            Some(v) => match v.to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Self::True,
                "0" | "false" | "no" | "off" => Self::False,
                _ => Self::Auto,
            },
        }
    }
}

/// OpenAI 兼容 chat 客户端（rerank / rewrite / intent 共用）。
pub struct OpenAILlmClient {
    http: Client,
    base_url: String,
    api_key: String,
    tpm_limiter: Option<TokenRateLimiter>,
    on_usage: Option<UsageCallback>,
    credential_id: i64,
    /// 是否在请求体注入 enable_thinking=false（Qwen3 混合思考模型兼容）
    enable_thinking_param: TriStateBool,
    /// SSE 块间超时（秒）：流空闲超过此时长视为网关断连
    chunk_timeout_seconds: f64,
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
        Self::with_thinking(
            base_url,
            api_key,
            timeout_seconds,
            proxy,
            tpm_limit,
            on_usage,
            credential_id,
            TriStateBool::from_env("LLM_ENABLE_THINKING"),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_thinking(
        base_url: &str,
        api_key: &str,
        timeout_seconds: f64,
        proxy: Option<&str>,
        tpm_limit: i64,
        on_usage: Option<UsageCallback>,
        credential_id: i64,
        enable_thinking_param: TriStateBool,
    ) -> Result<Self, String> {
        // 超时语义（流式后）：connect_timeout 管连接建立；timeout 管整个请求。
        // SSE 长响应（思考模型未关思考时可生成几分钟）不能被总超时掐断——
        // 否则服务端还在生成、客户端已判失败重试，双重烧配额。
        // 改为：connect 30s + 每块间隔上限 = timeout_seconds（流式块间由
        // tokio::time::timeout 包裹，见 chat 的流式循环）。
        let mut builder = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs_f64(
                timeout_seconds.max(1.0) * 10.0,
            ));
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| e.to_string())?);
        }
        Ok(Self {
            http: builder.build().map_err(|e| e.to_string())?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            tpm_limiter: if tpm_limit > 0 {
                Some(TokenRateLimiter::new(tpm_limit))
            } else {
                None
            },
            on_usage,
            credential_id,
            enable_thinking_param,
            chunk_timeout_seconds: timeout_seconds.max(1.0),
        })
    }

    /// chat 一次，返回首条 message 内容（reasoning 兜底）。SSE 流式接收。
    ///
    /// 流式的必要性（实测教训）：Qwen3 混合思考模型未关思考时，非流式请求
    /// 在服务端生成思考链的几分钟内零字节返回——客户端 120s 总超时先到，
    /// 整单判失败重试，而服务端还在烧配额。流式下首块秒到，思考进度可见，
    /// 超时语义变为「首块超时」，真正的网关故障快速暴露。
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
            "stream": true,
        });
        if let Some(mt) = max_tokens {
            payload["max_tokens"] = json!(mt);
        }
        // 思考模型关思考（三态）：Auto = 探测注入；True = 强制注入；
        // False = 不注入任何思考参数（严格校验未知参数的提供方用）。
        // 注入参数按模型分派：Qwen3 系 enable_thinking=false；
        // MiniMax-M3 thinking.type=disabled（省略时思考默认开，不同字段）。
        // 实测教训：非流式 + 思考并开时单次调用可生成几分钟思考链，
        // 客户端总超时先到 → 重试双重烧配额。流式 + 关思考双保险。
        let base = self.base_url.to_lowercase();
        let model_lower = model.to_lowercase();
        let is_minimax_m3 =
            model_lower.contains("minimax-m3") || model_lower.contains("minimax_m3");
        match self.enable_thinking_param {
            TriStateBool::False => {}
            TriStateBool::True => {
                if is_minimax_m3 {
                    payload["thinking"] = json!({"type": "disabled"});
                } else {
                    payload["enable_thinking"] = json!(false);
                }
            }
            TriStateBool::Auto => {
                // 两路探测（实测教训：域名探测不够）：
                // 1. base_url 含已知提供方域名（SiliconFlow/DashScope/自建代理常见形态）；
                // 2. 模型名含 qwen3 —— Qwen3 系全是混合思考模型，且只在支持
                //    该参数的端点上可用（自定义代理域名探测不到，模型名是
                //    更强的信号）。qwen3.8-flash 这类端点靠这路兑住。
                const KNOWN_THINKING_PROVIDERS: [&str; 6] = [
                    "siliconflow",
                    "dashscope",
                    "aliyuncs",
                    "qwen",
                    "vllm",
                    "ollama",
                ];
                if KNOWN_THINKING_PROVIDERS.iter().any(|k| base.contains(k))
                    || model_lower.contains("qwen3")
                {
                    payload["enable_thinking"] = json!(false);
                }
                if is_minimax_m3 {
                    payload["thinking"] = json!({"type": "disabled"});
                }
            }
        }
        if payload.get("enable_thinking").is_some() || payload.get("thinking").is_some() {
            tracing::debug!("LLM request with thinking disabled (model={model})");
        } else {
            // 未注入：混合思考模型（Qwen3 系）会先生成思考链——流式下思考进度
            // 可见，但耗时仍在；需要关思考时设 LLM_ENABLE_THINKING=false
            tracing::debug!("LLM request without enable_thinking param (model={model})");
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
                tracing::warn!(
                    "LLM 429, retry {attempt}/{MAX_ATTEMPTS} after {RETRY_BACKOFF_SECONDS}s"
                );
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
                return Err(OceError::new(
                    format!("LLM API error: {status} {body}"),
                    "LlmError",
                ));
            }
            // SSE 流式接收：逐行拼 content（reasoning_content 兑底，与
            // 非流式版的 reasoning 字段兑底同语义）。usage 在最后一个
            // chunk（含 usage 字段）里，OpenAI 兼容端点 stream_options
            // 不统一，缺失时 usage 记 0（监控旁路容忍）。
            // 块间超时：思考模型生成中块间隔不会超过 timeout_seconds——
            // 超过即视为流已死（网关断连），快速失败进入重试。
            let per_chunk_timeout =
                std::time::Duration::from_secs_f64(self.chunk_timeout_seconds.max(1.0));
            let mut stream = resp.bytes_stream();
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut usage_prompt: i64 = 0;
            let mut usage_completion: i64 = 0;
            let mut buf = String::new();
            loop {
                let chunk = {
                    let next = stream.next();
                    match tokio::time::timeout(per_chunk_timeout, next).await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(_) => {
                            return Err(OceError::new(
                                format!(
                                    "llm stream idle over {:.0}s (gateway dropped)",
                                    per_chunk_timeout.as_secs_f64()
                                ),
                                "LlmError",
                            ))
                        }
                    }
                };
                let chunk =
                    chunk.map_err(|e| OceError::new(format!("llm stream: {e}"), "LlmError"))?;
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // SSE 事件以空行分隔；逐行处理已完整的行
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim_end_matches('\r').to_string();
                    buf.drain(..pos + 1);
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        continue;
                    }
                    let Ok(evt) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    if let Some(delta) = evt["choices"][0]["delta"]["content"].as_str() {
                        content.push_str(delta);
                    }
                    if let Some(delta) = evt["choices"][0]["delta"]["reasoning_content"].as_str() {
                        reasoning.push_str(delta);
                    }
                    if let Some(u) = evt["usage"].as_object() {
                        usage_prompt = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                        usage_completion = u
                            .get("completion_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                    }
                }
            }
            let content = if content.is_empty() {
                reasoning
            } else {
                content
            };
            if let Some(cb) = &self.on_usage {
                if usage_prompt != 0 || usage_completion != 0 {
                    cb(
                        self.credential_id,
                        "llm",
                        model,
                        usage_prompt,
                        usage_completion,
                    );
                }
            }
            return Ok(content);
        }
        Err(OceError::new(
            "LLM chat exhausted retries without a response",
            "LlmError",
        ))
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
            return Err(OceError::new(
                format!("rerank API {status}: {body}"),
                "RerankError",
            ));
        }
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| OceError::new(e.to_string(), "RerankError"))?;
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
            cb(
                self.credential_id,
                "rerank",
                &self.model,
                prompt,
                completion,
            );
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tri_state_bool_env_parsing() {
        // 环境变量测试需串行（std::env 全局态）；用唯一 key 避免碰撞
        let key = "OCE_TEST_TRI_STATE_D9E2";
        std::env::remove_var(key);
        assert_eq!(TriStateBool::from_env(key), TriStateBool::Auto);
        std::env::set_var(key, "true");
        assert_eq!(TriStateBool::from_env(key), TriStateBool::True);
        std::env::set_var(key, "0");
        assert_eq!(TriStateBool::from_env(key), TriStateBool::False);
        std::env::set_var(key, "auto");
        assert_eq!(TriStateBool::from_env(key), TriStateBool::Auto);
        std::env::remove_var(key);
    }

    #[test]
    fn thinking_param_injected_for_known_providers() {
        // 已知提供方（auto 模式）→ 客户端构建成功即可；注入逻辑在请求体层，
        // 此处验证的是探测分支不 panic 且客户端可构造
        let client = OpenAILlmClient::with_thinking(
            "https://api.siliconflow.cn/v1",
            "k",
            5.0,
            None,
            0,
            None,
            0,
            TriStateBool::Auto,
        );
        assert!(client.is_ok());
    }

    /// SSE 行解析：跨 chunk 边界的 data: 行、CRLF、[DONE]、reasoning/content
    /// 混合、usage 尾包。提取为独立函数供测试（与 chat 流式循环同逻辑）。
    #[test]
    fn sse_stream_parsing_handles_chunk_boundaries() {
        // 模拟字节流：一个 data 行被切在两个 chunk 中间
        let chunks: Vec<&[u8]> = vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"He" as &[u8],
            b"llo\"}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think...\"}}]}\r\n",
            b"\r\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            b"data: [DONE]\n\n",
        ];
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage_prompt = 0i64;
        let mut usage_completion = 0i64;
        let mut buf = String::new();
        for chunk in chunks {
            buf.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..pos + 1);
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(evt) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                if let Some(delta) = evt["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(delta);
                }
                if let Some(delta) = evt["choices"][0]["delta"]["reasoning_content"].as_str() {
                    reasoning.push_str(delta);
                }
                if let Some(u) = evt["usage"].as_object() {
                    usage_prompt = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                    usage_completion = u
                        .get("completion_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                }
            }
        }
        assert_eq!(content, "Hello world", "跨 chunk 的 content 增量拼接");
        assert_eq!(reasoning, "think...", "思考链增量单独累积");
        assert_eq!((usage_prompt, usage_completion), (10, 5), "usage 尾包提取");
    }
}
