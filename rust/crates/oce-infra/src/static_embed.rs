//! Model2Vec 静态查表嵌入（默认向量化提供方）。
//!
//! 通过官方 Rust 实现 `model2vec-rs` 加载：tokenizer.json + model.safetensors +
//! config.json。查表 + 平均池化 + L2 归一，无神经网络前向——毫秒级、零网络、可复现。
//! `EMBED_STATIC_MODEL` 为 HF repo id（自动下载缓存）或本地目录（离线优先）。

use crate::settings::EmbeddingSettings;
use async_trait::async_trait;
use model2vec_rs::model::StaticModel as M2vModel;
use oce_core::error::OceResult;
use oce_core::search::Embedder;
use std::path::Path;
use std::sync::Arc;

impl std::fmt::Debug for StaticEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticEmbedder")
            .field("dim", &self.dim)
            .field("model_id", &self.model_id)
            .finish()
    }
}

/// 解析静态模型来源：本地目录存在则优先（离线），否则交给 hf-hub 按 repo id 解析。
fn resolve_model_source(settings: &EmbeddingSettings) -> String {
    let configured = settings
        .static_model
        .clone()
        .unwrap_or_else(|| "minishlab/potion-multilingual-128M".into());
    // 显式本地路径（存在目录）直接用
    if Path::new(&configured).is_dir() {
        return configured;
    }
    // 本地缓存目录（HF 缓存外的自定义离线目录）
    if let Some(dir) = &settings.static_model_dir {
        if Path::new(dir).is_dir() {
            return dir.clone();
        }
    }
    configured
}

pub struct StaticEmbedder {
    model: M2vModel,
    dim: usize,
    model_id: String,
}

impl StaticEmbedder {
    pub fn load(settings: &EmbeddingSettings) -> Result<Self, String> {
        let source = resolve_model_source(settings);
        let model = M2vModel::from_pretrained(&source, None, None, None)
            .map_err(|e| format!("load static embed model `{source}`: {e}"))?;
        // 无公开 dim 访问器：用探测向量取维度
        let probe = model.encode_single("dim probe");
        let dim = probe.len();
        if dim == 0 {
            return Err(format!("static embed model `{source}` has zero dim"));
        }
        Ok(Self {
            model,
            dim,
            model_id: source,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// 编码吞吐信息（benchmark/doctor 用）。
    pub fn encode_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.model.encode(texts)
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>> {
        // model2vec encode 是纯 CPU 查表（µs 级/条），无需 block_in_place
        Ok(self.model.encode(&texts))
    }

    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>> {
        Ok(self.model.encode_single(text))
    }
}

/// 便捷构造：供容器与 bench 共享。
pub fn load_static_embedder(settings: &EmbeddingSettings) -> Result<Arc<StaticEmbedder>, String> {
    Ok(Arc::new(StaticEmbedder::load(settings)?))
}

/// 测试辅助：合成微型 Model2Vec 模型目录（WordLevel tokenizer + f32 embeddings）。
#[doc(hidden)]
pub mod testing {
    use std::collections::HashMap;
    use std::path::Path;

    pub fn write_synthetic_model(dir: &Path, dim: usize) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let vocab = [
            "[UNK]", "hello", "world", "fn", "main", "token", "refresh", "config", "parse",
        ];
        let vocab_json: Vec<String> = vocab
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{t:?}: {i}"))
            .collect();
        let tokenizer = format!(
            r#"{{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],
"normalizer":null,"pre_tokenizer":{{"type":"Whitespace"}},"post_processor":null,
"decoder":null,
"model":{{"type":"WordLevel","vocab":{{{}}},"unk_token":"[UNK]"}}}}"#,
            vocab_json.join(",")
        );
        std::fs::write(dir.join("tokenizer.json"), tokenizer).unwrap();

        let rows = vocab.len();
        let mut data = Vec::with_capacity(rows * dim * 4);
        for i in 0..rows {
            for j in 0..dim {
                data.extend_from_slice(
                    &(((i * dim + j) as f32 / (rows * dim) as f32 - 0.5).to_le_bytes()),
                );
            }
        }
        let mut tensors: HashMap<String, safetensors::tensor::TensorView> = HashMap::new();
        tensors.insert(
            "embeddings".to_string(),
            safetensors::tensor::TensorView::new(
                safetensors::tensor::Dtype::F32,
                vec![rows, dim],
                &data,
            )
            .unwrap(),
        );
        let serialized = safetensors::tensor::serialize(tensors, None).unwrap();
        std::fs::write(dir.join("model.safetensors"), serialized).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"normalize": true}"#).unwrap();
        dir.to_string_lossy().into_owned()
    }
}
