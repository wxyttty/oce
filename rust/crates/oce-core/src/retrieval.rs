//! RetrievalPipeline — 检索编排。与 Python `domain/services/retrieval.py` 语义对齐。
//!
//! 流程：
//! embed_query → store.search（dense 向量检索）
//! → 精确标识符召回 → 多查询结果融合 → rerank（逐篇精排）
//! → 源码优先（降权文档/测试）→ 置信度门槛 → select（最终 K 条）
//!
//! rerank 解决「单篇多相关」，select 解决「这一组够全且不冗余」，职责不同。

use crate::classifier::{classify_query_intent, extract_code_identifiers, Intent};
use crate::planner::QueryPlanner;
use crate::priority::{meta_dir_factor, path_query_priority_factor, source_priority_factor};
use crate::retrieval_settings::RetrievalSettings;
use crate::search::{
    search_hit_key, Embedder, ExactSearchStore, IntentClassifier, LlmReranker, PathSearchStore,
    QueryRewriter, Reranker, RetrievalAudit, SearchHit, SearchStore,
};
use crate::selector::CoverageSelector;
use crate::strategy::{get_strategy, LlmIntent};
use async_trait::async_trait;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 路径检索的固定召回量（与 Python 常量一致）。
const PATH_SEARCH_TOP_K: usize = 20;

// ── rerank 悬崖截断（BCE rerank.go 常数，RETRIEVAL_RERANK_CUTOFF_ENABLED）──
// 仅作用于 API rerank 的端点校准分（0..1）；LLM 重排只返回顺序不返回校准分，
// 保序回退/NoopReranker 无分数信号，均不截断（截断门在调用侧判定）。
/// 头部分数的此比例以下视为悬崖外（BCE rerankCutoffRatio）
const RERANK_CUTOFF_RATIO: f32 = 0.35;
/// 绝对下限：头部本身弱时比例线会放过同样弱的噪声（BCE rerankFloorScore）
const RERANK_FLOOR_SCORE: f32 = 0.10;
/// 最少保留条数：过短窗口会被误读为「再无其他」（BCE rerankMinKeep）
const RERANK_MIN_KEEP: usize = 6;

/// rerank 悬崖截断：丢弃 head×RATIO 与绝对下限双线以下的尾部。
///
/// 输入是 API rerank 打过校准分的候选（降序）；返回 (保留, 丢弃)。
/// 最少保留 RERANK_MIN_KEEP 条（不足时全保留）；空输入或头部非正直接原样返回。
/// 位置在 source priority 之前——截断判据是纯端点分数，不与路径降权因子纠缠；
/// 丢弃的尾部不进入后续阶段（它们本来就排在被丢弃项之后）。
fn rerank_cutoff(ranked: Vec<SearchHit>) -> (Vec<SearchHit>, Vec<SearchHit>) {
    if ranked.len() <= RERANK_MIN_KEEP {
        return (ranked, vec![]);
    }
    let top = match ranked.first() {
        Some(h) if h.score > 0.0 => h.score,
        _ => return (ranked, vec![]),
    };
    let cut = (top * RERANK_CUTOFF_RATIO).max(RERANK_FLOOR_SCORE);
    // 保底 min_keep 条，之后的弱尾部丢弃（首个低于双线者的位置即截断点）
    let first_below = ranked.iter().position(|h| h.score < cut);
    let split = match first_below {
        Some(pos) => pos.max(RERANK_MIN_KEEP),
        None => ranked.len(),
    };
    let mut ranked = ranked;
    let dropped = ranked.split_off(split.min(ranked.len()));
    (ranked, dropped)
}

/// 近重复判定用的行集合：非空行 trim 后去重（BCE lineSet）。
fn line_set(content: &str) -> std::collections::HashSet<String> {
    content
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// 行集合 Jaccard（BCE lineJaccard；空集合返回 0）。
fn line_jaccard(
    a: &std::collections::HashSet<String>,
    b: &std::collections::HashSet<String>,
) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let inter = small.iter().filter(|l| big.contains(*l)).count();
    inter as f32 / (a.len() + b.len() - inter) as f32
}

/// line_set/line_jaccard 的跨模块复用出口（selector 的 DupGuard 使用）。
pub(crate) fn line_set_public(content: &str) -> HashSet<String> {
    line_set(content)
}

pub(crate) fn line_jaccard_public(content: &str, seated: &HashSet<String>) -> f32 {
    // 与 line_jaccard 同式，但候选侧惰性建集（省一次分配）
    if seated.is_empty() {
        return 0.0;
    }
    let candidate = line_set(content);
    if candidate.is_empty() {
        return 0.0;
    }
    line_jaccard(&candidate, seated)
}

/// 按分数 × 路径惩罚因子稳定重排（只重排，不改 rerank 决策）。
/// 泛型闭包：调用侧把查询相关的因子（meta 降权）与纯路径因子组合传入。
fn apply_source_priority<F: Fn(&str) -> f32>(hits: Vec<SearchHit>, factor: F) -> Vec<SearchHit> {
    let mut hits = hits;
    hits.sort_by(|a, b| {
        let sa = a.score * factor(&a.path);
        let sb = b.score * factor(&b.path);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    hits
}

/// 逐条按有效分（score × penalty）剔除低于门槛的弱匹配。
fn apply_confidence_floor<F: Fn(&str) -> f32>(
    hits: Vec<SearchHit>,
    floor: f32,
    factor: F,
) -> Vec<SearchHit> {
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
    let max_score: f32 = weights.iter().map(|w| w / (rrf_k as f32 + 1.0)).sum();

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
        let candidate_window = llm_max_candidates
            .unwrap_or(settings.default_top_k)
            .min(settings.default_top_k);
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
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(settings.default_top_k);
    merged
}

/// 符号定位时，框架 endpoint 定义稳定优先于同名内部实现（对应 `_promote_symbol_endpoints`）。
fn promote_symbol_endpoints(query: &str, hits: Vec<SearchHit>) -> Vec<SearchHit> {
    if classify_query_intent(query) != Intent::Symbol {
        return hits;
    }
    const LOCATION_MARKERS: [&str; 11] = [
        "实现位置",
        "实现文件",
        "源码位置",
        "哪个文件",
        "在哪个文件",
        "在哪里定义",
        "哪里定义",
        "函数在哪",
        "defined",
        "definition",
        "implementation",
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
    /// broad regime 专用选择器（per-path ≤2，覆盖优先于深度）
    pub selector_broad: Arc<CoverageSelector>,
    pub query_planner: Arc<dyn QueryPlanner>,
    pub intent_classifier: Option<Arc<dyn IntentClassifier>>,
    /// 按 blob 名取首个 chunk 的端口（路径回填用）。
    pub first_chunk_lookup: Option<Arc<dyn FirstChunkLookup>>,
    /// 按 blob 名重构行文本的端口（span 合并/补全；RETRIEVAL_SPAN_MERGE_ENABLED）
    pub file_content_lookup: Option<Arc<dyn FileContentLookup>>,
    pub settings: RetrievalSettings,
    /// 外部模型故障冷却门（借鉴 BCE）：embed/rerank/LLM 任一通路失败后
    /// 短时间冷却，冷却期内直接走降级路径，避免死服务把之后每次检索
    /// 都拖满超时（LLM 超时上限 120s，不冷却时最坏每次检索都等满）。
    pub cooldowns: PipelineCooldowns,
    /// related symbols hints（输出层追加，不动排序；RETRIEVAL_RELATED_SYMBOLS_ENABLED）
    pub related_symbols_enabled: bool,
    /// broad regime（架构/概览探索查询；RETRIEVAL_BROAD_MODE_ENABLED，默认关）。
    /// regime 差异见 `broad` 模块：宽窗口 20 / per-path 2、manifest prior、骨架化。
    pub broad_mode_enabled: bool,
}

/// 三条外部通路各自的冷却门。进程内状态即可（个人模式单进程）。
#[derive(Default)]
pub struct PipelineCooldowns {
    pub embed: crate::cooldown::CooldownGate,
    pub rerank: crate::cooldown::CooldownGate,
    pub llm: crate::cooldown::CooldownGate,
}

/// 冷却时长（BCE 同款 30s）：足够覆盖常见瞬时故障（网关抖动、限流窗口），
/// 又不会让一次偶发失败长时间禁用通路。
const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// 按 blob 名取首个 chunk 的端口（对应 Python `_fetch_content_for_paths` 的 SQL 直查）。
#[async_trait]
pub trait FirstChunkLookup: Send + Sync {
    async fn first_chunks(&self, blob_names: &[String]) -> Result<Vec<SearchHit>, String>;
}

/// 按 blob 名重构行文本的端口（span 合并/补全用；RETRIEVAL_SPAN_MERGE_ENABLED）。
/// 返回 (blob_name, 行号从 1 起的行文本) 列表；缺失的 blob 不出现在结果里
/// （调用方回退原样保留）。
#[async_trait]
pub trait FileContentLookup: Send + Sync {
    async fn blob_lines(
        &self,
        blob_names: &[String],
    ) -> Result<Vec<(String, Vec<(u32, String)>)>, String>;
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
            // broad regime 的覆盖度优先选择器：per-path 收紧到 2（上限，
            // 用户配得更紧时保持更紧），字符预算与重叠抑制沿用全局设置
            selector_broad: Arc::new(
                CoverageSelector::new(
                    settings
                        .max_chunks_per_path
                        .min(crate::broad::BROAD_MAX_CHUNKS_PER_PATH),
                    settings.max_context_chars,
                    settings.overlap_threshold,
                )
                .expect("invalid broad coverage selector settings"),
            ),
            query_planner: Arc::new(
                crate::planner::HeuristicQueryPlanner::new(
                    planner_max,
                    settings.query_min_facet_chars,
                )
                .expect("invalid planner settings"),
            ),
            reranker: Arc::new(crate::search::NoopReranker),
            llm_reranker: None,
            query_rewriter: None,
            path_store: None,
            exact_store: None,
            intent_classifier: None,
            first_chunk_lookup: None,
            file_content_lookup: None,
            embedder,
            store,
            settings,
            cooldowns: PipelineCooldowns::default(),
            related_symbols_enabled: false,
            broad_mode_enabled: false,
        }
    }

    /// 注入首个 chunk 查询端口。
    pub fn with_first_chunk_lookup(mut self, lookup: Arc<dyn FirstChunkLookup>) -> Self {
        self.first_chunk_lookup = Some(lookup);
        self
    }

    /// 执行一次检索，返回最终命中列表（按融合分降序）。
    /// related symbols hints（开启时）写入 audit.related_symbols，由调用方
    /// （formatter 层）拼进 formatted 输出——检索返回值本身仍是纯 hits，
    /// 不动结果集构成。
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
        // 语义路缺席标记：冷却门已触发即视为缺席（formatter 据此注入 degraded 提示）
        if let Some(a) = audit.as_deref_mut() {
            a.semantic_degraded = self.cooldowns.embed.is_down();
        }
        // 弱匹配判定（借鉴 BCE rerankWeakTop）：头部命中分低于阈值即视为弱。
        // 阈值取 0.30 —— 融合分是归一化 RRF（≤1），exact 召回按 kind 打 0.85-1.0；
        // 0.30 以下的头部意味着既无 exact 命中、语义/词法头部也弱，本仓库大概率没有答案。
        const WEAK_MATCH_HEAD: f32 = 0.30;
        let finish = |hits: Vec<SearchHit>, audit: Option<&mut RetrievalAudit>| {
            if let Some(a) = audit {
                a.weak_match = hits.first().is_none_or(|h| h.score < WEAK_MATCH_HEAD);
            }
            hits
        };

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

        // broad regime 触发（RETRIEVAL_BROAD_MODE_ENABLED 总闸）：LLM intent 判为
        // Overview ∨ 启发式架构词表（无 LLM 分类时的 fallback，二者 OR），
        // 定位语气查询（「…在哪里」）一票否决——locator 有明确目标，不是
        // 探索型，无论触发源（flask Q05 教训：环境变量命中词表的定位查询
        // 被 manifest prior 泛滥淹没）。路径增强查询不参与 broad——定位型
        // 查询有自己的 regime（BCE 裸「配置」教训同源）。
        let broad = self.broad_mode_enabled
            && !crate::broad::is_locator_query(query)
            && (strategy.as_ref().is_some_and(|s| s.broad)
                || crate::broad::query_wants_structure(query));
        if broad {
            if let Some(a) = audit.as_deref_mut() {
                a.broad = true;
            }
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
                lists.push(
                    self.recall(&planned_query, allowed_blob_names, num_queries)
                        .await,
                );
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

        // broad regime 的 manifest prior（BCE manifestBoost）：清单/配置/全局样式
        // 文件作为一路额外结果表进 RRF——架构类查询的应答散布在这些文件里，而
        // 它们在任何排序路上都不像查询文本。列表按 path 字典序（刻意不复述
        // dense 排序，注入新排序信息），以 facet 权重并入融合。
        if broad && !all_result_lists.is_empty() {
            let prior = crate::broad::manifest_prior_list(&all_result_lists);
            if !prior.is_empty() {
                all_result_lists.push(prior);
            }
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

        // rerank 候选池截断（RETRIEVAL_RERANK_POOL_K，默认 0=不截断）：
        // 向量召回 default_top_k 与 rerank 池解耦——大池保融合质量，
        // 小池让 reranker 集中在嵌入头部候选上。
        let hits = if self.settings.rerank_pool_k > 0 && hits.len() > self.settings.rerank_pool_k {
            hits.into_iter().take(self.settings.rerank_pool_k).collect()
        } else {
            hits
        };

        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("rerank"));
            // rerank 冷却期内直接保序回退，不再打外部端点
            if self.cooldowns.rerank.is_down() {
                hits
            } else {
                // rerank 失败保序回退（Python 侧 rerank 异常向上抛，这里选择不丢召回）
                match self.reranker.rerank(query, hits.clone()).await {
                    Ok(outcome) => {
                        // 悬崖截断（默认关）：只对端点校准分生效；未打分候选
                        // （融合分）不参与判定，直接接在保留尾部之后
                        let (ranked, dropped) =
                            if !outcome.ranked.is_empty() && self.settings.rerank_cutoff_enabled {
                                rerank_cutoff(outcome.ranked)
                            } else {
                                (outcome.ranked, vec![])
                            };
                        if !dropped.is_empty() {
                            tracing::debug!(
                                "rerank cutoff dropped {} tail candidates",
                                dropped.len()
                            );
                        }
                        let mut hits = outcome.unscored;
                        hits.splice(0..0, ranked);
                        hits
                    }
                    Err(exc) => {
                        self.cooldowns.rerank.trip(COOLDOWN);
                        tracing::warn!(
                            "rerank failed; keep fused order, paused {COOLDOWN:?}: {exc}"
                        );
                        hits
                    }
                }
            }
        };
        // 源码优先 × 元目录降权（RETRIEVAL_META_DIR_PENALTY_ENABLED）。meta 因子
        // 需要查询语境（.github 文件对 CI 意图查询豁免），只在主检索路生效——
        // 路径增强路保持文档中立（任何文件都可能是定位目标）。
        let meta_enabled = self.settings.meta_dir_penalty_enabled;
        let factor = |p: &str| source_priority_factor(p) * meta_dir_factor(meta_enabled, query, p);
        let hits = apply_source_priority(hits, factor);

        let use_llm_rerank = strategy
            .as_ref()
            .map(|s| s.enable_llm_rerank)
            .unwrap_or_else(|| self.llm_reranker.is_some());
        let hits = if use_llm_rerank {
            let _g = audit.as_deref_mut().map(|a| a.stage("llm_rerank"));
            match self.llm_reranker.as_ref() {
                // LLM 重排失败 → 静默退回原始顺序（与 Python 语义一致，绝不丢召回）
                // 冷却期内直接跳过，不再等满超时
                Some(r) if !self.cooldowns.llm.is_down() => {
                    match r.rerank(query, hits.clone()).await {
                        Ok(h) => h,
                        Err(exc) => {
                            self.cooldowns.llm.trip(COOLDOWN);
                            tracing::warn!("LLM rerank failed; fallback to original order, paused {COOLDOWN:?}: {exc}");
                            hits
                        }
                    }
                }
                _ => hits,
            }
        } else {
            hits
        };

        let hits = promote_symbol_endpoints(query, hits);

        {
            let mut selected = {
                let _g = audit.as_deref_mut().map(|a| a.stage("select"));
                let hits = apply_confidence_floor(hits, self.settings.confidence_floor, factor);
                let select_k = if broad {
                    crate::broad::BROAD_FINAL_SELECT_K
                } else {
                    self.settings.final_select_k
                };
                // 相邻合并 + 小片段补全（RETRIEVAL_SPAN_MERGE_ENABLED）：select
                // 池先加宽，合并缩窗后按 select_k 收口，融合腾出的席位由次优
                // 排名回填（semble M1 教训：截断后再合并窗口会净缩水）。
                let merge_enabled =
                    self.settings.span_merge_enabled && self.file_content_lookup.is_some();
                let pool = if merge_enabled {
                    select_k + crate::span_merge::MERGE_POOL_EXTRA
                } else {
                    select_k
                };
                let pooled = if broad {
                    // broad regime：宽窗口 + per-path 收紧，覆盖优先于深度
                    self.selector_broad.select(&hits, pool)
                } else {
                    self.selector.select(&hits, pool)
                };
                if !merge_enabled {
                    pooled
                } else {
                    let need = crate::span_merge::blobs_needing_lines(&pooled);
                    let lines: std::collections::HashMap<String, Vec<(u32, String)>> =
                        if need.is_empty() {
                            Default::default()
                        } else {
                            match self
                                .file_content_lookup
                                .as_ref()
                                .expect("checked Some in merge_enabled")
                                .blob_lines(&need)
                                .await
                            {
                                Ok(rows) => rows.into_iter().collect(),
                                Err(_) => Default::default(), // 端口失败 → 全部回退原样
                            }
                        };
                    let merged = crate::span_merge::merge_and_pad_spans(&pooled, &lines);
                    // 补全让内容变长：预算硬限制在此收口
                    crate::span_merge::enforce_budget(
                        merged,
                        select_k,
                        self.settings.max_context_chars,
                    )
                }
            };
            {
                let selected_ref: &[SearchHit] = &selected;
                if self.related_symbols_enabled {
                    self.attach_related_symbols(
                        query,
                        allowed_blob_names,
                        selected_ref,
                        audit.as_deref_mut(),
                    )
                    .await;
                }
            }
            if broad {
                // 骨架化在 related symbols 之后：hints 的标识符取自全文，
                // 省略段不该丢信号。长摘录压缩为候选骨架，真实行号区间
                // 由标记行携带（formatter 重同步渲染）。
                let terms = crate::broad::skeleton_terms(query);
                for hit in &mut selected {
                    hit.content =
                        crate::broad::skeletonize(&hit.path, hit.start_line, &hit.content, &terms);
                }
            }
            finish(selected, audit)
        }
    }

    /// related symbols hints：选中 hits 的标识符 → 定义位置（不在窗口内）。
    /// 失败静默跳过（旁路能力，不得影响主链路）。
    async fn attach_related_symbols(
        &self,
        query: &str,
        allowed_blob_names: Option<&[String]>,
        selected: &[SearchHit],
        audit: Option<&mut RetrievalAudit>,
    ) {
        let Some(audit) = audit else {
            return;
        };
        // 标识符来源：选中 hits 的 content（引用侧）+ 查询本身（用户点名但
        // 未入窗的符号）。去重后批量查定义。
        let mut identifiers: Vec<String> = Vec::new();
        for hit in selected {
            identifiers.extend(crate::related::ident_tokens(&hit.content));
        }
        identifiers.extend(crate::symbol::SymbolExtractor::extract_identifiers_from_query(query));
        identifiers.sort();
        identifiers.dedup();
        if identifiers.is_empty() {
            return;
        }
        let Some(store) = &self.exact_store else {
            return;
        };
        // stage guard 在 store 借用之前 drop，避免 audit 双重借用
        let hints = {
            let _g = audit.stage("related");
            match store
                .find_definitions(&identifiers, allowed_blob_names)
                .await
            {
                Ok(d) => crate::related::related_symbol_hints(selected, &d),
                Err(_) => return,
            }
        };
        audit.related_symbols = hints;
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
        // 嵌入服务故障冷却：冷却期内直接放弃语义召回（词法/结构路仍在），
        // 不再每次都等满超时
        if self.cooldowns.embed.is_down() {
            return vec![];
        }
        let query_vector = match self.embedder.embed_query(query).await {
            Ok(v) => v,
            Err(exc) => {
                self.cooldowns.embed.trip(COOLDOWN);
                tracing::warn!("embed_query failed; semantic recall paused {COOLDOWN:?}: {exc}");
                return vec![]; // 嵌入失败不阻断其它子查询
            }
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
            .search_exact(
                &identifiers,
                allowed_blob_names,
                self.settings.default_top_k,
            )
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
        // 弱匹配判定与主路径同一阈值；路径查询的头部分含 path boost，
        // 纯文件名命中（回填 0.9）不会误判为弱
        const WEAK_MATCH_HEAD: f32 = 0.30;
        let finish = |hits: Vec<SearchHit>, audit: Option<&mut RetrievalAudit>| {
            if let Some(a) = audit {
                a.weak_match = hits.first().is_none_or(|h| h.score < WEAK_MATCH_HEAD);
            }
            hits
        };

        let enable_query_rewrite = strategy
            .map(|s| s.enable_query_rewrite)
            .unwrap_or_else(|| self.query_rewriter.is_some());
        let enable_llm_rerank = strategy
            .map(|s| s.enable_llm_rerank)
            .unwrap_or_else(|| self.llm_reranker.is_some());

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
                // 嵌入冷却期内跳过路径向量化（词法路仍在），不再等满超时
                if !self.cooldowns.embed.is_down() {
                    for variant in
                        std::iter::once(query).chain(queries_to_search.iter().map(String::as_str))
                    {
                        let Ok(query_vector) = self.embedder.embed_query(variant).await else {
                            self.cooldowns.embed.trip(COOLDOWN);
                            break;
                        };
                        let Ok(path_results) = path_store
                            .search_paths(
                                variant,
                                &query_vector,
                                allowed_blob_names,
                                PATH_SEARCH_TOP_K,
                            )
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
                    all_result_lists.push(
                        self.recall(&planned_query, allowed_blob_names, num_queries)
                            .await,
                    );
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
                self.merge_path_and_content(&path_scores, content_hits)
                    .await
            }
        };

        // 4. 常规后处理。路径类查询使用文档中立的优先级因子。
        let hits = {
            let _g = audit.as_deref_mut().map(|a| a.stage("rerank"));
            if self.cooldowns.rerank.is_down() {
                hits
            } else {
                match self.reranker.rerank(query, hits.clone()).await {
                    Ok(outcome) => {
                        // 悬崖截断（默认关）：只对端点校准分生效
                        let (ranked, dropped) =
                            if !outcome.ranked.is_empty() && self.settings.rerank_cutoff_enabled {
                                rerank_cutoff(outcome.ranked)
                            } else {
                                (outcome.ranked, vec![])
                            };
                        if !dropped.is_empty() {
                            tracing::debug!(
                                "rerank cutoff dropped {} tail candidates",
                                dropped.len()
                            );
                        }
                        let mut hits = outcome.unscored;
                        hits.splice(0..0, ranked);
                        hits
                    }
                    Err(exc) => {
                        self.cooldowns.rerank.trip(COOLDOWN);
                        tracing::warn!(
                            "rerank failed; keep fused order, paused {COOLDOWN:?}: {exc}"
                        );
                        hits
                    }
                }
            }
        };
        let hits = apply_source_priority(hits, path_query_priority_factor);

        let hits = if enable_llm_rerank {
            let _g = audit.as_deref_mut().map(|a| a.stage("llm_rerank"));
            match self.llm_reranker.as_ref() {
                Some(r) if !self.cooldowns.llm.is_down() => {
                    match r.rerank(query, hits.clone()).await {
                        Ok(h) => h,
                        Err(exc) => {
                            self.cooldowns.llm.trip(COOLDOWN);
                            tracing::warn!("LLM rerank failed; fallback to original order, paused {COOLDOWN:?}: {exc}");
                            hits
                        }
                    }
                }
                _ => hits,
            }
        } else {
            hits
        };

        {
            let selected = {
                let _g = audit.as_deref_mut().map(|a| a.stage("select"));
                let hits = apply_confidence_floor(
                    hits,
                    self.settings.confidence_floor,
                    path_query_priority_factor,
                );
                self.selector.select(&hits, self.settings.final_select_k)
            };
            let selected_ref: &[SearchHit] = &selected;
            if self.related_symbols_enabled {
                self.attach_related_symbols(
                    query,
                    allowed_blob_names,
                    selected_ref,
                    audit.as_deref_mut(),
                )
                .await;
            }
            finish(selected, audit)
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
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scored_hit(path: &str, score: f32) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}"),
            path: path.into(),
            content: "x".into(),
            score,
            content_hash: String::new(),
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn cutoff_drops_tail_below_ratio_and_floor() {
        // head=0.90 → 比例线 0.315；绝对下限 0.10 不生效
        let hits: Vec<SearchHit> = [
            ("a.rs", 0.90),
            ("b.rs", 0.80),
            ("c.rs", 0.70),
            ("d.rs", 0.60),
            ("e.rs", 0.50),
            ("f.rs", 0.40),
            ("g.rs", 0.31), // 双线之上：保留（第 7 条）
            ("h.rs", 0.30), // 悬崖外：丢弃
            ("i.rs", 0.05),
        ]
        .into_iter()
        .map(|(p, s)| scored_hit(p, s))
        .collect();
        let (kept, dropped) = rerank_cutoff(hits);
        // 悬崖在 g.rs（0.31 < 0.315）出现：保底 6 条，g/h/i 丢弃
        assert_eq!(kept.len(), 6);
        assert_eq!(dropped.len(), 3);
        assert_eq!(dropped[0].path, "g.rs");
    }

    #[test]
    fn cutoff_floor_kicks_in_when_head_is_weak() {
        // head=0.20 → 比例线 0.07，绝对下限 0.10 接管
        let hits: Vec<SearchHit> = [
            ("a.rs", 0.20),
            ("b.rs", 0.18),
            ("c.rs", 0.15),
            ("d.rs", 0.12),
            ("e.rs", 0.11),
            ("f.rs", 0.10),
            ("g.rs", 0.09), // < 0.10 下限：丢弃
            ("h.rs", 0.08),
        ]
        .into_iter()
        .map(|(p, s)| scored_hit(p, s))
        .collect();
        let (kept, dropped) = rerank_cutoff(hits);
        assert_eq!(kept.len(), 6);
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn cutoff_min_keep_protects_short_windows() {
        // 只有 5 条（< MIN_KEEP=6）：全保留，即便尾部低于双线
        let hits: Vec<SearchHit> = [
            ("a.rs", 0.90),
            ("b.rs", 0.80),
            ("c.rs", 0.10),
            ("d.rs", 0.05),
            ("e.rs", 0.01),
        ]
        .into_iter()
        .map(|(p, s)| scored_hit(p, s))
        .collect();
        let (kept, dropped) = rerank_cutoff(hits);
        assert_eq!(kept.len(), 5);
        assert!(dropped.is_empty());
    }

    #[test]
    fn cutoff_min_keep_backfills_to_six() {
        // 悬崖在第 2 条就出现：仍保底 6 条（弱结果 + weak 提示好过过短窗口）
        let hits: Vec<SearchHit> = (0..10)
            .map(|i| {
                scored_hit(
                    &format!("f{i}.rs"),
                    if i < 2 { 0.9 - i as f32 * 0.1 } else { 0.05 },
                )
            })
            .collect();
        let (kept, dropped) = rerank_cutoff(hits);
        assert_eq!(kept.len(), RERANK_MIN_KEEP);
        assert_eq!(dropped.len(), 4);
    }

    #[test]
    fn cutoff_noop_on_zero_head_or_empty() {
        let (kept, dropped) = rerank_cutoff(vec![]);
        assert!(kept.is_empty() && dropped.is_empty());
        // 头部非正（无分数信号）：不截断
        let hits = vec![scored_hit("a.rs", 0.0), scored_hit("b.rs", 0.0)];
        let (kept, _) = rerank_cutoff(hits);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn line_set_and_jaccard_semantics() {
        let a = line_set("x = 1\n\n  y = 2  \nx = 1\n");
        assert_eq!(a.len(), 2, "空行剔除、trim、去重");
        let b = line_set("x = 1\ny = 2\nz = 3\n");
        // 交集 2，并集 3 → 2/3
        assert!((line_jaccard(&a, &b) - 2.0 / 3.0).abs() < 1e-6);
        // 完全相同 → 1.0；空集合 → 0.0
        assert!((line_jaccard(&a, &a) - 1.0).abs() < 1e-6);
        assert_eq!(line_jaccard(&a, &line_set("")), 0.0);
    }

    #[test]
    fn jaccard_gate_boundary() {
        // 10 行中 7 行重叠 → Jaccard = 7/13 ≈ 0.538 < 0.7：不拦
        let base: String = (0..10).map(|i| format!("line{i}\n")).collect();
        let variant: String = (0..7)
            .map(|i| format!("line{i}\n"))
            .chain((0..3).map(|i| format!("other{i}\n")))
            .collect();
        let seated = line_set(&base);
        assert!(line_jaccard_public(&variant, &seated) < 0.7);
        // 10 行中 9 行重叠 → Jaccard = 9/11 ≈ 0.818 ≥ 0.7：拦
        let near: String = (0..9)
            .map(|i| format!("line{i}\n"))
            .chain(std::iter::once("extra\n".to_string()))
            .collect();
        assert!(line_jaccard_public(&near, &seated) >= 0.7);
    }
}
