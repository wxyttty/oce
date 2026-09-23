//! 应用配置。与 Python `shared/config/settings.py` 的环境变量完全兼容
//! （同前缀、同默认值），另新增 TriviumDB 专属前缀 `TRIVIUM_`。
//! `.env` 文件由调用方（CLI）用 dotenvy 加载。

use std::env;

fn var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

fn var_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    var(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn var_bool(key: &str, default: bool) -> bool {
    match var(key).map(|v| v.to_lowercase()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

/// 数据库配置（个人模式固定 SQLite；服务模式 PostgreSQL 留待后续阶段）。
#[derive(Debug, Clone)]
pub struct DatabaseSettings {
    pub url: String,
    pub pool_size: usize,
    pub echo: bool,
}

impl DatabaseSettings {
    fn from_env() -> Self {
        Self {
            url: var("DB_URL").unwrap_or_else(|| "sqlite://oce.db".into()),
            pool_size: var_parse("DB_POOL_SIZE", 5usize),
            echo: var_bool("DB_ECHO", false),
        }
    }

    pub fn is_sqlite(&self) -> bool {
        self.url.starts_with("sqlite")
    }

    /// SQLite 文件路径（sqlite:///path 或 sqlite://path → path）。
    pub fn sqlite_path(&self) -> Option<String> {
        if !self.is_sqlite() {
            return None;
        }
        let rest = self
            .url
            .trim_start_matches("sqlite:///")
            .trim_start_matches("sqlite://")
            .trim_start_matches("sqlite+aiosqlite:///");
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    }
}

/// TriviumDB 配置（个人模式向量引擎，替代 Milvus Lite）。
#[derive(Debug, Clone)]
pub struct TriviumSettings {
    /// 单文件路径；空时随 data_dir 推导（data_dir/oce.tdb）
    pub path: String,
    /// WAL 同步模式："full" | "normal" | "off"
    pub sync_mode: String,
    /// 存储模式："rom"（单文件便携）| "mmap"（分离零拷贝）
    pub storage_mode: String,
    /// 向量维度（与 EMBED_DIMENSIONS 必须一致）
    pub dense_dim: usize,
    /// 引擎内 BM25 稀疏文本混合（AC 前缀 + BM25 求和）。强嵌入下实测
    /// 归零/负（8B：开/关差 0.2；与 FTS5 同开时 AC 噪声拖累 FTS -1.9），
    /// 默认关；弱嵌入（静态 potion）场景显式开（flask +4.40）。
    pub text_hybrid: bool,
    /// BM25 路 RRF 融合权重（TRIVIUM_TEXT_BOOST，默认 0.8）：
    /// 词法信号只做锦上添花，不应压过 dense 语义排序。
    pub text_boost: f32,
    /// FTS5 词法混合路（个人模式）：off | gated（默认，仅标识符）| full。
    /// trivium 引擎 BM25 的 AC 前缀噪声实测归零后，词法增益由 FTS5 bm25
    /// 承担（服务模式 pgvector ts_rank 路结论的复刻）。
    pub fts_lexical: String,
    /// SA-PPR 图扩散深度（TRIVIUM_EXPAND_DEPTH，0=关闭）
    pub expand_depth: usize,
    /// 是否允许查询自动构建 QuIVer ANN。
    /// 默认关闭：实测 Apple Silicon 上 ≤20 万节点 BruteForce 均快于 ANN 且 recall 相同
    /// （QuIVer 优势在百万级 + mmap 冷向量场景）；需要时显式置 true。
    pub auto_build_quiver: bool,
}

impl TriviumSettings {
    fn from_env() -> Self {
        Self {
            path: var("TRIVIUM_PATH").unwrap_or_default(),
            sync_mode: var("TRIVIUM_SYNC_MODE").unwrap_or_else(|| "normal".into()),
            storage_mode: var("TRIVIUM_STORAGE_MODE").unwrap_or_else(|| "rom".into()),
            dense_dim: var_parse("MILVUS_DENSE_DIM", 1024usize),
            // 默认 false：BM25 全词匹配对中文语义 query 是净伤害（flask 实测 -20 分）；
            // 词法精确信号已由 symbol_occurrences exact 路承担。开启时
            // trivium.rs 的标识符门控生效（仅真标识符进 BM25）。
            // 标识符门控 + 低权重 BM25：flask +4.40 / cc-switch +1.13（vs 全关）。
            // 词法信号只做锦上添花，不应压过 dense 语义排序。
            // 引擎内 BM25 的 AC 前缀噪声在强嵌入下实测归零/负（cc -1.9 与
            // FTS 冗余时）；词法信号改由 FTS5 bm25 承担（fts_lexical）。
            // 弱嵌入（静态 potion）场景显式设 true 恢复。
            text_hybrid: var_bool("TRIVIUM_TEXT_HYBRID", false),
            text_boost: var_parse("TRIVIUM_TEXT_BOOST", 0.3f32),
            fts_lexical: var("SQLITE_FTS_LEXICAL").unwrap_or_else(|| "gated".into()),
            expand_depth: var_parse("TRIVIUM_EXPAND_DEPTH", 0usize),
            auto_build_quiver: var_bool("TRIVIUM_AUTO_BUILD_QUIVER", false),
        }
    }
}

/// 嵌入模型配置。
#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub dimensions: usize,
    pub max_batch_size: usize,
    pub max_batch_chars: usize,
    pub max_input_chars: usize,
    pub input_overlap_chars: usize,
    pub max_concurrency: usize,
    /// llama.cpp 后端批量请求在单 slot 内串行处理；true 时拆批量
    /// 为并发单条请求，利用服务端多 slot 并行（实测长文本 7 倍提速）。
    /// 对真 OpenAI 兼容 API（SiliconFlow/Gitee）应保持 false——批量接口本身并行。
    pub single_request: bool,
    pub timeout_seconds: f64,
    pub proxy: Option<String>,
    pub query_instruction: String,
    /// 指令模板格式：none = 裸拼接 {}{}；instruct_query = Instruct: {}\nQuery: {}
    pub instruction_template: String,
    /// 向量化提供方：auto（有 key/凭据→openai，否则 static）| static | openai | local
    pub provider: String,
    /// 静态模型：HF repo id 或本地目录（默认 minishlab/potion-base-8M）
    pub static_model: Option<String>,
    /// 静态模型本地目录（优先于 repo id 下载）
    pub static_model_dir: Option<String>,
    /// 本地 candle 模型：HF repo id 或本地目录（默认 Qwen/Qwen3-Embedding-0.6B）
    pub local_model: Option<String>,
    /// 本地 candle 精度：f32|f16|bf16（默认 f32）
    pub local_dtype: Option<String>,
}

impl EmbeddingSettings {
    #[doc(hidden)]
    pub fn from_env() -> Self {
        // 模型名检测需在 struct 初始化外计算（Rust 不允许 let 在字段间）
        let model_for_instruct = var("EMBED_MODEL").unwrap_or_default();
        let model_lower = model_for_instruct.to_lowercase();
        let needs_instruct = model_lower.contains("qwen3-embedding")
            || model_lower.contains("qwen3_embed")
            || model_lower.contains("f2llm");
        Self {
            enabled: var_bool("EMBED_ENABLED", true),
            endpoint: var("EMBED_ENDPOINT")
                .unwrap_or_else(|| "https://api.siliconflow.cn/v1/embeddings".into()),
            api_key: var("EMBED_API_KEY"),
            model: var("EMBED_MODEL").unwrap_or_else(|| "Qwen/Qwen3-Embedding-4B".into()),
            dimensions: var_parse("EMBED_DIMENSIONS", 1024usize),
            max_batch_size: var_parse("EMBED_MAX_BATCH_SIZE", 32usize),
            max_batch_chars: var_parse("EMBED_MAX_BATCH_CHARS", 32_000usize),
            max_input_chars: var_parse("EMBED_MAX_INPUT_CHARS", 8_000usize),
            input_overlap_chars: var_parse("EMBED_INPUT_OVERLAP_CHARS", 400usize),
            max_concurrency: var_parse("EMBED_MAX_CONCURRENCY", 4usize),
            single_request: var_bool("EMBED_SINGLE_REQUEST", false),
            timeout_seconds: var_parse("EMBED_TIMEOUT_SECONDS", 60.0f64),
            proxy: var("EMBED_PROXY"),
            // Qwen3-Embedding / F2LLM-v2 官方 query 格式：
            //   Instruct: {task_description}\nQuery: {query}
            // 文档侧不加 instruction（OCE embed_documents 不注入）。
            // EMBED_QUERY_INSTRUCTION / EMBED_INSTRUCTION_TEMPLATE 显式设置时优先生效。
            query_instruction: var("EMBED_QUERY_INSTRUCTION").unwrap_or_else(|| {
                if needs_instruct {
                    "Given a code retrieval query, retrieve the most relevant code snippets or files that directly implement, explain, or help answer the query.".to_string()
                } else {
                    String::new()
                }
            }),
            instruction_template: var("EMBED_INSTRUCTION_TEMPLATE").unwrap_or_else(|| {
                if needs_instruct {
                    "instruct_query".to_string()
                } else {
                    "none".to_string()
                }
            }),
            provider: var("EMBED_PROVIDER").unwrap_or_else(|| "auto".into()),
            static_model: var("EMBED_STATIC_MODEL"),
            static_model_dir: var("EMBED_STATIC_MODEL_DIR"),
            local_model: var("EMBED_LOCAL_MODEL"),
            local_dtype: var("EMBED_LOCAL_DTYPE"),
        }
    }
}

/// 重排模型配置（API rerank）。
#[derive(Debug, Clone)]
pub struct RerankSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub top_n: usize,
    pub min_score: f32,
    /// 单次 rerank 请求文档数上限：超过则分批串行，全局重排后截断
    pub max_docs: usize,
    pub timeout_seconds: f64,
}

impl RerankSettings {
    fn from_env() -> Self {
        Self {
            enabled: var_bool("RERANK_ENABLED", false),
            endpoint: var("RERANK_ENDPOINT")
                .unwrap_or_else(|| "https://api.siliconflow.cn/v1/rerank".into()),
            api_key: var("RERANK_API_KEY"),
            model: var("RERANK_MODEL").unwrap_or_else(|| "Qwen/Qwen3-Reranker-0.6B".into()),
            top_n: var_parse("RERANK_TOP_N", 10usize),
            min_score: var_parse("RERANK_MIN_SCORE", 0.05f32),
            max_docs: var_parse("RERANK_MAX_DOCS", 24usize),
            timeout_seconds: var_parse("RERANK_TIMEOUT_SECONDS", 60.0f64),
        }
    }
}

/// LLM 配置（LLM 重排 / 查询改写 / 意图分类共用）。
#[derive(Debug, Clone)]
pub struct LlmSettings {
    pub rerank_enabled: bool,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: String,
    pub proxy: Option<String>,
    pub max_candidates: usize,
    pub output_top_k: usize,
    pub snippet_chars: usize,
    pub tpm_limit: usize,
    pub timeout_seconds: f64,
}

impl LlmSettings {
    fn from_env() -> Self {
        Self {
            rerank_enabled: var_bool("LLM_RERANK_ENABLED", true),
            model: var("LLM_MODEL").unwrap_or_else(|| "Qwen/Qwen2.5-7B-Instruct".into()),
            api_key: var("LLM_API_KEY"),
            base_url: var("LLM_BASE_URL").unwrap_or_else(|| "https://api.siliconflow.cn/v1".into()),
            proxy: var("LLM_PROXY"),
            max_candidates: var_parse("LLM_MAX_CANDIDATES", 50usize),
            output_top_k: var_parse("LLM_OUTPUT_TOP_K", 10usize),
            snippet_chars: var_parse("LLM_SNIPPET_CHARS", 1600usize),
            tpm_limit: var_parse("LLM_TPM_LIMIT", 60_000usize),
            timeout_seconds: var_parse("LLM_TIMEOUT_SECONDS", 120.0f64),
        }
    }
}

/// 检索配置（与 Python RetrievalSettings 同名同默认）。
#[derive(Debug, Clone)]
pub struct RetrievalEnvSettings {
    pub inner: oce_core::retrieval_settings::RetrievalSettings,
    pub query_rewrite_model: String,
    pub query_rewrite_num: usize,
    pub path_index_enabled: bool,
    /// 规则层文件描述注入 embedding_text + rerank 文档（默认关，A/B 实验
    /// 开关；开启时模型指纹 etext=v2，旧索引 fail-closed 提示重建）
    pub file_desc_enabled: bool,
}

impl RetrievalEnvSettings {
    fn from_env() -> Self {
        let mut inner = oce_core::retrieval_settings::RetrievalSettings::default();
        inner.default_top_k = var_parse("RETRIEVAL_DEFAULT_TOP_K", inner.default_top_k);
        inner.vector_threshold = var_parse("RETRIEVAL_VECTOR_THRESHOLD", inner.vector_threshold);
        inner.final_select_k = var_parse("RETRIEVAL_FINAL_SELECT_K", inner.final_select_k);
        inner.rrf_k = var_parse("RETRIEVAL_RRF_K", inner.rrf_k);
        inner.confidence_floor = var_parse("RETRIEVAL_CONFIDENCE_FLOOR", inner.confidence_floor);
        inner.exact_max_scope_blobs = var_parse(
            "RETRIEVAL_EXACT_MAX_SCOPE_BLOBS",
            inner.exact_max_scope_blobs,
        );
        inner.exact_timeout_seconds = var_parse(
            "RETRIEVAL_EXACT_TIMEOUT_SECONDS",
            inner.exact_timeout_seconds,
        );
        inner.query_decomposition_enabled = var_bool(
            "RETRIEVAL_QUERY_DECOMPOSITION_ENABLED",
            inner.query_decomposition_enabled,
        );
        inner.query_max_queries = var_parse("RETRIEVAL_QUERY_MAX_QUERIES", inner.query_max_queries);
        inner.query_min_facet_chars = var_parse(
            "RETRIEVAL_QUERY_MIN_FACET_CHARS",
            inner.query_min_facet_chars,
        );
        inner.query_facet_weight =
            var_parse("RETRIEVAL_QUERY_FACET_WEIGHT", inner.query_facet_weight);
        inner.per_query_top_k = var_parse("RETRIEVAL_PER_QUERY_TOP_K", inner.per_query_top_k);
        inner.max_chunks_per_path =
            var_parse("RETRIEVAL_MAX_CHUNKS_PER_PATH", inner.max_chunks_per_path);
        inner.max_context_chars = var_parse("RETRIEVAL_MAX_CONTEXT_CHARS", inner.max_context_chars);
        inner.overlap_threshold = var_parse("RETRIEVAL_OVERLAP_THRESHOLD", inner.overlap_threshold);
        inner.query_rewrite_enabled = var_bool(
            "RETRIEVAL_QUERY_REWRITE_ENABLED",
            inner.query_rewrite_enabled,
        );
        inner.path_boost_weight = var_parse("RETRIEVAL_PATH_BOOST_WEIGHT", inner.path_boost_weight);
        inner.intent_classification_enabled = var_bool(
            "RETRIEVAL_INTENT_CLASSIFICATION_ENABLED",
            inner.intent_classification_enabled,
        );
        inner.related_symbols_enabled = var_bool(
            "RETRIEVAL_RELATED_SYMBOLS_ENABLED",
            inner.related_symbols_enabled,
        );
        inner.rerank_cutoff_enabled = var_bool(
            "RETRIEVAL_RERANK_CUTOFF_ENABLED",
            inner.rerank_cutoff_enabled,
        );
        inner.broad_mode_enabled =
            var_bool("RETRIEVAL_BROAD_MODE_ENABLED", inner.broad_mode_enabled);
        inner.meta_dir_penalty_enabled = var_bool(
            "RETRIEVAL_META_DIR_PENALTY_ENABLED",
            inner.meta_dir_penalty_enabled,
        );
        inner.span_merge_enabled =
            var_bool("RETRIEVAL_SPAN_MERGE_ENABLED", inner.span_merge_enabled);
        inner.rerank_pool_k = var_parse("RETRIEVAL_RERANK_POOL_K", inner.rerank_pool_k);
        Self {
            inner,
            // 回落链：显式 RETRIEVAL_QUERY_REWRITE_MODEL > LLM_MODEL > 内置默认。
            // 三处 LLM 调用点（rerank/rewrite/intent）必须跟随同一个 LLM_MODEL，
            // 否则换模型时 rewrite 会悄悄掉回内置小模型（Python 硬编码默认的坑）。
            query_rewrite_model: var("RETRIEVAL_QUERY_REWRITE_MODEL")
                .or_else(|| var("LLM_MODEL"))
                .unwrap_or_else(|| "Qwen/Qwen2.5-7B-Instruct".into()),
            query_rewrite_num: var_parse("RETRIEVAL_QUERY_REWRITE_NUM", 3usize),
            path_index_enabled: var_bool("RETRIEVAL_PATH_INDEX_ENABLED", true),
            file_desc_enabled: var_bool("RETRIEVAL_FILE_DESC_ENABLED", false),
        }
    }
}

/// Worker 配置。
#[derive(Debug, Clone)]
pub struct WorkerSettings {
    pub enabled: bool,
    pub concurrency: usize,
    pub max_retries: u32,
}

/// 向量后端选择：trivium（默认，单文件）| pgvector（PG 一体化，服务模式）。
#[derive(Debug, Clone)]
pub struct VectorBackendSettings {
    pub backend: String,
    /// pgvector 词法路模式：off（纯 dense）| gated（仅标识符，对齐 trivium
    /// 门控）| full（全 query 文本进 tsquery——验证"无门控 BM25 是否有用"）。
    pub lexical: String,
}

impl VectorBackendSettings {
    fn from_env() -> Self {
        Self {
            backend: var("VECTOR_BACKEND").unwrap_or_else(|| "trivium".into()),
            lexical: var("PGVECTOR_LEXICAL").unwrap_or_else(|| "gated".into()),
        }
    }
}

/// Redis 配置（服务模式任务队列；个人模式不用）。
#[derive(Debug, Clone)]
pub struct RedisSettings {
    pub url: String,
    pub queue_name: String,
}

impl RedisSettings {
    fn from_env() -> Self {
        Self {
            url: var("REDIS_URL").unwrap_or_else(|| "redis://localhost:6379/0".into()),
            queue_name: var("REDIS_QUEUE_NAME").unwrap_or_else(|| "oce:embed_queue".into()),
        }
    }
}

impl WorkerSettings {
    fn from_env() -> Self {
        Self {
            enabled: var_bool("WORKER_ENABLED", false), // 个人模式默认关闭（同步索引）
            concurrency: var_parse("WORKER_CONCURRENCY", 2usize),
            max_retries: var_parse("WORKER_MAX_RETRIES", 3u32),
        }
    }
}

/// 日志配置。
#[derive(Debug, Clone)]
pub struct LogSettings {
    pub file_enabled: bool,
    pub file_path: Option<String>,
    pub level: String,
}

impl LogSettings {
    fn from_env() -> Self {
        Self {
            file_enabled: var_bool("LOG_FILE_ENABLED", false),
            file_path: var("LOG_FILE_PATH"),
            level: var("LOG_LEVEL").unwrap_or_else(|| "INFO".into()),
        }
    }
}

/// 监控配置。
#[derive(Debug, Clone)]
pub struct MonitoringSettings {
    pub enabled: bool,
    pub flush_interval_seconds: f64,
    pub flush_max_buffer: usize,
    pub resource_sample_interval_seconds: f64,
    pub retention_days: u32,
    pub cleanup_interval_seconds: f64,
    pub retrieval_audit_enabled: bool,
    pub store_query_text: bool,
}

impl MonitoringSettings {
    fn from_env() -> Self {
        Self {
            enabled: var_bool("MONITORING_ENABLED", true),
            flush_interval_seconds: var_parse("MONITORING_FLUSH_INTERVAL_SECONDS", 5.0f64),
            flush_max_buffer: var_parse("MONITORING_FLUSH_MAX_BUFFER", 500usize),
            resource_sample_interval_seconds: var_parse(
                "MONITORING_RESOURCE_SAMPLE_INTERVAL_SECONDS",
                60.0f64,
            ),
            retention_days: var_parse("MONITORING_RETENTION_DAYS", 30u32),
            cleanup_interval_seconds: var_parse("MONITORING_CLEANUP_INTERVAL_SECONDS", 3600.0f64),
            retrieval_audit_enabled: var_bool("MONITORING_RETRIEVAL_AUDIT_ENABLED", true),
            store_query_text: var_bool("MONITORING_STORE_QUERY_TEXT", false),
        }
    }
}

/// 全局配置聚合。
#[derive(Debug, Clone)]
pub struct Settings {
    pub api_key: String,
    pub admin_api_key: String,
    pub cors_origins: String,
    pub database: DatabaseSettings,
    pub trivium: TriviumSettings,
    pub embedding: EmbeddingSettings,
    pub rerank: RerankSettings,
    pub llm: LlmSettings,
    pub retrieval: RetrievalEnvSettings,
    pub worker: WorkerSettings,
    pub redis: RedisSettings,
    pub vector_backend: VectorBackendSettings,
    pub log: LogSettings,
    pub monitoring: MonitoringSettings,
    /// OCE 进程内存硬限制（MB），0=不限。超限时拒绝新嵌入请求并告警。
    pub memory_limit_mb: usize,
}

impl Settings {
    pub fn from_env() -> Self {
        Self {
            api_key: var("API_KEY").unwrap_or_else(|| "sk-opencontextengine".into()),
            admin_api_key: var("ADMIN_API_KEY").unwrap_or_default(),
            cors_origins: var("CORS_ORIGINS").unwrap_or_else(|| "https://oce-ai.github.io".into()),
            database: DatabaseSettings::from_env(),
            trivium: TriviumSettings::from_env(),
            embedding: EmbeddingSettings::from_env(),
            rerank: RerankSettings::from_env(),
            llm: LlmSettings::from_env(),
            retrieval: RetrievalEnvSettings::from_env(),
            worker: WorkerSettings::from_env(),
            redis: RedisSettings::from_env(),
            vector_backend: VectorBackendSettings::from_env(),
            log: LogSettings::from_env(),
            monitoring: MonitoringSettings::from_env(),
            memory_limit_mb: var_parse("OCE_MEMORY_LIMIT_MB", 0usize),
        }
    }

    /// admin key 为空时回落 API_KEY。
    pub fn effective_admin_key(&self) -> &str {
        if self.admin_api_key.is_empty() {
            &self.api_key
        } else {
            &self.admin_api_key
        }
    }
}
