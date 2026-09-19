//! Broad mode（BCE broad.go 移植）：Overview/架构类探索型查询专用 regime。
//!
//! 这类查询「没有单个应答 chunk」——答案散布在清单/配置/多个实现里，没有任何
//! chunk 与查询文本相似，focused 调参（10 hits、per-path 3、全文摘录）天然饿死。
//! regime 差异：宽窗口（20 hits / per-path 2）、清单文件结构先验（manifest prior
//! 作为一路额外 RRF 融合）、长摘录骨架化（省略段标注真实行号区间供跟进读取）。
//!
//! 触发（与 BCE 的 rerank 弱分触发不同，OCE 的 LLM 重排只返回顺序无校准分，
//! 弱分不可比——见 Note ④ Alternatives）：LLM intent = Overview **或**
//! [`query_wants_structure`] 启发式词表（无 LLM 时的 fallback，二者 OR）。
//! 词表沿用 BCE 修正后的版本：裸「配置」/「config」刻意缺席——定位型查询
//! （「路由配置在哪」）也含它们，一刀切 manifest boost 会把真实答案埋进
//! pom.xml/Application 噪声（BCE 自己踩过并从表里删掉）。
//!
//! 不搬运 BCE 的子查询分解（OCE 已有 query decomposition + rewrite，语义重叠）
//! 与 speculative prefetch（OCE 检索预算毫秒级，无延迟对冲需求）。

use crate::search::{search_hit_key, SearchHit};
use std::collections::{HashMap, HashSet};

/// broad regime 的最终窗口（focused 为 final_select_k=10；BCE broadMaxHits=24，
/// 本仓按 Note ④ 契约取 20）
pub const BROAD_FINAL_SELECT_K: usize = 20;
/// broad regime 的单文件片段上限（覆盖优先于深度；BCE broadPerFileCap）
pub const BROAD_MAX_CHUNKS_PER_PATH: usize = 2;

/// 超过此行数的摘录才骨架化（BCE skeletonMinLines）
const SKELETON_MIN_LINES: usize = 28;
/// 骨架保留头部行数（BCE skeletonHeadLines）
const SKELETON_HEAD_LINES: usize = 12;
/// 头部之外按查询词保留的行数上限（BCE skeletonTermKeeps）
const SKELETON_TERM_KEEPS: usize = 10;

/// 架构/结构意图词表（BCE structuralIntentTerms，中英双语，小写子串匹配）。
const STRUCTURAL_INTENT_TERMS: [&str; 37] = [
    // zh
    "架构",
    "技术栈",
    "项目配置",
    "如何配置",
    "怎么配置",
    "配置管理",
    "依赖",
    "部署",
    "通信",
    "微服务",
    "框架",
    "中间件",
    "注册中心",
    "网关",
    "构建",
    "打包",
    "环境变量",
    "整体风格",
    "主题",
    "样式风格",
    // en（lowercased 子串匹配）
    "architecture",
    "tech stack",
    "project config",
    "config management",
    "configuration structure",
    "dependenc",
    "deploy",
    "communicat",
    "microservice",
    "framework",
    "middleware",
    "registry",
    "gateway",
    "infra",
    "build system",
    "theme",
    "styling",
];

/// 查询是否在问结构/架构（BCE queryWantsStructure）。子串匹配：中文无词边界；
/// 英文词表本身是多词短语或带后缀的词干（dependenc/deploy），子串误命中风险可接受。
pub fn query_wants_structure(query: &str) -> bool {
    let q = query.to_lowercase();
    STRUCTURAL_INTENT_TERMS.iter().any(|term| q.contains(term))
}

/// 定位语气标记（BCE 无此表——它的 rerank 强头部天然挡住定位查询；OCE 无
/// 校准分，用表面形状当闸）。「环境变量」「依赖」这类词同样出现在
/// 「…的实现代码在哪里？」式查询里（flask Q05 实测：manifest prior 把
/// pyproject.toml 簇拉到答案前面，Top-1 得而复失）——问「某个东西在哪」
/// 的查询有明确目标，不是探索型，无论哪个信号源提议 broad 都不进。
const LOCATOR_MARKERS: [&str; 8] = [
    "在哪",
    "哪个文件",
    "哪些文件",
    "定义在",
    "实现在",
    "where",
    "which file",
    "what file",
];

/// 查询是否为定位型（文件/符号在哪）——broad regime 的否决闸。
pub fn is_locator_query(query: &str) -> bool {
    let q = query.to_lowercase();
    LOCATOR_MARKERS.iter().any(|m| q.contains(m))
}

/// 生态约定清单/配置/全局样式文件名（BCE manifestNames）。basename 小写精确匹配。
const MANIFEST_NAMES: [&str; 25] = [
    "package.json",
    "go.mod",
    "cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "composer.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "gemfile",
    "mix.exs",
    "makefile",
    "dockerfile",
    "docker-compose.yml",
    "compose.yaml",
    "compose.yml",
    "readme.md",
    "readme",
    // Spring 运行时配置与启动入口：服务通信/超时/注册中心类架构问题的应答处
    "application.yml",
    "application.yaml",
    "application.properties",
    "bootstrap.yml",
    "bootstrap.yaml",
    "bootstrap.properties",
];

/// 路径是否命中结构先验（BCE manifestBoost）。只认 basename，不认目录——
/// 先验针对的是「生态约定的文件名」，项目自身知识不进表。
pub fn is_manifest_path(path: &str) -> bool {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_lowercase();
    if MANIFEST_NAMES.contains(&base.as_str()) {
        return true;
    }
    // XxxApplication.java 携带框架装配注解（@EnableFeignClients 等），架构自陈
    if base.ends_with("application.java") {
        return true;
    }
    if base.contains(".config.") || base.starts_with("tsconfig") {
        return true;
    }
    // 全局样式表：主题/整体风格类问题的应答处
    if let Some(ext) = base
        .rsplit_once('.')
        .filter(|(_, e)| matches!(*e, "css" | "less" | "scss" | "sass" | "styl"))
    {
        let stem = ext.0;
        if matches!(
            stem,
            "main" | "global" | "app" | "index" | "style" | "styles" | "theme" | "variables"
        ) {
            return true;
        }
    }
    false
}

/// manifest prior 候选列表：从多路召回结果里收集命中清单表的 chunk，去重后
/// 按（路径深度，路径字典序）排列，每个 basename 最多 2 席。
///
/// 深度排序是先验的语义本身：架构问题问的是**项目**的清单，monorepo 里
/// examples/* 的子项目清单（flask 实测 5 个 pyproject.toml 同名共存）不该与
/// 根清单同权重；basename 限 2 席与 DupGuard 的防挤占哲学一致——同名清单
/// 再相似也是多个候选，全拉进来就是把窗口让给目录形状而不是内容。
pub fn manifest_prior_list(result_lists: &[Vec<SearchHit>]) -> Vec<SearchHit> {
    let mut seen: HashSet<(String, String, u32, u32, String)> = HashSet::new();
    let mut picked: Vec<SearchHit> = Vec::new();
    for hits in result_lists {
        for hit in hits {
            if !is_manifest_path(&hit.path) {
                continue;
            }
            if seen.insert(search_hit_key(hit)) {
                picked.push(hit.clone());
            }
        }
    }
    picked.sort_by(|a, b| {
        path_depth(&a.path)
            .cmp(&path_depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    // 同 basename 限 2 席（排序后线性扫描，保留最浅的两个）
    let mut per_basename: HashMap<String, usize> = HashMap::new();
    picked.retain(|hit| {
        let base = hit
            .path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&hit.path)
            .to_lowercase();
        let count = per_basename.entry(base).or_insert(0);
        *count += 1;
        *count <= 2
    });
    picked
}

/// 路径深度 = 路径段数（根文件为 1）。
fn path_depth(path: &str) -> usize {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).count()
}

/// 查询词提取（BCE tokens）：小写化、字母/数字/_/- 之外视作分隔、按 _/- 再拆、
/// 丢单字符、去重。snake_case/camelCase 拆词让骨架化按代码命名习惯命中行。
pub fn skeleton_terms(query: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(query.len());
    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            normalized.extend(ch.to_lowercase());
        } else {
            normalized.push(' ');
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in normalized.split_whitespace() {
        for piece in part.split(['_', '-']) {
            if piece.chars().count() > 1 && seen.insert(piece.to_string()) {
                out.push(piece.to_string());
            }
        }
    }
    out
}

/// 骨架化（BCE skeletonize）：超过 SKELETON_MIN_LINES 的摘录压缩为
/// 「头 SKELETON_HEAD_LINES 行 + 查询词命中行」，≥3 行的省略段收拢为一行
/// 标记 `... (N lines omitted, read path:start-end)`——标记引用真实行号区间，
/// agent 据此跟进读取。≤2 行的空洞原样保留（不值得为它付标记噪声）。
/// 短摘录原样透传。**Path/Lines 头与逐行行号真实性由 formatter 配合
/// [`elided_run`] 重同步保证。**
pub fn skeletonize(path: &str, start_line: u32, content: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() <= SKELETON_MIN_LINES {
        return content.to_string();
    }
    let mut keep = vec![false; lines.len()];
    for slot in keep.iter_mut().take(SKELETON_HEAD_LINES) {
        *slot = true;
    }
    let mut kept = 0usize;
    for i in SKELETON_HEAD_LINES..lines.len() {
        if kept >= SKELETON_TERM_KEEPS {
            break;
        }
        let lower = lines[i].to_lowercase();
        if terms
            .iter()
            .any(|t| !t.is_empty() && lower.contains(t.as_str()))
        {
            keep[i] = true;
            kept += 1;
        }
    }
    let mut out = String::with_capacity(content.len() / 2);
    let mut i = 0usize;
    while i < lines.len() {
        if keep[i] {
            out.push_str(lines[i]);
            out.push('\n');
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && !keep[j] {
            j += 1;
        }
        if j - i <= 2 {
            for line in &lines[i..j] {
                out.push_str(line);
                out.push('\n');
            }
        } else {
            out.push_str(&format!(
                "... ({} lines omitted, read {}:{}-{})\n",
                j - i,
                path,
                start_line as usize + i,
                start_line as usize + j - 1
            ));
        }
        i = j;
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// 识别本模块生成的省略标记行并解出真实行号区间（formatter 行号重同步用）。
/// 标记格式：`... ({N} lines omitted, read {path}:{a}-{b})`，N = b-a+1 自洽。
/// 锚定 section 自身的 path：内容行里恰好出现同一路径且算术自洽的标记格式
/// 概率可忽略，不同路径的引用行不识别——误报防护靠路径自洽而非转义。
pub fn elided_run(line: &str, path: &str) -> Option<(usize, usize, usize)> {
    let inner = line.strip_prefix("... (")?.strip_suffix(')')?;
    let (count_str, citation) = inner.split_once(" lines omitted, read ")?;
    if count_str.is_empty() || !count_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (cited_path, range) = citation.rsplit_once(':')?;
    if cited_path != path {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    let (first, last, omitted): (usize, usize, usize) = (
        start.parse().ok()?,
        end.parse().ok()?,
        count_str.parse().ok()?,
    );
    if last < first || omitted != last - first + 1 {
        return None;
    }
    Some((omitted, first, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, start: u32, score: f32) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}-{start}"),
            path: path.into(),
            content: "x".into(),
            score,
            content_hash: String::new(),
            start_line: start,
            end_line: start,
        }
    }

    #[test]
    fn structure_terms_fire_bilingually() {
        assert!(query_wants_structure(
            "sansio 抽象层和实现层怎么分工，架构是怎样的"
        ));
        assert!(query_wants_structure("how do services communicate"));
        assert!(query_wants_structure("项目用了什么技术栈"));
        assert!(query_wants_structure("frontend styling approach"));
    }

    #[test]
    fn bare_config_terms_deliberately_absent() {
        // BCE 踩过的坑：裸「配置」会把定位型查询（路由配置在哪）也拉进 broad
        assert!(!query_wants_structure("路由配置在哪里"));
        assert!(!query_wants_structure("router config file location"));
        assert!(!query_wants_structure("登录功能是怎么实现的"));
    }

    #[test]
    fn manifest_table_matches_ecosystem_conventions() {
        for path in [
            "pyproject.toml",
            "package.json",
            "Dockerfile",
            "deploy/docker-compose.yml",
            "src/FooApplication.java",
            "vite.config.ts",
            "tsconfig.json",
            "src/styles/theme.css",
            "app/less/global.less",
        ] {
            assert!(is_manifest_path(path), "{path} should be manifest");
        }
        for path in [
            "src/main.rs",
            "src/config.rs",
            "src/theme.rs",
            "docs/architecture.md",
            "application.go",
        ] {
            assert!(!is_manifest_path(path), "{path} should not be manifest");
        }
    }

    #[test]
    fn manifest_prior_dedups_and_prefers_shallow_paths() {
        let lists = vec![
            vec![hit("b/app.py", 1, 0.9), hit("pyproject.toml", 1, 0.5)],
            vec![
                hit("pyproject.toml", 1, 0.4), // 同 chunk 重复：去重
                hit("examples/celery/pyproject.toml", 1, 0.3),
                hit("src/main.rs", 1, 0.8), // 非清单：不进列表
                hit("examples/js/pyproject.toml", 1, 0.2),
                hit("examples/tutorial/pyproject.toml", 1, 0.1),
            ],
        ];
        let prior = manifest_prior_list(&lists);
        let paths: Vec<&str> = prior.iter().map(|h| h.path.as_str()).collect();
        // 根清单最浅排最前；同名子项目清单限 2 席（flask Q05 教训：5 个
        // pyproject.toml 全进窗口会把答案挤出头部）——4 个去重后只剩
        // 根 + 最浅的一个 example
        assert_eq!(paths, ["pyproject.toml", "examples/celery/pyproject.toml"]);
    }

    #[test]
    fn locator_queries_vetoed_from_broad() {
        // flask Q05：「环境变量」命中词表，但这是定位型查询（答案有明确目标）
        assert!(is_locator_query(
            "从带前缀的环境变量批量加载配置（FLASK_ 开头变量）的实现代码在哪里？"
        ));
        assert!(is_locator_query(
            "`url_for` 反向构建 URL 的函数定义在哪个文件？"
        ));
        assert!(is_locator_query("which file defines the router middleware"));
        // 真探索型查询不误伤
        assert!(!is_locator_query("项目的整体架构是怎样的"));
        assert!(!is_locator_query("how do the services communicate"));
        assert!(!is_locator_query("sansio 抽象层和完整实现层是怎么分工的"));
    }

    #[test]
    fn skeleton_terms_split_and_dedup() {
        // snake/camel 拆词、单字符丢弃、跨来源去重；CJK 连写按整段保留
        let terms = skeleton_terms("认证系统 Auth_Service 如何设计 auth-flow？a");
        assert_eq!(
            terms,
            ["认证系统", "auth", "service", "如何设计", "flow"]
                .map(String::from)
                .to_vec()
        );
    }

    #[test]
    fn skeleton_short_content_passes_through() {
        let content = "fn main() {}\n";
        assert_eq!(skeletonize("a.rs", 1, content, &["main".into()]), content);
    }

    #[test]
    fn skeleton_keeps_head_and_term_lines_with_true_ranges() {
        let mut lines: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        lines[19] = "handle_auth_request()".into(); // 第 20 行命中查询词 auth
        let content = lines.join("\n");
        let out = skeletonize("src/api.rs", 100, &content, &["auth".into()]);
        let out_lines: Vec<&str> = out.split('\n').collect();
        // 头 12 行 + 1 标记 + 命中行 + 尾部标记 = 15 行
        assert_eq!(out_lines.len(), 15);
        assert!(out_lines[11].starts_with("line 12"));
        assert_eq!(
            out_lines[12],
            "... (7 lines omitted, read src/api.rs:112-118)"
        );
        assert!(out_lines[13].contains("handle_auth_request"));
        assert_eq!(
            out_lines[14],
            "... (20 lines omitted, read src/api.rs:120-139)"
        );
        // 尾部无截断残留换行
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn skeleton_small_gaps_kept_verbatim() {
        // ≤2 行的空洞不收拢（标记噪声大于收益）
        let mut lines: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        lines[15] = "alpha()".into();
        lines[19] = "beta()".into();
        let content = lines.join("\n");
        let out = skeletonize("a.rs", 1, &content, &["alpha".into(), "beta".into()]);
        // 头 12 行后：空洞 13-15（3 行，收拢）→ alpha（16）→ 空洞 17-19（3 行，收拢）
        // → beta（20）→ 尾部 21-40（20 行，收拢）
        assert!(!out.contains("line 14\n"), "3-line gaps must collapse");
        assert!(out.contains("... (3 lines omitted, read a.rs:13-15)"));
        assert!(out.contains("alpha()"));
        assert!(out.contains("... (3 lines omitted, read a.rs:17-19)"));
        assert!(out.contains("beta()"));
        assert!(out.contains("... (20 lines omitted, read a.rs:21-40)"));
    }

    #[test]
    fn elided_run_parses_own_marker_only() {
        let marker = "... (8 lines omitted, read src/api.rs:112-119)";
        assert_eq!(elided_run(marker, "src/api.rs"), Some((8, 112, 119)));
        // 引用路径不同：不是本 section 的标记（内容行误撞防护）
        assert_eq!(elided_run(marker, "src/other.rs"), None);
        // 普通内容行
        assert_eq!(elided_run("x = call(...)  # note", "src/api.rs"), None);
        // 行数与区间不自洽：拒绝
        assert_eq!(elided_run("... (9 lines omitted, read a:1-8)", "a"), None);
    }
}
