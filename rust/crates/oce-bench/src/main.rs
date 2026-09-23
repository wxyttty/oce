//! oce-bench：索引/查询阶段耗时基准。
//!
//! 用法：
//!   oce-bench chunk <dir>              # 切块吞吐（纯本地 CPU）
//!   oce-bench index <dir> [--dim N]    # 切块 + 假嵌入 + TriviumDB 写入 + SQLite 元数据
//!   oce-bench query <dir> [--n N]      # 假嵌入查询 + 向量检索 + 精确召回
//!
//! 嵌入阶段用确定性假向量（字符 4-gram 哈希），隔离本地处理耗时与外部 API 耗时；
//! 真实端到端耗时由部署环境主导（embedding 网络调用与语言无关）。

use clap::{Parser, Subcommand};
use oce_core::chunk::Chunker;
use oce_core::indexing::BlobRepository;
use oce_core::search::VectorIndex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 切块吞吐
    Chunk { dir: PathBuf },
    /// QuIVer ANN 大库基准（≥1 万节点；ANN vs 精确暴力对比）
    Quiver {
        /// 合成节点数（应 ≥ 10000 以触发 QuIVer 构建）
        #[arg(long, default_value_t = 12000)]
        nodes: usize,
        /// 向量维度
        #[arg(long, default_value_t = 256)]
        dim: usize,
        /// 查询条数
        #[arg(long, default_value_t = 50)]
        n: usize,
    },
    /// 静态查表嵌入（Model2Vec）吞吐
    StaticEmbed {
        /// 模型：HF repo id 或本地目录
        #[arg(long, default_value = "minishlab/potion-base-8M")]
        model: String,
        /// 文本条数
        #[arg(long, default_value_t = 1000)]
        n: usize,
    },
    /// 全量索引（切块 + 假嵌入 + 存储）
    Index {
        dir: PathBuf,
        /// 向量维度
        #[arg(long, default_value_t = 8)]
        dim: usize,
    },
    /// 查询基准
    Query {
        dir: PathBuf,
        /// 查询条数
        #[arg(long, default_value_t = 20)]
        n: usize,
        /// 向量维度
        #[arg(long, default_value_t = 8)]
        dim: usize,
    },
}

/// 确定性假嵌入器（字符 4-gram 哈希）。
fn fake_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0f32; dim];
    let chars: Vec<char> = text.chars().collect();
    for w in chars.windows(4) {
        let s: String = w.iter().collect();
        let h = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        };
        v[(h as usize) % dim] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// 遍历目录中可索引的文本文件（复用 Python 版准入规则的 Rust 移植）。
fn walk_files(root: &Path, limit: usize) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = HashSet::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= limit {
                return out;
            }
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "__pycache__"
                        | ".venv"
                        | "venv"
                        | ".pytest_cache"
                        | "vendor"
                        | "coverage"
                ) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            // 只取常见文本源码/文档扩展
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let keep = matches!(
                ext.as_str(),
                "py" | "rs"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "go"
                    | "java"
                    | "c"
                    | "h"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "kt"
                    | "swift"
                    | "scala"
                    | "md"
                    | "rst"
                    | "txt"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "json"
                    | "sql"
                    | "sh"
                    | "vue"
                    | "html"
                    | "css"
                    | "jsp"
                    | "tag"
            ) || name == "Dockerfile"
                || name == "Makefile";
            if !keep {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if content.contains('\0') {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if !seen.insert(rel.clone()) {
                continue;
            }
            out.push((PathBuf::from(rel), content));
        }
    }
    out
}

fn chunk_dir(
    root: &Path,
    limit: usize,
) -> (Vec<(String, Vec<oce_core::chunk::Chunk>)>, usize, f64) {
    let router = oce_core::chunk::build_chunker().expect("chunker");
    let files = walk_files(root, limit);
    let mut results = Vec::with_capacity(files.len());
    let start = Instant::now();
    let mut total_chars = 0usize;
    let mut total_chunks = 0usize;
    for (rel, content) in &files {
        total_chars += content.chars().count();
        let chunks = router.chunk(content, rel.to_str().unwrap_or("file"));
        total_chunks += chunks.len();
        results.push((rel.to_string_lossy().into_owned(), chunks));
    }
    let elapsed = start.elapsed().as_secs_f64();
    (results, total_chars, elapsed).pipe(|r| {
        println!(
            "files={} chunks={} chars={} chunk_time={:.1}ms throughput={:.0} Kchars/s",
            files.len(),
            total_chunks,
            total_chars,
            elapsed * 1000.0,
            total_chars as f64 / 1024.0 / elapsed
        );
        r
    })
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn temp_paths(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("oce-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    (dir.join("bench.db"), dir.join("bench.tdb"))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Chunk { dir } => {
            let (results, _chars, _elapsed) = chunk_dir(&dir, 10_000);
            let total: usize = results.iter().map(|(_, c)| c.len()).sum();
            println!("total_chunks={total}");
        }
        Command::Quiver { nodes, dim, n } => {
            quiver_bench(nodes, dim, n).await;
        }
        Command::StaticEmbed { model, n } => {
            static_embed_bench(&model, n);
        }
        Command::Index { dir, dim } => {
            let (db_path, tdb_path) = temp_paths("index");
            let (results, chars, chunk_elapsed) = chunk_dir(&dir, 10_000);
            let total_chunks: usize = results.iter().map(|(_, c)| c.len()).sum();

            // 假嵌入
            let embed_start = Instant::now();
            let mut upserts: Vec<oce_core::search::VectorUpsert> = Vec::new();
            let mut located: Vec<oce_core::chunk::LocatedChunk> = Vec::new();
            for (rel, chunks) in &results {
                let blob_name = oce_app::service::compute_blob_name(rel, &format!("{rel}"));
                for chunk in chunks {
                    let located_chunk = oce_core::chunk::LocatedChunk {
                        blob_name: blob_name.clone(),
                        content_hash: chunk.content_hash.clone(),
                        path: rel.clone(),
                        content: chunk.content.clone(),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                    };
                    let vector = fake_embed(&chunk.embedding_text(), dim);
                    upserts.push(oce_core::search::VectorUpsert {
                        chunk_id: located_chunk.chunk_id(),
                        content_hash: chunk.content_hash.clone(),
                        blob_name: blob_name.clone(),
                        content: chunk.content.clone(),
                        vector,
                        path: rel.clone(),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                    });
                    located.push(located_chunk);
                }
            }
            let embed_elapsed = embed_start.elapsed().as_secs_f64();

            // TriviumDB 写入
            let tdb = Arc::new(
                oce_infra::trivium::TriviumStore::open(oce_infra::settings::TriviumSettings {
            fts_lexical: "off".into(),
                    path: tdb_path.to_string_lossy().into_owned(),
                    sync_mode: "off".into(),
                    storage_mode: "rom".into(),
                    dense_dim: dim,
                    auto_build_quiver: false,
                    text_hybrid: true,
                                        text_boost: 0.8,
                    expand_depth: 0,
                })
                .expect("open tdb"),
            );
            let write_start = Instant::now();
            tdb.upsert(upserts).await.expect("upsert");
            let write_elapsed = write_start.elapsed().as_secs_f64();

            // SQLite 元数据（blob 注册 + chunks）
            let sql_start = Instant::now();
            let db = oce_infra::sqlite::SqlDb::open(&db_path.to_string_lossy()).unwrap();
            let repo = Arc::new(oce_infra::sqlite::repos::SqlBlobRepository { db });
            // 顺序与真实管线一致：blob 元数据（pending）→ chunk 内容+出现位置 → blob 置 ready
            for (rel, _chunks) in &results {
                let blob_name = oce_app::service::compute_blob_name(rel, &format!("{rel}"));
                let blob = oce_core::blob::Blob::new(
                    &blob_name,
                    rel,
                    oce_core::blob::BlobStatus::Pending,
                    0,
                    None,
                    "text",
                );
                repo.save(&blob).await.unwrap();
            }
            repo.save_chunks_and_mark(&located).await.unwrap();
            for (rel, chunks) in &results {
                let blob_name = oce_app::service::compute_blob_name(rel, &format!("{rel}"));
                let mut blob = oce_core::blob::Blob::new(
                    &blob_name,
                    rel,
                    oce_core::blob::BlobStatus::Ready,
                    0,
                    None,
                    "text",
                );
                blob.chunks = chunks.iter().map(|c| c.to_ref()).collect();
                repo.save(&blob).await.unwrap();
            }
            let sql_elapsed = sql_start.elapsed().as_secs_f64();

            let total_elapsed = chunk_elapsed + embed_elapsed + write_elapsed + sql_elapsed;
            let ms = |s: f64| s * 1000.0;
            println!("=== index bench (dim={dim}) ===");
            println!(
                "chunk  {:>8.1} ms   ({:.0} Kchars/s)",
                ms(chunk_elapsed),
                chars as f64 / 1024.0 / chunk_elapsed
            );
            println!(
                "embed  {:>8.1} ms   (fake, {total_chunks} chunks)",
                ms(embed_elapsed)
            );
            println!(
                "tdb    {:>8.1} ms   ({:.0} upserts/s)",
                ms(write_elapsed),
                total_chunks as f64 / write_elapsed
            );
            println!(
                "sqlite {:>8.1} ms   (blobs+chunks+symbols)",
                ms(sql_elapsed)
            );
            println!(
                "total  {:>8.1} ms   nodes={}",
                ms(total_elapsed),
                tdb.node_count()
            );
            let _ = std::fs::remove_dir_all(db_path.parent().unwrap());
        }
        Command::Query { dir, n, dim } => {
            let (db_path, tdb_path) = temp_paths("query");
            let (results, _chars, _chunk_elapsed) = chunk_dir(&dir, 10_000);
            let tdb = Arc::new(
                oce_infra::trivium::TriviumStore::open(oce_infra::settings::TriviumSettings {
            fts_lexical: "off".into(),
                    path: tdb_path.to_string_lossy().into_owned(),
                    sync_mode: "off".into(),
                    storage_mode: "rom".into(),
                    dense_dim: dim,
                    auto_build_quiver: false,
                    text_hybrid: true,
                                        text_boost: 0.8,
                    expand_depth: 0,
                })
                .expect("open tdb"),
            );
            let mut upserts = Vec::new();
            for (rel, chunks) in &results {
                let blob_name = oce_app::service::compute_blob_name(rel, &format!("{rel}"));
                for chunk in chunks {
                    upserts.push(oce_core::search::VectorUpsert {
                        chunk_id: format!(
                            "{blob_name}-{}-{}-{}",
                            chunk.content_hash, chunk.start_line, chunk.end_line
                        ),
                        content_hash: chunk.content_hash.clone(),
                        blob_name: blob_name.clone(),
                        content: chunk.content.clone(),
                        vector: fake_embed(&chunk.embedding_text(), dim),
                        path: rel.clone(),
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                    });
                }
            }
            tdb.upsert(upserts).await.unwrap();
            let node_count = tdb.node_count();
            let blobs: Vec<String> = results
                .iter()
                .map(|(rel, _)| oce_app::service::compute_blob_name(rel, &format!("{rel}")))
                .collect();

            // 查询：每次都 embed + search（scope = 全部 blob）
            let queries: Vec<String> = results
                .iter()
                .filter_map(|(_, chunks)| {
                    chunks.first().map(|c| c.content.chars().take(60).collect())
                })
                .take(n)
                .collect();
            let start = Instant::now();
            let mut hit_count = 0usize;
            for q in &queries {
                let qv = fake_embed(q, dim);

                let hits = oce_core::search::SearchStore::search(
                    tdb.as_ref(),
                    q,
                    &qv,
                    Some(&blobs),
                    50,
                    0.0,
                )
                .await
                .unwrap();
                hit_count += hits.len();
            }
            let elapsed = start.elapsed().as_secs_f64();
            println!("=== query bench (dim={dim}, nodes={node_count}) ===");
            println!(
                "queries={} total={:.1} ms avg={:.3} ms hits={}",
                queries.len(),
                elapsed * 1000.0,
                elapsed * 1000.0 / queries.len().max(1) as f64,
                hit_count
            );
            let _ = std::fs::remove_dir_all(db_path.parent().unwrap());
        }
    }
}

/// 合成向量：确定性伪随机单位向量。
fn synthetic_vector(seed: usize, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim)
        .map(|i| {
            let seed_u = seed as u64;
            let x = (seed_u
                .wrapping_mul(6364136223846793005u64)
                .wrapping_add((i as u64).wrapping_mul(1442695040888963407u64)))
                >> 33;
            (x % 2000) as f32 / 1000.0 - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// QuIVer 大库基准：构建 ≥1 万节点库，对比 ANN 检索与精确暴力检索的耗时与召回。
async fn quiver_bench(nodes: usize, dim: usize, n: usize) {
    use oce_core::search::{SearchStore, VectorIndex};
    let dir = std::env::temp_dir().join(format!("oce-bench-quiver-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let tdb = Arc::new(
        oce_infra::trivium::TriviumStore::open(oce_infra::settings::TriviumSettings {
            fts_lexical: "off".into(),
            path: dir.join("quiver.tdb").to_string_lossy().into_owned(),
            sync_mode: "off".into(),
            storage_mode: "rom".into(),
            dense_dim: dim,
            auto_build_quiver: true,
            text_hybrid: true, // ≥1 万节点时查询自动构建 QuIVer ANN
            text_boost: 0.8,
            expand_depth: 0,
        })
        .expect("open tdb"),
    );

    let start = Instant::now();
    let upserts: Vec<oce_core::search::VectorUpsert> = (0..nodes)
        .map(|i| oce_core::search::VectorUpsert {
            chunk_id: format!("synthetic-{i}"),
            content_hash: format!("{i:064x}"),
            blob_name: format!("blob-{}", i / 10),
            content: String::new(),
            vector: synthetic_vector(i, dim),
            path: format!("synthetic/{i}.rs"),
            start_line: 1,
            end_line: 2,
        })
        .collect();
    tdb.upsert(upserts).await.unwrap();
    let index_elapsed = start.elapsed().as_secs_f64();
    println!("=== QuIVer bench (dim={dim}, nodes={nodes}) ===");
    println!(
        "index: {:.1} ms ({:.0} upserts/s)",
        index_elapsed * 1000.0,
        nodes as f64 / index_elapsed
    );

    let queries: Vec<Vec<f32>> = (0..n)
        .map(|i| synthetic_vector(nodes / 2 + i, dim))
        .collect();

    // 预热：首次查询触发 QuIVer 构建，单独计时
    let build_start = Instant::now();
    {
        use oce_core::search::SearchStore;
        let _ = SearchStore::search(tdb.as_ref(), "warmup", &queries[0], None, 10, -1.0)
            .await
            .unwrap();
    }
    let build_elapsed = build_start.elapsed().as_secs_f64();
    println!(
        "quiver build (first query): {:.1} ms",
        build_elapsed * 1000.0
    );

    // ANN 稳态
    let ann_start = Instant::now();
    let mut ann_out: Vec<Vec<(String, f32)>> = Vec::with_capacity(queries.len());
    for qv in &queries {
        let hits = SearchStore::search(tdb.as_ref(), "q", qv, None, 10, -1.0)
            .await
            .unwrap();
        ann_out.push(
            hits.into_iter()
                .map(|h| (h.content_hash.clone(), h.score))
                .collect(),
        );
    }
    let ann_elapsed = ann_start.elapsed().as_secs_f64();

    // 精确基线
    let exact_start = Instant::now();
    let exact_hits: Vec<Vec<(String, f32)>> = queries
        .iter()
        .map(|qv| {
            tdb.search_exact_hits_sync(qv, 10)
                .unwrap()
                .into_iter()
                .map(|(h, s)| (h.content_hash.clone(), s))
                .collect::<Vec<_>>()
        })
        .collect();
    let exact_elapsed = exact_start.elapsed().as_secs_f64();

    // Recall@10
    let mut recall_sum = 0.0f64;
    for (a, e) in ann_out.iter().zip(&exact_hits) {
        let exact_ids: HashSet<&String> = e.iter().map(|(h, _)| h).collect();
        let overlap = a.iter().filter(|(h, _)| exact_ids.contains(h)).count();
        recall_sum += overlap as f64 / e.len().max(1) as f64;
    }
    println!(
        "query avg: ANN {:.3} ms | exact {:.3} ms | recall@10={:.3}",
        ann_elapsed * 1000.0 / n as f64,
        exact_elapsed * 1000.0 / n as f64,
        recall_sum / n as f64
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Model2Vec 静态嵌入吞吐。
fn static_embed_bench(model: &str, n: usize) {
    let mut settings = oce_infra::settings::EmbeddingSettings::from_env();
    settings.static_model = Some(model.to_string());
    let embedder =
        oce_infra::static_embed::StaticEmbedder::load(&settings).unwrap_or_else(|e| panic!("{e}"));
    let texts: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "File: src/module_{i}.rs\n\npub fn process_{i}(input: &str) -> Result<String, Error> {{\n    let parsed = parse(input)?;\n    Ok(format!(\"{{parsed}}\"))\n}}\n"
            )
        })
        .collect();

    let start = Instant::now();
    let vectors = embedder.encode_batch(&texts);
    let elapsed = start.elapsed().as_secs_f64();
    assert_eq!(vectors.len(), n, "embed count mismatch");
    println!("=== static embed bench ===");
    println!(
        "model={} dim={} texts={} total={:.1} ms ({:.0} texts/s)",
        embedder.model_id(),
        embedder.dim(),
        n,
        elapsed * 1000.0,
        n as f64 / elapsed
    );
}
