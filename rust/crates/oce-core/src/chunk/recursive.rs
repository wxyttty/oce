//! RecursiveChunker — LangChain RecursiveCharacterTextSplitter 的忠实移植。
//!
//! 切分器决定边界落点；chunk 文本始终从源码行上切割（与 CastChunker 同一契约）。
//! 切分器自身输出不能直接作为 chunk 文本：它在每个分隔符处 strip 空白，
//! 返回的字符串不再与所属行对应，行号会指错位置。
//!
//! 移植语义（langchain_text_splitters 1.x，keep_separator="start"，add_start_index）：
//! - 递归选分隔符：选第一个在当前文本中出现的；空分隔符退化为逐字符
//! - 合并：贪心打包到 chunk_size，超出时按 chunk_overlap 弹出队首
//! - start_index：strip 后文本在原文中的偏移（等价 langchain 的 `text.find`）
//! - 最终 chunk 由起始行 tile 成连续不重叠行区间，块间无重叠

use super::lang::detect_language;
use super::spans::{cap_span, char_len, trim_trailing_blank_lines};
use super::types::Chunk;
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const DEFAULT_MAX_CHUNK_CHARS: usize = 6_000;
pub const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// chunk 文本硬上限，独立于 chunk_size。单行超限直接丢弃。
pub const MAX_SPAN_CHARS: usize = 6_000;

/// 是否含有效信息（至少一个字母/数字，Unicode alnum）。
pub fn is_meaningful(text: &str) -> bool {
    text.chars().any(|ch| ch.is_alphanumeric())
}

/// 语言分隔符表（来自 langchain `get_separators_for_language`，is_separator_regex=True）。
/// 只收录 Python 版 RecursiveChunker 实际路由到的语言；其余走默认分隔符。
fn language_separators(lang: &str) -> Option<&'static [&'static str]> {
    Some(match lang {
        "python" => &["\nclass ", "\ndef ", "\n\tdef ", "\n\n", "\n", " ", ""][..],
        "javascript" | "jsx" => &[
            "\nfunction ",
            "\nconst ",
            "\nlet ",
            "\nvar ",
            "\nclass ",
            "\nif ",
            "\nfor ",
            "\nwhile ",
            "\nswitch ",
            "\ncase ",
            "\ndefault ",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "typescript" | "tsx" => &[
            "\nenum ",
            "\ninterface ",
            "\nnamespace ",
            "\ntype ",
            "\nclass ",
            "\nfunction ",
            "\nconst ",
            "\nlet ",
            "\nvar ",
            "\nif ",
            "\nfor ",
            "\nwhile ",
            "\nswitch ",
            "\ncase ",
            "\ndefault ",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "java" => &[
            "\nclass ",
            "\npublic ",
            "\nprotected ",
            "\nprivate ",
            "\nstatic ",
            "\nif ",
            "\nfor ",
            "\nwhile ",
            "\nswitch ",
            "\ncase ",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "cpp" => &[
            "\nclass ",
            "\nvoid ",
            "\nint ",
            "\nfloat ",
            "\ndouble ",
            "\nif ",
            "\nfor ",
            "\nwhile ",
            "\nswitch ",
            "\ncase ",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "go" => &[
            "\nfunc ",
            "\nvar ",
            "\nconst ",
            "\ntype ",
            "\nif ",
            "\nfor ",
            "\nswitch ",
            "\ncase ",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "rust" => &[
            "\nfn ", "\nconst ", "\nlet ", "\nif ", "\nwhile ", "\nfor ", "\nloop ", "\nmatch ",
            "\nconst ", "\n\n", "\n", " ", "",
        ][..],
        "markdown" => &[
            "\n#{1,6} ",
            "```\n",
            "\n\\*\\*\\*+\n",
            "\n---+\n",
            "\n___+\n",
            "\n\n",
            "\n",
            " ",
            "",
        ][..],
        "html" => &[
            "<body", "<div", "<p", "<br", "<li", "<h1", "<h2", "<h3", "<h4", "<h5", "<h6", "<span",
            "<table", "<tr", "<td", "<th", "<ul", "<ol", "<header", "<footer", "<nav", "<head",
            "<style", "<script", "<meta", "<title", "",
        ][..],
        _ => return None,
    })
}

const DEFAULT_SEPARATORS: &[&str] = &["\n\n", "\n", " ", ""];

/// 带原文偏移的切分片段：offset 是片段（含前导分隔符）首字符的字节偏移。
type Piece = (usize, String);

#[derive(Clone)]
pub struct RecursiveChunker {
    chunk_size: usize,
    chunk_overlap: usize,
}

impl RecursiveChunker {
    pub fn new(chunk_size: usize, chunk_overlap: usize) -> Result<Self, String> {
        if chunk_size == 0 {
            return Err("chunk_size 必须 > 0".into());
        }
        if chunk_overlap >= chunk_size {
            return Err("chunk_overlap 必须 ∈ [0, chunk_size)".into());
        }
        Ok(Self {
            chunk_size,
            chunk_overlap,
        })
    }

    pub fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        if !is_meaningful(content) {
            return vec![];
        }
        let lines: Vec<&str> = split_lines(content);
        if lines.is_empty() {
            return vec![];
        }
        let pieces = self.split_with_offsets(content, detect_language(path));
        let starts = self.start_lines(&lines, &pieces);
        if starts.is_empty() {
            return vec![];
        }
        self.emit(self.tile(starts, lines.len()), &lines, path)
    }

    /// 递归切分并跟踪每个片段的原文偏移。
    fn split_with_offsets(&self, content: &str, language: Option<&str>) -> Vec<Piece> {
        let (separators, is_regex) = match language.and_then(language_separators) {
            Some(seps) => (seps, true),
            None => (DEFAULT_SEPARATORS, false),
        };
        self.split_recursive(content, separators, is_regex, 0)
    }

    fn split_recursive(
        &self,
        text: &str,
        separators: &[&str],
        is_regex: bool,
        base_offset: usize,
    ) -> Vec<Piece> {
        if text.is_empty() {
            return vec![];
        }
        // 选第一个在文本中出现的分隔符；空分隔符退化逐字符
        let mut separator: &str = separators[separators.len() - 1];
        let mut rest: &[&str] = &[];
        for (i, s) in separators.iter().enumerate() {
            if s.is_empty() {
                separator = s;
                rest = &[];
                break;
            }
            let appears = if is_regex {
                regex_for(s).is_match(text)
            } else {
                text.contains(s)
            };
            if appears {
                separator = s;
                rest = &separators[i + 1..];
                break;
            }
        }

        let pieces = split_with_separator(text, separator, is_regex, base_offset);

        let mut final_chunks: Vec<Piece> = Vec::new();
        let mut good: Vec<Piece> = Vec::new();
        for piece in pieces {
            if char_len(&piece.1) < self.chunk_size {
                good.push(piece);
                continue;
            }
            if !good.is_empty() {
                final_chunks.extend(self.merge_splits(std::mem::take(&mut good)));
            }
            if rest.is_empty() {
                final_chunks.push(piece);
            } else {
                let (off, sub) = piece;
                final_chunks.extend(self.split_recursive(&sub, rest, is_regex, off));
            }
        }
        if !good.is_empty() {
            final_chunks.extend(self.merge_splits(good));
        }
        final_chunks
    }

    /// 对应 langchain `merge_splits` + `_join_docs`（keep_separator → 合并分隔符为空，
    /// strip_whitespace=True）。返回的 Piece 偏移是 strip 后文本的起始偏移。
    fn merge_splits(&self, splits: Vec<Piece>) -> Vec<Piece> {
        let mut docs: Vec<Piece> = Vec::new();
        let mut current: Vec<Piece> = Vec::new();
        let mut total: usize = 0;

        for (off, d) in splits {
            let len_ = char_len(&d);
            if total + len_ > self.chunk_size {
                if total > self.chunk_size {
                    tracing::warn!(
                        "Created a chunk of size {}, longer than the specified {}",
                        total,
                        self.chunk_size
                    );
                }
                if !current.is_empty() {
                    if let Some(doc) = join_docs(&current) {
                        docs.push(doc);
                    }
                    while total > self.chunk_overlap
                        || (total + len_ > self.chunk_size && total > 0)
                    {
                        total -= char_len(&current[0].1);
                        current.remove(0);
                    }
                }
            }
            current.push((off, d));
            total += len_;
        }
        if let Some(doc) = join_docs(&current) {
            docs.push(doc);
        }
        docs
    }

    /// 把片段起始偏移映射到 1-based 行号；行中落点回拉到所属行行首。
    fn start_lines(&self, lines: &[&str], pieces: &[Piece]) -> Vec<u32> {
        let offsets = line_offsets(lines);
        pieces
            .iter()
            .map(|(off, _)| line_of(&offsets, *off as u64))
            .collect()
    }

    /// 起始行 → 连续、不重叠的行区间。块间重叠在此丢弃：两个声明相同行的
    /// chunk 会把同一段源码索引两遍，占掉两个检索位。
    fn tile(&self, starts: Vec<u32>, total_lines: usize) -> Vec<(u32, u32)> {
        let mut ordered: Vec<u32> = starts
            .into_iter()
            .filter(|s| (1..=total_lines as u32).contains(s))
            .collect();
        ordered.sort_unstable();
        ordered.dedup();
        if ordered.is_empty() || ordered[0] != 1 {
            ordered.insert(0, 1);
        }
        let mut spans = Vec::with_capacity(ordered.len());
        for (i, start) in ordered.iter().enumerate() {
            let end = if i + 1 < ordered.len() {
                ordered[i + 1] - 1
            } else {
                total_lines as u32
            };
            if end >= *start {
                spans.push((*start, end));
            }
        }
        spans
    }

    fn emit(&self, spans: Vec<(u32, u32)>, lines: &[&str], path: &str) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        for (start, end) in spans {
            let trimmed = trim_trailing_blank_lines(lines, start, end);
            for (span_start, span_end, text) in
                cap_span(lines, start, trimmed, self.chunk_size.max(MAX_SPAN_CHARS))
            {
                if !is_meaningful(&text) {
                    continue;
                }
                if let Ok(chunk) = Chunk::new(
                    Chunk::compute_hash(&text),
                    path,
                    text,
                    span_start,
                    span_end,
                    Some("recursive".into()),
                ) {
                    chunks.push(chunk);
                }
            }
        }
        chunks
    }
}

/// strip 后的合并文本 + strip 起始偏移；全空白返回 None。
fn join_docs(current: &[Piece]) -> Option<Piece> {
    let (first_off, _) = current[0];
    let joined_len: usize = current.iter().map(|(_, t)| t.len()).sum();
    // 统一分配：所有片段都来自同一原始 buffer，直接拼
    let mut joined = String::with_capacity(joined_len);
    for (_, t) in current {
        joined.push_str(t);
    }
    let leading_ws = joined.len() - joined.trim_start().len();
    let trimmed = joined.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    // trim_start 移除的是字符级空白；偏移按字节推进同一 buffer
    let offset = first_off + leading_ws;
    Some((offset, trimmed))
}

/// 分隔符出现位置（字节区间，非重叠左优先，等价 Python re.split 捕获组语义）。
fn separator_occurrences(text: &str, separator: &str, is_regex: bool) -> Vec<(usize, usize)> {
    if is_regex {
        regex_for(separator)
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect()
    } else {
        let mut occ = Vec::new();
        let mut pos = 0usize;
        while let Some(idx) = text[pos..].find(separator) {
            let start = pos + idx;
            occ.push((start, start + separator.len()));
            pos = start + separator.len();
        }
        occ
    }
}

/// `_split_text_with_regex` 移植：keep_separator="start"（分隔符留在片段头部）。
/// 空分隔符逐字符切分。每片段携带其在原文中的字节偏移。
/// pieces = [前导文本, sep+正文, sep+正文, ...]（与 langchain 一致）。
fn split_with_separator(text: &str, separator: &str, is_regex: bool, base: usize) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    if separator.is_empty() {
        for (off, part) in text.char_indices() {
            pieces.push((base + off, part.to_string()));
        }
        return pieces;
    }
    let occ = separator_occurrences(text, separator, is_regex);
    if occ.is_empty() {
        pieces.push((base, text.to_string()));
        return pieces;
    }
    if occ[0].0 > 0 {
        pieces.push((base, text[..occ[0].0].to_string()));
    }
    for i in 0..occ.len() {
        let end = if i + 1 < occ.len() {
            occ[i + 1].0
        } else {
            text.len()
        };
        let seg = &text[occ[i].0..end];
        if !seg.is_empty() {
            pieces.push((base + occ[i].0, seg.to_string()));
        }
    }
    // Python 的 [s for s in splits if s] 过滤空片段
    pieces.retain(|(_, s)| !s.is_empty());
    pieces
}

/// 预编译的切分正则表：模式全部来自静态分隔符集合，一次性构建。
fn regex_for(pattern: &str) -> &'static Regex {
    static TABLE: OnceLock<HashMap<&'static str, Regex>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut m: HashMap<&'static str, Regex> = HashMap::new();
        for lang in [
            "python",
            "javascript",
            "jsx",
            "typescript",
            "tsx",
            "java",
            "cpp",
            "go",
            "rust",
            "markdown",
            "html",
        ] {
            if let Some(seps) = language_separators(lang) {
                for s in seps {
                    if !m.contains_key(s) {
                        m.insert(s, Regex::new(s).expect("invalid splitter regex"));
                    }
                }
            }
        }
        m
    });
    table
        .get(pattern)
        .unwrap_or_else(|| panic!("unregistered regex separator: {pattern}"))
}

/// Python `str.splitlines()` 语义（不保留行尾）。
pub fn split_lines(content: &str) -> Vec<&str> {
    content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// 每行首字符的字节偏移。
pub(crate) fn line_offsets(lines: &[&str]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(lines.len());
    let mut position: u64 = 0;
    for line in lines {
        offsets.push(position);
        position += (line.len() + 1) as u64;
    }
    offsets
}

/// 二分定位字节偏移所属的 1-based 行。
pub(crate) fn line_of(offsets: &[u64], position: u64) -> u32 {
    let mut low = 0usize;
    let mut high = offsets.len() - 1;
    while low < high {
        let mid = (low + high + 1) / 2;
        if offsets[mid] <= position {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    (low + 1) as u32
}

#[async_trait]
impl super::Chunker for RecursiveChunker {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        self.chunk(content, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_meaningful_requires_alnum() {
        assert!(is_meaningful("abc"));
        assert!(is_meaningful("  é  "));
        assert!(!is_meaningful("  ×  ")); // × 是数学符号不是字母数字
        assert!(!is_meaningful("   \n\t"));
        assert!(!is_meaningful("/*+-"));
    }

    #[test]
    fn chunk_small_file_single_chunk() {
        let c = RecursiveChunker::new(6000, 200).unwrap();
        let chunks = c.chunk("fn main() {\n    println!(\"hi\");\n}\n", "a.rs");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert_eq!(chunks[0].content, "fn main() {\n    println!(\"hi\");\n}");
    }

    #[test]
    fn chunk_preamble_folded_into_first_span() {
        let c = RecursiveChunker::new(200, 20).unwrap();
        let content = "preamble line\n\nfn a() {\n}\n\nfn b() {\n}\n\nfn c() {\n}\n\nfn d() {\n}\n\nfn e() {\n}\n";
        let chunks = c.chunk(content, "x.py");
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].start_line, 1);
        // 连续覆盖：第一块到第二块之间无空洞
        for w in chunks.windows(2) {
            assert_eq!(w[1].start_line, w[0].end_line + 1);
        }
        assert_eq!(chunks.last().unwrap().end_line, 16);
        // 行区间文本逐字对齐
        let all: Vec<&str> = split_lines(content);
        for chunk in &chunks {
            assert_eq!(
                chunk.content,
                all[chunk.start_line as usize - 1..chunk.end_line as usize].join("\n")
            );
        }
    }

    #[test]
    fn empty_and_blank_content_return_empty() {
        let c = RecursiveChunker::new(6000, 200).unwrap();
        assert!(c.chunk("", "a.py").is_empty());
        assert!(c.chunk("\n\n\n", "a.py").is_empty());
    }

    #[test]
    fn oversized_line_is_skipped() {
        let long_line = "x".repeat(7000);
        let content = format!("ok\n{long_line}\nalso ok\n");
        let c = RecursiveChunker::new(6000, 200).unwrap();
        let chunks = c.chunk(&content, "a.txt");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].content, "ok");
        assert_eq!(chunks[1].content, "also ok");
    }
}
