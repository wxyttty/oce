//! 索引准入规则。与 Python `domain/services/source_filter.py` 对齐。


const IGNORED_DIRECTORY_NAMES: [&str; 29] = [
    ".cache", ".eggs", ".git", ".gradle", ".hg", ".idea", ".mypy_cache", ".next", ".nuxt",
    ".output", ".pytest_cache", ".ruff_cache", ".svelte-kit", ".svn", ".tox", ".turbo",
    ".venv", ".vscode", "bower_components", "build", "coverage", "dist", "generated",
    "node_modules", "site-packages", "target", "vendor", "venv", "__pycache__",
];

fn ignored_file_suffixes() -> &'static [&'static str] {
    &[
        ".min.js", ".min.css", ".map", ".lock", ".log", ".tmp", ".bak", ".swp", ".jsonl",
        ".csv", ".tsv", ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".icns", ".webp",
        ".tiff", ".svg", ".mp3", ".mp4", ".wav", ".avi", ".mov", ".flac", ".ogg", ".webm",
        ".mkv", ".woff", ".woff2", ".ttf", ".otf", ".eot", ".pdf", ".doc", ".docx", ".xls",
        ".xlsx", ".ppt", ".pptx", ".zip", ".tar", ".gz", ".tgz", ".rar", ".7z", ".bz2",
        ".xz", ".pyc", ".pyo", ".class", ".o", ".obj", ".a", ".so", ".dll", ".dylib",
        ".exe", ".bin", ".wasm", ".sqlite", ".db",
    ]
}

/// 路径是否为依赖、生成产物或非源码内容。
pub fn is_ignored_source_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_lowercase();
    let parts: Vec<&str> = normalized.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() > 1 {
        for part in &parts[..parts.len() - 1] {
            // dist-prod / dist-dev 等 Vue 构建产物变体目录实测会稀释检索质量
            let is_dist_variant = *part == "dist" || part.starts_with("dist-") || part.starts_with("dist.");
            if IGNORED_DIRECTORY_NAMES.contains(part)
                || is_dist_variant
                || part.ends_with(".egg-info")
                || part.ends_with("-retrieval-eval")
            {
                return true;
            }
        }
    }
    ignored_file_suffixes().iter().any(|s| normalized.ends_with(s))
}

/// 与 Git 相同的低成本二进制信号：NUL 字节。
pub fn is_binary_source(content: &str) -> bool {
    content.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_dependency_dirs() {
        assert!(is_ignored_source_path("node_modules/pkg/index.js"));
        assert!(is_ignored_source_path("a/.git/config"));
        assert!(is_ignored_source_path("pkg.egg-info/x"));
        assert!(is_ignored_source_path("x/bundle.min.js"));
    }

    #[test]
    fn allows_source() {
        assert!(!is_ignored_source_path("src/main.rs"));
        assert!(!is_ignored_source_path("README.md"));
    }

    #[test]
    fn ignores_dist_variants() {
        // Vue 构建产物变体（dist-prod / dist.dev）实测混入索引稀释结果
        assert!(is_ignored_source_path("jcfx-admin/dist-prod/assets/app.css"));
        assert!(is_ignored_source_path("dist.dev/x.js"));
        assert!(!is_ignored_source_path("src/distribution/main.rs")); // 非构建目录不误伤
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary_source("a\0b"));
        assert!(!is_binary_source("normal"));
    }
}
