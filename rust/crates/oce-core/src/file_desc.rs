//! 规则层文件描述（移植 BCE `internal/indexer/filedesc.go`）。
//!
//! 已知清单/配置/入口文件的内容（TOML 键值、依赖列表、CSS 规则）与
//! 自然语言查询零词法重叠，dense 嵌入也够不着——「Python 包配置文件在
//! 哪里」匹配不上 `[project]\nname = "demo"`。这类文件的桥接句由文件名
//! 完全决定（生态约定），无需模型：`pyproject.toml` → "Python project
//! manifest: build system, dependencies and tool configuration"。
//!
//! 只做规则层（零 LLM 成本）；BCE 的 LLM 摘要层（summarize.go）明确不做
//! （个人模式 API 成本敏感，增益未证实）。描述注入 embedding_text（索引时）
//! 与 rerank 文档（查询时）两处，由 `RETRIEVAL_FILE_DESC_ENABLED` 门控
//! （默认关，A/B 实验开关；开启触发模型指纹 etext=v2 强制重建）。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 桥接句长度上限（BCE summaryMaxChars 同值）。
const MAX_DESC_CHARS: usize = 240;

/// 已知清单文件名（小写）→ 英文桥接句。保持英文：与嵌入/重排文档语言一致。
fn desc_by_name() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            (
                "package.json",
                "npm package manifest: project metadata, dependencies and scripts",
            ),
            (
                "package-lock.json",
                "npm lockfile: resolved dependency versions",
            ),
            (
                "go.mod",
                "Go module definition: module path and dependency requirements",
            ),
            ("go.sum", "Go module checksums for dependency verification"),
            (
                "cargo.toml",
                "Rust crate manifest: package metadata and dependencies",
            ),
            (
                "pyproject.toml",
                "Python project manifest: build system, dependencies and tool configuration",
            ),
            ("requirements.txt", "Python dependency list"),
            (
                "composer.json",
                "PHP Composer manifest: package metadata and dependencies",
            ),
            (
                "pom.xml",
                "Maven project manifest: modules, dependencies and build plugins",
            ),
            (
                "build.gradle",
                "Gradle build script: dependencies and build configuration",
            ),
            (
                "build.gradle.kts",
                "Gradle Kotlin build script: dependencies and build configuration",
            ),
            ("settings.gradle", "Gradle settings: project modules"),
            ("gemfile", "Ruby gem dependency manifest"),
            ("mix.exs", "Elixir Mix project manifest"),
            ("makefile", "Make build and task automation targets"),
            ("justfile", "Just task runner recipes"),
            ("dockerfile", "container image build instructions"),
            (
                "docker-compose.yml",
                "multi-container orchestration configuration",
            ),
            (
                "compose.yaml",
                "multi-container orchestration configuration",
            ),
            ("compose.yml", "multi-container orchestration configuration"),
            (
                ".env.example",
                "environment variable template listing required configuration",
            ),
            ("nginx.conf", "Nginx web server configuration"),
            ("caddyfile", "Caddy web server configuration"),
            (
                "application.yml",
                "application runtime configuration (profiles, service wiring, timeouts)",
            ),
            (
                "application.yaml",
                "application runtime configuration (profiles, service wiring, timeouts)",
            ),
            (
                "application.properties",
                "application runtime configuration (profiles, service wiring, timeouts)",
            ),
            (
                "bootstrap.yml",
                "application bootstrap configuration (config server, registry)",
            ),
            (
                "bootstrap.yaml",
                "application bootstrap configuration (config server, registry)",
            ),
            (
                "bootstrap.properties",
                "application bootstrap configuration (config server, registry)",
            ),
        ])
    })
}

/// `<tool>.config.<ext>` 构建工具配置识别（stem → 桥接句）。
fn desc_by_config_stem() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("vite", "Vite build and dev server configuration"),
            ("webpack", "Webpack bundler configuration"),
            ("rollup", "Rollup bundler configuration"),
            ("babel", "Babel transpiler configuration"),
            ("jest", "Jest test runner configuration"),
            ("vitest", "Vitest test runner configuration"),
            (
                "tailwind",
                "Tailwind CSS design token and theme configuration",
            ),
            ("postcss", "PostCSS plugin configuration"),
            ("eslint", "ESLint lint rule configuration"),
            ("prettier", "Prettier code formatting configuration"),
            ("next", "Next.js framework configuration"),
            ("nuxt", "Nuxt framework configuration"),
            ("svelte", "Svelte framework configuration"),
            ("playwright", "Playwright end-to-end test configuration"),
        ])
    })
}

fn basename_lower(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_lowercase()
}

/// 文件的确定性规则描述；无规则命中返回空串。
///
/// `content` 只在 README 分支使用（首段是作者写好的摘要）。索引侧对
/// README 传入 staging 全文（同文件所有 chunk 共享同一描述，BCE
/// ruleSummaries 语义）；rerank 侧拿不到全文，传空串走静态兜底。
pub fn file_description(path: &str, content: &str) -> String {
    let normalized = path.replace('\\', "/");
    let base = basename_lower(path);

    if let Some(desc) = desc_by_name().get(base.as_str()) {
        return (*desc).to_string();
    }
    if base.starts_with("readme") {
        let paragraph = first_paragraph(content);
        if !paragraph.is_empty() {
            return paragraph;
        }
        return "project README: overview and usage documentation".into();
    }
    if base.starts_with("changelog") {
        return "project changelog: release history".into();
    }
    if base.starts_with("tsconfig") {
        return "TypeScript compiler configuration".into();
    }
    if normalized.contains(".github/workflows/") {
        return "CI workflow definition (GitHub Actions)".into();
    }
    if base == ".gitlab-ci.yml" {
        return "CI pipeline definition (GitLab CI)".into();
    }
    if let Some(i) = base.find(".config.") {
        if i > 0 {
            if let Some(desc) = desc_by_config_stem().get(&base[..i]) {
                return (*desc).to_string();
            }
            return "build or tool configuration file".into();
        }
    }
    let ext = base.rfind('.').map(|i| &base[i..]).unwrap_or("");
    if matches!(ext, ".css" | ".less" | ".scss" | ".sass" | ".styl") {
        let stem = &base[..base.len() - ext.len()];
        if matches!(
            stem,
            "main" | "global" | "app" | "index" | "style" | "styles" | "theme" | "variables"
        ) {
            return "global stylesheet: layout, theme colors and typography rules".into();
        }
    }
    if ext == ".sql"
        && (base.contains("schema") || base.contains("migration") || base.contains("init"))
    {
        return "database schema or migration script".into();
    }
    // 入口点回答「应用从哪启动」；main.* 是跨生态约定。index.* 在 JS 生态
    // 太泛（每个目录都有），不列入——BCE 同款取舍。
    if matches!(
        base.as_str(),
        "main.go"
            | "main.py"
            | "main.rs"
            | "main.ts"
            | "main.tsx"
            | "main.js"
            | "main.jsx"
            | "manage.py"
            | "app.py"
    ) {
        return "application entry point: startup and wiring".into();
    }
    String::new()
}

/// 描述是否依赖文件全文（README 首段）。索引侧据此决定是否取 staging。
pub fn description_needs_content(path: &str) -> bool {
    basename_lower(path).starts_with("readme")
}

/// 提取 markdown 文档的首个散文段：标题/徽章/图片/HTML/引用行跳过，
/// 连续文本行拼接，按字符截断到 MAX_DESC_CHARS（不撕裂多字节序列）。
pub fn first_paragraph(content: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in content.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        let structural = line.starts_with('#')
            || line.starts_with('<')
            || line.starts_with("![")
            || line.starts_with("[!")
            || line.starts_with("---")
            || line.starts_with("```")
            || line.starts_with('>');
        if structural {
            if !lines.is_empty() {
                break;
            }
            continue;
        }
        lines.push(line);
    }
    let joined = lines.join(" ");
    joined
        .chars()
        .take(MAX_DESC_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_filenames_get_bridge_sentences() {
        assert_eq!(
            file_description("pyproject.toml", ""),
            "Python project manifest: build system, dependencies and tool configuration"
        );
        // 文件名大小写不敏感
        assert_eq!(
            file_description("Cargo.TOML", ""),
            "Rust crate manifest: package metadata and dependencies"
        );
        assert_eq!(
            file_description("Dockerfile", ""),
            "container image build instructions"
        );
        assert_eq!(
            file_description(".env.example", ""),
            "environment variable template listing required configuration"
        );
    }

    #[test]
    fn plain_source_files_have_no_description() {
        assert!(file_description("src/lib.rs", "").is_empty());
        assert!(file_description("lib/util.py", "").is_empty());
    }

    #[test]
    fn entry_point_convention() {
        assert_eq!(
            file_description("src/main.py", ""),
            "application entry point: startup and wiring"
        );
        assert_eq!(
            file_description("manage.py", ""),
            "application entry point: startup and wiring"
        );
    }

    #[test]
    fn readme_uses_first_paragraph_with_fallback() {
        let content =
            "# Demo\n\n[![ci](badge)]\n\nA demo service.\nIt refreshes tokens.\n\n## Usage\n\nRun it.\n";
        assert_eq!(
            file_description("README.md", content),
            "A demo service. It refreshes tokens."
        );
        // 无正文可提取时走静态兜底
        assert_eq!(
            file_description("docs/README.zh.md", ""),
            "project README: overview and usage documentation"
        );
    }

    #[test]
    fn config_stem_ci_and_compiler_rules() {
        assert_eq!(
            file_description("vite.config.ts", ""),
            "Vite build and dev server configuration"
        );
        assert_eq!(
            file_description("foo.config.js", ""),
            "build or tool configuration file"
        );
        assert_eq!(
            file_description(".github/workflows/tests.yml", ""),
            "CI workflow definition (GitHub Actions)"
        );
        assert_eq!(
            file_description(".gitlab-ci.yml", ""),
            "CI pipeline definition (GitLab CI)"
        );
        assert_eq!(
            file_description("tsconfig.json", ""),
            "TypeScript compiler configuration"
        );
    }

    #[test]
    fn stylesheet_and_sql_rules() {
        assert_eq!(
            file_description("src/styles/theme.css", ""),
            "global stylesheet: layout, theme colors and typography rules"
        );
        // 非全局样式表主干不误伤
        assert!(file_description("src/component.css", "").is_empty());
        assert_eq!(
            file_description("migrations/0001_init.sql", ""),
            "database schema or migration script"
        );
    }

    #[test]
    fn description_content_requirement() {
        assert!(description_needs_content("README.md"));
        assert!(description_needs_content("readme.zh-CN.md"));
        assert!(!description_needs_content("pyproject.toml"));
        assert!(!description_needs_content("src/main.rs"));
    }

    #[test]
    fn first_paragraph_skips_structure_and_caps_length() {
        assert_eq!(
            first_paragraph("<!-- comment -->\n# H\n\n\nfirst\nsecond\n\nthird"),
            "first second"
        );
        let long = "x".repeat(500);
        assert!(first_paragraph(&long).chars().count() <= MAX_DESC_CHARS);
        assert!(first_paragraph("# only headings\n\n## more").is_empty());
    }
}
