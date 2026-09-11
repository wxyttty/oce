//! 检索领域类型：SearchHit 值对象 + 存储协议。
//!
//! SearchHit 是检索命中的不可变值对象；存储由基础设施层实现协议，
//! 领域层只依赖 trait。

use crate::error::OceResult;
use async_trait::async_trait;

/// 检索命中的代码片段。
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub blob_name: String,
    pub path: String,
    pub content: String,
    pub score: f32,
    pub content_hash: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// 标识一个源码出现位置（兼容无 hash 的旧命中）。
pub fn search_hit_key(hit: &SearchHit) -> (String, String, u32, u32, String) {
    (
        hit.blob_name.clone(),
        hit.path.clone(),
        hit.start_line,
        hit.end_line,
        if hit.content_hash.is_empty() {
            hit.content.clone()
        } else {
            hit.content_hash.clone()
        },
    )
}

/// 向量检索存储协议（对应 Python `SearchStore`）。
#[async_trait]
pub trait SearchStore: Send + Sync {
    /// 向量检索，返回按相似度降序的命中列表。
    /// `allowed_blob_names` 非空时做索引级过滤（范围外不参与排序）。
    async fn search(
        &self,
        query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
        vector_threshold: f32,
    ) -> OceResult<Vec<SearchHit>>;
}

/// 按代码标识符精确召回已索引片段（对应 Python `ExactSearchStore`）。
#[async_trait]
pub trait ExactSearchStore: Send + Sync {
    async fn search_exact(
        &self,
        identifiers: &[String],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<SearchHit>>;
}

/// 路径搜索结果（文件名查询专用索引）。
#[derive(Debug, Clone)]
pub struct PathSearchResult {
    pub path: String,
    pub blob_name: String,
    pub score: f32,
}

/// 路径索引协议（检索 + 写入 + 删除）。
#[async_trait]
pub trait PathSearchStore: Send + Sync {
    async fn search_paths(
        &self,
        // query：原始查询文本（BM25 稀疏召回用；路径文档含文件名词汇）
        query: &str,
        query_vector: &[f32],
        allowed_blob_names: Option<&[String]>,
        top_k: usize,
    ) -> OceResult<Vec<PathSearchResult>>;

    /// 写入路径文档：每项含 path_id / blob_name / path / path_document / path_vector。
    async fn insert(&self, path_docs: Vec<PathDoc>) -> OceResult<u64>;

    async fn delete_by_blob_names(&self, blob_names: &[String]) -> OceResult<()>;
}

/// 路径索引写入项（对应 Python path_docs dict）。
pub struct PathDoc {
    pub path_id: String,
    pub blob_name: String,
    pub path: String,
    pub path_document: String,
    pub path_vector: Vec<f32>,
}

/// 向量索引写入项（对应 Python VectorIndex.upsert 的 dict）。
pub struct VectorUpsert {
    pub chunk_id: String,
    pub content_hash: String,
    pub blob_name: String,
    pub content: String,
    pub vector: Vec<f32>,
    /// metadata.path
    pub path: String,
    /// metadata.start_line
    pub start_line: u32,
    /// metadata.end_line
    pub end_line: u32,
}

/// 向量索引写路径协议（对应 Python `VectorIndex`）。
#[async_trait]
pub trait VectorIndex: Send + Sync {
    async fn upsert(&self, items: Vec<VectorUpsert>) -> OceResult<u64>;

    async fn delete(&self, blob_names: &[String]) -> OceResult<()>;
}

/// 嵌入器协议（对应 Python `Embedder`）。
/// 约束：embed_documents 与 embed_query 必须同 model + 同维度。
#[async_trait]
pub trait Embedder: Send + Sync {
    /// 批量文档向量化，返回与输入等长的向量列表。
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>>;

    /// 查询向量化（与文档使用同 model + dims）。
    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>>;
}

/// 重排器协议（对应 Python `Reranker`）。
#[async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(&self, query: &str, hits: Vec<SearchHit>) -> OceResult<Vec<SearchHit>>;
}

/// 不重排（原样返回）。
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(&self, _query: &str, hits: Vec<SearchHit>) -> OceResult<Vec<SearchHit>> {
        Ok(hits)
    }
}

/// LLM 语义重排器协议（对应 Python `LLMReranker`）。
/// 输入候选带正文与行号；返回重排后的 SearchHit 子集。
#[async_trait]
pub trait LlmReranker: Send + Sync {
    /// LLM 重排的最大候选数（merge_exact_hits 的锚点窗口用）。
    fn max_candidates(&self) -> usize;

    async fn rerank(&self, query: &str, candidates: Vec<SearchHit>) -> OceResult<Vec<SearchHit>>;
}

/// 查询改写器协议（对应 Python `QueryRewriter`）。
#[async_trait]
pub trait QueryRewriter: Send + Sync {
    async fn rewrite(&self, query: &str) -> OceResult<Vec<String>>;
}

/// LLM 意图分类器协议（对应 Python `IntentClassifier`）。
#[async_trait]
pub trait IntentClassifier: Send + Sync {
    async fn classify(&self, query: &str) -> OceResult<crate::strategy::LlmIntent>;
}

/// 检索管线阶段耗时的可变收集容器（对应 Python `RetrievalAudit`）。
/// 同名阶段累加（多子查询召回不会互相覆盖）。
#[derive(Debug, Default, Clone)]
pub struct RetrievalAudit {
    pub intent: Option<String>,
    pub path_boosted: bool,
    pub scope_size: Option<usize>,
    pub stages: std::collections::HashMap<String, u64>,
}

impl RetrievalAudit {
    pub fn stage(&mut self, name: &str) -> AuditGuard<'_> {
        AuditGuard {
            audit: self,
            name: name.to_string(),
            start: std::time::Instant::now(),
        }
    }
}

/// Drop 时把耗时累加进 audit.stages（括住 await 也能测出墙钟耗时）。
pub struct AuditGuard<'a> {
    audit: &'a mut RetrievalAudit,
    name: String,
    start: std::time::Instant,
}

impl Drop for AuditGuard<'_> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        *self.audit.stages.entry(self.name.clone()).or_insert(0) += elapsed;
    }
}
