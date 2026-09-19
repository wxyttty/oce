//! 源码优先先验与置信度。与 Python `domain/services/retrieval.py` 中的
//! `source_priority_factor` / `path_query_priority_factor` 对齐。

use regex::Regex;
use std::sync::OnceLock;

/// 源码优先先验：文档/测试类路径乘性降权，普通源码 1.0。
pub fn source_priority_factor(path: &str) -> f32 {
    let p = path.replace('\\', "/").to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let stem = name.split('.').next().unwrap_or(name);

    // 法律文件最重降权
    if matches!(stem, "license" | "notice" | "copying") {
        return 0.1;
    }
    // 主 README 显式不降权，多语言 README 降权
    if stem == "readme" {
        return 1.0;
    }
    if stem.starts_with("readme") {
        return 0.2;
    }
    // 文档目录 / 文档扩展
    if format!("/{p}").contains("/docs/")
        || p.ends_with(".md")
        || p.ends_with(".rst")
        || p.ends_with(".txt")
    {
        return 0.5;
    }
    if format!("/{p}").contains("/tests/")
        || name.starts_with("test_")
        || name == "conftest.py"
        || name.contains(".test.")
        || name.contains(".spec.")
    {
        return 0.6;
    }
    if matches!(
        name,
        "index.ts" | "index.tsx" | "index.js" | "index.jsx" | "types.ts"
    ) {
        return 0.85;
    }
    1.0
}

/// 路径类查询的优先级因子：恒为 1.0（文档中立）。
///
/// 「XX 文件在哪里」类查询中，任何文件类型都可能是目标；排序完全交由
/// 路径 boost + 内容分数 + rerank 决定。
pub fn path_query_priority_factor(_path: &str) -> f32 {
    1.0
}

// ── 元目录降权（semble_rs M2 / RETRIEVAL_META_DIR_PENALTY_ENABLED）──
// .github 等 CI/模板基础设施对普通代码问题是纯噪声（flask 基准里 .github/*.yaml
// 频繁挤占 top-10），套用测试文件降权 ×0.5；查询明确点名该基础设施时豁免——
// 那时它就是答案（Q53「GitHub Actions 工作流文件是哪个」）。仅主检索路生效，
// 路径增强路保持文档中立（任何文件都可能是定位目标）。

/// 元目录降权幅度（与测试文件 0.6 同族的保守档，semble 实测值）。
const META_DIR_PENALTY: f32 = 0.5;

/// 路径是否位于仓库元基础设施目录（CI 托管方 + 模板/机器人目录段）。
pub fn is_meta_dir_path(path: &str) -> bool {
    path.replace('\\', "/")
        .to_lowercase()
        .split('/')
        .any(|segment| {
            matches!(
                segment,
                ".github"
                    | ".gitlab"
                    | ".gitea"
                    | ".gitee"
                    | ".circleci"
                    | ".teamcity"
                    | ".drone"
                    | ".buildkite"
            )
        })
}

/// 查询是否明确点名 CI/工作流基础设施（豁免降权）。
pub fn is_meta_intent_query(query: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(github|gitlab|action|workflow|\bci\b|pipeline|issue.?template|labeler|dependabot|pull.?request|工作流|流水线|持续集成)").unwrap()
    })
    .is_match(query)
}

/// 元目录因子：启用且路径在元目录、查询又没有点名基础设施时 ×0.5，否则 1.0。
pub fn meta_dir_factor(enabled: bool, query: &str, path: &str) -> f32 {
    if !enabled || !is_meta_dir_path(path) || is_meta_intent_query(query) {
        return 1.0;
    }
    META_DIR_PENALTY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_files_heaviest_penalty() {
        assert_eq!(source_priority_factor("LICENSE"), 0.1);
        assert_eq!(source_priority_factor("docs/NOTICE.md"), 0.1);
    }

    #[test]
    fn readme_variants() {
        assert_eq!(source_priority_factor("README.md"), 1.0);
        assert_eq!(source_priority_factor("README.zh.md"), 1.0); // stem == "readme"
        assert_eq!(source_priority_factor("README_CN"), 0.2);
    }

    #[test]
    fn docs_and_tests_downweighted() {
        assert_eq!(source_priority_factor("docs/guide.md"), 0.5);
        assert_eq!(source_priority_factor("a/b.md"), 0.5);
        assert_eq!(source_priority_factor("tests/test_x.py"), 0.6);
        assert_eq!(source_priority_factor("foo.test.ts"), 0.6);
        assert_eq!(source_priority_factor("src/index.ts"), 0.85);
        assert_eq!(source_priority_factor("src/main.rs"), 1.0);
    }

    #[test]
    fn path_queries_neutral() {
        assert_eq!(path_query_priority_factor("docs/changes.rst"), 1.0);
    }

    #[test]
    fn meta_dir_detection_is_segment_anchored() {
        assert!(is_meta_dir_path(".github/workflows/tests.yaml"));
        assert!(is_meta_dir_path("src/.gitlab/ci.yml"));
        assert!(is_meta_dir_path(".circleci/config.yml"));
        // 目录段锚定：同名前缀不误伤
        assert!(!is_meta_dir_path("src/mygithub/util.rs"));
        assert!(!is_meta_dir_path("a/github-actions.yml"));
        assert!(!is_meta_dir_path("src/main.rs"));
    }

    #[test]
    fn meta_intent_exempt_and_plain_queries_not() {
        // Q53 类：明确点名工作流基础设施 → 豁免
        assert!(is_meta_intent_query(
            "运行单元测试套件的 GitHub Actions 工作流文件是哪个？"
        ));
        assert!(is_meta_intent_query("ci pipeline 在哪里配置"));
        assert!(is_meta_intent_query("dependabot 的配置"));
        // 普通代码问题 → 不豁免
        assert!(!is_meta_intent_query(
            "Flask 应用的所有默认配置项定义在哪里"
        ));
        assert!(!is_meta_intent_query("登录功能是怎么实现的"));
    }

    #[test]
    fn meta_factor_gated_by_flag_intent_and_path() {
        // 关闭：恒 1.0
        assert_eq!(meta_dir_factor(false, "任意", ".github/x.yml"), 1.0);
        // 开启 + 元目录 + 非 meta 意图：降权
        assert_eq!(
            meta_dir_factor(
                true,
                "应用的默认配置项在哪里",
                ".github/ISSUE_TEMPLATE/bug.md"
            ),
            0.5
        );
        // 开启 + 元目录 + meta 意图：豁免
        assert_eq!(
            meta_dir_factor(
                true,
                "github workflow 文件在哪",
                ".github/workflows/ci.yaml"
            ),
            1.0
        );
        // 开启 + 非元目录：1.0
        assert_eq!(meta_dir_factor(true, "任意查询", "src/main.rs"), 1.0);
    }
}
