//! Markdown 结构切块器。与 Python `infrastructure/chunkers/markdown_chunker.py` 对齐。
//!
//! 结构边界来自 ExperimentalMarkdownSyntaxTextSplitter 的移植（标题层级/围栏代码块/
//! 水平线）；chunk 文本仍从源码行切割，保证行号逐字对齐。
//! 过小的 section 向后合并：参考文档标题密集，每个标题一两个句子会稀释索引。

use super::recursive::{is_meaningful, split_lines};
use super::spans::{cap_span, char_len, trim_trailing_blank_lines};
use super::types::Chunk;
use super::Chunker;
use async_trait::async_trait;

pub const DEFAULT_MAX_CHUNK_CHARS: usize = 6_000;
/// 低于此大小的 section 向后合并。
pub const DEFAULT_MIN_CHUNK_CHARS: usize = 700;

#[derive(Clone)]
pub struct MarkdownChunker {
    fallback: std::sync::Arc<dyn Chunker>,
    max_chunk_chars: usize,
    min_chunk_chars: usize,
}

impl MarkdownChunker {
    pub fn new(
        fallback: std::sync::Arc<dyn Chunker>,
        max_chunk_chars: usize,
        min_chunk_chars: usize,
    ) -> Result<Self, String> {
        if max_chunk_chars == 0 {
            return Err("max_chunk_chars 必须 > 0".into());
        }
        if min_chunk_chars >= max_chunk_chars {
            return Err("min_chunk_chars 必须 ∈ [0, max_chunk_chars)".into());
        }
        Ok(Self {
            fallback,
            max_chunk_chars,
            min_chunk_chars,
        })
    }

    /// 把源文本切成 section 文本（逐字子串；标题行被 strip_headers 语义排除）。
    /// 状态机与 langchain ExperimentalMarkdownSyntaxTextSplitter.split_text 一致。
    fn section_texts(content: &str) -> Vec<String> {
        let mut sections: Vec<String> = Vec::new();
        let mut current = String::new();
        let raw_lines = split_lines(content);

        let mut iter = raw_lines.iter().copied().peekable();
        while let Some(line) = iter.next() {
            // 注意：langchain 用 splitlines(keepends=True)，行尾换行保留；
            // 水平线匹配要求行尾有 \n。此处用「是否还有下一行」判断行尾换行。
            let has_newline = iter.peek().is_some();
            if let Some(depth) = match_header(line) {
                if (1..=3).contains(&depth) {
                    complete_chunk(&mut sections, &mut current);
                    continue; // strip_headers=True：标题行不进入正文
                }
            }
            if let Some(_fence) = match_code_fence(line) {
                complete_chunk(&mut sections, &mut current);
                // 围栏块整体为独立 section：收集到配对围栏或文件尾。
                // 注：langchain 对未闭合围栏返回空串（内容丢弃），此处保留内容，
                // 行号 tile 语义不变。
                let mut block = format!("{line}\n");
                for inner in iter.by_ref() {
                    block.push_str(inner);
                    block.push('\n');
                    if match_code_fence(inner).is_some() {
                        break;
                    }
                }
                sections.push(block);
                continue;
            }
            if has_newline && match_horizontal_rule(line) {
                complete_chunk(&mut sections, &mut current);
                continue;
            }
            current.push_str(line);
            current.push('\n');
        }
        complete_chunk(&mut sections, &mut current);
        sections
    }

    /// 定位每个 section 的起始行（含回溯标题行），再 tile、合并、产出。
    fn section_spans(&self, content: &str, lines: &[&str]) -> Vec<(u32, u32)> {
        let starts = self.section_start_lines(content, lines);
        if starts.is_empty() {
            return vec![];
        }
        let starts = drop_code_starts(starts, lines);
        let mut spans = tile(starts, lines.len());
        self.merge_short(&mut spans, lines);
        spans
    }

    /// 每个 section 的 1-based 起始行：定位 section 正文，回溯跳过空行后
    /// 认领其上的标题行。游标推进避免重复文本（如相同命令块）折叠到同一位置。
    fn section_start_lines(&self, content: &str, lines: &[&str]) -> Vec<u32> {
        let offsets = super::recursive::line_offsets(lines);
        let mut starts: Vec<u32> = Vec::new();
        let mut cursor = 0usize;
        for section in Self::section_texts(content) {
            let text = section.trim();
            if text.is_empty() {
                continue;
            }
            let found = content[cursor..].find(text).map(|i| i + cursor);
            let found = match found {
                Some(f) => f,
                None => return starts, // 对应 Python 抛 ValueError → fallback
            };
            cursor = found + text.len();
            let body_line = super::recursive::line_of(&offsets, found as u64);
            starts.push(claim_heading(lines, body_line));
        }
        starts
    }

    /// 过小的 section 向后合并，保留外层标题在合并 span 顶部。
    fn merge_short(&self, spans: &mut Vec<(u32, u32)>, lines: &[&str]) {
        if self.min_chunk_chars == 0 {
            return;
        }
        let original = spans.clone();
        spans.clear();
        let mut pending_start: Option<u32> = None;
        for (start, end) in &original {
            let current_start = pending_start.unwrap_or(*start);
            let size: usize = (current_start as usize..=*end as usize)
                .map(|row| char_len(lines[row - 1]) + 1)
                .sum();
            if size < self.min_chunk_chars {
                pending_start = Some(current_start);
                continue;
            }
            spans.push((current_start, *end));
            pending_start = None;
        }
        if let Some(pending) = pending_start {
            if let Some(last) = spans.last_mut() {
                *last = (last.0, original.last().unwrap().1);
            } else {
                spans.push((pending, original.last().unwrap().1));
            }
        }
    }

    fn emit(&self, spans: Vec<(u32, u32)>, lines: &[&str], path: &str) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        for (start, end) in spans {
            let trimmed = trim_trailing_blank_lines(lines, start, end);
            for (span_start, span_end, text) in
                cap_span(lines, start, trimmed, self.max_chunk_chars)
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
                    Some("markdown".into()),
                ) {
                    chunks.push(chunk);
                }
            }
        }
        chunks
    }
}

/// 丢弃会以围栏标记开头的边界：围栏块保持与引入它的 section 相连。
fn drop_code_starts(starts: Vec<u32>, lines: &[&str]) -> Vec<u32> {
    starts
        .into_iter()
        .filter(|s| {
            let line = lines[*s as usize - 1];
            let t = line.trim_start();
            !(t.starts_with("```") || t.starts_with("~~~"))
        })
        .collect()
}

/// section 起始行 → 连续区间；首个 section 从第 1 行起（preamble 并入第一块）。
fn tile(starts: Vec<u32>, total_lines: usize) -> Vec<(u32, u32)> {
    let mut ordered: Vec<u32> = starts
        .into_iter()
        .filter(|s| (1..=total_lines as u32).contains(s))
        .collect();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.is_empty() {
        return vec![];
    }
    let mut spans = Vec::with_capacity(ordered.len());
    for (i, start) in ordered.iter().enumerate() {
        let start = if i == 0 { 1 } else { *start };
        let end = if i + 1 < ordered.len() {
            ordered[i + 1] - 1
        } else {
            total_lines as u32
        };
        if end >= start {
            spans.push((start, end));
        }
    }
    spans
}

/// 从 section 正文回溯到引入它的标题行。
fn claim_heading(lines: &[&str], body_line: u32) -> u32 {
    let mut candidate = body_line as i64 - 1;
    while candidate >= 1 && lines[candidate as usize - 1].trim().is_empty() {
        candidate -= 1;
    }
    if candidate >= 1 && lines[candidate as usize - 1].trim_start().starts_with('#') {
        return candidate as u32;
    }
    body_line
}

/// `^(#{1,6}) (.*)`；返回标题深度。只匹配配置的层级时才真正拆分。
fn match_header(line: &str) -> Option<usize> {
    let hash_count = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hash_count) {
        let rest = &line[hash_count..];
        if let Some(after) = rest.strip_prefix(' ') {
            let _ = after;
            return Some(hash_count);
        }
    }
    None
}

/// `^```(.*)` / `^~~~(.*)`；返回围栏标记。
fn match_code_fence(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `^\*\*\*+\n` / `^---+\n` / `^___+\n`（langchain 语义：匹配要求行尾换行符）。
fn match_horizontal_rule(line: &str) -> bool {
    let (marker, rest) = if let Some(r) = line.strip_prefix('*') {
        ('*', r)
    } else if let Some(r) = line.strip_prefix('-') {
        ('-', r)
    } else if let Some(r) = line.strip_prefix('_') {
        ('_', r)
    } else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c == marker)
}

fn complete_chunk(sections: &mut Vec<String>, current: &mut String) {
    if !current.trim().is_empty() {
        sections.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

#[async_trait]
impl Chunker for MarkdownChunker {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        if !is_meaningful(content) {
            return vec![];
        }
        let lines = split_lines(content);
        if lines.is_empty() {
            return vec![];
        }
        let spans = self.section_spans(content, &lines);
        if spans.is_empty() {
            return self.fallback.chunk(content, path);
        }
        self.emit(spans, &lines, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::RecursiveChunker;

    fn fallback() -> std::sync::Arc<dyn Chunker> {
        std::sync::Arc::new(RecursiveChunker::new(6000, 200).unwrap())
    }

    #[test]
    fn splits_on_headings_and_keeps_fence_atomic() {
        let chunker = MarkdownChunker::new(fallback(), 6000, 700).unwrap();
        let content = "# Title\n\nintro text\n\n```python\n# not a heading\ncode()\n```\n\n## Section\n\nbody\n";
        let chunks = chunker.chunk(content, "a.md");
        assert_eq!(chunks.len(), 1); // 700 字符下限 → 全部合并
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 12);
    }

    #[test]
    fn fence_not_mistaken_as_heading_boundary() {
        let chunker = MarkdownChunker::new(fallback(), 6000, 0).unwrap();
        let content = "# A\n\n```sh\n# comment inside fence\necho hi\n```\n\n# B\n\nsecond\n";
        let chunks = chunker.chunk(content, "a.md");
        let starts: Vec<u32> = chunks.iter().map(|c| c.start_line).collect();
        // 围栏内 `# comment` 不得成为边界
        assert_eq!(starts, vec![1, 8]);
        assert!(chunks[0].content.contains("# comment inside fence"));
    }
}
