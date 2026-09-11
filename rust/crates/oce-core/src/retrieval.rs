//! RetrievalPipeline — 检索编排。与 Python `domain/services/retrieval.py` 语义对齐。
//!
//! 流程：
//! embed_query → store.search（dense 向量检索）
//! → 精确标识符召回 → 多查询结果融合 → rerank（逐篇精排）
//! → 源码优先（降权文档/测试）→ 置信度门槛 → select（最终 K 条）
//!
//! rerank 解决「单篇多相关」，select 解决「这一组够全且不冗余」，职责不同。

use crate::classifier::{
    classify_query_intent, extract_code_identifiers, Intent,
};
use crate::planner::QueryPlanner;
use crate::priority::{path_query_priority_factor, source_priority_factor};
use crate::retrieval_settings::RetrievalSettings;
use crate::search::{
    search_hit_key, Embedder, ExactSearchStore, IntentClassifier, LlmReranker,
    PathSearchStore, QueryRewriter, RetrievalAudit, Reranker, SearchHit, SearchStore,
};
use crate::selector::CoverageSelector;
use crate::strategy::{get_strategy, LlmIntent};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;

/// 路径检索的固定召回量（与 Python 常量一致）。
const PATH_SEARCH_TOP_K: usize = 20;

/// 按分数 × 路径惩罚因子稳定重排（只重排，不改 rerank 决策）。
fn apply_source_priority(
    hits: Vec<SearchHit>,
    factor: fn(&str) -> f32,
) -> Vec<SearchHit> {
    let mut hits = hits;
    hits.sort_by(|a, b| {
        let sa = a.score * factor(&a.path);
        let sb = b.score * factor(&b.path);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

/// 逐条按有效分（score × penalty）剔除低于门槛的弱匹配。
fn apply_confidence_floor(hits: Vec<SearchHit>, floor: f32, factor: fn(&str) -> f32) -> Vec<SearchHit> {
    hits.into_iter()
        .filter(|h| h.score * factor(&h.path) >= floor)
        .collect()
}

/// 多查询 RRF 融合。首个查询权重 1.0，facet 查询用 facet_weight。
fn fuse(
    result_lists: Vec<Vec<SearchHit>>,
    rrf_k: usize,
    facet_weight: f32,
    default_top_k: usize,
) -> Vec<SearchHit> {
    if result_lists.len() == 1 {
        return result_lists.into_iter().next().unwrap();
    }
    let weights: Vec<f32> = std::iter::once(1.0)
        .chain(std::iter::repeat(facet_weight).take(result_lists.len() - 1))
        .collect();
    let max_score: f32 = weights
        .iter()
        .map(|w| w / (rrf_k as f32 + 1.0))
        .sum();

    let mut scores: HashMap<(String, String, u32, u32, String), f32> = HashMap::new();
    let mut hits_by_key: HashMap<(String, String, u32, u32, String), SearchHit> = HashMap::new();
    let mut first_seen: HashMap<(String, String, u32, u32, String), usize> = HashMap::new();
    let mut ordinal = 0usize;

    for (weight, hits) in weights.into_iter().zip(result_lists.into_iter()) {
        for (rank, hit) in hits.into_iter().enumerate() {
            let key = search_hit_key(&hit);
            if !first_seen.contains_key(&key) {
                first_seen.insert(key.clone(), ordinal);
                ordinal += 1;
                hits_by_key.insert(key.clone(), hit);
            }
            *scores.entry(key).or_insert(0.0) += weight / (rrf_k as f32 + rank as f32 + 1.0);
        }
    }

    let mut keys: Vec<_> = scores.into_iter().collect();
    keys.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| first_seen[&a.0].cmp(&first_seen[&b.0]))
    });
    keys.into_iter()
        .take(default_top_k)
        .filter_map(|(key, score)| {
            hits_by_key.remove(&key).map(|mut hit| {
                hit.score = score / max_score;
                hit
            })
        })
        .collect()
}

/// 合并精确召回与语义召回（对应 Python `_merge_exact_hits`）。
fn merge_exact_hits(
    query: &str,
    exact_hits: Vec<SearchHit>,
    semantic_hits: Vec<SearchHit>,
    settings: &RetrievalSettings,
    llm_max_candidates: Option<usize>,
) -> Vec<SearchHit> {
    if classify_query_intent(query) == Intent::CallChain && !semantic_hits.is_empty() {
        let semantic_keys: std::collections::HashSet<_> =
            semantic_hits.iter().map(search_hit_key).collect();
        let mut exact_only: Vec<SearchHit> = exact_hits
            .into_iter()
            .filter(|hit| !semantic_keys.contains(&search_hit_key(hit)))
            .collect();
        let candidate_window = llm_max_candidates.unwrap_or(settings.default_top_k).min(settings.default_top_k);
        let reserved = exact_only.len().min((candidate_window / 3).max(1));
        let semantic_slots = (candidate_window - reserved).max(1);
        let anchor_index = semantic_hits.len().min(semantic_slots) - 1;
        let anchor_score = semantic_hits[anchor_index].score;
        exact_only = exact_only
            .into_iter()
            .take(reserved)
            .map(|mut hit| {
                hit.score = hit.score.min(anchor_score);
                hit
            })
            .collect();
        let mut merged = semantic_hits;
        merged.extend(exact_only);
        merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(settings.default_top_k);
        return merged;
    }

    let mut merged: Vec<SearchHit> = Vec::new();
    let mut positions: HashMap<(String, String, u32, u32, String), usize> = HashMap::new();
    for hit in exact_hits.into_iter().chain(semantic_hits.into_iter()) {
        let key = search_hit_key(&hit);
        match positions.get(&key) {
            None => {
                positions.insert(key, merged.len());
                merged.push(hit);
            }
            Some(&pos) => {
                if hit.score > merged[pos].score {
                    merged[pos].score = hit.score;
                }
            }
        }
    }
    merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(settings.default_top_k);
    merged
}

/// 符号定位时，框架 endpoint 定义稳定优先于同名内部实现（对应 `_promote_symbol_endpoints`）。
fn promote_symbol_endpoints(query: &str, hits: Vec<SearchHit>) -> Vec<SearchHit> {
    if classify_query_intent(query) != Intent::Symbol {
        return hits;
    }
    const LOCATION_MARKERS: [&str; 11] = [
        "实现位置", "实现文件", "源码位置", "哪个文件", "在哪个文件", "在哪里定义",
        "哪里定义", "函数在哪", "defined", "definition", "implementation",
    ];
    let query_folded = query.to_lowercase();
    if !LOCATION_MARKERS.iter().any(|m| query_folded.contains(m)) {
        return hits;
    }
    let identifiers = extract_code_identifiers(query);
    if identifiers.is_empty() {
        return hits;
    }

    let patterns: Vec<Regex> = identifiers
        .iter()
        .filter_map(|identifier| {
            let last = identifier.rsplit("::").next()?;
            Regex::new(&format!(
                r"(?s)(?:#\[(?:tauri::command|pytauri::command)[^\]]*\]|@(?:app|router)\.(?:get|post|put|patch|delete)\([^\n]*\))\s*(?:(?:pub|export)(?:\([^)]*\))?\s+)?(?:(?:async|default)\s+)?(?:fn|def|function)\s+{}\b",
                regex::escape(last)
            ))
            .ok()
        })
        .collect();
    let mut hits = hits;
    hits.sort_by_key(|hit| !patterns.iter().any(|p| p.is_match(&hit.content)));
    hits
}

pub struct RetrievalPipeline {
    pub embedder: Arc<dyn Embedder>,
    pub store: Arc<dyn SearchStore>,
    pub reranker: Arc<dyn Reranker>,
    pub llm_reranker: Option<Arc<dyn LlmReranker>>,
    pub query_rewriter: Option<Arc<dyn QueryRewriter>>,
    pub path_store: Option<Arc<dyn PathSearchStore>>,
    pub exact_store: Option<Arc<dyn ExactSearchStore>>,
    pub selector: Arc<CoverageSelector>,
    pub query_planner: Arc<dyn QueryPlanner>,
    pub intent_classifier: Option<Arc<dyn IntentClassifier>>,
    /// 按 blob 名取首个 chunk 的端口（路径回填用）。
    pub first_chunk_lookup: Option<Arc<dyn FirstChunkLookup>>,
    pub settings: RetrievalSettings,
}

/// 按 blob 名取首个 chunk 的端口（对应 Python `_fetch_content_for_paths` 的 SQL 直查）。
#[async_trait]
pub trait FirstChunkLookup: Send + Sync {
    async fn first_chunks(&self, blob_names: &[String]) -> Result<Vec<SearchHit>, String>;
}

impl RetrievalPipeline {
    pub fn new(
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn SearchStore>,
        settings: RetrievalSettings,
    ) -> Self {
        let planner_max = if settings.query_decomposition_enabled {
            settings.query_max_queries
        } else {
            1
        };
        Self {
            selector: Arc::new(
                CoverageSelector::new(
                    settings.max_chunks_per_path,
                    settings.max_context_chars,
                    settings.overlap_threshold,
                )
                .expect("invalid coverage selector settings"),
            ),
            query_planner: Arc::new(crate::planner::HeuristicQueryPlanner::new(
                planner_max,
                settings.query_min_facet_chars,
            )
            .expect("invalid planner settings")),
            reranker: Arc::new(crate::search::NoopReranker),
            llm_reranker: None,
            query_rewriter: None,
            path_store: None,
            exact_store: None,
            intent_classifier: None,
            first_chunk_lookup: None,
            embedder,
            store,
            settings,
        }
    }

    /// 注入首个 chunk 查询端口。
    pub fn with_first_chunk_lookup(mut self, lookup: Arc<dyn FirstChunkLookup>) -> Self {
        self.first_chunk_lookup = Some(lookup);
        self
    }

    /// 执行一次检索，返回最终命中列表（按融合分降序）。
    pub async fn search(
        &self,
        query: &str,
        allowed_blob_names: Option<&[String]>,
        mut audit: Option<&mut RetrievalAudit>,
    ) -> Vec<SearchHit> {
        if let Some(a) = audit.as_deref_mut() {
            a.scope_size = allowed_blob_names.map(|s| s.len());
        }
        // None 表示不过滤，空集合表示无可搜索内容
        if allowed_blob_names.is_some_and(|s| s.is_empty()) {
            return vec![];
        }

        // 意图驱动的策略选择
        let mut strategy = None;
        if let Some(classifier) = &self.intent_classifier {
            let intent = match classifier.classify(query).await {
                Ok(i) => i,
                Err(_) => LlmIntent::Feature, // 分类失败不阻断检索
            };
            if let Some(audit) = audit.as_deref_mut() {
                audit.intent = Some(intent.as_str().to_string());
            }
            strategy = Some(get_strategy(intent));
        }

        // 路径索引增强：与 Python 一致 —— path_store 存在 &&
        // (strategy.enable_path_index || 启发式判为 PATH)。启发式永远参与 OR，
        // intent 分类器开启也不屏蔽（"config.json 在哪里" 即便被判成 FEATURE 也走路径增强）。
        let use_path_index = self.path_store.is_some()
            && (strategy.as_ref().is_some_and(|s| s.enable_path_index)
                || crate::classifier::should_use_path_index(query));
        if use_path_index {
            if let Some(a) = audit.as_deref_mut() {
                a.path_boosted = true;
            }
            return self
                .search_with_path_boost(query, allowed_blob_names, strategy.as_ref(), audit)
                .await;
        }

        // Query rewrite：与原查询召回并发（改写是 LLM 调用 ~1-3s，原查询召回
        // 不依赖其结果；tokio::join 让总耗时 ≈ max 而非 sum）。
        // RRF 权重约定：首位结果列表权重 1.0 —— 原查询的列表必须保持居首。
        let use_query_rewrite = strategy
            .as_ref()
            .map(|s| s.enable_query_rewrite)
            .unwrap_or_else(|| self.query_rewriter.is_some());
        let rewrite_fut = async {
            if use_query_rewrite {
                if let Some(rewriter) = &self.query_rewriter {
                    let _g = audit.as_deref_mut().map(|a| a.stage("rewrite"));
                    return rewriter.rewrite(query).await.ok();
                }
            }
            None
        };
        let original_recall_fut = async {
            let planned = self.query_planner.plan(query);
            let num_queries = planned.len();
            let mut lists = Vec::with_capacity(num_queries);
            for planned_query in planned {
                lists
                    .push(self.recall(&planned_query, allowed_blob_names, num_queries).await);
            }
            lists
        };
        let (rewritten_opt, mut all_result_lists) = tokio::join!(rewrite_fut, original_recall_fut);
        let mut queries_to_search = vec![query.to_string()];
        if let Some(rewritten) = rewritten_opt {
            if !rewritten.is_empty() {
                queries_to_search = rewritten;
            }
        }
        {
            let _g = audit.as_deref_mut().map(|a| a.stage("dense"));
            for search_query in &queries_to_search {
                if search_query == query {
                    continue; // 原查询已随 join 召回，避免重复
                }
                let planned = self.query_planner.plan(search_query);
                let num_queries = planned.len();
                if num_queries == 0 {
                    continue;
                }
                let mut joined = Vec::with_capacity(num_queries);
                for planned_query in planned {
                    joined.push(
                        self.recall(&planned_query, allowed_blob_names, num_queries)
                            .await,
                    );
                }
                all_result_lists.extend(joined);
            }
        }

        let exact_hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("exact"));
            self.recall_exact(query, allowed_blob_names).await
        };
        if all_result_lists.is_empty() && exact_hits.is_empty() {
            return vec![];
        }

        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("fuse"));
            let semantic = if all_result_lists.is_empty() {
                vec![]
            } else {
                fuse(
                    all_result_lists,
                    self.settings.rrf_k,
                    self.settings.query_facet_weight,
                    self.settings.default_top_k,
                )
            };
            merge_exact_hits(
                query,
                exact_hits,
                semantic,
                &self.settings,
                self.llm_reranker.as_ref().map(|r| r.max_candidates()),
            )
        };

        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("rerank"));
            // rerank 失败保序回退（Python 侧 rerank 异常向上抛，这里选择不丢召回）
            match self.reranker.rerank(query, hits.clone()).await {
                Ok(h) => h,
                Err(exc) => {
                    tracing::warn!("rerank failed; keep fused order: {exc}");
                    hits
                }
            }
        };
        let hits = apply_source_priority(hits, source_priority_factor);

        let use_llm_rerank = strategy
            .as_ref()
            .map(|s| s.enable_llm_rerank)
            .unwrap_or_else(|| self.llm_reranker.is_some());
        let hits = if use_llm_rerank {
            let _g = audit.as_deref_mut().map(|a| a.stage("llm_rerank"));
            match self.llm_reranker.as_ref() {
                // LLM 重排失败 → 静默退回原始顺序（与 Python 语义一致，绝不丢召回）
                Some(r) => match r.rerank(query, hits.clone()).await {
                    Ok(h) => h,
                    Err(exc) => {
                        // LLM 重排失败 → 静默退回原始顺序（与 Python 语义一致，绝不丢召回）
                        tracing::warn!("LLM rerank failed; fallback to original order: {exc}");
                        hits
                    }
                },
                None => hits,
            }
        } else {
            hits
        };

        let hits = promote_symbol_endpoints(query, hits);

        {
            let _g = audit.as_deref_mut().map(|a| a.stage("select"));
            let hits = apply_confidence_floor(hits, self.settings.confidence_floor, source_priority_factor);
            return self.selector.select(&hits, self.settings.final_select_k);
        }
    }

    /// 单查询向量召回。多查询用 per_query_top_k，单查询用 default_top_k。
    async fn recall(
        &self,
        query: &str,
        allowed_blob_names: Option<&[String]>,
        num_queries: usize,
    ) -> Vec<SearchHit> {
        let top_k = if num_queries == 1 {
            self.settings.default_top_k
        } else {
            self.settings.per_query_top_k
        };
        let query_vector = match self.embedder.embed_query(query).await {
            Ok(v) => v,
            Err(_) => return vec![], // 嵌入失败不阻断其它子查询
        };
        self.store
            .search(
                query,
                &query_vector,
                allowed_blob_names,
                top_k,
                self.settings.vector_threshold,
            )
            .await
            .unwrap_or_default()
    }

    /// 精确标识符召回。scope 超限或提取不到标识符时返回空。
    async fn recall_exact(
        &self,
        query: &str,
        allowed_blob_names: Option<&[String]>,
    ) -> Vec<SearchHit> {
        let Some(store) = &self.exact_store else {
            return vec![];
        };
        let scope_limit = self.settings.exact_max_scope_blobs;
        if allowed_blob_names.is_none()
            || scope_limit == 0
            || allowed_blob_names.unwrap().len() > scope_limit
        {
            return vec![];
        }
        let identifiers = extract_code_identifiers(query);
        if identifiers.is_empty() {
            return vec![];
        }
        match store
            .search_exact(&identifiers, allowed_blob_names, self.settings.default_top_k)
            .await
        {
            Ok(hits) => hits,
            Err(_) => vec![], // 精确召回失败回退语义候选
        }
    }

    /// 路径索引增强的检索（文件名查询）。
    ///
    /// 路径索引回答「哪个文件」，内容索引回答「文件里哪一段」，两者按 chunk 粒度
    /// 合并：路径命中的文件若已有内容命中则加权提分，不替换。
    async fn search_with_path_boost(
        &self,
        query: &str,
        allowed_blob_names: Option<&[String]>,
        strategy: Option<&crate::strategy::RetrievalStrategy>,
        mut audit: Option<&mut RetrievalAudit>,
    ) -> Vec<SearchHit> {
        tracing::info!("Path-boosted search for query: {query}");

        let enable_query_rewrite = strategy.map(|s| s.enable_query_rewrite).unwrap_or_else(|| self.query_rewriter.is_some());
        let enable_llm_rerank = strategy.map(|s| s.enable_llm_rerank).unwrap_or_else(|| self.llm_reranker.is_some());

        // 0. 查询改写变体（路径索引与内容索引共用）+ 启发式变体
        let mut queries_to_search = vec![query.to_string()];
        if enable_query_rewrite {
            if let Some(rewriter) = &self.query_rewriter {
                let _g = audit.as_deref_mut().map(|a| a.stage("rewrite"));
                if let Ok(rewritten) = rewriter.rewrite(query).await {
                    if !rewritten.is_empty() {
                        queries_to_search = rewritten;
                    }
                }
            }
        }

        // 1. 路径索引检索：原查询 + 改写变体分别检索，每个 blob 取最高路径分
        let mut path_scores: HashMap<String, f32> = HashMap::new();
        {
            let _g = audit.as_deref_mut().map(|a| a.stage("dense"));
            if let Some(path_store) = &self.path_store {
                for variant in std::iter::once(query).chain(queries_to_search.iter().map(String::as_str)) {
                    let Ok(query_vector) = self.embedder.embed_query(variant).await else { continue };
                    let Ok(path_results) = path_store
                        .search_paths(variant, &query_vector, allowed_blob_names, PATH_SEARCH_TOP_K)
                        .await
                    else {
                        continue;
                    };
                    for r in path_results {
                        let score = path_scores.entry(r.blob_name).or_insert(0.0);
                        if r.score > *score {
                            *score = r.score;
                        }
                    }
                }
                tracing::info!("Path index returned {} results", path_scores.len());
            }
        }

        // 2. 内容索引检索（常规流程）
        let mut content_hits: Vec<SearchHit> = vec![];
        {
            let _g = audit.as_deref_mut().map(|a| a.stage("dense"));
            let mut all_result_lists: Vec<Vec<SearchHit>> = Vec::new();
            for search_query in &queries_to_search {
                let planned = self.query_planner.plan(search_query);
                let num_queries = planned.len();
                if num_queries == 0 {
                    continue;
                }
                for planned_query in planned {
                    all_result_lists
                        .push(self.recall(&planned_query, allowed_blob_names, num_queries).await);
                }
            }
            if !all_result_lists.is_empty() {
                content_hits = fuse(
                    all_result_lists,
                    self.settings.rrf_k,
                    self.settings.query_facet_weight,
                    self.settings.default_top_k,
                );
            }
        }

        // 3. 融合：路径分数作为文件级加权，排序仍在 chunk 粒度上进行
        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("fuse"));
            if path_scores.is_empty() {
                content_hits
            } else {
                self.merge_path_and_content(&path_scores, content_hits).await
            }
        };

        // 4. 常规后处理。路径类查询使用文档中立的优先级因子。
        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("rerank"));
            match self.reranker.rerank(query, hits.clone()).await {
                Ok(h) => h,
                Err(exc) => {
                    tracing::warn!("rerank failed; keep fused order: {exc}");
                    hits
                }
            }
        };
        let hits = apply_source_priority(hits, path_query_priority_factor);

        let hits = if enable_llm_rerank {
            let _g = audit.as_deref_mut().map(|a| a.stage("llm_rerank"));
            match self.llm_reranker.as_ref() {
                Some(r) => match r.rerank(query, hits.clone()).await {
                    Ok(h) => h,
                    Err(exc) => {
                        // LLM 重排失败 → 静默退回原始顺序（与 Python 语义一致，绝不丢召回）
                        tracing::warn!("LLM rerank failed; fallback to original order: {exc}");
                        hits
                    }
                },
                None => hits,
            }
        } else {
            hits
        };

        {
            let _g = audit.as_deref_mut().map(|a| a.stage("select"));
            let hits = apply_confidence_floor(hits, self.settings.confidence_floor, path_query_priority_factor);
            self.selector.select(&hits, self.settings.final_select_k)
        }
    }

    /// 按 chunk 粒度合并路径命中与内容命中。
    ///
    /// 内容检索完全没覆盖到的文件才回填首个 chunk，保住纯文件名查询的召回。
    async fn merge_path_and_content(
        &self,
        path_scores: &HashMap<String, f32>,
        content_hits: Vec<SearchHit>,
    ) -> Vec<SearchHit> {
        let weight = self.settings.path_boost_weight;
        let mut merged: Vec<SearchHit> = Vec::new();
        let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();

        for hit in content_hits {
            let boost = path_scores.get(&hit.blob_name).copied();
            match boost {
                None => merged.push(hit),
                Some(b) => {
                    covered.insert(hit.blob_name.clone());
                    let mut hit = hit;
                    hit.score += weight * b;
                    merged.push(hit);
                }
            }
        }

        let missing: Vec<&String> = path_scores
            .keys()
            .filter(|name| !covered.contains(*name))
            .collect();
        if !missing.is_empty() {
            let backfilled = self.fetch_content_for_paths(&missing, path_scores).await;
            merged.extend(backfilled);
            tracing::info!(
                "Merged content hits with {} path hits (boosted={}, backfilled={})",
                path_scores.len(),
                covered.len(),
                missing.len()
            );
        }
        let mut merged = merged;
        merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        merged
    }

    /// 为路径索引结果获取实际内容：取该 blob 的首个 chunk 作为代表。
    ///
    /// 仅用于内容索引完全没召回的文件，让纯文件名查询至少能命中目标文件。
    /// 实现经由 [`BlobContentLookup`](self) 端口注入（Python 版直接查 SQL）。
    async fn fetch_content_for_paths(
        &self,
        blob_names: &[&String],
        path_scores: &HashMap<String, f32>,
    ) -> Vec<SearchHit> {
        let Some(lookup) = &self.first_chunk_lookup else {
            return vec![];
        };
        let names: Vec<String> = blob_names.iter().map(|s| (*s).clone()).collect();
        match lookup.first_chunks(&names).await {
            Ok(rows) => rows
                .into_iter()
                .map(|row| SearchHit {
                    score: path_scores.get(&row.blob_name).copied().unwrap_or(0.9),
                    ..row
                })
                .collect(),
            Err(_) => vec![],
        }
    }
}
