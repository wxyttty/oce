//! 源码优先先验与置信度。与 Python `domain/services/retrieval.py` 中的
//! `source_priority_factor` / `path_query_priority_factor` 对齐。

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
    if format!("/{p}").contains("/docs/") || p.ends_with(".md") || p.ends_with(".rst") || p.ends_with(".txt") {
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
}
