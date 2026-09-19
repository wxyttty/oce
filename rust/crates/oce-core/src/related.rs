//! Related symbols hints（借鉴 BCE `relate.go` 的 `relatedSymbolHints`）。
//!
//! 输出层追加：对最终选中的 hits，从其 content 提取标识符 token，查
//! `symbol_occurrences` 倒排找「被引用但定义不在窗口内」的符号，输出
//! `RelatedSymbol { name, kind, path }` 列表——agent 消费面的 grep leads。
//!
//! 关键设计（与 BCE 对齐的取舍）：
//! - **不动排序**：hints 只追加提示块，不改变结果集构成（这是它与
//!   relation expansion 的本质区别，后者风险高一档，暂缓）。
//! - **fanout 门控**：被超过 `FANOUT_MAX` 个文件引用的通用符号
//!   （String、Result、Config 之类）不进 hints——基础设施级符号的
//!   定义位置对探索没有信息量，纯噪声。
//! - **窗口内定义排除**：定义已出现在选中 hits 的路径里则跳过——
//!   hint 的价值在「没看到但存在」，已看到的不再提示。
//!
//! 评测兼容：formatter 只在 sections 之后追加 `<related_symbols>` 块，
//! 评测脚本只解析 `Path: ` 行，评分不受影响。

/// hints 上限（BCE maxRelatedHints 同值）。
pub const MAX_RELATED_HINTS: usize = 8;
/// 标识符 token 最小长度（BCE minIdentLen 同值）。
const MIN_IDENT_LEN: usize = 3;
/// fanout 门控：定义所在文件数超过此值的通用符号不进 hints
/// （BCE callerFanoutMax=15 同源，取值略保守）。
const FANOUT_MAX: usize = 15;

/// 停用词：docstring/散文里被宽松定义正则误提取的英文常见词 + 语言关键字。
/// symbol_occurrences 的提取正则（Python 版同款）会命中 docstring 里行首的
/// "Subclass and has..."；这些伪定义在 exact 召回里被分数体系淹没，但在
/// hints 里直接可见，必须在 hints 侧过滤（不动索引语义——提取器还有别的
/// 消费方，改提取器会触发全量重索引）。
const STOPWORDS: &[&str] = &[
    // 英文常见词（docstring/注释高频）
    "and",
    "or",
    "not",
    "if",
    "else",
    "for",
    "while",
    "with",
    "from",
    "import",
    "class",
    "def",
    "return",
    "yield",
    "raise",
    "assert",
    "async",
    "await",
    "lambda",
    "true",
    "false",
    "none",
    "null",
    "undefined",
    "self",
    "this",
    "super",
    "cls",
    "the",
    "a",
    "an",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "than",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "new",
    "delete",
    "typeof",
    "instanceof",
    "void",
    "static",
    "public",
    "private",
    "test",
    "spec",
    "default",
    "error",
    "value",
    "name",
    "type",
    "data",
    "args",
    // 解构赋值伪提取高发词
    "added",
    "removed",
    "active",
    "enabled",
    "disabled",
    "set",
    "get",
    "has",
    "app",
    "config",
    "options",
    "params",
    "props",
    "state",
    "item",
    "items",
    // Rust/TS 生态高频短名：定义处少（fanout 门控拦不住）但引用极广，
    // 定义位置对探索无信息量
    "err",
    "ok",
    "some",
    "none",
    "result",
    "option",
    "vec",
    "box",
    "rc",
    "arc",
    "str",
    "string",
    "int",
    "uint",
    "f32",
    "f64",
    "i32",
    "u32",
    "u64",
    "usize",
    "bool",
    "char",
    "byte",
    "bytes",
    "map",
    "list",
    "array",
    "command",
    "cwd",
    "format",
    "exists",
    "read",
    "write",
    "open",
    "close",
    "send",
    "recv",
    "call",
    "run",
    "init",
    "main",
];

fn is_stopword(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

/// 单条 related symbol 提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedSymbol {
    pub name: String,
    /// 'endpoint' | 'definition'（symbol_occurrences.kind）
    pub kind: String,
    pub path: String,
}

/// 从源文本提取标识符形态的 token（BCE identTokens 移植）。
/// 与已知符号名精确匹配，语言关键字无害；长度下限滤掉单字母噪声。
pub fn ident_tokens(content: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in content.char_indices() {
        let is_ident = ch == '_' || ch.is_ascii_alphanumeric();
        if is_ident {
            if start.is_none() {
                start = Some(idx);
            }
            continue;
        }
        if let Some(s) = start.take() {
            push_token(&content[s..idx], &mut out);
        }
    }
    if let Some(s) = start {
        push_token(&content[s..], &mut out);
    }
    out
}

fn push_token(token: &str, out: &mut std::collections::BTreeSet<String>) {
    // 长度按字符计（多字节安全）；首字符不能是数字（BCE 同款规则）
    if token.chars().count() >= MIN_IDENT_LEN && !token.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(token.to_string());
    }
}

/// 定义位置查询结果（端口返回）：一个符号在 scope 内的定义。
#[derive(Debug, Clone)]
pub struct SymbolDefinition {
    pub identifier: String,
    pub kind: String,
    pub path: String,
    /// 该符号定义出现的文件数（fanout 门控用）
    pub file_fanout: usize,
}

/// 计算最终选中 hits 的 related symbols。
///
/// `definitions` 来自 ExactSearchStore 端口的 `find_definitions`：
/// 只含 scope 内、且 fanout ≤ FANOUT_MAX 的定义（门控在 SQL 侧或此处做均可，
/// 此处按 file_fanout 字段过滤，端口实现负责提供该值）。
pub fn related_symbol_hints(
    selected: &[crate::search::SearchHit],
    definitions: &[SymbolDefinition],
) -> Vec<RelatedSymbol> {
    if selected.is_empty() || definitions.is_empty() {
        return vec![];
    }

    // 选中窗口内已出现的路径集合：定义已可见的符号不再提示
    let shown_paths: std::collections::HashSet<&str> =
        selected.iter().map(|h| h.path.as_str()).collect();

    // 候选定义索引：identifier → 定义（fanout 门控 + 窗口内排除）。
    // 同名多定义取第一个（出现顺序即索引顺序）；endpoint 优先由端口保证。
    let mut defs: std::collections::HashMap<&str, &SymbolDefinition> =
        std::collections::HashMap::new();
    for def in definitions {
        if def.file_fanout > FANOUT_MAX {
            continue;
        }
        if shown_paths.contains(def.path.as_str()) {
            continue;
        }
        // 停用词过滤：docstring 伪定义/解构赋值噪声不进 hints
        if is_stopword(&def.identifier) {
            continue;
        }
        defs.entry(def.identifier.as_str()).or_insert(def);
    }
    if defs.is_empty() {
        return vec![];
    }

    // 统计选中内容里各 token 的引用次数，按频次降序（BCE 同款排序：
    // 频次高 = 上下文反复依赖，定义位置更有探索价值）
    let mut refs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for hit in selected {
        for token in ident_tokens(&hit.content) {
            if defs.contains_key(token.as_str()) {
                *refs.entry(token).or_insert(0) += 1;
            }
        }
    }
    let mut names: Vec<String> = refs.keys().cloned().collect();
    names.sort_by(|a, b| {
        refs[b].cmp(&refs[a]).then_with(|| a.cmp(b)) // 频次同则字典序稳定
    });
    names.truncate(MAX_RELATED_HINTS);

    names
        .into_iter()
        .map(|name| {
            let def = defs[name.as_str()];
            RelatedSymbol {
                name: def.identifier.clone(),
                kind: def.kind.clone(),
                path: def.path.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchHit;

    fn hit(path: &str, content: &str) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}"),
            path: path.into(),
            content: content.into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 10,
        }
    }

    fn def(identifier: &str, kind: &str, path: &str, fanout: usize) -> SymbolDefinition {
        SymbolDefinition {
            identifier: identifier.into(),
            kind: kind.into(),
            path: path.into(),
            file_fanout: fanout,
        }
    }

    #[test]
    fn ident_tokens_extracts_and_filters() {
        let tokens = ident_tokens("let cfg = TokenRefresher::new(); use x::y; 1ab _ok ab");
        assert!(tokens.contains("TokenRefresher"));
        assert!(tokens.contains("cfg"));
        assert!(tokens.contains("_ok"));
        // 首字符数字、长度 < 3 排除
        assert!(!tokens.contains("1ab"));
        assert!(!tokens.contains("ab"));
        assert!(!tokens.contains("y"));
    }

    #[test]
    fn hints_reference_unshown_definitions() {
        let selected = vec![hit(
            "src/api.rs",
            "let r = TokenRefresher::new(); r.refresh();",
        )];
        let defs = vec![
            def("TokenRefresher", "definition", "src/token.rs", 2),
            def("refresh", "endpoint", "src/token.rs", 1),
        ];
        let hints = related_symbol_hints(&selected, &defs);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].name, "TokenRefresher"); // 频次高在前
        assert_eq!(hints[0].path, "src/token.rs");
    }

    #[test]
    fn definitions_in_shown_paths_excluded() {
        let selected = vec![hit("src/token.rs", "pub struct TokenRefresher {}")];
        let defs = vec![def("TokenRefresher", "definition", "src/token.rs", 1)];
        assert!(related_symbol_hints(&selected, &defs).is_empty());
    }

    #[test]
    fn high_fanout_symbols_gated() {
        let selected = vec![hit("src/api.rs", "let s: Widget = Widget::new();")];
        // Widget 被 16 个文件引用（>15 门控）：不进 hints
        let defs = vec![def("Widget", "definition", "src/widget.rs", 16)];
        assert!(related_symbol_hints(&selected, &defs).is_empty());
        // 恰好 15（≤ 门控）放行
        let defs = vec![def("Widget", "definition", "src/widget.rs", 15)];
        assert_eq!(related_symbol_hints(&selected, &defs).len(), 1);
    }

    #[test]
    fn hints_capped_at_eight() {
        let content: String = (0..12).map(|i| format!("call_fn_{i:02}(); ")).collect();
        let selected = vec![hit("src/api.rs", &content)];
        let defs: Vec<_> = (0..12)
            .map(|i| {
                def(
                    &format!("call_fn_{i:02}"),
                    "definition",
                    &format!("src/f{i}.rs"),
                    1,
                )
            })
            .collect();
        let hints = related_symbol_hints(&selected, &defs);
        assert_eq!(hints.len(), MAX_RELATED_HINTS);
    }

    #[test]
    fn empty_inputs_return_empty() {
        assert!(related_symbol_hints(&[], &[def("x", "definition", "a.rs", 1)]).is_empty());
        assert!(related_symbol_hints(&[hit("a.rs", "x")], &[]).is_empty());
    }

    #[test]
    fn first_definition_wins_on_duplicates() {
        let selected = vec![hit("src/api.rs", "run_handler();")];
        let defs = vec![
            def("run_handler", "endpoint", "src/routes.rs", 1),
            def("run_handler", "definition", "src/other.rs", 1),
        ];
        let hints = related_symbol_hints(&selected, &defs);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, "endpoint"); // 端口保证 endpoint 优先，此处取首见
    }

    #[test]
    fn docstring_pseudo_definitions_filtered() {
        // docstring 里行首 "Subclass and has..." 会被 Python 定义正则误提取为
        // identifier="and"；解构赋值 "const { added } = ..." 同理。停用词拦截。
        let selected = vec![hit("src/api.rs", "call and(); use added; take active;")];
        let defs = vec![
            def("and", "definition", "src/flask/json/provider.py", 1),
            def("added", "definition", "src-tauri/x.rs", 1),
            def("active", "definition", "src-tauri/y.rs", 1),
        ];
        assert!(related_symbol_hints(&selected, &defs).is_empty());
    }

    #[test]
    fn stopword_match_is_case_insensitive() {
        // "config" 在停用词表（解构赋值高发）；真实符号 ConfigX 不受影响。
        // 引用侧也必须出现 ConfigX 才会命中（hints 只提示被引用的符号）
        let selected = vec![hit("src/api.rs", "use ConfigX;")];
        let defs = vec![
            def("Config", "definition", "src/a.rs", 1),
            def("ConfigX", "definition", "src/b.rs", 1),
        ];
        let hints = related_symbol_hints(&selected, &defs);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].name, "ConfigX");
    }
}
