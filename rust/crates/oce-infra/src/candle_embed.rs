//! Candle 本地嵌入（EMBED_PROVIDER=local）。
//!
//! 用 candle-transformers 在 CPU 上跑 Qwen3-Embedding 系列模型。
//! EOS/last-token 池化 + L2 归一 + MRL 截断。
//!
//! 内存控制：
//! - 每次前向新建 Model，前向后 drop（KV cache 无法跨请求清除）
//! - 权重 from_buffered_safetensors 加载到堆内存（避免 macOS mmap SIGSYS）
//! - 不用 sysinfo（避免 refresh_processes 和 candle CPU 前向冲突）

use async_trait::async_trait;
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen3;
use hf_hub::api::sync::ApiBuilder;
use oce_core::error::{OceError, OceResult};
use oce_core::search::Embedder;
use std::path::PathBuf;
use std::sync::Arc;
use tokenizers::Tokenizer;
use tracing::info;

fn err(msg: impl std::fmt::Display) -> OceError {
    OceError::new(msg.to_string(), "EmbeddingError")
}

pub struct CandleEmbedder {
    model_id: String,
    dtype: DType,
    dimensions: usize,
    query_instruction: String,
    instruction_template: String,
    tokenizer: Tokenizer,
    eos_token_id: u32,
    /// 合并后的权重数据（堆内存，'static via Box::leak）
    weights_data: &'static [u8],
    device: Device,
    config: qwen3::Config,
}

impl CandleEmbedder {
    pub fn new(
        model_id: &str,
        dtype_str: &str,
        dimensions: usize,
        query_instruction: &str,
        instruction_template: &str,
    ) -> OceResult<Self> {
        let dtype = match dtype_str {
            "f16" => DType::F16,
            "bf16" => DType::BF16,
            _ => DType::F32,
        };
        let device = Device::Cpu;

        let api = ApiBuilder::new()
            .with_progress(false)
            .build()
            .map_err(|e| err(format!("hf-hub: {e}")))?;
        let repo = api.model(model_id.to_string());

        let tokenizer_path = repo
            .get("tokenizer.json")
            .map_err(|e| err(format!("tokenizer: {e}")))?;
        let config_path = repo
            .get("config.json")
            .map_err(|e| err(format!("config: {e}")))?;

        let model_paths: Vec<PathBuf> = repo
            .info()
            .map_err(|e| err(format!("repo info: {e}")))?
            .siblings
            .iter()
            .filter(|s| s.rfilename.ends_with(".safetensors"))
            .map(|s| repo.get(&s.rfilename).unwrap())
            .collect();

        let config_str = std::fs::read_to_string(&config_path)
            .map_err(|e| err(format!("read config: {e}")))?;
        let config_val: serde_json::Value = serde_json::from_str(&config_str)
            .map_err(|e| err(format!("parse config: {e}")))?;

        let eos_token_id = config_val
            .get("eos_token_id")
            .and_then(|v| {
                if v.is_i64() {
                    Some(v.as_i64().unwrap() as u32)
                } else if v.is_array() {
                    v.as_array()
                        .unwrap()
                        .first()
                        .and_then(|v| v.as_i64())
                        .map(|i| i as u32)
                } else {
                    None
                }
            })
            .unwrap_or(151643);

        let hidden_size = config_val["hidden_size"].as_u64().unwrap() as usize;
        let intermediate_size = config_val["intermediate_size"].as_u64().unwrap() as usize;
        let num_attention_heads = config_val["num_attention_heads"].as_u64().unwrap() as usize;
        let num_key_value_heads = config_val["num_key_value_heads"].as_u64().unwrap() as usize;
        let num_hidden_layers = config_val["num_hidden_layers"].as_u64().unwrap() as usize;
        let vocab_size = config_val["vocab_size"].as_u64().unwrap() as usize;
        let max_position_embeddings = config_val
            .get("max_position_embeddings")
            .and_then(|v| v.as_u64())
            .unwrap_or(40960) as usize;
        let rope_theta = config_val
            .get("rope_theta")
            .and_then(|v| v.as_f64())
            .unwrap_or(1_000_000.0);
        let head_dim = config_val
            .get("head_dim")
            .and_then(|v| v.as_u64())
            .unwrap_or((hidden_size / num_attention_heads) as u64) as usize;

        let rms_norm_eps = config_val
            .get("rms_norm_eps")
            .and_then(|v| v.as_f64())
            .unwrap_or(1e-6);
        let hidden_act = match config_val
            .get("hidden_act")
            .and_then(|v| v.as_str())
            .unwrap_or("silu")
        {
            "silu" => candle_nn::Activation::Silu,
            "gelu" => candle_nn::Activation::Gelu,
            _ => candle_nn::Activation::Silu,
        };
        let attention_bias = config_val
            .get("attention_bias")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tie_word_embeddings = config_val
            .get("tie_word_embeddings")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let config = qwen3::Config {
            hidden_size,
            intermediate_size,
            num_attention_heads,
            num_key_value_heads,
            num_hidden_layers,
            vocab_size,
            max_position_embeddings,
            rope_theta,
            head_dim,
            rms_norm_eps,
            hidden_act,
            attention_bias,
            tie_word_embeddings,
            sliding_window: None,
            max_window_layers: 0,
            use_sliding_window: false,
        };

        // 合并所有 safetensors 文件到一块堆内存
        // from_buffered_safetensors 期望单个 Vec<u8>
        // 多文件场景：逐个加载并用 VarBuilder::from_safetensors 拼接
        // 但 candle 0.11 没有 from_safetensors 多文件 API
        // 方案：假设 Qwen3-0.6B 只有一个 safetensors 文件（~1.2GB）
        assert!(
            model_paths.len() == 1,
            "candle embed currently requires a single safetensors file"
        );
        let data = std::fs::read(&model_paths[0])
            .map_err(|e| err(format!("read weights: {e}")))?;
        let weights_data: &'static [u8] = Box::leak(data.into_boxed_slice());

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| err(format!("tokenizer load: {e}")))?;

        info!(
            "candle embed: model={}, dtype={:?}, layers={}, hidden={}, dim={}, eos={}",
            model_id, dtype, num_hidden_layers, hidden_size, dimensions, eos_token_id
        );

        Ok(Self {
            model_id: model_id.to_string(),
            dtype,
            dimensions,
            query_instruction: query_instruction.to_string(),
            instruction_template: instruction_template.to_string(),
            tokenizer,
            eos_token_id,
            weights_data,
            device,
            config,
        })
    }

    fn encode_single(&self, text: &str) -> OceResult<Vec<f32>> {
        let enc = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| err(format!("tokenize: {e}")))?;
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        if ids.last().copied() != Some(self.eos_token_id) {
            ids.push(self.eos_token_id);
        }

        let input_ids = Tensor::new(ids.as_slice(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| err(format!("tensor: {e}")))?;

        let seq_len = ids.len();

        // 独立 OS 线程：新建 Model → forward → drop
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<f32>, OceError>>();
        let dtype = self.dtype;
        let config = self.config.clone();
        let weights_data = self.weights_data;
        let dimensions = self.dimensions;

        std::thread::spawn(move || {
            let result = (|| -> Result<Vec<f32>, OceError> {
                // 权重键: embed_tokens.weight, layers.0.*, norm.weight
                // candle Model::new 内部查: model.embed_tokens, model.layers, model.norm
                // 需要 rename_f 去掉 model. 前缀
                let vb = VarBuilder::from_buffered_safetensors(
                    weights_data.to_vec(),
                    dtype,
                    &Device::Cpu,
                )
                .map_err(|e| err(format!("varbuilder: {e}")))?
                .rename_f(|name| {
                    // candle 请求 "model.X"，映射到文件中的 "X"
                    name.strip_prefix("model.").map(|s| s.to_string()).unwrap_or(name.to_string())
                });

                let mut model = qwen3::Model::new(&config, vb)
                    .map_err(|e| err(format!("model load: {e}")))?;

                // offset=0（新 Model，无 KV cache）
                let hidden = model
                    .forward(&input_ids, 0)
                    .map_err(|e| err(format!("forward: {e}")))?;

                // last-token pooling
                let pooled = hidden
                    .narrow(1, seq_len - 1, 1)
                    .and_then(|t| t.squeeze(0).and_then(|t| t.squeeze(0)))
                    .map_err(|e| err(format!("pool: {e}")))?;

                // 提取向量
                let vec = pooled
                    .to_dtype(DType::F32)
                    .and_then(|t| t.to_vec1::<f32>())
                    .map_err(|e| err(format!("to_vec: {e}")))?;

                // L2 归一化
                let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
                let mut normalized = if norm > 0.0 {
                    vec.iter().map(|v| v / norm).collect()
                } else {
                    vec
                };

                // MRL 截断
                if dimensions < normalized.len() {
                    normalized.truncate(dimensions);
                }

                Ok(normalized)
                // model 在此处 drop，释放中间张量和 KV cache
            })();
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| err(format!("thread: {e}")))?
    }
}

#[async_trait]
impl Embedder for CandleEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            let emb = self.encode_single(&text)?;
            results.push(emb);
        }
        Ok(results)
    }

    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>> {
        let query_text = if self.query_instruction.is_empty() {
            text.to_string()
        } else if self.instruction_template == "instruct_query" {
            format!("Instruct: {}\nQuery: {}", self.query_instruction, text)
        } else {
            format!("{}{}", self.query_instruction, text)
        };
        let mut out = self.embed_documents(vec![query_text]).await?;
        out.pop()
            .ok_or_else(|| OceError::new("empty embedding response", "EmbeddingError"))
    }
}
