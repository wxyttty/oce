//! OpenContextEngine（Rust 版）HTTP 服务入口。
//!
//! 个人模式零依赖：SQLite + TriviumDB 单文件 + 进程内 worker。
//! `oce serve` 默认 127.0.0.1:8986。

use oce_server::routes;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "oce", version, about = "OpenContextEngine (Rust edition)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 在数据目录生成个人模式 .env（零配置起步）
    Init {
        /// 数据目录（默认 ~/.oce/data）
        #[arg(long, default_value_t = default_data_dir_str())]
        data_dir: String,
    },
    /// 启动嵌入式 MCP 服务器（单进程直连引擎，无需后台服务）
    Mcp {
        /// 工作区根目录（索引数据存放在 <workspace>/.oce/）
        #[arg(long, default_value = ".")]
        workspace: String,
    },
    /// 打印版本（等价 --version）
    Version,
    /// 体检：数据文件、schema、向量引擎、嵌入提供方逐项检查（迁移自检用）
    Doctor {
        /// 数据目录（默认 ~/.oce/data）
        #[arg(long, default_value_t = default_data_dir_str())]
        data_dir: String,
    },
    /// 启动 HTTP 服务（个人模式）
    Serve {
        /// 数据目录（默认 ~/.oce/data）
        #[arg(long, default_value_t = default_data_dir_str())]
        data_dir: String,
        /// 显式 .env 文件路径
        #[arg(long)]
        env_file: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8986)]
        port: u16,
        /// 日志级别：-v INFO / -vv DEBUG（默认 WARNING）
        #[arg(short = 'v', action = clap::ArgAction::Count)]
        verbose: u8,
    },
}

fn default_data_dir_str() -> String {
    dirs_home().join(".oce").join("data").to_string_lossy().into_owned()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

const PERSONAL_ENV_TEMPLATE: &str = r#"# OpenContextEngine 个人模式配置（Rust 版）

# ==================== 鉴权 ====================
# HTTP Bearer 鉴权令牌（客户端用 Authorization: Bearer <token> 访问）。
API_KEY=sk-opencontextengine
# Admin 接口密钥；空则回落 API_KEY
ADMIN_API_KEY=

# ==================== 向量引擎（TriviumDB 单文件） ====================
# 向量维度（必须与 EMBED_DIMENSIONS 一致）
MILVUS_DENSE_DIM=1024
# TRIVIUM_PATH 默认随数据目录推导（data_dir/oce.tdb）
# TRIVIUM_SYNC_MODE=normal
# TRIVIUM_STORAGE_MODE=rom

# ==================== 嵌入 ====================
# 提供方：auto（配置了 EMBED_API_KEY 或凭据则用 openai，否则用静态模型）| static | openai
EMBED_PROVIDER=auto
# 静态查表模型（Model2Vec）：本地目录优先，否则按 HF repo id 自动下载缓存。
# 默认 minishlab/potion-multilingual-128M（256 维，多语言，~500MB 下载/内存）；
# 中文业务查询实测显著优于纯英文的 potion-base-8M。代码字段名为主场景可试
# minishlab/potion-code-16M-v2（中文弱）。换模型会触发索引重建提示。
# EMBED_STATIC_MODEL=minishlab/potion-multilingual-128M
# EMBED_STATIC_MODEL_DIR=
EMBED_ENABLED=true
EMBED_ENDPOINT=https://api.siliconflow.cn/v1/embeddings
EMBED_API_KEY=
EMBED_MODEL=Qwen/Qwen3-Embedding-4B
EMBED_DIMENSIONS=1024
MILVUS_DENSE_DIM=1024

# ==================== 检索 ====================
RETRIEVAL_FINAL_SELECT_K=10

# ==================== LLM（可选：重排/改写/意图分类） ====================
LLM_RERANK_ENABLED=true
LLM_BASE_URL=https://api.siliconflow.cn/v1
LLM_API_KEY=
LLM_MODEL=Qwen/Qwen2.5-7B-Instruct

# ==================== 监控 ====================
MONITORING_ENABLED=true
"#;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { data_dir } => {
            init_data_dir(&PathBuf::from(data_dir));
        }
        Command::Mcp { workspace } => {
            // MCP stdio：stdout 只承载协议消息，日志全部进 stderr
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("oce=warn")),
                )
                .with_writer(std::io::stderr)
                .init();
            if let Err(e) = oce_server::mcp::run_stdio(PathBuf::from(workspace)).await {
                eprintln!("oce mcp: {e}");
                std::process::exit(1);
            }
        }
        Command::Version => {
            println!("oce {} (rust)", env!("CARGO_PKG_VERSION"));
        }
        Command::Doctor { data_dir } => {
            doctor(PathBuf::from(&data_dir));
        }
        Command::Serve {
            data_dir,
            env_file,
            host,
            port,
            verbose,
        } => {
            serve(PathBuf::from(data_dir), env_file, host, port, verbose).await;
        }
    }
}

fn init_data_dir(data_dir: &PathBuf) {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        eprintln!("创建数据目录失败: {e}");
        std::process::exit(1);
    }
    let env_path = data_dir.join(".env");
    if env_path.exists() {
        println!("已存在 {}，跳过初始化", env_path.display());
    } else {
        if std::fs::write(&env_path, PERSONAL_ENV_TEMPLATE).is_err() {
            eprintln!("写入 .env 失败");
            std::process::exit(1);
        }
        println!("已生成 {}", env_path.display());
    }
    println!("个人模式数据目录: {}", data_dir.display());
    println!("启动服务: oce serve --data-dir {}", data_dir.display());
}

async fn serve(data_dir: PathBuf, env_file: Option<PathBuf>, host: String, port: u16, verbose: u8) {
    // .env 加载顺序：显式 env_file > data_dir/.env
    let env_path = env_file.unwrap_or_else(|| data_dir.join(".env"));
    if env_path.exists() {
        let _ = dotenvy::from_path(&env_path);
    }

    // 日志级别：-v 次数（默认 WARNING）
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("oce={level}"))),
        )
        .init();

    // 显式性检测必须在 from_env 之前：默认值（相对路径）不能冒充用户显式配置
    let db_url_explicit = std::env::var("DB_URL").is_ok();
    let trivium_path_explicit = std::env::var("TRIVIUM_PATH").is_ok();
    let settings = oce_infra::settings::Settings::from_env();
    let settings = {
        let mut s = settings;
        if !db_url_explicit {
            s.database.url = format!("sqlite:///{}", data_dir.join("oce.db").display());
        }
        if !trivium_path_explicit {
            s.trivium.path = data_dir.join("oce.tdb").to_string_lossy().into_owned();
        }
        s
    };

    let container = match oce_app::container::Container::build(settings).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("服务装配失败: {e}");
            std::process::exit(1);
        }
    };

    // worker（WORKER_ENABLED=true 时启动；个人模式默认同步索引）
    if std::env::var("WORKER_ENABLED")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
    {
        let queue = Arc::new(oce_app::worker::InProcessQueue::new());
        let worker = oce_app::worker::EmbedWorker::new(
            queue,
            container.application.indexing.clone(),
            container.application.blob_repo.clone(),
            container.settings.worker.concurrency,
            container.settings.worker.max_retries,
        );
        worker.start().await;
        std::mem::forget(worker); // 常驻
        tracing::info!("worker started");
    }

    let state = routes::AppState {
        application: container.application.clone(),
        container: container.clone(),
    };
    let app = routes::router(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::AllowOrigin::list(
            parse_cors_origins(&container.settings.cors_origins)
                .into_iter()
                .filter_map(|o| o.parse().ok()),
        ))
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PATCH,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                ])
                .max_age(std::time::Duration::from_secs(600)),
        );

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)));
    tracing::info!("OpenContextEngine (Rust) listening on http://{addr}");
    println!("OpenContextEngine (Rust) listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

fn parse_cors_origins(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}


/// `oce doctor`：迁移/排障自检。逐项报告，全部通过才输出 ok。
fn doctor(data_dir: PathBuf) {
    println!("oce doctor — 数据目录 {}", data_dir.display());
    let mut failures = 0usize;

    // 1. .env
    let env_path = data_dir.join(".env");
    if env_path.exists() {
        println!("  [ok] .env: {}", env_path.display());
    } else {
        println!("  [warn] .env 不存在（oce init 可生成；环境变量也可来自外部）");
    }

    // 2. 加载 env 并检查 DB
    if env_path.exists() {
        let _ = dotenvy::from_path_override(&env_path);
    }
    let settings = oce_infra::settings::Settings::from_env();

    // 相对路径一律相对数据目录解析（doctor 不创建任何文件）
    let resolve = |p: String| -> String {
        let path = Path::new(&p);
        if path.is_absolute() {
            p
        } else {
            data_dir.join(path).to_string_lossy().into_owned()
        }
    };
    let db_path = settings
        .database
        .sqlite_path()
        .map(resolve)
        .unwrap_or_else(|| data_dir.join("oce.db").to_string_lossy().into_owned());
    if Path::new(&db_path).exists() {
        // 只读打开：doctor 不允许副作用（schema 已由 serve/init 创建）
        let db = match rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(e) => {
                println!("  [fail] SQLite 打开失败 {db_path}: {e}");
                std::process::exit(1);
            }
        };
        match {
                let blob_count: i64 = db
                    .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
                    .unwrap_or(-1);
                if blob_count >= 0 {
                    println!("  [ok] SQLite: {db_path}（blobs={blob_count}，schema 可打开）");
                    Ok::<(), String>(())
                } else {
                    println!("  [fail] SQLite schema 不可用：{db_path}");
                    failures += 1;
                    Ok::<(), String>(())
                }
            } {
                Ok(()) => {}
                Err(e) => {
                    println!("  [fail] SQLite 检查失败: {e}");
                    failures += 1;
                }
            }
    } else {
        println!("  [info] SQLite 不存在（首次启动自动创建）：{db_path}");
    }

    // 3. TriviumDB（维度一致性最常见坑）
    let tdb_path = if settings.trivium.path.is_empty() {
        db_path
            .rsplit_once('/')
            .map(|(d, _)| format!("{d}/oce.tdb"))
            .unwrap_or_else(|| data_dir.join("oce.tdb").to_string_lossy().into_owned())
    } else {
        resolve(settings.trivium.path.clone())
    };
    let _ = std::env::set_var("TRIVIUM_PATH", &tdb_path);
    // 只有主文件存在才打开（lock/wal 不构成可读库）；doctor 严禁创建文件
    if Path::new(&tdb_path).exists() {
        let probe_settings = oce_infra::settings::TriviumSettings {
            path: tdb_path.clone(),
            dense_dim: settings.trivium.dense_dim,
            ..settings.trivium.clone()
        };
        match oce_infra::trivium::TriviumStore::open(probe_settings) {
            Ok(store) => println!(
                "  [ok] TriviumDB: {tdb_path}（dim={}，nodes={}）",
                store.dim(),
                store.node_count()
            ),
            Err(e) => {
                // 写锁被运行中的 serve 持有是正常并发状态（TriviumDB 单写者），不是故障
                let msg = format!("{e}");
                if msg.contains("already opened") || msg.contains("locked") || msg.contains("锁定")
                {
                    println!("  [info] TriviumDB 正被运行中的服务持有（写锁），跳过打开检查：{tdb_path}");
                } else {
                    println!("  [fail] TriviumDB 打开失败 {tdb_path}: {e}");
                    failures += 1;
                }
            }
        }
    } else if Path::new(&format!("{tdb_path}.lock")).exists()
        || Path::new(&format!("{tdb_path}.wal")).exists()
    {
        // 未 flush 前只有 lock/wal 是正常状态（TriviumDB 主文件在首次 flush 落盘）
        println!("  [info] TriviumDB 已初始化（主文件待首次 flush）：{tdb_path}");
    } else {
        println!("  [info] TriviumDB 不存在（首次启动自动创建）：{tdb_path}");
    }

    // 4. 嵌入提供方
    let provider = settings.embedding.provider.as_str();
    let provider_name = match provider {
        "static" => "static",
        "openai" => "openai",
        _ => {
            if settings.embedding.api_key.is_some() {
                "openai"
            } else {
                "static"
            }
        }
    };
    match provider_name {
        "static" => {
            match oce_infra::static_embed::StaticEmbedder::load(&settings.embedding) {
                Ok(e) => println!(
                    "  [ok] 嵌入提供方 static：{}（dim={}）",
                    e.model_id(),
                    e.dim()
                ),
                Err(e) => {
                    println!("  [fail] 静态模型加载失败：{e}");
                    failures += 1;
                }
            }
        }
        _ => {
            if settings.embedding.api_key.is_some() {
                println!("  [ok] 嵌入提供方 openai：EMBED_API_KEY 已配置");
            } else {
                println!("  [info] 嵌入提供方 openai：无 EMBED_API_KEY，运行时将查 model_credentials，缺省报 ServiceNotReady");
            }
        }
    }

    if failures == 0 {
        println!("doctor: ok");
    } else {
        println!("doctor: {failures} 项失败");
        std::process::exit(1);
    }
}
