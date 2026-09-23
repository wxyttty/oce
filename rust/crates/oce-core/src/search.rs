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

    /// 查标识符在 scope 内的定义位置（related symbols hints 用）。
    /// 返回按 (identifier, kind=endpoint 优先) 排序的定义列表，含 fanout
    /// （该标识符定义出现的文件数，门控通用符号）。默认返回空（端口可选实现）。
    async fn find_definitions(
        &self,
        identifiers: &[String],
        allowed_blob_names: Option<&[String]>,
    ) -> OceResult<Vec<crate::related::SymbolDefinition>> {
        let _ = (identifiers, allowed_blob_names);
        Ok(vec![])
    }
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

/// 向量引擎统计端口（storage 报表 / workspace 状态用；只读旁路）。
pub trait VectorStatsSource: Send + Sync {
    /// 引擎内节点总数（chunk + path）。
    fn node_count(&self) -> usize;
    /// 按 kind 的节点统计（[("chunk", n), ("path", m)]）。
    fn kind_stats(&self) -> Vec<(String, usize)>;
}

/// 向量引擎：检索 + 写路径 + 路径索引 + 统计（组合端口；容器单句柄装配四个角色）。
/// TriviumDB/pgvector 实现均满足；Qdrant 等远程后端接入时同样实现全部四个。
pub trait VectorEngine:
    SearchStore + VectorIndex + PathSearchStore + VectorStatsSource
{
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

/// 查询嵌入缓存装饰器：相同 query 的向量直接命中，跳过 API 往返
/// （查询延迟的大头是 embed API ~400ms；MCP/交互场景重复查询常见）。
/// 文档嵌入不缓存（内容寻址、几乎不重复）。容量上限 FIFO 淘汰，
/// 失败结果不缓存（瞬时故障不应被记忆）。
pub struct CachedEmbedder {
    inner: std::sync::Arc<dyn Embedder>,
    cache: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Vec<f32>>>>,
    capacity: usize,
}

impl CachedEmbedder {
    pub fn new(inner: std::sync::Arc<dyn Embedder>, capacity: usize) -> Self {
        Self {
            inner,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            capacity: capacity.max(16),
        }
    }
}

#[async_trait]
impl Embedder for CachedEmbedder {
    async fn embed_documents(&self, texts: Vec<String>) -> OceResult<Vec<Vec<f32>>> {
        self.inner.embed_documents(texts).await
    }

    async fn embed_query(&self, text: &str) -> OceResult<Vec<f32>> {
        if let Some(v) = self.cache.lock().unwrap().get(text) {
            return Ok(v.as_ref().clone());
        }
        let vec = self.inner.embed_query(text).await?;
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= self.capacity {
            // FIFO 淘汰一个任意键（HashMap 无序，等价随机）
            if let Some(k) = cache.keys().next().cloned() {
                cache.remove(&k);
            }
        }
        cache.insert(text.to_string(), std::sync::Arc::new(vec.clone()));
        Ok(vec)
    }
}

/// API 重排结果：打分命中与未打分候选分离。
///
/// 悬崖截断（retrieval.rs，RETRIEVAL_RERANK_CUTOFF_ENABLED）依赖这个边界：
/// 只有 `ranked` 携带端点校准分（通常 0..1、按相关性降序），`unscored` 的
/// 分数仍是融合分——两者混在同一列表里无法判分数悬崖。
#[derive(Debug, Clone)]
pub struct RerankOutcome {
    /// 被端点打分的命中，按相关性降序；score 为端点校准分。
    pub ranked: Vec<SearchHit>,
    /// 未被端点返回的候选，保持原序与融合分数（select 层的完整候选池）。
    pub unscored: Vec<SearchHit>,
}

/// 重排器协议（对应 Python `Reranker`；返回值扩展出打分边界）。
#[async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(&self, query: &str, hits: Vec<SearchHit>) -> OceResult<RerankOutcome>;
}

/// 不重排（全部作为未打分候选原样返回：无分数信号，不触发截断）。
pub struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(&self, _query: &str, hits: Vec<SearchHit>) -> OceResult<RerankOutcome> {
        Ok(RerankOutcome {
            ranked: vec![],
            unscored: hits,
        })
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
    /// 语义通路缺席（嵌入冷却/故障）：formatter 据此注入 degraded 提示
    pub semantic_degraded: bool,
    /// 最终命中头部分数低于阈值：formatter 据此注入 weak 提示
    pub weak_match: bool,
    /// broad regime 已生效（formatter 据此注入骨架化提示）
    pub broad: bool,
    /// related symbols hints（输出层追加；空 = 未开启或无可用定义）
    pub related_symbols: Vec<crate::related::RelatedSymbol>,
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
