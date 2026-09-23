//! 嵌入式工作区索引器测试：扫描/忽略/增量同步/删除/检索 全周期。

use oce_app::workspace::WorkspaceIndexer;
use oce_infra::settings::Settings;
use std::sync::Arc;

fn temp_ws(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oce-ws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings_for(ws: &std::path::Path) -> Settings {
    let mut s = Settings::from_env();
    s.database.url = format!("sqlite:///{}", ws.join(".oce/oce.db").display());
    s.trivium.path = ws.join(".oce/oce.tdb").to_string_lossy().into_owned();
    s.trivium.dense_dim = 8;
    s.trivium.sync_mode = "off".into();
    s.trivium.auto_build_quiver = false;
    s.embedding.dimensions = 8;
    s.retrieval.inner.intent_classification_enabled = false;
    s.retrieval.inner.query_decomposition_enabled = false;
    s.llm.rerank_enabled = false;
    s.retrieval.inner.query_rewrite_enabled = false;
    s
}

/// file_desc 开启版设置（描述注入链路验证用）。
fn settings_for_with_file_desc(ws: &std::path::Path) -> Settings {
    let mut s = settings_for(ws);
    s.retrieval.file_desc_enabled = true;
    s
}

/// related symbols 开启版设置（hints 输出验证用）。
fn settings_for_with_related(ws: &std::path::Path) -> Settings {
    let mut s = settings_for(ws);
    s.retrieval.inner.related_symbols_enabled = true;
    s
}

async fn make_indexer(ws: &std::path::Path) -> Arc<WorkspaceIndexer> {
    make_indexer_with(ws, settings_for(ws)).await
}

async fn make_indexer_with(ws: &std::path::Path, settings: Settings) -> Arc<WorkspaceIndexer> {
    let container = oce_app::container::Container::build_with_embedder(
        settings,
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await
    .unwrap();
    Arc::new(WorkspaceIndexer::new(
        ws.to_path_buf(),
        container.application.indexing.clone(),
        container.application.retrieval.clone(),
        container.application.blob_repo.clone(),
        container.db.clone().unwrap(),
        container.application.vector.clone(),
    ))
}

struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let chars: Vec<char> = text.chars().collect();
        for w in chars.windows(4) {
            let s: String = w.iter().collect();
            let h = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                s.hash(&mut h);
                h.finish()
            };
            v[(h as usize) % self.dim] += 1.0;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

#[async_trait::async_trait]
impl oce_core::search::Embedder for FakeEmbedder {
    async fn embed_documents(
        &self,
        texts: Vec<String>,
    ) -> oce_core::error::OceResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }
    async fn embed_query(&self, text: &str) -> oce_core::error::OceResult<Vec<f32>> {
        Ok(self.embed(text))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_search_change_delete_cycle() {
    let ws = temp_ws("cycle");
    std::fs::write(ws.join("app.py"), "def hello_world():\n    return 1\n").unwrap();
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/order.rs"), "pub fn place_order() {}\n").unwrap();
    // 应被 .oceignore 排除
    std::fs::write(ws.join(".oceignore"), "generated/\n").unwrap();
    std::fs::create_dir_all(ws.join("generated")).unwrap();
    std::fs::write(ws.join("generated/gen.rs"), "pub fn generated_fn() {}\n").unwrap();

    let indexer = make_indexer(&ws).await;

    // 首次 search 触发全量同步并命中
    let out = indexer.search("hello_world", None).await.unwrap();
    assert!(out.formatted.contains("app.py"), "got: {}", out.formatted);
    assert_eq!(out.synced_files, 2); // app.py + sub/order.rs；generated 被 ignore

    // status
    let status = indexer.status().await.unwrap();
    assert_eq!(status.tracked_files, 2);
    assert_eq!(status.indexed_blobs, 2);

    // 修改文件：mtime 变化 → 增量同步感知
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(ws.join("app.py"), "def hello_world():\n    return 42\n").unwrap();
    let out = indexer.search("return 42", None).await.unwrap();
    assert!(
        out.formatted.contains("return 42"),
        "modified content should be re-indexed: {}",
        out.formatted
    );

    // 删除文件：下次同步移除
    std::fs::remove_file(ws.join("sub/order.rs")).unwrap();
    let status = indexer.status().await.unwrap();
    assert_eq!(status.tracked_files, 1);
    assert_eq!(status.indexed_blobs, 1);

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test(flavor = "multi_thread")]
async fn reindex_rebuilds_from_scratch() {
    let ws = temp_ws("reindex");
    std::fs::write(ws.join("x.py"), "value = unique_marker_xyz\n").unwrap();
    let indexer = make_indexer(&ws).await;
    indexer.search("unique_marker_xyz", None).await.unwrap();
    let status_before = indexer.status().await.unwrap();
    assert_eq!(status_before.indexed_blobs, 1);

    let report = indexer.reindex().await.unwrap();
    assert_eq!(report.uploaded, 1);
    let status = indexer.status().await.unwrap();
    assert_eq!(status.indexed_blobs, 1);
    assert!(status.vector_nodes > 0);

    let _ = std::fs::remove_dir_all(&ws);
}

/// file_desc 开启：清单文件描述注入 embedding_text，中文查询能命中
/// TOML 内容（FakeEmbedder 是 4-gram 哈希，桥接句的英文 token 与查询
/// 「python project manifest」直接重叠）；同时验证 sidecar 指纹带 etext=v2。
#[tokio::test(flavor = "multi_thread")]
async fn file_desc_enriches_manifest_embedding() {
    let ws = temp_ws("filedesc");
    std::fs::write(
        ws.join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = []\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("README.md"),
        "# Demo\n\nA demo service for tokens.\n",
    )
    .unwrap();

    let indexer = make_indexer_with(&ws, settings_for_with_file_desc(&ws)).await;
    let out = indexer
        .search("python project manifest dependencies", None)
        .await
        .unwrap();
    assert!(
        out.formatted.contains("Path: pyproject.toml"),
        "file_desc bridge sentence should surface the manifest for a Chinese-style config query, got: {}",
        out.formatted
    );

    // README 首段注入：描述来自 staging 全文
    let out = indexer
        .search("demo service for tokens", None)
        .await
        .unwrap();
    assert!(
        out.formatted.contains("Path: README.md"),
        "README first-paragraph description should be embedded, got: {}",
        out.formatted
    );

    // 指纹 sidecar：etext=v2 标记存在
    let sidecar = ws.join(".oce/oce.tdb.model");
    let recorded = std::fs::read_to_string(sidecar).unwrap();
    assert!(recorded.contains("etext=v2"), "sidecar: {recorded}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// 同一数据目录：etext=v2 索引存在时，关闭 file_desc 启动应 fail-closed
/// （与换嵌入模型同语义，防止新旧文本格式向量混入）。
#[tokio::test(flavor = "multi_thread")]
async fn file_desc_fingerprint_mismatch_fails_closed() {
    let ws = temp_ws("filedesc-fp");
    std::fs::write(ws.join("a.py"), "def alpha():\n    pass\n").unwrap();

    // 第一次：file_desc 开启建索引（etext=v2）。必须实际写入数据——
    // TriviumDB Rom 模式懒建主文件，仅 open+close 不落 .tdb，指纹检查
    // 会因 tdb_exists=false 而跳过（真实使用中索引过至少一个 blob）
    let c1 = oce_app::container::Container::build_with_embedder(
        settings_for_with_file_desc(&ws),
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await
    .unwrap();
    let uploads = vec![oce_app::service::BlobUpload {
        path: "a.py".into(),
        content: "def alpha():\n    pass\n".into(),
    }];
    let r = c1.application.batch_upload(uploads, None).await.unwrap();
    let cp = c1
        .application
        .checkpoint(None, &r.blob_names, &[])
        .await
        .unwrap();
    let _ = c1
        .application
        .retrieve("alpha", Some(&cp.new_checkpoint_id), &[], &[])
        .await
        .unwrap();
    drop(c1);

    // 第二次：同一数据目录关闭 file_desc → 指纹不匹配，拒绝启动
    let second = oce_app::container::Container::build_with_embedder(
        settings_for(&ws),
        Some(Arc::new(FakeEmbedder::new(8))),
    )
    .await;
    let err = match second {
        Err(e) => e,
        Ok(_) => panic!("fingerprint mismatch must fail closed"),
    };
    assert!(
        err.contains("etext"),
        "error should mention etext mismatch: {err}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// related symbols hints 端到端：选中 api.rs（引用 TokenRefresher），
/// token.rs 里的定义未入窗 → <related_symbols> 块出现在 formatted 输出，
/// 且不影响 Path: 行（评测兼容）。
#[tokio::test(flavor = "multi_thread")]
async fn related_symbols_hints_appended_to_output() {
    let ws = temp_ws("related");
    // api.rs 引用 TokenRefresher；token.rs 定义它。FakeEmbedder 4-gram 哈希
    // 下查询「api_client」与 api.rs 的 token 重叠更高，token.rs 不入窗
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src/api.rs"),
        "pub fn api_client() -> u32 {\n    let r = TokenRefresher::new();\n    r.refresh_token();\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("src/token.rs"),
        "pub struct TokenRefresher;\n\nimpl TokenRefresher {\n    pub fn refresh_token(&self) {}\n}\n",
    )
    .unwrap();

    let indexer = make_indexer_with(&ws, settings_for_with_related(&ws)).await;
    let out = indexer.search("api_client", None).await.unwrap();

    // hints 块存在且指向 token.rs（定义未入窗时）
    if out.formatted.contains("Path: src/token.rs") {
        // token.rs 恰好入窗（FakeEmbedder 哈希可能召回它）：定义可见则无 hint
        assert!(
            !out.formatted.contains("<related_symbols"),
            "definition shown in window should not be hinted: {}",
            out.formatted
        );
    } else {
        assert!(
            out.formatted.contains("<related_symbols"),
            "expected related_symbols block when definition not in window: {}",
            out.formatted
        );
        assert!(out.formatted.contains("<symbol name=\"TokenRefresher\""));
        assert!(out.formatted.contains("path=\"src/token.rs\""));
    }
    // 评测兼容：Path: 行仍是路径唯一来源，hints 块在其后
    if out.formatted.contains("<related_symbols") {
        let path_pos = out.formatted.find("Path: ").unwrap();
        let rel_pos = out.formatted.find("<related_symbols").unwrap();
        assert!(rel_pos > path_pos);
    }

    let _ = std::fs::remove_dir_all(&ws);
}

/// related symbols 关闭（默认）：输出无 <related_symbols> 块。
#[tokio::test(flavor = "multi_thread")]
async fn related_symbols_disabled_by_default() {
    let ws = temp_ws("related-off");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src/api.rs"),
        "pub fn api_client() -> u32 {\n    let r = TokenRefresher::new();\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(ws.join("src/token.rs"), "pub struct TokenRefresher;\n").unwrap();

    let indexer = make_indexer(&ws).await;
    let out = indexer.search("api_client", None).await.unwrap();
    assert!(!out.formatted.contains("<related_symbols"));

    let _ = std::fs::remove_dir_all(&ws);
}

/// broad mode 开启版设置（探索型查询宽窗口 regime 验证用）。
fn settings_for_with_broad(ws: &std::path::Path) -> Settings {
    let mut s = settings_for(ws);
    s.retrieval.inner.broad_mode_enabled = true;
    s
}

/// 构造 broad fixture：28 个与查询共享词法信号的代码文件 + 1 个与查询
/// 零词法重叠的清单文件（manifest prior 的提升对象）+ 1 个 60 行长文件
/// （骨架化对象）。共 30 个 blob，focused 10 座席装不下、broad 20 座席。
fn write_broad_fixture(ws: &std::path::Path) {
    std::fs::create_dir_all(ws.join("src")).unwrap();
    for i in 0..28 {
        std::fs::write(
            ws.join(format!("src/feature_{i:02}_service.rs")),
            format!(
                "pub fn feature_{i}_service() {{\n    // service architecture feature {i}\n}}\n"
            ),
        )
        .unwrap();
    }
    // 清单文件：内容与查询无共享 4-gram，dense 排不进头部，靠 prior 入窗
    std::fs::write(
        ws.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nrequires-python = \">=3.13\"\n\n[tool.ruff]\nline-length = 100\n",
    )
    .unwrap();
    // 60 行长文件：头 12 行富集查询词（dense 排名高，且不占 term 预算），
    // 13-44 行无查询词填充，第 45 行含查询词（骨架化 term 保留行，用于
    // 验证省略段之后的行号重同步），46-60 行继续填充。
    let mut big = String::new();
    for i in 1..=12 {
        big.push_str(&format!("// service architecture header section {i}\n"));
    }
    for i in 13..=44 {
        big.push_str(&format!("// filler {i}\n"));
    }
    big.push_str("pub const SERVICE_HUB: &str = \"hub\";\n");
    for i in 46..=60 {
        big.push_str(&format!("// filler {i}\n"));
    }
    std::fs::write(ws.join("src/big_service.rs"), big).unwrap();
}

/// broad 关闭（默认）：架构类查询不进入宽窗口 regime——10 hits、无
/// exploratory 提示、无骨架化、清单文件不因 prior 入窗。
#[tokio::test(flavor = "multi_thread")]
async fn broad_mode_disabled_by_default() {
    let ws = temp_ws("broad-off");
    write_broad_fixture(&ws);
    let indexer = make_indexer(&ws).await;
    let out = indexer
        .search("service architecture overview", None)
        .await
        .unwrap();
    assert_eq!(out.hit_count, 10, "focused window stays at final_select_k");
    assert!(!out.formatted.contains("exploratory query"));
    assert!(!out.formatted.contains("lines omitted, read"));
    assert!(
        !out.formatted.contains("Path: pyproject.toml"),
        "manifest file must not be pulled in without the prior: {}",
        out.formatted
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// broad 开启：启发式词表触发（无 LLM 分类，nollm 路径）——20 hits 宽窗口、
/// manifest prior 把清单文件拉入窗口、长摘录骨架化且省略段后的行号保持真实。
#[tokio::test(flavor = "multi_thread")]
async fn broad_mode_engages_with_wide_window_and_skeleton() {
    let ws = temp_ws("broad-on");
    write_broad_fixture(&ws);
    let indexer = make_indexer_with(&ws, settings_for_with_broad(&ws)).await;
    let out = indexer
        .search("service architecture overview", None)
        .await
        .unwrap();
    assert_eq!(out.hit_count, 20, "broad window widens to 20");
    assert!(out.formatted.contains("exploratory query"));
    // manifest prior：零词法重叠的 pyproject.toml 被 prior 拉进宽窗口
    assert!(
        out.formatted.contains("Path: pyproject.toml"),
        "manifest prior must seat pyproject.toml: {}",
        out.formatted
    );
    // 骨架化：60 行长文件压缩，标记行引用真实行号区间
    assert!(
        out.formatted
            .contains("lines omitted, read src/big_service.rs:13-44"),
        "long excerpt must be skeletonized with true cited range: {}",
        out.formatted
    );
    // 行号重同步：第 45 行（查询词命中行，紧随标记之后）必须编成 45 而非
    // 骨架内的偏移序号
    assert!(
        out.formatted.contains("    45\tpub const SERVICE_HUB"),
        "line number after elision marker must resync to true lineno: {}",
        out.formatted
    );
    // 标记行本身不带行号前缀（紧跟前一行换行后原样出现）
    assert!(out.formatted.contains("\n... (32 lines omitted"),);
    let _ = std::fs::remove_dir_all(&ws);
}

/// broad 开启但 focused 查询（不命中架构词表）：regime 不误触发。
#[tokio::test(flavor = "multi_thread")]
async fn broad_mode_skips_focused_queries() {
    let ws = temp_ws("broad-skip");
    write_broad_fixture(&ws);
    let indexer = make_indexer_with(&ws, settings_for_with_broad(&ws)).await;
    let out = indexer.search("feature_07_service", None).await.unwrap();
    assert!(
        out.hit_count <= 10,
        "focused query must not widen the window: {}",
        out.hit_count
    );
    assert!(!out.formatted.contains("exploratory query"));
    let _ = std::fs::remove_dir_all(&ws);
}

/// span 合并开启版设置。
fn settings_for_with_span_merge(ws: &std::path::Path) -> Settings {
    let mut s = settings_for(ws);
    s.retrieval.inner.span_merge_enabled = true;
    s
}

/// 合并 fixture：service.py 约 3570 字符，cAST 实测切分单位 ~1820 字符，
/// 恰好切成两个相邻 chunk（两半都含查询词），另加陪衬文件保证窗口有其他候选。
fn write_span_merge_fixture(ws: &std::path::Path) {
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let mut service = String::new();
    for i in 1..=38 {
        service.push_str(&format!(
            "service alpha part one detail {i} of the module\n"
        ));
    }
    for i in 39..=76 {
        service.push_str(&format!(
            "service alpha part two detail {i} of the module\n"
        ));
    }
    std::fs::write(ws.join("src/service.py"), service).unwrap();
    std::fs::write(
        ws.join("src/unrelated.rs"),
        "pub fn unrelated_helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("src/more_unrelated.py"),
        "def another_helper():\n    return 7\n",
    )
    .unwrap();
}

/// 关闭（默认）：service.py 的两个相邻 chunk 各自成段（同 Path 两节）。
#[tokio::test(flavor = "multi_thread")]
async fn span_merge_disabled_keeps_fragments() {
    let ws = temp_ws("spanmerge-off");
    write_span_merge_fixture(&ws);
    let indexer = make_indexer(&ws).await;
    let out = indexer.search("service alpha", None).await.unwrap();
    let sections = out.formatted.matches("Path: src/service.py").count();
    assert!(
        sections >= 2,
        "未合并时相邻 chunk 应各自成段（实际 {sections} 节）：{}",
        out.formatted
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// 开启：两个相邻 chunk 合并成一个连续 section，行号区间连续且内容完整。
#[tokio::test(flavor = "multi_thread")]
async fn span_merge_joins_adjacent_fragments() {
    let ws = temp_ws("spanmerge-on");
    write_span_merge_fixture(&ws);
    let indexer = make_indexer_with(&ws, settings_for_with_span_merge(&ws)).await;
    let out = indexer.search("service alpha", None).await.unwrap();
    let sections = out.formatted.matches("Path: src/service.py").count();
    assert_eq!(sections, 1, "相邻 chunk 必须合并成单节：{}", out.formatted);
    // 合并段内容两半都在（chunk 重构自完整行文本）
    assert!(out.formatted.contains("part one detail 1"));
    assert!(out.formatted.contains("part two detail 76"));
    let _ = std::fs::remove_dir_all(&ws);
}
