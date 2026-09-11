//! 凭据解析运行时（对应 Python `CredentialConfiguredEmbedder` / `CredentialConfiguredReranker` /
//! `CredentialConfiguredLLMClient`）。
//!
//! 语义：首次调用时从 model_credentials 解析（kind + active + 最小 priority），
//! 取不到回落环境变量；admin reload 原子替换 delegate（Arc 保证旧实例在飞行中调用
//! 结束前存活）。维度不匹配 → ServiceNotReady。

use crate::openai::embedder::{OpenAIEmbedder, UsageCallback};
use crate::openai::llm::OpenAILlmClient;
use crate::settings::{EmbeddingSettings, LlmSettings, RerankSettings};
use crate::sqlite::credentials::{RuntimeCredential, SqlCredentialAdminStore};
use oce_core::error::{OceError, OceResult};
use async_trait::async_trait;
use oce_core::search::Embedder;
use std::sync::Arc;
use tokio::sync::RwLock;

/// embedding 运行时：延迟解析 + 热重载。
pub struct CredentialConfiguredEmbedder {
    store: SqlCredentialAdminStore,
    fallback: EmbeddingSettings,
    expected_dimensions: usize,
    on_usage: Option<UsageCallback>,
    delegate: RwLock<Option<Arc<OpenAIEmbedder>>>,
}

impl CredentialConfiguredEmbedder {
    pub fn new(
        store: SqlCredentialAdminStore,
        fallback: EmbeddingSettings,
        expected_dimensions: usize,
        on_usage: Option<UsageCallback>,
    ) -> Self {
        Self {
            store,
            fallback,
            expected_dimensions,
            on_usage,
            delegate: RwLock::new(None),
        }
    }

    async fn resolve_config(&self) -> OceResult<EmbedRuntimeConfig> {
        let credential = self.store.resolve_active("embed").await?;
        let config = match credential {
            None => {
                let key = self.fallback.api_key.clone().unwrap_or_default();
                if key.is_empty() {
                    return Err(OceError::service_not_ready(Some(
                        "No active embedding credential or EMBED_API_KEY is configured",
                    )));
                }
                let fb = &self.fallback;
                EmbedRuntimeConfig {
                    endpoint: fb.endpoint.clone(),
                    api_key: key,
                    model: fb.model.clone(),
                    dimensions: fb.dimensions,
                    max_batch_size: fb.max_batch_size,
                    max_batch_chars: fb.max_batch_chars,
                    max_input_chars: fb.max_input_chars,
                    input_overlap_chars: fb.input_overlap_chars,
                    max_concurrency: fb.max_concurrency,
                    timeout_seconds: fb.timeout_seconds,
                    proxy: fb.proxy.clone(),
                    credential_id: 0,
                }
            }
            Some(cred) => {
                let fb = &self.fallback;
                // kind 专属参数列可能为空，逐字段回落
                EmbedRuntimeConfig {
                    endpoint: cred.endpoint,
                    api_key: cred.api_key,
                    model: cred.model,
                    dimensions: cred.dimensions.map(|d| d as usize).unwrap_or(fb.dimensions),
                    max_batch_size: cred.max_batch_size.map(|v| v as usize).unwrap_or(fb.max_batch_size),
                    max_batch_chars: cred.max_batch_chars.map(|v| v as usize).unwrap_or(fb.max_batch_chars),
                    max_input_chars: cred.max_input_chars.map(|v| v as usize).unwrap_or(fb.max_input_chars),
                    input_overlap_chars: cred
                        .input_overlap_chars
                        .map(|v| v as usize)
                        .unwrap_or(fb.input_overlap_chars),
                    max_concurrency: fb.max_concurrency,
                    timeout_seconds: cred.timeout_seconds as f64,
                    proxy: fb.proxy.clone(),
                    credential_id: cred.id,
                }
            }
        };
        if config.dimensions != self.expected_dimensions {
            return Err(OceError::service_not_ready(Some(
                "Embedding credential dimensions do not match MILVUS_DENSE_DIM",
            )));
        }
        Ok(config)
    }

    async fn build_delegate(
        &self,
        config: &EmbedRuntimeConfig,
    ) -> OceResult<Arc<OpenAIEmbedder>> {
        Ok(Arc::new(
            OpenAIEmbedder::new(
                &config.endpoint,
                &config.api_key,
                &config.model,
                config.dimensions,
                config.max_batch_size,
                config.max_concurrency,
                config.max_batch_chars,
                config.max_input_chars,
                config.input_overlap_chars,
                config.timeout_seconds,
                config.proxy.as_deref(),
                // query_instruction 只作用于查询侧：始终取 env/settings 层配置
                // （Qwen3-Embedding 官方支持按场景写指令，此前被硬编码 "" 丢弃）
                self.fallback.query_instruction.as_str(),
                config.credential_id,
                self.on_usage.clone(),
            )
            .map_err(|m| OceError::new(m, "EmbeddingError"))?,
        ))
    }

    async fn acquire_delegate(&self) -> OceResult<Arc<OpenAIEmbedder>> {
        {
            let guard = self.delegate.read().await;
            if let Some(d) = guard.as_ref() {
                return Ok(d.clone());
            }
        }
        let mut guard = self.delegate.write().await;
        if let Some(d) = guard.as_ref() {
            return Ok(d.clone());
        }
        let config = self.resolve_config().await?;
        let d = self.build_delegate(&config).await?;
        *guard = Some(d.clone());
        Ok(d)
    }

    /// 热重载：解析并替换 delegate；失败时旧实例保持不变。
    pub async fn reload(&self) -> OceResult<usize> {
        let config = self.resolve_config().await?;
        let replacement = self.build_delegate(&config).await?;
        *self.delegate.write().await = Some(replacement);
        Ok(1)
    }
}

#[derive(Debug, Clone)]
struct EmbedRuntimeConfig {
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: usize,
    max_batch_size: usize,
    max_batch_chars: usize,
    max_input_chars: usize,
    input_overlap_chars: usize,
    max_concurrency: usize,
    timeout_seconds: f64,
    proxy: Option<String>,
    credential_id: i64,
}

#[async_trait]
impl Embedder for CredentialConfiguredEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>> {
        let delegate = self.acquire_delegate().await?;
        delegate.embed_documents(texts).await
    }

    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>> {
        let delegate = self.acquire_delegate().await?;
        delegate.embed_query(text).await
    }
}

/// LLM 客户端运行时（llm_rerank / query_rewrite / intent 各一份，kind 区分）。
pub struct CredentialConfiguredLlmClient {
    store: SqlCredentialAdminStore,
    kind: String,
    fallback: LlmSettings,
    fallback_model: String,
    on_usage: Option<crate::openai::llm::UsageCallback>,
    delegate: RwLock<Option<Arc<OpenAILlmClient>>>,
}

impl CredentialConfiguredLlmClient {
    pub fn new(
        store: SqlCredentialAdminStore,
        kind: &str,
        fallback: LlmSettings,
        fallback_model: String,
        on_usage: Option<crate::openai::llm::UsageCallback>,
    ) -> Self {
        Self {
            store,
            kind: kind.to_string(),
            fallback,
            fallback_model,
            on_usage,
            delegate: RwLock::new(None),
        }
    }

    async fn acquire(&self) -> OceResult<Arc<OpenAILlmClient>> {
        {
            let guard = self.delegate.read().await;
            if let Some(d) = guard.as_ref() {
                return Ok(d.clone());
            }
        }
        let mut guard = self.delegate.write().await;
        if let Some(d) = guard.as_ref() {
            return Ok(d.clone());
        }
        let cred: Option<RuntimeCredential> = self.store.resolve_active(&self.kind).await?;
        let (base_url, api_key, tpm, credential_id) = match cred {
            Some(c) => (
                c.endpoint,
                c.api_key,
                c.tpm_limit.unwrap_or(self.fallback.tpm_limit as i64),
                c.id,
            ),
            None => {
                let key = self.fallback.api_key.clone().unwrap_or_default();
                if key.is_empty() {
                    return Err(OceError::service_not_ready(Some(&format!(
                        "No active {} credential or LLM_API_KEY is configured",
                        self.kind
                    ))));
                }
                (
                    self.fallback.base_url.clone(),
                    key,
                    self.fallback.tpm_limit as i64,
                    0,
                )
            }
        };
        let client = Arc::new(
            OpenAILlmClient::new(
                &base_url,
                &api_key,
                // 长上下文 rerank（50 候选 × 3000 字符 ≈ 50k+ token）非流式生成
                // 可能超过网关默认超时；客户端超时必须大于网关自身超时才能收到
                // 真实响应而不是连接被切断
                self.fallback
                    .timeout_seconds
                    .max(300.0),
                self.fallback.proxy.as_deref(),
                tpm,
                self.on_usage.clone(),
                credential_id,
            )
            .map_err(|m| OceError::new(m, "LlmError"))?,
        );
        *guard = Some(client.clone());
        Ok(client)
    }

    pub async fn reload(&self) -> OceResult<usize> {
        // 解析失败不替换旧 delegate（旁路容错与 Python 一致）
        let _ = self.acquire().await?;
        Ok(1)
    }

    pub async fn chat(
        &self,
        messages: &[serde_json::Value],
        model: &str,
        temperature: f32,
        max_tokens: Option<i64>,
    ) -> OceResult<String> {
        let client = self.acquire().await?;
        client.chat(messages, model, temperature, max_tokens).await
    }

    pub fn fallback_model(&self) -> &str {
        &self.fallback_model
    }
}

/// API rerank 凭据运行时。
pub struct CredentialConfiguredReranker {
    store: SqlCredentialAdminStore,
    fallback: RerankSettings,
    fallback_api_key: Option<String>,
    on_usage: Option<crate::openai::llm::UsageCallback>,
    delegate: RwLock<Option<Arc<crate::openai::llm::OpenAIReranker>>>,
}

impl CredentialConfiguredReranker {
    pub fn new(
        store: SqlCredentialAdminStore,
        fallback: RerankSettings,
        fallback_api_key: Option<String>,
        on_usage: Option<crate::openai::llm::UsageCallback>,
    ) -> Self {
        Self {
            store,
            fallback,
            fallback_api_key,
            on_usage,
            delegate: RwLock::new(None),
        }
    }

    async fn acquire(&self) -> OceResult<Arc<crate::openai::llm::OpenAIReranker>> {
        {
            let guard = self.delegate.read().await;
            if let Some(d) = guard.as_ref() {
                return Ok(d.clone());
            }
        }
        let mut guard = self.delegate.write().await;
        if let Some(d) = guard.as_ref() {
            return Ok(d.clone());
        }
        let cred = self.store.resolve_active("rerank").await?;
        let (endpoint, api_key, model, top_n, min_score, credential_id) = match cred {
            Some(c) => (
                c.endpoint,
                c.api_key,
                c.model,
                c.top_n.map(|v| v as usize).unwrap_or(self.fallback.top_n),
                c.min_score.map(|v| v as f32).unwrap_or(self.fallback.min_score),
                c.id,
            ),
            None => {
                let key = self
                    .fallback
                    .api_key
                    .clone()
                    .or_else(|| self.fallback_api_key.clone())
                    .unwrap_or_default();
                if key.is_empty() {
                    return Err(OceError::service_not_ready(Some(
                        "No active rerank credential or RERANK_API_KEY/EMBED_API_KEY is configured",
                    )));
                }
                (
                    self.fallback.endpoint.clone(),
                    key,
                    self.fallback.model.clone(),
                    self.fallback.top_n,
                    self.fallback.min_score,
                    0,
                )
            }
        };
        let client = Arc::new(
            crate::openai::llm::OpenAIReranker::new(
                &endpoint,
                &api_key,
                &model,
                top_n,
                min_score,
                self.fallback.timeout_seconds,
                self.on_usage.clone(),
                credential_id,
            )
            .map_err(|m| OceError::new(m, "RerankError"))?,
        );
        *guard = Some(client.clone());
        Ok(client)
    }

    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: Option<usize>,
    ) -> OceResult<Vec<(usize, f32)>> {
        let client = self.acquire().await?;
        client.rerank(query, documents, top_n).await
    }

    pub async fn reload(&self) -> OceResult<usize> {
        let _ = self.acquire().await?;
        Ok(1)
    }
}
