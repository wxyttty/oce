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

    /// 按 blob 批量取 definition 行（符号注解用；一次查询覆盖整批命中，
    /// 不逐命中查库）。默认空实现（端口可选）。
    async fn definitions_for_blobs(&self, blob_names: &[String]) -> OceResult<Vec<SymbolRow>> {
        let _ = blob_names;
        Ok(vec![])
    }
}

/// 本次检索范围内的索引就绪度。
///
/// 「绝不把 pending 的缺失当正常空结果」：调用方据此判断这次结果是否完整，
/// 而不是把还没嵌入完的 blob 当成"查不到"。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReadiness {
    pub scope_size: usize,
    pub pending: usize,
    pub failed: usize,
    pub ready: usize,
    pub last_error_type: Option<String>,
}

impl IndexReadiness {
    /// `ready` | `pending` | `degraded`。
    ///
    /// 有 pending 就是 `pending`（还在等，结果可能不完整）；没有 pending 但有失败
    /// 就是 `degraded`（结果确定不完整，且不会自己变好）。
    pub fn mode(&self) -> &'static str {
        if self.pending > 0 {
            "pending"
        } else if self.failed > 0 {
            "degraded"
        } else {
            "ready"
        }
    }
}

/// `symbol_occurrences` 里的一行 definition，用于把命中标注到符号名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub identifier: String,
    pub content_hash: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// 把命中标注到符号名：`search_hit_key` → `(符号名, kind)`。
///
/// 匹配规则：按 `content_hash` 对齐（空的 content_hash 不参与），且 occurrence 的
/// `start_line` 落在命中 span 内；同 chunk 有多个 definition 时取 `start_line`
/// 最小者——最靠近 chunk 起点的那个才是这段代码的定义点。取不到就不标注，
/// 调用方按「无符号」处理，不做猜测。
pub fn annotate_symbols(
    hits: &[SearchHit],
    rows: &[SymbolRow],
) -> std::collections::HashMap<(String, String, u32, u32, String), (String, String)> {
    use std::collections::HashMap;
    let mut by_hash: HashMap<&str, Vec<&SymbolRow>> = HashMap::new();
    for row in rows {
        if !row.content_hash.is_empty() {
            by_hash.entry(row.content_hash.as_str()).or_default().push(row);
        }
    }
    let mut annotations: HashMap<(String, String, u32, u32, String), (String, String)> = HashMap::new();
    for hit in hits {
        if hit.content_hash.is_empty() {
            continue;
        }
        let Some(candidates) = by_hash.get(hit.content_hash.as_str()) else {
            continue;
        };
        let best = candidates
            .iter()
            .filter(|row| row.start_line >= hit.start_line && row.start_line <= hit.end_line)
            .min_by_key(|row| row.start_line);
        if let Some(row) = best {
            annotations.insert(
                search_hit_key(hit),
                (row.identifier.clone(), row.kind.clone()),
            );
        }
    }
    annotations
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
/// 输入候选带正文与行号；返回重排后的候选与降级信息。
#[async_trait]
pub trait LlmReranker: Send + Sync {
    /// LLM 重排的最大候选数（merge_exact_hits 的锚点窗口用）。
    fn max_candidates(&self) -> usize;

    async fn rerank(
        &self,
        query: &str,
        candidates: Vec<SearchHit>,
    ) -> OceResult<LlmRerankOutcome>;
}

/// LLM 重排结果：除排序后的候选外，显式带上"是否降级"与请求/返回条数。
///
/// LLM 只返回编号列表、不返回校准分，因此"少给了几条"与"调用失败"过去只能靠
/// 日志察觉。这里把两者变成可断言的数据，调用方写进 `RetrievalAudit`。
#[derive(Debug, Clone)]
pub struct LlmRerankOutcome {
    /// 重排后的候选（不足时已按原序补齐，保证下游拿到足够的候选）。
    pub ranked: Vec<SearchHit>,
    /// 降级原因：`no_valid_index`（无有效编号）/ `padded`（返回不足已补齐）/
    /// `chat_error`（调用失败，由调用方按错误码填入）。
    pub degraded: Option<String>,
    pub requested: usize,
    pub returned: usize,
}

/// LLM 重排降级分类：0 条有效编号 = `no_valid_index`；少于要求 = `padded`。
///
/// 独立成纯函数，便于在 core 层直接断言（不需要起 HTTP 端点）。
pub fn classify_llm_rerank_degraded(requested: usize, returned: usize) -> Option<String> {
    if returned == 0 {
        Some("no_valid_index".to_string())
    } else if returned < requested {
        Some("padded".to_string())
    } else {
        None
    }
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
    /// 最终命中头部分数低于阈值，或查询点名的标识符在窗口内容里零命中
    /// （OwnMem query-coverage 借鉴：覆盖率区分对错 AUC 0.886 vs 置信分 0.667）：
    /// formatter 据此注入 weak 提示
    pub weak_match: bool,
    /// broad regime 已生效（formatter 据此注入骨架化提示）
    pub broad: bool,
    /// exact 标识符召回因 scope 超限整体跳过（查询含标识符、store 在场、
    /// 却被 exact_max_scope_blobs 拦下）：「没查到符号」与「没查符号」
    /// 对外观相同，formatter 据此注入提示（Astrolabe unknown-vs-empty 原则）
    pub exact_skipped_scope: bool,
    /// select 窗口被预算/单文件上限截短（池内还有候选但窗口未填满）：
    /// 「只有这些」与「还有更多被省略」是不同信号，formatter 据此注入提示
    pub select_truncated: bool,
    /// related symbols hints（输出层追加；空 = 未开启或无可用定义）
    pub related_symbols: Vec<crate::related::RelatedSymbol>,
    /// API rerank 通路降级原因（熔断/畸形响应）：空 = 未降级
    pub rerank_degraded: Option<String>,
    /// LLM rerank 通路降级原因（`no_valid_index` / `padded` / `chat_error`）
    pub llm_rerank_degraded: Option<String>,
    /// LLM rerank 实际返回的候选条数（含补齐后），用于判断"少给了几条"
    pub llm_rerank_returned: Option<usize>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, hash: &str, start: u32, end: u32) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}"),
            path: path.into(),
            content: "x".into(),
            score: 0.5,
            content_hash: hash.into(),
            start_line: start,
            end_line: end,
        }
    }

    fn row(identifier: &str, hash: &str, start: u32) -> SymbolRow {
        SymbolRow {
            identifier: identifier.into(),
            content_hash: hash.into(),
            kind: "definition".into(),
            start_line: start,
            end_line: start,
        }
    }

    #[test]
    fn annotate_symbols_matches_by_hash_and_span() {
        let hits = vec![
            hit("a.rs", "h-a", 10, 40),
            hit("b.rs", "h-b", 1, 5),
            hit("c.rs", "", 1, 5), // 无 content_hash：不参与标注
        ];
        let rows = vec![
            row("alpha", "h-a", 10),   // 落在 a.rs 命中 span 内
            row("beta", "h-a", 80),    // 同一 chunk 但在 span 外
            row("gamma", "h-z", 1),    // 该 blob 没命中
        ];
        let annotations = annotate_symbols(&hits, &rows);
        assert_eq!(annotations.len(), 1);
        let key = search_hit_key(&hits[0]);
        assert_eq!(
            annotations.get(&key).map(|(symbol, kind)| (symbol.as_str(), kind.as_str())),
            Some(("alpha", "definition"))
        );
        assert!(annotations.get(&search_hit_key(&hits[1])).is_none());
        assert!(annotations.get(&search_hit_key(&hits[2])).is_none());
    }

    #[test]
    fn annotate_symbols_prefers_earliest_definition_in_span() {
        let hits = vec![hit("a.rs", "h-a", 10, 40)];
        let rows = vec![row("later", "h-a", 30), row("earlier", "h-a", 12)];
        let annotations = annotate_symbols(&hits, &rows);
        let key = search_hit_key(&hits[0]);
        assert_eq!(
            annotations.get(&key).map(|(symbol, _)| symbol.as_str()),
            Some("earlier")
        );
    }
}
