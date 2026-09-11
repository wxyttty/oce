//! 静态查表嵌入的封闭测试：合成微型 Model2Vec 模型（无需下载真模型）。

use oce_infra::settings::EmbeddingSettings;
use oce_core::search::Embedder;
use oce_infra::static_embed::StaticEmbedder;
use std::collections::HashMap;
use std::path::Path;

/// 生成最小 Model2Vec 模型目录：WordLevel tokenizer + [vocab, dim] f32 embeddings。
fn write_synthetic_model(dir: &Path, dim: usize) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let vocab = [
        "[UNK]", "hello", "world", "fn", "main", "token", "refresh", "config", "parse",
    ];
    // tokenizer.json：WordLevel + Whitespace 预切分
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

    // model.safetensors：embeddings [vocab_len, dim]
    let rows = vocab.len();
    let mut data = Vec::with_capacity(rows * dim * 4);
    for i in 0..rows {
        for j in 0..dim {
            data.extend_from_slice(&(((i * dim + j) as f32 / (rows * dim) as f32 - 0.5).to_le_bytes()));
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

    std::fs::write(dir.join("config.json"), r#"{"normalize": true, "dim": 8}"#).unwrap();
    dir.to_string_lossy().into_owned()
}

fn settings_with(model_dir: &str) -> EmbeddingSettings {
    let mut s = EmbeddingSettings::from_env();
    s.static_model = Some(model_dir.to_string());
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_synthetic_model_and_embeds() {
    let dir = std::env::temp_dir().join(format!("oce-m2v-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model_dir = write_synthetic_model(&dir, 8);

    let embedder = StaticEmbedder::load(&settings_with(&model_dir)).unwrap();
    assert_eq!(embedder.dim(), 8);

    let vecs = embedder
        .embed_documents(vec!["hello world".into(), "fn main".into()])
        .await
        .unwrap();
    assert_eq!(vecs.len(), 2);
    for v in &vecs {
        assert_eq!(v.len(), 8);
        // normalize=true → 单位向量
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "norm={norm}");
    }

    let q = embedder.embed_query("hello").await.unwrap();
    assert_eq!(q.len(), 8);
    // 相同文本的文档/查询向量一致（同模型同池化）
    let d = embedder.embed_documents(vec!["hello".into()]).await.unwrap();
    assert!(q.iter().zip(&d[0]).all(|(a, b)| (a - b).abs() < 1e-6));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_model_dir_errors_clearly() {
    let mut s = EmbeddingSettings::from_env();
    s.static_model = Some("/nonexistent/oce-m2v-missing".into());
    let err = StaticEmbedder::load(&s).unwrap_err();
    assert!(err.contains("static embed model"), "err={err}");
}
