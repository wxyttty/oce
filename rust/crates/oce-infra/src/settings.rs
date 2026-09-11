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
    /// BM25 稀疏文本混合检索（CJK 2-gram 分词，中文词法兜底）。
    pub text_hybrid: bool,
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
            text_hybrid: var_bool("TRIVIUM_TEXT_HYBRID", true),
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
    pub timeout_seconds: f64,
    pub proxy: Option<String>,
    pub query_instruction: String,
    /// 向量化提供方：auto（有 key/凭据→openai，否则 static）| static | openai
    pub provider: String,
    /// 静态模型：HF repo id 或本地目录（默认 minishlab/potion-base-8M）
    pub static_model: Option<String>,
    /// 本地神经嵌入模型（EMBED_PROVIDER=local）：HF repo id 或本地目录
    /// llama 路线的 GGUF 文件名（repo 内）
    /// 权重存储精度：f32 | f16 | bf16 | int8 | int4（量化后反回 F32 计算，
    /// 测量的是存储精度对质量的影响；真 int 算力需 ort/GGUF 内核）
    /// 静态模型本地目录（优先于 repo id 下载）
    pub static_model_dir: Option<String>,
}

impl EmbeddingSettings {
    #[doc(hidden)]
    pub fn from_env() -> Self {
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
            timeout_seconds: var_parse("EMBED_TIMEOUT_SECONDS", 60.0f64),
            proxy: var("EMBED_PROXY"),
            // Qwen3-Embedding 官方支持 query 侧 instruction（训练时即指令感知），
            // 代码检索场景实测 nollm 双仓 +1.1/+2.4 分。模型条件默认：Qwen3 系列
            // 自动启用；其他嵌入模型保持 Python 原默认（空，不引入未训练指令的
            // 干扰）；EMBED_QUERY_INSTRUCTION 显式设置时优先生效。
            query_instruction: var("EMBED_QUERY_INSTRUCTION").unwrap_or_else(|| {
                let model = var("EMBED_MODEL").unwrap_or_default();
                if model.contains("Qwen3-Embedding") {
                    "Given a code retrieval query, retrieve the most relevant code snippets or files that directly implement, explain, or help answer the query.".to_string()
                } else {
                    String::new()
                }
            }),
            provider: var("EMBED_PROVIDER").unwrap_or_else(|| "auto".into()),
            static_model: var("EMBED_STATIC_MODEL"),
            static_model_dir: var("EMBED_STATIC_MODEL_DIR"),
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
    pub log: LogSettings,
    pub monitoring: MonitoringSettings,
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
            log: LogSettings::from_env(),
            monitoring: MonitoringSettings::from_env(),
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
