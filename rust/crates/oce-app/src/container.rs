//! 进程级 composition root。与 Python `application/container.py` 对齐：
//! 装配存储（SQLite + TriviumDB）、嵌入器（凭据解析）、切块器、检索管线、监控。
//! API router 不编排业务流程，只消费本层暴露的 `RetrievalApplication`。

use oce_core::chunk::Chunker;
use oce_core::indexing::{EmbeddingGate, IndexingPipeline};
use oce_core::retrieval::RetrievalPipeline;
use oce_core::retrieval_settings::RetrievalSettings;
use oce_core::search::{
    IntentClassifier, LlmReranker, PathSearchStore, QueryRewriter, Reranker,
    SearchHit, SearchStore,
};
use oce_infra::credentials::CredentialConfiguredReranker;
use oce_infra::settings::Settings;
use oce_infra::static_embed::StaticEmbedder;
use oce_infra::sqlite::chains::SqlChainRepository;
use oce_infra::sqlite::credentials::{SqlCredentialAdminStore, RuntimeCredential};
use oce_infra::sqlite::metrics::SqlMetricsSink;
use oce_infra::sqlite::repos::SqlBlobRepository;
use std::sync::Arc;

/// 嵌入开关（EMBED_ENABLED 的运行时快照）。
struct EmbeddingGateImpl {
    enabled: std::sync::atomic::AtomicBool,
}

impl EmbeddingGate for EmbeddingGateImpl {
    fn enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// API rerank 适配器：把 Cohere 风格 /v1/rerank 端点接到 Reranker 协议。
///
/// 文档文本沿用 Python `OpenAIReranker._document_text` 的 "File/Lines/正文" 格式；
/// 端点失败时上抛错误，由检索管线保序回退（不丢召回）。
struct ApiRerankAdapter {
    runtime: Arc<CredentialConfiguredReranker>,
    top_n: usize,
    max_doc_chars: usize,
}

#[async_trait::async_trait]
impl Reranker for ApiRerankAdapter {
    async fn rerank(&self, query: &str, hits: Vec<SearchHit>) -> oce_core::error::OceResult<Vec<SearchHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        // 候选窗口与 LLM 重排对齐：rerank 是逐篇精排，窗口过浅会截断融合结果
        let window = hits.len().min(self.top_n.max(1));
        let candidates: Vec<SearchHit> = hits.into_iter().take(window).collect();
        let documents: Vec<String> = candidates
            .iter()
            .map(|hit| {
                let body: String = hit.content.chars().take(self.max_doc_chars).collect();
                format!(
                    "File: {}\nLines: {}-{}\n\n{}",
                    hit.path, hit.start_line, hit.end_line, body
                )
            })
            .collect();
        let results = self
            .runtime
            .rerank(query, &documents, None)
            .await?;
        let mut ranked: Vec<SearchHit> = Vec::with_capacity(results.len());
        let mut leftover: Vec<SearchHit> = candidates;
        for (index, score) in results {
            if let Some(mut hit) = leftover.get(index).cloned() {
                hit.score = score;
                ranked.push(hit);
            }
        }
        // 未被端点返回的候选按原序补齐，保证 rerank 不减少结果数（select 层依赖完整候选）
        let ranked_keys: std::collections::HashSet<_> =
            ranked.iter().map(oce_core::search::search_hit_key).collect();
        for hit in leftover.drain(..) {
            let key = oce_core::search::search_hit_key(&hit);
            if !ranked_keys.contains(&key) {
                ranked.push(hit);
            }
        }
        Ok(ranked)
    }
}

/// LLM 语义重排器（llm_rerank kind）。
pub struct LlmRerankerImpl {
    client: Arc<oce_infra::credentials::CredentialConfiguredLlmClient>,
    model: String,
    max_candidates: usize,
    output_top_k: usize,
    snippet_chars: usize,
}

/// LLM rerank / rewrite / intent 的 prompt —— 从 Python prompts.py 逐字移植（AST 提取），
/// 含 few-shot 示例与决策树；小参数模型对 prompt 结构高度敏感，禁止意译压缩。
const RERANK_SYSTEM_PROMPT: &str = r##"You are a code search reranker. Your only job is to order the candidate code snippets by how well they answer the query.

Ranking priorities (highest first):
1. Whether the snippet body actually implements or defines what the query asks about — this outweighs how closely the path matches.
2. If the query names a symbol (function, class, type, constant), prefer the snippet holding its definition over one that merely re-exports or calls it.
3. Rely on the path as the main signal only when the query is about a config file or a file location.
4. The query may be in any language (often Chinese) while the identifiers are English — match on meaning, not on literal characters.
5. When the query asks where a specific feature or module is implemented, the dedicated file that actually contains that logic outranks an aggregation entry point that only registers, re-exports, or forwards it (lib.rs / mod.rs / index.ts / an App or store barrel). An aggregation entry only wires the feature up; it is not the feature itself.

<example>
<query>Where is the hook for dark mode switching?</query>
<candidates>
<candidate id="1" path="src/hooks/useTheme.ts" lines="1-3">
export { useDarkMode } from '../lib/appearance';
</candidate>
<candidate id="2" path="src/styles/dark.css" lines="1-4">
.dark { background: #111; }
</candidate>
<candidate id="3" path="src/lib/appearance.ts" lines="42-58">
export function useDarkMode() {
  const [dark, setDark] = useState(false);
  return { dark, toggle: () => setDark(v => !v) };
}
</candidate>
</candidates>
<answer>
3
1
2
</answer>
</example>

Why: 3 is the real implementation, so it ranks first; 1 has the closest-looking path but only re-exports, so it ranks second; 2 is unrelated to the Hook, so it ranks last.

<example>
<query>where is the application startup initialization implemented?</query>
<candidates>
<candidate id="1" path="src/lib.rs" lines="10-14">
mod startup;
pub fn run() { startup::bootstrap(); }
</candidate>
<candidate id="2" path="src/startup.rs" lines="1-18">
pub fn bootstrap() {
    load_config();
    connect_database();
    spawn_workers();
}
</candidate>
<candidate id="3" path="src/main.rs" lines="1-3">
fn main() { app::run(); }
</candidate>
</candidates>
<answer>
2
1
3
</answer>
</example>

Why: 2 actually implements the initialization flow, so it ranks first; 1 only declares the module and calls it (an aggregation entry), so it ranks second; 3 is just the process entry shell, unrelated to the flow, so it ranks last.

Output only the id numbers, one per line. No explanations, no paths, code, or tags."##;

const RERANK_USER_TEMPLATE: &str = r##"<query>{query}</query>

<candidates count="{count}">
{candidates}
</candidates>

From the {count} candidates above, choose at most {top_k} that best match <query>, ordered by relevance (most relevant first).
Output only the candidate id numbers, one per line. If fewer than {top_k} are relevant, output fewer — do not pad."##;

const REWRITE_PROMPT_TEMPLATE: &str = r##"You are a code-search query rewriting assistant. Rewrite the user query into {num_rewrites} search variants from different angles to improve recall.

User query: {query}

Rewrite strategies:
1. Filename variant: list 2-4 likely REAL filenames separated by spaces, each with a file extension. E.g. for a Python package config file output "pyproject.toml setup.py setup.cfg requirements.txt"; for a version-history/changelog file output "CHANGES.rst CHANGELOG.md HISTORY.rst"; for a JS config output "package.json tsconfig.json webpack.config.js"
2. English keyword variant: translate non-English terms into English technical terminology
3. Functional-description variant: describe it using code-related functional terms

Requirements:
- The filename variant must end with a file extension (.toml .py .rst .md .json .yaml .txt .cfg .ini)
- Prefer exact, common filename conventions used by real projects over vague descriptions
- Keep each query short and precise (within 5-10 words)
- Avoid repeating words
- Output exactly {num_rewrites} lines, one query per line, with no numbering and no extra text

Output the rewritten queries directly (one per line):"##;

const INTENT_SYSTEM_PROMPT: &str = r##"You are an expert at classifying the intent of code-search queries. Task: label the query and return only a single letter (S/C/R/P/F/O/M).

Classification rules (in priority order):

1. A concrete code symbol is present (backticked `func`, snake_case, CamelCase, :: paths):
   - Asks "where is it defined" / "implementation location" / "source" / "which file defines it" / "where is the function" → S
   - Asks "what does it register" / "what does it contain" (querying the symbol's contents) → S
   - Asks "in which files is it used" / "usage locations" (static reference lookup) → S
   - Asks "full call chain" / "from X to Y" / "call path" / "how is it triggered" / "how is it used" / "front-to-back-end" → C
   - Asks "how to call it" / "how to use it" / "API usage" (single-point lookup, no flow words) → R

2. No concrete symbol, but a filename/extension is present:
   - An explicit filename (.toml/.json/.rs) or "where is the config file" → P

3. No concrete symbol and no filename:
   - Asks about "architecture" / "mechanism" / "scheduling" / "event handling" / "state management" / "front-back-end interaction" → O
   - Asks "how is it handled" / "where is the logic" / "where is it implemented" (functional description) → F
   - Asks about "the definition of X" but X is not a concrete symbol (e.g. "error types") → F
   - Asks about "front-back-end" / cross-language types or data flow (no concrete symbol) → O
   - Multiple "and" / "as well as" / "plus" conditions → M

Important:
- Symbols take priority! "`func` in file.rs" is still S, not P.
- R vs C is about scope: single-point API usage = R, multi-step flow = C.
- For "definition", check whether a concrete symbol is present: with a symbol = S, without = F."##;

const INTENT_USER_TEMPLATE: &str = r##"
Query: {query}
Label:"##;


/// prompt 回显拦截（小参数模型会把指令原样回显）。
const PROMPT_ECHO_MARKERS: [&str; 12] = [
    "改写策略", "用户查询", "改写后的查询", "搜索关键词", "召回率", "每行一个",
    "不要编号", "文件名版本", "英文关键词版本", "功能描述版本", "查询改写助手", "要求:",
];
const MAX_REWRITE_CHARS: usize = 80;

#[async_trait::async_trait]
impl LlmReranker for LlmRerankerImpl {
    fn max_candidates(&self) -> usize {
        self.max_candidates
    }

    async fn rerank(
        &self,
        query: &str,
        candidates: Vec<oce_core::search::SearchHit>,
    ) -> oce_core::error::OceResult<Vec<oce_core::search::SearchHit>> {
        use oce_core::search::SearchHit;
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let top_k = self.output_top_k.min(candidates.len());
        // 与 Python _format_candidate 对齐：正文 strip 后截断、中和闭合标签、
        // 路径引号转义——候选正文若含 "</candidate" 会提前撕裂结构。
        let mut parts: Vec<String> = Vec::new();
        for (i, hit) in candidates.iter().take(self.max_candidates).enumerate() {
            let path = hit.path.replace('"', "&quot;");
            let raw = hit.content.trim();
            let mut snippet: String = raw.chars().take(self.snippet_chars).collect();
            if snippet.chars().count() < raw.chars().count() {
                snippet.push_str("\n…");
            }
            snippet = snippet.replace("</candidate", "<\\/candidate");
            let open_tag = format!(
                "<candidate id=\"{}\" path=\"{}\" lines=\"{}-{}\">",
                i + 1,
                path,
                hit.start_line,
                hit.end_line
            );
            if snippet.is_empty() {
                parts.push(format!("{open_tag}</candidate>"));
            } else {
                parts.push(format!("{open_tag}\n{snippet}\n</candidate>"));
            }
        }
        let user = RERANK_USER_TEMPLATE
            .replace("{query}", query)
            .replace("{count}", &candidates.len().min(self.max_candidates).to_string())
            .replace("{candidates}", &parts.join("\n"))
            .replace("{top_k}", &self.output_top_k.to_string());
        let messages = vec![
            serde_json::json!({"role": "system", "content": RERANK_SYSTEM_PROMPT}),
            serde_json::json!({"role": "user", "content": user}),
        ];
        // Python 同款温度 0.1
        let response = self.client.chat(&messages, &self.model, 0.1, None).await?;
        // 解析：每行取首个整数（容忍 "1." / "- 1" / "[1] path" 变体），去重 + 越界丢弃
        let mut order: Vec<usize> = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for line in response.lines() {
            let mut num = String::new();
            for c in line.chars() {
                if c.is_ascii_digit() {
                    num.push(c);
                } else if !num.is_empty() {
                    break;
                }
            }
            let Ok(idx) = num.parse::<usize>() else { continue };
            if idx >= 1 && idx <= self.max_candidates {
                let index = idx - 1;
                if index < candidates.len() && seen.insert(index) {
                    order.push(index);
                }
            }
        }
        order.truncate(top_k);
        let mut ranked: Vec<SearchHit> = order.iter().map(|&i| candidates[i].clone()).collect();
        // LLM 返回不足时按原始顺序补齐；完全无有效输出 → 保持原序
        if ranked.len() < top_k {
            for hit in &candidates {
                if ranked.len() >= top_k {
                    break;
                }
                let key = oce_core::search::search_hit_key(hit);
                if !ranked.iter().any(|r| oce_core::search::search_hit_key(r) == key) {
                    ranked.push(hit.clone());
                }
            }
        }
        if ranked.is_empty() {
            ranked = candidates.into_iter().take(top_k).collect();
        }
        Ok(ranked)
    }
}
/// 查询改写器。
struct QueryRewriterImpl {
    client: Arc<oce_infra::credentials::CredentialConfiguredLlmClient>,
    model: String,
    num_rewrites: usize,
}

#[async_trait::async_trait]
impl QueryRewriter for QueryRewriterImpl {
    async fn rewrite(&self, query: &str) -> oce_core::error::OceResult<Vec<String>> {
        if query.trim().is_empty() {
            return Ok(vec![query.to_string()]);
        }
        let prompt = REWRITE_PROMPT_TEMPLATE
            .replace("{num_rewrites}", &self.num_rewrites.to_string())
            .replace("{query}", query);
        let messages = vec![serde_json::json!({"role": "user", "content": prompt})];
        let response = self.client.chat(&messages, &self.model, 0.2, None).await?;
        let mut rewritten: Vec<String> = Vec::new();
        for line in response.lines() {
            let mut cleaned = line.trim();
            // 去编号/markdown 前缀
            cleaned = cleaned
                .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '*' | '•' | ' ' | '\t'));
            if cleaned.len() > MAX_REWRITE_CHARS
                || cleaned.chars().count() < 3
                || PROMPT_ECHO_MARKERS.iter().any(|m| cleaned.contains(m))
            {
                continue;
            }
            rewritten.push(cleaned.to_string());
            if rewritten.len() >= self.num_rewrites {
                break;
            }
        }
        // 原查询始终在列表首位
        if !rewritten.iter().any(|r| r == query) {
            rewritten.insert(0, query.to_string());
        }
        Ok(rewritten)
    }
}

/// 意图分类器。
struct IntentClassifierImpl {
    client: Arc<oce_infra::credentials::CredentialConfiguredLlmClient>,
    model: String,
}

#[async_trait::async_trait]
impl IntentClassifier for IntentClassifierImpl {
    async fn classify(
        &self,
        query: &str,
    ) -> oce_core::error::OceResult<oce_core::strategy::LlmIntent> {
        let user = INTENT_USER_TEMPLATE.replace("{query}", query);
        let messages = vec![
            serde_json::json!({"role": "system", "content": INTENT_SYSTEM_PROMPT}),
            serde_json::json!({"role": "user", "content": user}),
        ];
        let response = self.client.chat(&messages, &self.model, 0.0, Some(2)).await?;
        Ok(oce_core::strategy::LlmIntent::from_label(response.trim()))
    }
}

/// 容器：进程内单例装配。
pub struct Container {
    pub settings: Settings,
    /// 共享 SQLite 句柄（MCP 嵌入模式的工作区登记表复用同一连接池）
    pub db: oce_infra::sqlite::SqlDb,
    pub application: Arc<crate::service::RetrievalApplication>,
    pub metrics: Option<Arc<SqlMetricsSink>>,
    pub credential_admin: Arc<SqlCredentialAdminStore>,
    pub embedder: Arc<oce_infra::credentials::CredentialConfiguredEmbedder>,
    /// 实际生效的向量化提供方（static | openai）
    pub embed_provider: &'static str,
    /// 生效的向量维度
    pub vector_dim: usize,
    pub llm_reranker: Option<Arc<LlmRerankerImpl>>,
    pub rerank_api: Option<Arc<CredentialConfiguredReranker>>,
    /// 数据目录（SQLite/TriviumDB 所在目录；storage 报表用）。SQLite 相对路径时为 None。
    pub data_dir: Option<String>,
    /// 资源采样器（monitoring 关闭时 None；drop 时停止）
    pub resource_sampler: Option<oce_infra::resource_sampler::ResourceSampler>,
}

impl Container {
    /// 按配置装配全部组件。解析失败（维度不匹配、存储打不开）返回错误。
    pub async fn build(settings: Settings) -> Result<Arc<Self>, String> {
        Self::build_with_embedder(settings, None).await
    }

    /// 装配（可注入自定义嵌入器：测试 / 本地模型 / benchmark 用）。
    pub async fn build_with_embedder(
        settings: Settings,
        embedder_override: Option<Arc<dyn oce_core::search::Embedder>>,
    ) -> Result<Arc<Self>, String> {
        if settings.database.sqlite_path().is_none() {
            return Err("Rust 版第一阶段仅支持 SQLite 个人模式（DB_URL 需以 sqlite 开头）".into());
        }
        let sqlite_path = settings.database.sqlite_path().unwrap();

        // ── SQLite 元数据 ──
        let db = oce_infra::sqlite::SqlDb::open(&sqlite_path)?;
        let blob_repo = Arc::new(SqlBlobRepository { db: db.clone() });
        let chain_repo = Arc::new(SqlChainRepository { db: db.clone() });
        let credential_store = SqlCredentialAdminStore { db: db.clone() };
        let credential_admin = Arc::new(credential_store.clone());
        let credential_store = Arc::new(credential_store);

        // ── 向量化提供方解析（维度在打开 TriviumDB 前确定） ──
        let (embedder, vector_dim, embed_runtime, provider_name, model_tag) =
            resolve_embed_provider(&settings, &credential_store, embedder_override).await?;
        let mut tdb_settings = settings.trivium.clone();
        tdb_settings.dense_dim = vector_dim;
        if tdb_settings.path.is_empty() {
            // 未显式配置时与 SQLite 同目录
            tdb_settings.path = sqlite_path
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/oce.tdb"))
                .unwrap_or_else(|| "oce.tdb".into());
        }
        if let Some(parent) = std::path::Path::new(&tdb_settings.path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 嵌入模型指纹 sidecar：同维度不同模型的向量混入同一索引会静默污染检索，
        // 换模型必须重建索引（fail-closed 提示，不做静默兼容）
        let sidecar_path = format!("{}.model", tdb_settings.path);
        let tdb_path_display = tdb_settings.path.clone();
        let model_fingerprint = format!("{model_tag} dim={vector_dim}");
        let tdb_exists = std::path::Path::new(&tdb_settings.path).exists();
        if tdb_exists {
            match std::fs::read_to_string(&sidecar_path) {
                Ok(recorded) => {
                    let recorded = recorded.trim();
                    if !recorded.is_empty() && recorded != model_fingerprint {
                        return Err(format!(
                            "既有索引的嵌入模型与当前配置不符：\n  索引由 [{recorded}] 构建\n  当前配置 [{model_fingerprint}]\n                             请删除 {tdb_path_display}（或改用新数据目录）并清空客户端 .oce-client/state.sqlite3 后重新上传，以重建索引。"
                        ));
                    }
                }
                Err(_) if !std::path::Path::new(&sidecar_path).exists() => {
                    return Err(format!(
                        "既有索引缺少模型指纹（{sidecar_path} 不存在，由旧版本创建）。\n                         请删除 {tdb_path_display}（或改用新数据目录）并清空客户端 .oce-client/state.sqlite3 后重新上传。"
                    ));
                }
                _ => {}
            }
        }
        std::fs::write(&sidecar_path, &model_fingerprint)
            .map_err(|e| format!("write model sidecar: {e}"))?;
        let trivium_store = oce_infra::trivium::TriviumStore::open(tdb_settings.clone())
            .map_err(|e| {
                format!(
                    "open triviumdb `{}`: {e}\n提示：TriviumDB 维度创建后不可变。若更换了 embedding \
                     模型，请设置 MILVUS_DENSE_DIM/EMBED_DIMENSIONS 与既有 .tdb 一致，或改用新数据目录。",
                    tdb_settings.path
                )
            })?;
        let trivium: oce_infra::trivium::TriviumHandle = Arc::new(trivium_store);

        // ── 监控（旁路，非阻塞） ──
        let metrics = if settings.monitoring.enabled {
            let sink = Arc::new(SqlMetricsSink::new(db.clone()));
            sink.clone().spawn_flush_task(settings.monitoring.flush_interval_seconds);
            sink.clone().spawn_cleanup_task(
                settings.monitoring.retention_days,
                settings.monitoring.cleanup_interval_seconds,
            );
            Some(sink)
        } else {
            None
        };
        // 数据目录：SQLite 绝对路径时取其父目录（storage/resources 报表用）
        let data_dir = std::path::Path::new(&sqlite_path)
            .parent()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string())
            .filter(|_| std::path::Path::new(&sqlite_path).is_absolute());
        // 资源采样：monitoring 开启时后台周期采集（旁路，drop 时停止）
        let resource_sampler = oce_infra::resource_sampler::ResourceSampler::start(
            metrics
                .clone()
                .map(|m| m as Arc<dyn oce_core::metrics::MetricsSink>),
            settings.monitoring.resource_sample_interval_seconds,
            data_dir.clone(),
        );
        let on_usage: Option<oce_infra::openai::embedder::UsageCallback> = metrics
            .as_ref()
            .map(|m| {
                let m = m.clone() as Arc<dyn oce_core::metrics::MetricsSink>;
                Arc::new(move |credential_id: i64, kind: &str, model: &str, prompt: i64, completion: i64| {
                    m.record_token_usage(oce_core::metrics::TokenUsageRecord {
                        kind: kind.to_string(),
                        model: model.to_string(),
                        credential_id,
                        prompt_tokens: prompt.max(0) as u64,
                        completion_tokens: completion.max(0) as u64,
                        total_tokens: (prompt.max(0) + completion.max(0)) as u64,
                    });
                }) as oce_infra::openai::embedder::UsageCallback
            });

        let embedder_runtime = embed_runtime;

        // ── 切块器 ──
        let chunker: Box<dyn Chunker> = Box::new(
            oce_core::chunk::build_chunker().map_err(|e| format!("build chunker: {e}"))?,
        );

        // ── 索引管线 ──
        let gate = Arc::new(EmbeddingGateImpl {
            enabled: std::sync::atomic::AtomicBool::new(settings.embedding.enabled),
        });
        let vector_index: Arc<dyn oce_core::search::VectorIndex> = trivium.clone();
        let path_store_for_index: Option<Arc<dyn oce_core::search::PathSearchStore>> =
            if settings.retrieval.path_index_enabled {
                Some(trivium.clone())
            } else {
                None
            };
        let indexing = Arc::new(IndexingPipeline::new(
            chunker,
            embedder.clone(),
            vector_index,
            blob_repo.clone(),
            path_store_for_index,
            gate,
        ));

        // ── 检索管线 ──
        let retrieval_settings: RetrievalSettings = settings.retrieval.inner.clone();
        let search_store: Arc<dyn SearchStore> = trivium.clone();
        let mut pipeline = RetrievalPipeline::new(
            embedder.clone(),
            search_store,
            retrieval_settings.clone(),
        )
        .with_first_chunk_lookup(Arc::new(oce_infra::sqlite::chains::SqlFirstChunkLookup {
            db: db.clone(),
        }));
        pipeline.exact_store = Some(Arc::new(oce_infra::sqlite::chains::SqlExactStore {
            db: db.clone(),
            max_scope_blobs: retrieval_settings.exact_max_scope_blobs,
        }));
        if settings.retrieval.path_index_enabled {
            let ps: Arc<dyn PathSearchStore> = trivium.clone();
            pipeline.path_store = Some(ps);
        }
        // API rerank（RERANK_ENABLED）：本地 llama-server /v1/rerank 等端点接入主重排层。
        // 与 LLM 重排互斥使用——两者都开时 LLM 重排优先（后装配覆盖）。
        let mut rerank_api: Option<Arc<CredentialConfiguredReranker>> = None;
        if settings.rerank.enabled {
            // env 通道需要 RERANK_API_KEY（或回落 EMBED_API_KEY）；凭据通道走 model_credentials(kind=rerank)。
            // 本地 llama-server 无鉴权，占位 key 即可。
            let has_channel = settings.rerank.api_key.is_some()
                || settings.embedding.api_key.is_some()
                || credential_store
                    .resolve_active("rerank")
                    .await
                    .map_err(|e| format!("resolve rerank credential: {e}"))?
                    .is_some();
            if has_channel {
                let runtime = Arc::new(CredentialConfiguredReranker::new(
                    credential_store.as_ref().clone(),
                    settings.rerank.clone(),
                    settings.embedding.api_key.clone(),
                    on_usage.clone(),
                ));
                pipeline.reranker = Arc::new(ApiRerankAdapter {
                    top_n: settings.rerank.top_n,
                    // 与 LLM 重排 snippet_chars 同量级：候选窗口 50 × 1600 字符在
                    // llama-server CPU 上单次约 20s；更长正文收益递减且延迟线性上涨
                    max_doc_chars: 1_600,
                    runtime: runtime.clone(),
                });
                rerank_api = Some(runtime);
                tracing::info!(
                    "API reranker enabled (endpoint={}, model={}, top_n={})",
                    settings.rerank.endpoint,
                    settings.rerank.model,
                    settings.rerank.top_n
                );
            } else {
                tracing::info!("RERANK_ENABLED but no RERANK_API_KEY/EMBED_API_KEY/credential; API rerank disabled");
            }
        }

        // LLM 三类（LLM 重排 / 查询改写 / 意图分类）各自按 kind 解析凭据
        let mut llm_reranker: Option<Arc<LlmRerankerImpl>> = None;
        // LLM 组件按通道可用性装配：无 env key 且无对应 kind 凭据时不构建，
        // 避免每次检索白试失败调用（降级语义：rerank 退回原序、intent 走启发式）
        let mut any_llm = settings.llm.rerank_enabled
            || settings.retrieval.inner.query_rewrite_enabled
            || settings.retrieval.inner.intent_classification_enabled;
        if any_llm && settings.llm.api_key.is_none() {
            let mut any_credential = false;
            for kind in ["llm_rerank", "query_rewrite", "intent"] {
                if credential_store
                    .resolve_active(kind)
                    .await
                    .map_err(|e| format!("resolve {kind} credential: {e}"))?
                    .is_some()
                {
                    any_credential = true;
                    break;
                }
            }
            if !any_credential {
                any_llm = false;
                tracing::info!("no LLM key/credential; LLM rerank/rewrite/intent disabled");
            }
        }
        if any_llm {
            let make_client = |kind: &str, fallback_model: String| {
                Arc::new(oce_infra::credentials::CredentialConfiguredLlmClient::new(
                    credential_store.as_ref().clone(),
                    kind,
                    settings.llm.clone(),
                    fallback_model,
                    on_usage.clone(),
                ))
            };
            if settings.llm.rerank_enabled {
                let client = make_client("llm_rerank", settings.llm.model.clone());
                llm_reranker = Some(Arc::new(LlmRerankerImpl {
                    client,
                    model: settings.llm.model.clone(),
                    max_candidates: settings.llm.max_candidates,
                    output_top_k: settings.llm.output_top_k,
                    snippet_chars: settings.llm.snippet_chars,
                }));
                pipeline.llm_reranker = llm_reranker.clone().map(|r| Arc::clone(&r) as Arc<dyn LlmReranker>);
            }
            if settings.retrieval.inner.query_rewrite_enabled {
                let client = make_client(
                    "query_rewrite",
                    settings.retrieval.query_rewrite_model.clone(),
                );
                pipeline.query_rewriter = Some(Arc::new(QueryRewriterImpl {
                    client,
                    model: settings.retrieval.query_rewrite_model.clone(),
                    num_rewrites: settings.retrieval.query_rewrite_num,
                }) as Arc<dyn QueryRewriter>);
            }
            if settings.retrieval.inner.intent_classification_enabled {
                let client = make_client("intent", settings.llm.model.clone());
                pipeline.intent_classifier = Some(Arc::new(IntentClassifierImpl {
                    client,
                    model: settings.llm.model.clone(),
                }) as Arc<dyn IntentClassifier>);
            }
        }

        let application = Arc::new(crate::service::RetrievalApplication::new(
            indexing,
            Arc::new(pipeline),
            blob_repo,
            chain_repo,
            trivium,
            metrics.clone(),
            settings.monitoring.enabled && settings.monitoring.retrieval_audit_enabled,
            settings.monitoring.store_query_text,
        ));

        Ok(Arc::new(Self {
            settings,
            db: db.clone(),
            application,
            metrics,
            credential_admin,
            embedder: embedder_runtime,
            embed_provider: provider_name,
            vector_dim,
            llm_reranker,
            rerank_api,
            data_dir,
            resource_sampler,
        }))
    }

    /// 凭据热重载（对应 ReloadEmbeddingCredentialsCommand）。
    pub async fn reload_credentials(&self) -> (bool, usize, Option<String>) {
        match self.embedder.reload().await {
            Ok(pool_size) => (true, pool_size, None),
            Err(exc) => (false, 0, Some(exc.message)),
        }
    }
}

/// resolve_active 返回类型的便捷 re-export（server 层使用）。
pub type ResolvedCredential = RuntimeCredential;


/// 向量化提供方解析：auto（有 key/凭据→openai，否则 static）| static | openai。
/// 返回 (嵌入器, 向量维度, reload 运行时, 提供方名)。
async fn resolve_embed_provider(
    settings: &Settings,
    credential_store: &SqlCredentialAdminStore,
    embedder_override: Option<Arc<dyn oce_core::search::Embedder>>,
) -> Result<
    (
        Arc<dyn oce_core::search::Embedder>,
        usize,
        Arc<oce_infra::credentials::CredentialConfiguredEmbedder>,
        &'static str,
        String,
    ),
    String,
> {
    // reload 通道始终挂 DB 凭据运行时（openai 语义的 lazy 解析）
    let runtime = Arc::new(oce_infra::credentials::CredentialConfiguredEmbedder::new(
        credential_store.clone(),
        settings.embedding.clone(),
        settings.trivium.dense_dim,
        None,
    ));

    if let Some(e) = embedder_override {
        // 测试/bench 注入：维度沿用配置（容器校验交给既有逻辑）
        return Ok((e, settings.trivium.dense_dim, runtime, "override", "override".into()));
    }

    let provider = match settings.embedding.provider.as_str() {
        "static" => "static",
        "openai" => "openai",
        _ => {
            // auto：有 API key 或 DB 凭据 → openai，否则 static
            let has_key = settings.embedding.api_key.is_some()
                || credential_store
                    .resolve_active("embed")
                    .await
                    .map_err(|e| format!("resolve embed credential: {e}"))?
                    .is_some();
            if has_key {
                "openai"
            } else {
                "static"
            }
        }
    };

    match provider {
        "static" => {
            let se = StaticEmbedder::load(&settings.embedding)?;
            let dim = se.dim();
            let model_id = se.model_id().to_string();
            tracing::info!("embed provider: static (model={model_id}, dim={dim})");
            Ok((Arc::new(se), dim, runtime, "static", model_id))
        }
        _ => {
            if settings.embedding.dimensions != settings.trivium.dense_dim {
                return Err("EMBED_DIMENSIONS must equal MILVUS_DENSE_DIM".into());
            }
            tracing::info!(
                "embed provider: openai (model={}, dim={})",
                settings.embedding.model,
                settings.embedding.dimensions
            );
            Ok((
                runtime.clone() as Arc<dyn oce_core::search::Embedder>,
                settings.trivium.dense_dim,
                runtime,
                "openai",
                format!("openai:{}:{}", settings.embedding.model, settings.embedding.dimensions),
            ))
        }
    }
}
