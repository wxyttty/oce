//! 工作区嵌入式索引器：MCP 单进程模式的核心。
//!
//! 与 HTTP 客户端（ace-client）语义对齐但无网络层：本进程直接持有
//! SQLite + TriviumDB + 嵌入器，工作区扫描/忽略/哈希/增量同步全部内联完成。
//! 数据目录默认 `<workspace>/.oce/`，删除该目录即完全重置。
//!
//! 同步策略：惰性增量——每次 `search` 前做一次轻量 walk（mtime/size 比对，
//! 毫秒级），有变化才读内容、哈希、入库；消失的路径连同其向量一并删除。
//! 全量重建由 `reindex()` 显式触发。

use oce_core::error::{OceError, OceResult};
use oce_core::formatter::format_retrieval;
use oce_core::indexing::{BlobRepository, IndexingPipeline};
use oce_core::retrieval::RetrievalPipeline;
use oce_infra::sqlite::repos::SqlBlobRepository;
use oce_infra::sqlite::SqlDb;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// 单文件准入上限：超大文件（bundle/产物）不进索引。
const MAX_FILE_BYTES: u64 = 1_000_000;

/// 默认忽略的目录名（配合 .gitignore/.oceignore 使用；.git 内容必须排除）。
const DEFAULT_IGNORED_DIRS: [&str; 15] = [
    ".git", ".oce", "node_modules", "target", "dist", "build", "out",
    "__pycache__", ".venv", "venv", ".pytest_cache", ".idea", ".vscode",
    "vendor", "coverage",
];

/// 一个文件的扫描结果（轻量字段，walk 阶段即可比对）。
#[derive(Debug, Clone)]
struct ScannedFile {
    path: String,
    size: u64,
    mtime_ns: i64,
}

/// 工作区索引器。持有全部存储句柄；`sync`/`search` 均可并发调用（内部串行化）。
pub struct WorkspaceIndexer {
    root: PathBuf,
    indexing: Arc<IndexingPipeline>,
    retrieval: Arc<RetrievalPipeline>,
    blob_repo: Arc<SqlBlobRepository>,
    db: SqlDb,
    trivium: oce_infra::trivium::TriviumHandle,
    /// 同步互斥（避免并发 search 触发双 sync）
    sync_lock: tokio::sync::Mutex<()>,
}

impl WorkspaceIndexer {
    pub fn new(
        root: PathBuf,
        indexing: Arc<IndexingPipeline>,
        retrieval: Arc<RetrievalPipeline>,
        blob_repo: Arc<SqlBlobRepository>,
        db: SqlDb,
        trivium: oce_infra::trivium::TriviumHandle,
    ) -> Self {
        Self {
            root,
            indexing,
            retrieval,
            blob_repo,
            db,
            trivium,
            sync_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 遍历工作区（.gitignore/.oceignore/默认忽略集），返回文本文件清单。
    /// 使用 `ignore` crate：与 ripgrep 同源的忽略语义，`require_git(false)`
    /// 让非 git 工作区也遵守 ignore 文件。
    fn scan(&self) -> Result<Vec<ScannedFile>, String> {
        let mut builder = ignore::WalkBuilder::new(&self.root);
        builder
            .hidden(false)
            .require_git(false)
            .git_ignore(true)
            .git_global(true)
            .parents(true)
            .add_custom_ignore_filename(".oceignore");
        // 按名字全深度剪枝：同名目录（.git/.oce/node_modules…）与其内容一并排除
        builder.filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !DEFAULT_IGNORED_DIRS.contains(&name.as_str())
        });

        let mut out = Vec::new();
        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let rel = match entry.path().strip_prefix(&self.root) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => continue,
            };
            if rel.is_empty() {
                continue;
            }
            // 忽略规则文件自身不是代码，不进索引
            let fname = entry.file_name().to_string_lossy();
            if fname == ".gitignore" || fname == ".oceignore" || fname == ".gitattributes" {
                continue;
            }
            // 准入规则与 HTTP 模式一致：构建产物/依赖目录/二进制不进索引
            if oce_core::source_filter::is_ignored_source_path(&rel) {
                continue;
            }
            out.push(ScannedFile {
                path: rel,
                size: meta.len(),
                mtime_ns: {
                    use std::os::unix::fs::MetadataExt;
                    meta.mtime_nsec()
                },
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// 惰性增量同步：walk 比对 mtime/size → 只读/哈希/入库变化文件 → 删除消失路径。
    /// 干净时开销为一次 walk（毫秒级）。
    pub async fn sync_if_dirty(&self) -> OceResult<SyncReport> {
        let _guard = self.sync_lock.lock().await;
        let start = Instant::now();
        let scanned = self.scan().map_err(|m| OceError::new(m, "WalkError"))?;

        // 轻量比对：mtime/size 与登记一致则跳过
        let mut known: HashMap<String, (String, i64, i64)> = HashMap::new(); // path -> (blob_name, size, mtime)
        self.db
            .with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT path, blob_name, size, mtime_ns FROM workspace_files")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })
                    .map_err(|e| e.to_string())?;
                for (path, blob, size, mtime) in rows.flatten() {
                    known.insert(path, (blob, size, mtime));
                }
                Ok(())
            })
            .map_err(|m| OceError::new(m, "SqliteError"))?;

        // 变化集：新路径或 mtime/size 漂移
        let mut changed: Vec<ScannedFile> = Vec::new();
        let scanned_paths: HashSet<&str> = scanned.iter().map(|f| f.path.as_str()).collect();
        for f in &scanned {
            match known.get(&f.path) {
                Some((_, size, mtime)) if *size == f.size as i64 && *mtime == f.mtime_ns => {}
                _ => changed.push(f.clone()),
            }
        }
        // 消失集：登记过但 walk 不再出现
        let deleted_paths: Vec<String> = known
            .keys()
            .filter(|p| !scanned_paths.contains(p.as_str()))
            .cloned()
            .collect();

        let mut report = SyncReport {
            scanned: scanned.len(),
            uploaded: 0,
            deleted: 0,
            elapsed_ms: 0,
        };

        // 1) 删除消失路径的 blob（向量 + 元数据）
        if !deleted_paths.is_empty() {
            for path in &deleted_paths {
                if let Some((blob_name, _, _)) = known.get(path) {
                    self.delete_blob(blob_name).await?;
                    report.deleted += 1;
                }
            }
        }

        // 2) 变化文件：读内容（rayon 并行）→ 哈希 → 内容未变则只更新登记，否则 ingest
        let mut registrations: Vec<(String, String, i64, i64)> = Vec::new();
        if !changed.is_empty() {
            let contents: Vec<(ScannedFile, Option<String>)> = changed
                .par_iter()
                .map(|f| {
                    let full = self.root.join(&f.path);
                    let content = std::fs::read_to_string(&full).ok();
                    (f.clone(), content)
                })
                .collect();

            for (f, content) in contents {
                let Some(content) = content else {
                    continue; // 二进制/读取失败：不索引
                };
                if oce_core::source_filter::is_binary_source(&content) {
                    continue;
                }
                let blob_name = crate::service::compute_blob_name(&f.path, &content);
                let unchanged = known
                    .get(&f.path)
                    .map(|(b, _, _)| b == &blob_name)
                    .unwrap_or(false);
                if !unchanged {
                    self.indexing.ingest(&blob_name, &f.path, &content).await?;
                    self.indexing
                        .embed_pending(Some(&[blob_name.clone()]), true)
                        .await?;
                    report.uploaded += 1;
                }
                registrations.push((f.path.clone(), blob_name, f.size as i64, f.mtime_ns));
            }
            // 登记批量落库（单事务）
            self.db
                .with_conn(|conn| {
                    let tx = conn.transaction().map_err(|e| e.to_string())?;
                    for (path, blob, size, mtime) in &registrations {
                        tx.execute(
                            "INSERT INTO workspace_files (path, blob_name, size, mtime_ns)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(path) DO UPDATE SET
                                blob_name = excluded.blob_name,
                                size = excluded.size,
                                mtime_ns = excluded.mtime_ns",
                            rusqlite::params![path, blob, size, mtime],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    tx.commit().map_err(|e| e.to_string())
                })
                .map_err(|m| OceError::new(m, "SqliteError"))?;
        }

        // 3) 删除路径的登记行
        if !deleted_paths.is_empty() {
            self.db
                .with_conn(|conn| {
                    for path in &deleted_paths {
                        conn.execute("DELETE FROM workspace_files WHERE path = ?1", [path])
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })
                .map_err(|m| OceError::new(m, "SqliteError"))?;
        }

        report.elapsed_ms = start.elapsed().as_millis() as i64;
        Ok(report)
    }

    /// 全量重建：清空该工作区全部 blob（向量+元数据）与登记，再完整同步。
    pub async fn reindex(&self) -> OceResult<SyncReport> {
        let all = self.blob_repo.find_all_blob_names().await?;
        for blob_name in &all {
            self.delete_blob(blob_name).await?;
        }
        self.db
            .with_conn(|conn| {
                conn.execute("DELETE FROM workspace_files", [])
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
            .map_err(|m| OceError::new(m, "SqliteError"))?;
        let mut report = self.sync_if_dirty().await?;
        report.deleted = all.len();
        Ok(report)
    }

    async fn delete_blob(&self, blob_name: &str) -> OceResult<()> {
        use oce_core::search::VectorIndex;
        self.trivium.delete(&[blob_name.to_string()]).await?;
        self.blob_repo.delete(blob_name).await?;
        Ok(())
    }

    /// 检索：先惰性同步，再走完整检索管线（scope = 当前全部 blob）。
    pub async fn search(&self, query: &str, _top_k: Option<usize>) -> OceResult<SearchOutcome> {
        let sync = self.sync_if_dirty().await?;
        let names = self.current_blob_names().await?;
        let started = Instant::now();
        // RetrievalPipeline::search 返回 Vec<SearchHit>（非 Result）
        let hits = self.retrieval.search(query, Some(&names), None).await;
        let elapsed_ms = started.elapsed().as_millis() as i64;
        let formatted = format_retrieval(&hits);
        Ok(SearchOutcome {
            hit_count: hits.len(),
            formatted,
            elapsed_ms,
            synced_files: sync.scanned,
            indexed_blobs: names.len(),
        })
    }

    pub async fn status(&self) -> OceResult<IndexStatus> {
        // status 报告实时状态：先做惰性同步（干净时仅一次轻量 walk）
        let _ = self.sync_if_dirty().await?;
        let files = self
            .db
            .with_conn(|conn| {
                let n: i64 = conn
                    .query_row("SELECT COUNT(*) FROM workspace_files", [], |r| r.get(0))
                    .map_err(|e| e.to_string())?;
                Ok(n)
            })
            .map_err(|m| OceError::new(m, "SqliteError"))?;
        Ok(IndexStatus {
            workspace: self.root.display().to_string(),
            tracked_files: files.max(0) as usize,
            indexed_blobs: self.current_blob_names().await?.len(),
            vector_nodes: self.trivium.node_count(),
        })
    }

    async fn current_blob_names(&self) -> OceResult<Vec<String>> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT blob_name FROM workspace_files ORDER BY path")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
        })
        .await
        .map_err(|e| OceError::new(e.to_string(), "JoinError"))?;
        result.map_err(|m| OceError::new(m, "SqliteError"))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncReport {
    pub scanned: usize,
    pub uploaded: usize,
    pub deleted: usize,
    pub elapsed_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchOutcome {
    pub hit_count: usize,
    pub formatted: String,
    pub elapsed_ms: i64,
    pub synced_files: usize,
    pub indexed_blobs: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexStatus {
    pub workspace: String,
    pub tracked_files: usize,
    pub indexed_blobs: usize,
    pub vector_nodes: usize,
}
