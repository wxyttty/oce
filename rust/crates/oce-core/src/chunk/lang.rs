//! 文件路径 → 切块语言标识映射。与 Python `domain/chunk/lang.py` 对齐。
//!
//! 标识由 [`LanguageChunkerRouter`](super::router::LanguageChunkerRouter) 分发给
//! 专用切块器；未命中者走 RecursiveChunker 兜底。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 扩展名（小写，含点）或特殊文件名 → 规范化语言标识。
fn ext_to_lang() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        // Python
        m.insert(".py", "python");
        m.insert(".pyi", "python");
        // Java
        m.insert(".java", "java");
        // JSP
        m.insert(".jsp", "jsp");
        m.insert(".jspx", "jsp");
        m.insert(".jspf", "jsp");
        m.insert(".tag", "jsp");
        m.insert(".tagx", "jsp");
        // C#
        m.insert(".cs", "csharp");
        // TypeScript / TSX
        m.insert(".ts", "typescript");
        m.insert(".mts", "typescript");
        m.insert(".cts", "typescript");
        m.insert(".tsx", "tsx");
        // JavaScript / JSX
        m.insert(".js", "javascript");
        m.insert(".mjs", "javascript");
        m.insert(".cjs", "javascript");
        m.insert(".jsx", "jsx");
        // C / C++
        m.insert(".c", "c");
        m.insert(".h", "c");
        m.insert(".cpp", "cpp");
        m.insert(".cxx", "cpp");
        m.insert(".cc", "cpp");
        m.insert(".hpp", "cpp");
        m.insert(".hxx", "cpp");
        // Go
        m.insert(".go", "go");
        // Rust
        m.insert(".rs", "rust");
        // Ruby
        m.insert(".rb", "ruby");
        // PHP
        m.insert(".php", "php");
        // Swift
        m.insert(".swift", "swift");
        // Kotlin
        m.insert(".kt", "kotlin");
        m.insert(".kts", "kotlin");
        // Scala
        m.insert(".scala", "scala");
        m.insert(".sc", "scala");
        // HTML / CSS
        m.insert(".html", "html");
        m.insert(".htm", "html");
        m.insert(".css", "css");
        // 数据格式
        m.insert(".json", "json");
        m.insert(".yaml", "yaml");
        m.insert(".yml", "yaml");
        m.insert(".toml", "toml");
        m.insert(".xml", "xml");
        // Markdown
        m.insert(".md", "markdown");
        m.insert(".markdown", "markdown");
        m.insert(".mdx", "markdown");
        // Shell
        m.insert(".sh", "bash");
        m.insert(".bash", "bash");
        m.insert(".zsh", "bash");
        // SQL
        m.insert(".sql", "sql");
        // Lua
        m.insert(".lua", "lua");
        // R
        m.insert(".r", "r");
        // Julia
        m.insert(".jl", "julia");
        // Haskell
        m.insert(".hs", "haskell");
        // Elixir
        m.insert(".ex", "elixir");
        m.insert(".exs", "elixir");
        // Erlang
        m.insert(".erl", "erlang");
        m.insert(".hrl", "erlang");
        // Clojure
        m.insert(".clj", "clojure");
        m.insert(".cljs", "clojure");
        m.insert(".cljc", "clojure");
        // OCaml
        m.insert(".ml", "ocaml");
        m.insert(".mli", "ocaml");
        // Zig
        m.insert(".zig", "zig");
        // Nim
        m.insert(".nim", "nim");
        // Dart
        m.insert(".dart", "dart");
        // Perl
        m.insert(".pl", "perl");
        m.insert(".pm", "perl");
        // Dockerfile / Makefile / CMake（basename 命中，键保留原始大小写）
        m.insert("Dockerfile", "dockerfile");
        m.insert("Makefile", "make");
        m.insert("makefile", "make");
        m.insert(".cmake", "cmake");
        // Vue / Svelte
        m.insert(".vue", "vue");
        m.insert(".svelte", "svelte");
        m
    })
}

/// 路由器可接受的已知语言集合（对应 Python `SUPPORTED_LANGUAGES`）。
pub fn supported_languages() -> &'static [&'static str] {
    static LANGS: OnceLock<Vec<&'static str>> = OnceLock::new();
    LANGS.get_or_init(|| {
        let mut langs: Vec<&'static str> = ext_to_lang().values().copied().collect();
        langs.sort_unstable();
        langs.dedup();
        langs
    })
}

/// 从路径检测切块语言；无法识别时返回 `None`（走 RecursiveChunker 兜底）。
///
/// 与 Python 版语义一致：先按 `os.path.splitext` 的扩展名（前导点不算扩展名），
/// 再按原始大小写的 basename 匹配（Dockerfile / Makefile / makefile）。
pub fn detect_language(path: &str) -> Option<&'static str> {
    let normalized = path.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower_basename = basename.to_lowercase();

    // splitext 语义：basename 内最后一个点且不是前导点才是扩展名
    if let Some(dot) = lower_basename.rfind('.') {
        if dot > 0 {
            let ext = &lower_basename[dot..];
            if let Some(lang) = ext_to_lang().get(ext) {
                return Some(lang);
            }
        }
    }

    // 无扩展名时按文件名匹配（Dockerfile, Makefile 等，大小写敏感与 Python 一致）
    ext_to_lang().get(basename).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect_language("a/b/c.py"), Some("python"));
        assert_eq!(detect_language("x.tsx"), Some("tsx"));
        assert_eq!(detect_language("y.vue"), Some("vue"));
        assert_eq!(detect_language("z.md"), Some("markdown"));
        assert_eq!(detect_language("w.rs"), Some("rust"));
        assert_eq!(detect_language("Dockerfile"), Some("dockerfile"));
        assert_eq!(detect_language("Makefile"), Some("make"));
        assert_eq!(detect_language("makefile"), Some("make"));
        assert_eq!(detect_language("a/noext"), None);
        assert_eq!(detect_language("dir.v2/file"), None);
        assert_eq!(detect_language("a/.gitignore"), None);
    }
}
