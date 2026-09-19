//! 路径文档生成器。与 Python `domain/services/path_document_builder.py` 对齐。
//!
//! 约束：只使用路径自身的结构信息（目录/文件名/扩展名分词）加上
//! 「扩展名 → 类型」这一层通用语义；不注入具体文件名或单一技术栈先验。

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

fn token_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[/\\._\-]+").unwrap())
}

fn extension_semantics() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(".rs", "Rust source code 源码");
        m.insert(".py", "Python source code 源码");
        m.insert(".ts", "TypeScript source code 源码");
        m.insert(".tsx", "TypeScript React JSX component 组件 源码");
        m.insert(".js", "JavaScript source code 源码");
        m.insert(".jsx", "JavaScript React JSX component 组件 源码");
        m.insert(".vue", "Vue component 组件 源码");
        m.insert(".go", "Go source code 源码");
        m.insert(".java", "Java source code 源码");
        m.insert(".kt", "Kotlin source code 源码");
        m.insert(".rb", "Ruby source code 源码");
        m.insert(".php", "PHP source code 源码");
        m.insert(".cs", "C# source code 源码");
        m.insert(".cpp", "C++ source code 源码");
        m.insert(".c", "C source code 源码");
        m.insert(".h", "C/C++ header 头文件");
        m.insert(".json", "JSON configuration data 配置 数据");
        m.insert(".toml", "TOML configuration 配置");
        m.insert(".yaml", "YAML configuration 配置");
        m.insert(".yml", "YAML configuration 配置");
        m.insert(".ini", "INI configuration 配置");
        m.insert(".md", "Markdown documentation 文档");
        m.insert(".rst", "reStructuredText documentation 文档");
        m.insert(".sql", "SQL database schema query 数据库 查询");
        m
    })
}

/// camelCase 边界切分：小写/数字后跟大写处断开（等价 (?<=[a-z0-9])(?=[A-Z])）。
fn split_camel(part: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut prev_lower_digit = false;
    for (i, ch) in part.char_indices() {
        let is_upper = ch.is_uppercase();
        if is_upper && prev_lower_digit && i > start {
            pieces.push(&part[start..i]);
            start = i;
        }
        prev_lower_digit = ch.is_lowercase() || ch.is_numeric();
    }
    pieces.push(&part[start..]);
    pieces
}

/// 把路径拆成小写 token（目录、文件名片段、camelCase 边界），保序去重。
fn tokenize(path: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for part in token_split_re().split(path) {
        if part.is_empty() {
            continue;
        }
        for piece in split_camel(part) {
            let piece = piece.trim().to_lowercase();
            if !piece.is_empty() && !tokens.contains(&piece) {
                tokens.push(piece);
            }
        }
    }
    tokens
}

/// 构建路径索引的 embedding 文本：完整路径 + 文件名 + 文件名主干 + 结构化 token
/// + 扩展名类型语义。全部来自路径本身，跨仓库可泛化。
pub fn build_path_document(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let filename = normalized.rsplit('/').next().unwrap_or(&normalized);
    let stem = filename.split('.').next().unwrap_or(filename);

    let mut parts: Vec<String> = vec![normalized.clone(), filename.to_string()];
    if !stem.is_empty() && stem != filename {
        parts.push(stem.to_string());
    }
    parts.extend(tokenize(&normalized));

    for (ext, keywords) in extension_semantics() {
        if filename.ends_with(ext) {
            parts.push(keywords.to_string());
            break;
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut ordered: Vec<&str> = Vec::new();
    for part in &parts {
        if !part.is_empty() && seen.insert(part.as_str()) {
            ordered.push(part);
        }
    }
    ordered.join(" ")
}

/// 判断路径是否应被索引：排除依赖/构建目录与二进制、媒体等非文本文件。
pub fn is_indexable_path(path: &str) -> bool {
    const EXCLUDE_PATTERNS: [&str; 8] = [
        "node_modules/",
        ".git/",
        "dist/",
        "build/",
        "target/",
        "__pycache__/",
        ".pytest_cache/",
        ".venv/",
    ];
    const EXCLUDE_EXTENSIONS: [&str; 17] = [
        ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".woff", ".woff2", ".ttf", ".eot", ".zip",
        ".tar", ".gz", ".exe", ".dll", ".so", ".dylib",
    ];
    for pattern in EXCLUDE_PATTERNS {
        if path.contains(pattern) {
            return false;
        }
    }
    // Python 语义：命中排除扩展名 → 不可索引
    !EXCLUDE_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_generalizable_document() {
        let doc = build_path_document("src/auth/token_refresh.rs");
        assert!(doc.starts_with("src/auth/token_refresh.rs"));
        assert!(doc.contains("Rust source code 源码"));
        assert!(doc.contains("token"));
        assert!(doc.contains("refresh"));
    }

    #[test]
    fn camel_case_split() {
        let doc = build_path_document("src/UserProfile.vue");
        assert!(doc.contains("user"));
        assert!(doc.contains("profile"));
        assert!(doc.contains("Vue component 组件 源码"));
    }

    #[test]
    fn excludes_dependency_and_binary_paths() {
        assert!(!is_indexable_path("node_modules/foo/index.js"));
        assert!(!is_indexable_path("target/debug/x"));
        assert!(!is_indexable_path("img/logo.png"));
        assert!(is_indexable_path("src/main.rs"));
    }
}
