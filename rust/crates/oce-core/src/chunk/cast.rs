//! cAST/tree-sitter 语义切块。与 Python `infrastructure/astchunk/` 对齐：
//! [`cast_chunker.py`]（CastChunker 外壳：合并小区间、cap_span、fallback）+
//! [`astchunk_builder.py`]（AST 分窗算法）。
//!
//! AST 决定 chunk 边界；chunk 文本始终从源码行切割——astchunk 按节点坐标重建文本
//! 并用空格填补缝隙，输出与文件不是逐字节一致，不能用于行号渲染。
//!
//! 分窗算法（与 astchunk_builder 一致）：
//! 1. 按非空白字符数（nws）贪心把 AST 节点分窗；超限节点递归进子节点
//! 2. 适度超限的「完整声明」保持整块（intact rule），避免 chunk 以 `return {` 开头
//! 3. 兄弟窗口贪心合并；React 组件（tsx/jsx）顶层强制独立窗口
//! 4. 由窗口首尾节点的行列坐标解析 1-based 行区间，尾部空行收回

use super::lang::detect_language;
use super::recursive::{is_meaningful, split_lines};
use super::spans::{cap_span, char_len, trim_trailing_blank_lines};
use super::types::Chunk;
use super::Chunker;
use async_trait::async_trait;
use std::sync::OnceLock;

pub const DEFAULT_MAX_CHUNK_CHARS: usize = 6_000;
pub const DEFAULT_MIN_CHUNK_CHARS: usize = 300;

/// 低端非空白字符密度下限（Swift 0.67 最低）。用下限换算预算，
/// 保证任何语言下派生的 intact 预算都成立。
const MIN_NWS_DENSITY: f64 = 0.67;

/// 超出 max_chunk_size 多少倍以内仍保持整块。
const INTACT_NODE_SIZE_FACTOR: usize = 3;

/// 声明体字段名（tree-sitter grammars 的负载字段）。
const DECLARATION_BODY_FIELDS: [&str; 4] = [
    "body",
    "block",
    "declaration_list",
    "field_declaration_list",
];

/// 以子节点类型表达结构的后缀约定（Kotlin 等不命名子节点的语法）。
const BODY_NODE_SUFFIXES: [&str; 4] = ["_body", "_block", "_statements", "_declaration_list"];

const BODY_NODE_TYPES: [&str; 3] = ["block", "statements", "statement_block"];

/// 包裹声明的节点类型：自身不是声明，但体属于内部声明。
const DECLARATION_WRAPPER_TYPES: [&str; 15] = [
    "decorated_definition",
    "export_statement",
    "expression_statement",
    "lexical_declaration",
    "variable_declaration",
    "variable_declarator",
    "public_field_definition",
    "property_declaration",
    "call_expression",
    "arguments",
    "call_suffix",
    "annotated_lambda",
    "lambda_literal",
    "function_body",
    "assignment",
];

const REACT_COMPONENT_LANGUAGES: [&str; 2] = ["jsx", "tsx"];
const REACT_DECLARATION_TYPES: [&str; 4] = [
    "class_declaration",
    "function_declaration",
    "lexical_declaration",
    "variable_declaration",
];
const JSX_NODE_TYPES: [&str; 3] = ["jsx_element", "jsx_fragment", "jsx_self_closing_element"];

/// 语言 → tree-sitter 语言标识（与 astchunk_builder.LANGUAGE_MAP 对齐的子集：
/// Rust 侧有成熟 grammar crate 的语言；其余语言由 RecursiveChunker 兜底，
/// 与 Python 版 parser 缺失时的行为一致）。
pub fn tree_sitter_language(lang: &str) -> Option<tree_sitter::Language> {
    use tree_sitter::Language;
    Some(match lang {
        "python" => Language::from(tree_sitter_python::LANGUAGE),
        "java" => Language::from(tree_sitter_java::LANGUAGE),
        "csharp" | "c_sharp" => Language::from(tree_sitter_c_sharp::LANGUAGE),
        "typescript" => Language::from(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        "tsx" => Language::from(tree_sitter_typescript::LANGUAGE_TSX),
        "javascript" | "jsx" => Language::from(tree_sitter_javascript::LANGUAGE),
        "c" => Language::from(tree_sitter_c::LANGUAGE),
        "cpp" | "c++" => Language::from(tree_sitter_cpp::LANGUAGE),
        "go" | "golang" => Language::from(tree_sitter_go::LANGUAGE),
        "rust" => Language::from(tree_sitter_rust::LANGUAGE),
        "ruby" => Language::from(tree_sitter_ruby::LANGUAGE),
        "php" => Language::from(tree_sitter_php::LANGUAGE_PHP),
        "kotlin" => Language::from(tree_sitter_kotlin_ng::LANGUAGE),
        "scala" => Language::from(tree_sitter_scala::LANGUAGE),
        "bash" | "shell" => Language::from(tree_sitter_bash::LANGUAGE),
        "lua" => Language::from(tree_sitter_lua::LANGUAGE),
        "haskell" => Language::from(tree_sitter_haskell::LANGUAGE),
        "elixir" => Language::from(tree_sitter_elixir::LANGUAGE),
        "erlang" => Language::from(tree_sitter_erlang::LANGUAGE),
        "clojure" => Language::from(tree_sitter_clojure::LANGUAGE),
        "zig" => Language::from(tree_sitter_zig::LANGUAGE),
        "cmake" => Language::from(tree_sitter_cmake::LANGUAGE),
        _ => return None,
    })
}

/// 窗口内单个 AST 节点：字节区间 + nws 大小 + 祖先链。
// 字段与 Python astchunk_builder 的一一对应；部分字段当前分支未读但保留算法完整性
#[allow(dead_code)]
#[derive(Clone)]
struct AstNode {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
    size: usize,
    ancestors: Vec<usize>, // 节点在 arena 中的下标链（外层在前）
}

#[derive(Clone)]
struct NodeInfo {
    kind: String,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
    children: Vec<usize>,
    field_child: [Option<usize>; 4], // DECLARATION_BODY_FIELDS 对应的子节点
}

/// tree-sitter 树的 arena 快照：Rust 侧本无 pyo3 生命周期问题，
/// 但统一快照让算法代码与 Python 版逐行对应。
struct TreeArena {
    nodes: Vec<NodeInfo>,
    source: Vec<u8>,
}

impl TreeArena {
    fn build(root: tree_sitter::Node, source: &[u8]) -> Self {
        let mut arena = Self {
            nodes: Vec::new(),
            source: source.to_vec(),
        };
        arena.add(root, None);
        arena
    }

    fn add(&mut self, node: tree_sitter::Node, parent: Option<usize>) -> usize {
        let idx = self.nodes.len();
        let mut children_idx = Vec::new();
        // 先占位，避免父子互指时 borow 冲突
        self.nodes.push(NodeInfo {
            kind: node.kind().to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_line: node.start_position().row,
            start_col: node.start_position().column,
            end_line: node.end_position().row,
            end_col: node.end_position().column,
            children: vec![],
            field_child: [None; 4],
        });
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            children_idx.push(self.add(child, Some(idx)));
        }
        for (fi, field) in DECLARATION_BODY_FIELDS.iter().enumerate() {
            if let Some(f) = node.child_by_field_name(field) {
                // field 子节点也在 children 里；找它的下标
                let fb = f.byte_range();
                self.nodes[idx].field_child[fi] = children_idx.iter().copied().find(|&c| {
                    self.nodes[c].start_byte == fb.start && self.nodes[c].end_byte == fb.end
                });
            }
        }
        self.nodes[idx].children = children_idx;
        let _ = parent;
        idx
    }

    fn nws_count(&self, start: usize, end: usize) -> usize {
        self.source[start..end]
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            // 非空白字节数下限近似：多字节 UTF-8 首字节计 1。
            // 与 Python 按码点计数的差异 ≤ 非空白多字节字符数，预算场景可接受。
            .count()
    }
}

#[allow(dead_code)]
struct CastBuilder {
    max_chunk_size: usize,
    intact_node_size: usize,
    language: String,
}

impl CastBuilder {
    /// 把整棵树分窗。整树不超预算时单窗返回。
    fn assign_tree_to_windows(&self, arena: &TreeArena, root: usize) -> Vec<Vec<AstNode>> {
        let root_info = &arena.nodes[root];
        let tree_size = arena.nws_count(root_info.start_byte, root_info.end_byte);
        if tree_size <= self.max_chunk_size {
            return vec![vec![AstNode {
                start_byte: root_info.start_byte,
                end_byte: root_info.end_byte,
                start_line: root_info.start_line,
                start_col: root_info.start_col,
                end_line: root_info.end_line,
                end_col: root_info.end_col,
                size: tree_size,
                ancestors: vec![],
            }]];
        }
        let children = root_info.children.clone();
        self.assign_nodes_to_windows(arena, &children, vec![root])
    }

    /// React 顶层组件强制独立窗口（tsx/jsx 专用）。
    fn assign_react_top_level_windows(
        &self,
        arena: &TreeArena,
        root: usize,
    ) -> Option<Vec<Vec<AstNode>>> {
        let children = arena.nodes[root].children.clone();
        if !children.iter().any(|&n| self.is_react_component(arena, n)) {
            return None;
        }
        let mut windows: Vec<Vec<AstNode>> = Vec::new();
        let mut ordinary: Vec<usize> = Vec::new();
        let flush = |ordinary: &mut Vec<usize>, windows: &mut Vec<Vec<AstNode>>| {
            if !ordinary.is_empty() {
                windows.extend(self.assign_nodes_to_windows(arena, ordinary, vec![root]));
                ordinary.clear();
            }
        };
        for &node in &children {
            if !self.is_react_component(arena, node) {
                ordinary.push(node);
                continue;
            }
            flush(&mut ordinary, &mut windows);
            windows.extend(self.assign_nodes_to_windows(arena, &[node], vec![root]));
        }
        flush(&mut ordinary, &mut windows);
        Some(windows)
    }

    fn is_react_component(&self, arena: &TreeArena, node: usize) -> bool {
        let info = &arena.nodes[node];
        let mut declaration = node;
        if info.kind == "export_statement" {
            declaration = info
                .children
                .iter()
                .copied()
                .find(|&c| REACT_DECLARATION_TYPES.contains(&arena.nodes[c].kind.as_str()))
                .unwrap_or(node);
        }
        let decl = &arena.nodes[declaration];
        if !REACT_DECLARATION_TYPES.contains(&decl.kind.as_str()) {
            return false;
        }
        if !Self::has_pascal_case_name(arena, declaration) {
            return false;
        }
        Self::contains_jsx(arena, declaration)
    }

    fn has_pascal_case_name(arena: &TreeArena, node: usize) -> bool {
        static RE: OnceLock<regex::Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            regex::Regex::new(
                r"^(?:(?:async\s+)?function|class|const|let|var)\s+([A-Z][A-Za-z0-9_$]*)\b",
            )
            .unwrap()
        });
        let text = String::from_utf8_lossy(
            &arena.source[arena.nodes[node].start_byte..arena.nodes[node].end_byte],
        );
        let first_line = text.lines().next().unwrap_or("");
        re.is_match(first_line.trim_start())
    }

    fn contains_jsx(arena: &TreeArena, node: usize) -> bool {
        let mut pending: Vec<usize> = arena.nodes[node].children.clone();
        while let Some(current) = pending.pop() {
            if JSX_NODE_TYPES.contains(&arena.nodes[current].kind.as_str()) {
                return true;
            }
            pending.extend(arena.nodes[current].children.iter().copied());
        }
        false
    }

    /// 贪心分窗：节点超限时递归子节点；intact 声明保持整块；
    /// 叶子超限（无子可递归）独立成窗，由 cap_span 硬切。
    fn assign_nodes_to_windows(
        &self,
        arena: &TreeArena,
        nodes: &[usize],
        ancestors: Vec<usize>,
    ) -> Vec<Vec<AstNode>> {
        if nodes.is_empty() {
            return vec![];
        }
        let mut windows: Vec<Vec<AstNode>> = Vec::new();
        let mut current_window: Vec<AstNode> = Vec::new();
        let mut current_window_size = 0usize;

        for &node in nodes {
            let info = &arena.nodes[node];
            let node_size = arena.nws_count(info.start_byte, info.end_byte);
            let node_exceeds = node_size > self.max_chunk_size;

            let cannot_pack = (current_window.is_empty() && node_exceeds)
                || current_window_size + node_size > self.max_chunk_size;
            if cannot_pack {
                if !current_window.is_empty() {
                    windows.push(std::mem::take(&mut current_window));
                    current_window_size = 0;
                }
                if node_exceeds {
                    if self.is_intact_declaration(arena, node, node_size) {
                        windows.push(vec![AstNode {
                            start_byte: info.start_byte,
                            end_byte: info.end_byte,
                            start_line: info.start_line,
                            start_col: info.start_col,
                            end_line: info.end_line,
                            end_col: info.end_col,
                            size: node_size,
                            ancestors: ancestors.clone(),
                        }]);
                        continue;
                    }
                    let mut child_ancestors = ancestors.clone();
                    child_ancestors.push(node);
                    let children = info.children.clone();
                    let child_windows =
                        self.assign_nodes_to_windows(arena, &children, child_ancestors);
                    if !child_windows.is_empty() {
                        windows.extend(self.merge_adjacent_windows(child_windows));
                    } else {
                        // P1 FIX：叶子节点超限且无子节点 → 独立成窗，cap_span 硬切
                        windows.push(vec![AstNode {
                            start_byte: info.start_byte,
                            end_byte: info.end_byte,
                            start_line: info.start_line,
                            start_col: info.start_col,
                            end_line: info.end_line,
                            end_col: info.end_col,
                            size: node_size,
                            ancestors: ancestors.clone(),
                        }]);
                    }
                } else {
                    current_window.push(AstNode {
                        start_byte: info.start_byte,
                        end_byte: info.end_byte,
                        start_line: info.start_line,
                        start_col: info.start_col,
                        end_line: info.end_line,
                        end_col: info.end_col,
                        size: node_size,
                        ancestors: ancestors.clone(),
                    });
                    current_window_size += node_size;
                }
            } else {
                current_window.push(AstNode {
                    start_byte: info.start_byte,
                    end_byte: info.end_byte,
                    start_line: info.start_line,
                    start_col: info.start_col,
                    end_line: info.end_line,
                    end_col: info.end_col,
                    size: node_size,
                    ancestors: ancestors.clone(),
                });
                current_window_size += node_size;
            }
        }
        if !current_window.is_empty() {
            windows.push(current_window);
        }
        windows
    }

    fn is_intact_declaration(&self, arena: &TreeArena, node: usize, node_size: usize) -> bool {
        node_size <= self.intact_node_size && self.carries_body(arena, node, 0)
    }

    /// 是否拥有「体」：先看字段子节点，再看子节点类型后缀；
    /// 包裹类型逐层跟进（`export const handler = () => {...}` 的体挂在箭头函数上）。
    fn carries_body(&self, arena: &TreeArena, node: usize, depth: usize) -> bool {
        let info = &arena.nodes[node];
        if info.field_child.iter().any(|f| f.is_some()) {
            return true;
        }
        if info
            .children
            .iter()
            .any(|&c| Self::looks_like_body(&arena.nodes[c].kind))
        {
            return true;
        }
        if depth >= 4 || !DECLARATION_WRAPPER_TYPES.contains(&info.kind.as_str()) {
            return false;
        }
        info.children
            .iter()
            .any(|&c| self.carries_body(arena, c, depth + 1))
    }

    fn looks_like_body(kind: &str) -> bool {
        BODY_NODE_TYPES.contains(&kind) || BODY_NODE_SUFFIXES.iter().any(|s| kind.ends_with(s))
    }

    /// 兄弟窗口贪心合并：合并后不超预算才并（保持 AST 结构）。
    fn merge_adjacent_windows(&self, ast_windows: Vec<Vec<AstNode>>) -> Vec<Vec<AstNode>> {
        debug_assert!(!ast_windows.is_empty());
        let mut merged: Vec<Vec<AstNode>> = Vec::new();
        for window in ast_windows {
            if let Some(last) = merged.last_mut() {
                let total: usize = last.iter().map(|n| n.size).sum::<usize>()
                    + window.iter().map(|n| n.size).sum::<usize>();
                if total <= self.max_chunk_size {
                    last.extend(window);
                    continue;
                }
            }
            merged.push(window);
        }
        merged
    }
}

/// 每个窗口解析出的行区间（astchunk 元数据）。
pub struct RawChunk {
    pub start_line: usize, // 0-based
    pub end_line: usize,   // 0-based（节点最后一字节所在行）
    pub end_column: usize,
    pub node_count: usize,
}

#[derive(Clone)]
pub struct CastChunker {
    fallback: std::sync::Arc<dyn Chunker>,
    max_chunk_chars: usize,
    min_chunk_chars: usize,
    max_chunk_size: usize,
}

impl CastChunker {
    pub fn new(
        fallback: std::sync::Arc<dyn Chunker>,
        max_chunk_size: usize,
        max_chunk_chars: usize,
        min_chunk_chars: usize,
    ) -> Result<Self, String> {
        if max_chunk_size == 0 || max_chunk_chars == 0 {
            return Err("max_chunk_size / max_chunk_chars 必须 > 0".into());
        }
        if min_chunk_chars >= max_chunk_chars {
            return Err("min_chunk_chars 必须 ∈ [0, max_chunk_chars)".into());
        }
        Ok(Self {
            fallback,
            max_chunk_chars,
            min_chunk_chars,
            max_chunk_size,
        })
    }

    fn builder(&self, language: &str) -> Option<CastBuilder> {
        tree_sitter_language(language)?;
        // intact 预算：max_chunk_size 与 max_chunk_chars × 密度下限的较大者
        let derived = (self.max_chunk_chars as f64 * MIN_NWS_DENSITY) as usize;
        Some(CastBuilder {
            max_chunk_size: self.max_chunk_size,
            intact_node_size: self.max_chunk_size.max(derived) * INTACT_NODE_SIZE_FACTOR,
            language: language.to_string(),
        })
    }

    /// astchunk chunkify：解析 → 分窗 → 行区间。
    fn chunkify(&self, language: &str, content: &str) -> Option<Vec<RawChunk>> {
        let ts_lang = tree_sitter_language(language)?;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).ok()?;
        let tree = parser.parse(content, None)?;
        let arena = TreeArena::build(tree.root_node(), content.as_bytes());
        let builder = self.builder(language)?;

        // React 顶层组件强制独立窗口（仅 tsx/jsx）
        let windows = if REACT_COMPONENT_LANGUAGES.contains(&language) {
            builder
                .assign_react_top_level_windows(&arena, 0)
                .unwrap_or_else(|| builder.assign_tree_to_windows(&arena, 0))
        } else {
            builder.assign_tree_to_windows(&arena, 0)
        };
        if windows.is_empty() {
            return Some(vec![]);
        }

        let line_count = split_lines(content).len();
        let mut raw = Vec::with_capacity(windows.len());
        for window in &windows {
            let first = &window[0];
            let last = window.last().unwrap();
            raw.push(RawChunk {
                start_line: first.start_line,
                end_line: last.end_line,
                end_column: last.end_col,
                node_count: window.len(),
            });
        }
        let _ = line_count;
        Some(raw)
    }

    /// astchunk 的 0-based 行 → 1-based 闭区间。节点止于行首列 0 时该行归下个 chunk。
    fn resolve_range(&self, raw: &RawChunk, lines: &[&str]) -> Result<(u32, u32), ()> {
        let line_count = lines.len();
        let start = raw.start_line as u32 + 1;
        let end = if raw.end_column == 0 && raw.end_line as u32 + 1 > start {
            raw.end_line as u32
        } else {
            raw.end_line as u32 + 1
        };
        if start < 1 || end < start || end as usize > line_count {
            return Err(());
        }
        Ok((start, trim_trailing_blank_lines(lines, start, end)))
    }

    /// 过小区间折入邻居，保持覆盖有序（与 Python `_merge_small` 对齐）。
    fn merge_small(
        &self,
        ranges: Vec<(u32, u32, String)>,
        lines: &[&str],
    ) -> Vec<(u32, u32, String)> {
        if self.min_chunk_chars == 0 {
            return ranges;
        }
        let mut merged: Vec<(u32, u32, String)> = Vec::new();
        for (start, end, chunk_type) in ranges {
            if let Some((prev_start, prev_end, _)) = merged.last().cloned() {
                if end <= prev_end {
                    continue;
                }
                if span_chars(lines, prev_start, prev_end) < self.min_chunk_chars {
                    *merged.last_mut().unwrap() = (prev_start, end, chunk_type);
                    continue;
                }
            }
            if let Some((prev_start, _, prev_type)) = merged.last().cloned() {
                if span_chars(lines, start, end) < self.min_chunk_chars {
                    *merged.last_mut().unwrap() = (prev_start, end, prev_type);
                    continue;
                }
            }
            merged.push((start, end, chunk_type));
        }
        merged
    }

    fn chunk_ast(&self, content: &str, path: &str, language: &str) -> Vec<Chunk> {
        let lines = split_lines(content);
        let raw_chunks = match self.chunkify(language, content) {
            Some(r) => r,
            None => return self.fallback.chunk(content, path),
        };
        let mut ranges = Vec::with_capacity(raw_chunks.len());
        for raw in &raw_chunks {
            match self.resolve_range(raw, &lines) {
                Ok((s, e)) => ranges.push((s, e, "ast".to_string())),
                Err(()) => return self.fallback.chunk(content, path),
            }
        }
        let mut chunks = Vec::new();
        for (start, end, chunk_type) in self.merge_small(ranges, &lines) {
            for (span_start, span_end, text) in cap_span(&lines, start, end, self.max_chunk_chars) {
                if let Ok(chunk) = Chunk::new(
                    Chunk::compute_hash(&text),
                    path,
                    text,
                    span_start,
                    span_end,
                    Some(chunk_type.clone()),
                ) {
                    chunks.push(chunk);
                }
            }
        }
        if !chunks.is_empty() {
            return chunks;
        }
        // 解析成功但所有行都超预算：压缩包/单行产物，不产出（与 Python 一致）
        if !raw_chunks.is_empty() {
            return vec![];
        }
        self.fallback.chunk(content, path)
    }
}

/// 1-based 闭区间的字符数（含换行，与 Python `_span_chars` 一致）。
fn span_chars(lines: &[&str], start: u32, end: u32) -> usize {
    let mut total: usize = (start as usize..=end as usize)
        .map(|row| char_len(lines[row - 1]) + 1)
        .sum();
    total = total.saturating_sub(1);
    total
}

/// CastChunker 承接的语言集合：SUPPORTED ∩ LANGUAGE_MAP − 专用 chunker 语言。
/// 由 [`LanguageChunkerRouter`](super::router::LanguageChunkerRouter) 分发。
pub fn cast_languages() -> &'static std::collections::HashSet<&'static str> {
    static LANGS: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    LANGS.get_or_init(|| {
        super::lang::supported_languages()
            .iter()
            .copied()
            .filter(|l| *l != "markdown" && *l != "jsp" && *l != "vue" && *l != "svelte")
            .filter(|l| tree_sitter_language(l).is_some())
            .collect()
    })
}

#[async_trait]
impl Chunker for CastChunker {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        if content.is_empty() {
            return vec![];
        }
        if !is_meaningful(content) {
            return vec![];
        }
        let lines = split_lines(content);
        if lines.is_empty() {
            return vec![];
        }
        // 语言由 router 保证已知；防御性再检测一次
        match detect_language(path) {
            Some(lang) if cast_languages().contains(lang) => self.chunk_ast(content, path, lang),
            _ => self.fallback.chunk(content, path),
        }
    }
}
