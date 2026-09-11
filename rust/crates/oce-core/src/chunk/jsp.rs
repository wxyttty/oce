//! JSP 系模板切块器。与 Python `chunkers/jsp_chunker.py` 对齐。
//!
//! 在顶层渲染内容边界（html tree-sitter 语法下的 element/script/style 节点）切分；
//! `<% %>` scriptlet 与 `<jsp:scriptlet>` 等内嵌 Java 先被空格掩码再解析，
//! 保持行号与列位不变。

use super::recursive::{is_meaningful, split_lines};
use super::spans::{cap_span, trim_trailing_blank_lines};
use super::types::Chunk;
use super::Chunker;
use async_trait::async_trait;
use regex::Regex;
use std::sync::OnceLock;

pub const DEFAULT_MAX_CHUNK_CHARS: usize = 6_000;

const CONTENT_NODE_TYPES: [&str; 3] = ["element", "script_element", "style_element"];

#[derive(Clone)]
pub struct JspChunker {
    fallback: std::sync::Arc<dyn Chunker>,
    max_chunk_chars: usize,
}

impl JspChunker {
    pub fn new(fallback: std::sync::Arc<dyn Chunker>, max_chunk_chars: usize) -> Result<Self, String> {
        if max_chunk_chars == 0 {
            return Err("max_chunk_chars 必须 > 0".into());
        }
        Ok(Self {
            fallback,
            max_chunk_chars,
        })
    }

    /// 掩码内嵌 Java：`<%...%>` 与 `<jsp:scriptlet|expression|declaration>...</jsp:...>`
    /// 的 body 换成等长空白（换行保留）。
    fn mask_jsp_code(content: &str) -> String {
        static JSP_BLOCK: OnceLock<Regex> = OnceLock::new();
        static JSP_XML: OnceLock<Regex> = OnceLock::new();
        let block = JSP_BLOCK.get_or_init(|| Regex::new(r"(?s)<%.*?%>").unwrap());
        let xml = JSP_XML.get_or_init(|| {
            Regex::new(
                r"(?is)(?P<open><jsp:(?P<kind>scriptlet|expression|declaration)\b[^>]*>)(?P<body>.*?)(?P<close></jsp:(?P=kind)\s*>)",
            )
            .unwrap()
        });
        let blank = |s: &str| {
            s.chars()
                .map(|c| if c == '\n' { '\n' } else { ' ' })
                .collect::<String>()
        };
        let masked = block.replace_all(content, |c: &regex::Captures| blank(&c[0]));
        xml.replace_all(&masked, |c: &regex::Captures| {
            format!("{}{}{}", &c["open"], blank(&c["body"]), &c["close"])
        })
        .into_owned()
    }

    /// 顶层内容边界：(起始行, 标签名)。
    fn content_boundaries(root: tree_sitter::Node, source: &[u8]) -> Vec<(u32, String)> {
        if let Some(body) = find_element(root, "body", source) {
            let boundaries = direct_content_children(body, source);
            if !boundaries.is_empty() {
                return boundaries;
            }
        }
        let top_level: Vec<tree_sitter::Node> = root
            .children(&mut root.walk())
            .filter(|c| CONTENT_NODE_TYPES.contains(&c.kind()))
            .collect();
        if top_level.len() == 1
            && tag_name(top_level[0], source)
                .is_some_and(|t| t == "html" || t == "jsp:root")
        {
            let nested = direct_content_children(top_level[0], source);
            if nested.len() > 1 {
                return nested;
            }
        }
        as_boundaries(top_level, source)
    }

    fn emit(
        &self,
        boundaries: Vec<(u32, String)>,
        lines: &[&str],
        path: &str,
    ) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        for (index, (boundary_start, tag)) in boundaries.iter().enumerate() {
            let start = if index == 0 { 1 } else { *boundary_start };
            let mut end = if index + 1 < boundaries.len() {
                boundaries[index + 1].0 - 1
            } else {
                lines.len() as u32
            };
            end = trim_trailing_blank_lines(lines, start, end);
            for (span_start, span_end, text) in
                cap_span(lines, start, end, self.max_chunk_chars)
            {
                if text.trim().is_empty() {
                    continue;
                }
                if let Ok(chunk) = Chunk::new(
                    Chunk::compute_hash(&text),
                    path,
                    text,
                    span_start,
                    span_end,
                    Some(format!("jsp:{tag}")),
                ) {
                    chunks.push(chunk);
                }
            }
        }
        chunks
    }
}

fn find_element<'a>(node: tree_sitter::Node<'a>, tag: &str, source: &'a [u8]) -> Option<tree_sitter::Node<'a>> {
    if CONTENT_NODE_TYPES.contains(&node.kind()) && tag_name(node, source).as_deref() == Some(tag) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_element(child, tag, source) {
            return Some(found);
        }
    }
    None
}

fn direct_content_children(
    node: tree_sitter::Node,
    source: &[u8],
) -> Vec<(u32, String)> {
    let children: Vec<tree_sitter::Node> = node
        .children(&mut node.walk())
        .filter(|c| CONTENT_NODE_TYPES.contains(&c.kind()))
        .collect();
    as_boundaries(children, source)
}

fn as_boundaries(nodes: Vec<tree_sitter::Node>, source: &[u8]) -> Vec<(u32, String)> {
    let mut boundaries: Vec<(u32, String)> = Vec::new();
    for node in nodes {
        let line = node.start_position().row as u32 + 1;
        let tag = tag_name(node, source).unwrap_or_else(|| "section".to_string());
        if boundaries.last().is_some_and(|(l, _)| *l == line) {
            continue;
        }
        boundaries.push((line, tag));
    }
    boundaries
}

/// BFS 找 tag_name 子节点（穿透 start_tag / self_closing_tag）。
fn tag_name(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut pending: Vec<tree_sitter::Node> = node.children(&mut node.walk()).collect();
    while let Some(current) = pending.pop() {
        if current.kind() == "tag_name" {
            return Some(
                String::from_utf8_lossy(&source[current.byte_range()])
                    .to_lowercase(),
            );
        }
        if current.kind() == "start_tag" || current.kind() == "self_closing_tag" {
            // Python 版 pop(0) + extend 前插：BFS；此处保持一致
            let children: Vec<tree_sitter::Node> =
                current.children(&mut current.walk()).collect();
            pending.splice(0..0, children);
        }
    }
    None
}

#[async_trait]
impl Chunker for JspChunker {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        if !is_meaningful(content) {
            return vec![];
        }
        let lines = split_lines(content);
        if lines.is_empty() {
            return vec![];
        }
        let boundaries = (|| -> Option<Vec<(u32, String)>> {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter::Language::from(tree_sitter_html::LANGUAGE))
                .ok()?;
            let masked = Self::mask_jsp_code(content);
            let tree = parser.parse(&masked, None)?;
            Some(Self::content_boundaries(tree.root_node(), masked.as_bytes()))
        })()
        .unwrap_or_default();
        if boundaries.is_empty() {
            return self.fallback.chunk(content, path);
        }
        self.emit(boundaries, &lines, path)
    }
}
