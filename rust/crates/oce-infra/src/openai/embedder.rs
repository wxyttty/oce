//! OpenAI 兼容的异步 embedding 客户端。与 Python `openai_embedder.py` 语义对齐：
//! 分段（max_input_chars + 重叠）→ 分批（batch_size/chars 预算）→ 有界并发 →
//! 多段按字符数加权池化 + L2 归一。

use oce_core::error::{OceError, OceResult};
use async_trait::async_trait;
use oce_core::search::Embedder;
use reqwest::Client;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// 用量回调：(credential_id, kind, model, prompt_tokens, completion_tokens)。
pub type UsageCallback = Arc<dyn Fn(i64, &str, &str, i64, i64) + Send + Sync>;

pub struct OpenAIEmbedder {
    http: Client,
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: usize,
    /// Voyage 系 API 的请求契约与 OpenAI 不同：无 encoding_format/dimensions，
    /// 需要 input_type=query|document（检索提示注入）
    is_voyage: bool,
    max_batch_size: usize,
    max_batch_chars: usize,
    max_input_chars: usize,
    input_overlap_chars: usize,
    max_concurrency: usize,
    query_instruction: String,
    credential_id: i64,
    on_usage: Option<UsageCallback>,
}

impl OpenAIEmbedder {
    /// endpoint 语义与 Python 一致：/v1/embeddings 完整 URL 或 base_url 均可。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: &str,
        api_key: &str,
        model: &str,
        dimensions: usize,
        max_batch_size: usize,
        max_concurrency: usize,
        max_batch_chars: usize,
        max_input_chars: usize,
        input_overlap_chars: usize,
        timeout_seconds: f64,
        proxy: Option<&str>,
        query_instruction: &str,
        credential_id: i64,
        on_usage: Option<UsageCallback>,
    ) -> Result<Self, String> {
        if max_batch_size < 1 || max_concurrency < 1 {
            return Err("Embedding batch size and concurrency must be positive".into());
        }
        if max_input_chars < 1 || max_batch_chars < max_input_chars {
            return Err("Embedding character budgets are invalid".into());
        }
        if input_overlap_chars >= max_input_chars {
            return Err("Embedding input overlap must be smaller than its window".into());
        }
        let base = endpoint.trim_end_matches('/');
        let base = base.strip_suffix("/embeddings").unwrap_or(base);
        let url = format!("{base}/embeddings");
        let mut builder = Client::builder()
            .timeout(std::time::Duration::from_secs_f64(timeout_seconds))
            .pool_max_idle_per_host(max_concurrency * 2);
        if let Some(proxy) = proxy {
            let p = reqwest::Proxy::all(proxy).map_err(|e| e.to_string())?;
            builder = builder.proxy(p);
        }
        let http = builder.build().map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            endpoint: url,
            api_key: api_key.to_string(),
            model: model.to_string(),
            dimensions,
            is_voyage: model.to_lowercase().starts_with("voyage"),
            max_batch_size,
            max_batch_chars,
            max_input_chars,
            input_overlap_chars,
            max_concurrency,
            query_instruction: query_instruction.to_string(),
            credential_id,
            on_usage,
        })
    }

    /// 长输入按 max_input_chars 分段，段间留 overlap；优先在换行/空格断开。
    fn split_input(&self, text: &str) -> Vec<String> {
        if text.chars().count() <= self.max_input_chars {
            return vec![text.to_string()];
        }
        let chars: Vec<char> = text.chars().collect();
        let mut segments = Vec::new();
        let mut start = 0usize;
        while start < chars.len() {
            let hard_end = (start + self.max_input_chars).min(chars.len());
            let mut end = hard_end;
            if hard_end < chars.len() {
                let search_start = start + self.max_input_chars / 2;
                // 从后往前找 \n 或空格边界
                let mut boundary = None;
                for i in (search_start..hard_end).rev() {
                    if chars[i] == '\n' || chars[i] == ' ' {
                        boundary = Some(i);
                        break;
                    }
                }
                if let Some(b) = boundary {
                    end = b + 1;
                }
            }
            segments.push(chars[start..end].iter().collect());
            if end >= chars.len() {
                break;
            }
            start = end.saturating_sub(self.input_overlap_chars).max(start + 1);
        }
        segments
    }

    fn make_batches(&self, texts: &[String]) -> Vec<Vec<String>> {
        let mut batches = Vec::new();
        let mut batch: Vec<String> = Vec::new();
        let mut batch_chars = 0usize;
        for text in texts {
            let text_chars = text.chars().count().max(1);
            if !batch.is_empty()
                && (batch.len() >= self.max_batch_size
                    || batch_chars + text_chars > self.max_batch_chars)
            {
                batches.push(std::mem::take(&mut batch));
                batch_chars = 0;
            }
            batch.push(text.clone());
            batch_chars += text_chars;
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        batches
    }

    /// 多段池化：按段长加权平均 + L2 归一。
    fn pool_vectors(&self, vectors: &[Vec<f32>], segments: &[String]) -> OceResult<Vec<f32>> {
        if vectors.len() != segments.len() || vectors.is_empty() {
            return Err(OceError::new("Embedding segment count mismatch", "EmbeddingMismatch"));
        }
        if vectors.len() == 1 {
            return Ok(vectors[0].clone());
        }
        let weights: Vec<f32> = segments
            .iter()
            .map(|s| s.chars().count().max(1) as f32)
            .collect();
        let total: f32 = weights.iter().sum();
        let mut pooled = vec![0f32; self.dimensions];
        for (vector, weight) in vectors.iter().zip(&weights) {
            for (i, v) in vector.iter().take(self.dimensions).enumerate() {
                pooled[i] += v * weight;
            }
        }
        for v in &mut pooled {
            *v /= total;
        }
        let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut pooled {
                *v /= norm;
            }
        }
        Ok(pooled)
    }

    async fn embed_batch(&self, texts: &[String]) -> OceResult<Vec<Vec<f32>>> {
        self.embed_batch_typed(texts, "document").await
    }

    async fn embed_batch_typed(&self, texts: &[String], input_type: &str) -> OceResult<Vec<Vec<f32>>> {
        let mut payload = json!({
            "model": self.model,
            "input": texts,
        });
        if self.is_voyage {
            // Voyage 契约：encoding_format 仅收 base64（省略=默认 float）；
            // 维度字段叫 output_dimension（缺省返回 1024 的 MRL 档，需显式请求原生维）；
            // input_type 注入检索提示（query/document 分别对应官方推荐用法）
            payload["input_type"] = json!(input_type);
            payload["output_dimension"] = json!(self.dimensions);
        } else {
            payload["dimensions"] = json!(self.dimensions);
            payload["encoding_format"] = json!("float");
        }
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| OceError::new(format!("embeddings request: {e}"), "EmbeddingError"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OceError::new(
                format!("embedding API {status}: {body}"),
                "EmbeddingError",
            ));
        }
        let data: serde_json::Value =
            resp.json().await.map_err(|e| OceError::new(e.to_string(), "EmbeddingError"))?;
        let mut items: Vec<(usize, Vec<f32>)> = data["data"]
            .as_array()
            .ok_or_else(|| OceError::new("missing data", "EmbeddingError"))?
            .iter()
            .filter_map(|item| {
                let index = item["index"].as_u64()? as usize;
                let vector: Vec<f32> = item["embedding"]
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                Some((index, vector))
            })
            .collect();
        items.sort_by_key(|(i, _)| *i);
        let vectors: Vec<Vec<f32>> = items.into_iter().map(|(_, v)| v).collect();
        if vectors.len() != texts.len() {
            return Err(OceError::new(
                format!(
                    "Embedding response count mismatch: expected {}, got {}",
                    texts.len(),
                    vectors.len()
                ),
                "EmbeddingMismatch",
            ));
        }
        if let Some(actual) = vectors.first().map(|v| v.len()) {
            if actual != self.dimensions {
                return Err(OceError::new(
                    format!("Embedding response dimension mismatch: API returned {actual}, expected {}", self.dimensions),
                    "EmbeddingError",
                ));
            }
        }
        if vectors.iter().any(|v| v.len() != self.dimensions) {
            return Err(OceError::new("Embedding response dimension mismatch", "EmbeddingMismatch"));
        }
        if let Some(cb) = &self.on_usage {
            let total = data["usage"]["total_tokens"].as_i64().unwrap_or(0);
            if total > 0 {
                // embed 无 prompt/completion 之分：总量记入 prompt
                cb(self.credential_id, "embed", &self.model, total, 0);
            }
        }
        Ok(vectors)
    }
}

#[async_trait]
impl Embedder for OpenAIEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let segment_groups: Vec<Vec<String>> =
            texts.iter().map(|t| self.split_input(t)).collect();
        let segments: Vec<String> = segment_groups.iter().flatten().cloned().collect();
        let segment_vectors = {
            let batches = self.make_batches(&segments);
            let semaphore = Arc::new(Semaphore::new(self.max_concurrency));
            let mut futs = Vec::with_capacity(batches.len());
            for batch in batches {
                let sem = semaphore.clone();
                futs.push(async move {
                    // permit 失败与嵌入失败分别报错（此前统一吞成
                    // "semaphore closed"，上游 API 的真实 4xx 被掩盖）
                    let Some(permit) = sem.acquire_owned().await.ok() else {
                        return Err(OceError::new("semaphore closed", "EmbeddingError"));
                    };
                    let result = self.embed_batch(&batch).await;
                    drop(permit);
                    result
                });
            }
            let results = ::futures::future::join_all(futs).await;
            let mut all: Vec<Vec<f32>> = Vec::with_capacity(segments.len());
            for r in results {
                match r {
                    Ok(v) => all.extend(v),
                    Err(e) => return Err(e),
                }
            }
            all
        };

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        let mut offset = 0usize;
        for group in &segment_groups {
            let next = offset + group.len();
            vectors.push(self.pool_vectors(&segment_vectors[offset..next], group)?);
            offset = next;
        }
        Ok(vectors)
    }

    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>> {
        if self.is_voyage {
            return self.embed_batch_typed(&[text.to_string()], "query").await.map(|v| v.into_iter().next().unwrap());
        }
        let text = if self.query_instruction.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", self.query_instruction, text)
        };
        let mut out = self.embed_documents(vec![text]).await?;
        out.pop().ok_or_else(|| OceError::new("empty embedding response", "EmbeddingError"))
    }
}
