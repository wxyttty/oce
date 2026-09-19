//! Vue / Svelte 单文件组件切块器。与 Python `chunkers/vue_chunker.py` 对齐。
//!
//! 按 `<template|script|style>` 顶层块定位边界，不把标签和内容拆开；
//! Svelte 额外把 script/style 之外的有内容根级文本视为 markup。
//! 主块（template/script/markup）满足预算时合并为一个 chunk；
//! style 块独立产出。

use super::recursive::{is_meaningful, split_lines};
use super::spans::{cap_span, char_len, trim_trailing_blank_lines};
use super::types::Chunk;
use super::Chunker;
use async_trait::async_trait;
use regex::Regex;
use std::sync::OnceLock;

pub const DEFAULT_MAX_CHUNK_CHARS: usize = 6_000;

/// (tag, start_line, end_line) 1-based 闭区间。
type Section = (String, u32, u32);

#[derive(Clone)]
pub struct VueChunker {
    fallback: std::sync::Arc<dyn Chunker>,
    max_chunk_chars: usize,
}

fn section_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<\s*(?P<closing>/)?\s*(?P<tag>template|script|style)\b[^>]*>").unwrap()
    })
}

impl VueChunker {
    pub fn new(
        fallback: std::sync::Arc<dyn Chunker>,
        max_chunk_chars: usize,
    ) -> Result<Self, String> {
        if max_chunk_chars == 0 {
            return Err("max_chunk_chars 必须 > 0".into());
        }
        Ok(Self {
            fallback,
            max_chunk_chars,
        })
    }

    fn language(path: &str) -> &'static str {
        let lower = path.to_lowercase();
        let basename = lower.rsplit('/').next().unwrap_or(&lower);
        if std::path::Path::new(basename)
            .extension()
            .is_some_and(|e| e == "svelte")
        {
            "svelte"
        } else {
            "vue"
        }
    }

    /// 定位完整的顶层 SFC 块（1-based 闭区间行）。未闭合块 → ValueError → fallback。
    fn locate_sections(content: &str) -> Result<Vec<Section>, ()> {
        let newline_offsets: Vec<usize> = content
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .map(|(i, _)| i)
            .collect();
        let mut sections: Vec<Section> = Vec::new();
        let mut active_tag: Option<String> = None;
        let mut active_start = 0usize;
        let mut template_depth = 0u32;

        for caps in section_tag_re().captures_iter(content) {
            let closing = caps.name("closing").is_some();
            let tag = caps["tag"].to_lowercase();
            match &active_tag {
                None => {
                    if closing {
                        continue;
                    }
                    active_tag = Some(tag);
                    active_start = caps.get(0).unwrap().start();
                    template_depth = 1;
                }
                Some(active) => {
                    if tag != *active {
                        continue;
                    }
                    if active == "template" && !closing {
                        template_depth += 1;
                        continue;
                    }
                    if !closing {
                        continue;
                    }
                    template_depth -= 1;
                    if template_depth > 0 {
                        continue;
                    }
                    let start_line = count_le(&newline_offsets, active_start) + 1;
                    let end_line = count_le(&newline_offsets, caps.get(0).unwrap().end() - 1) + 1;
                    sections.push((active.clone(), start_line as u32, end_line as u32));
                    active_tag = None;
                }
            }
        }
        if let Some(tag) = active_tag {
            tracing::error!("unclosed <{tag}> section");
            return Err(());
        }
        Ok(sections)
    }

    /// Svelte：script/style 之外有内容的根级行段视为 markup。
    fn locate_svelte_markup(lines: &[&str], sections: &[Section]) -> Vec<Section> {
        let mut excluded = std::collections::HashSet::new();
        for (tag, start, end) in sections {
            if tag == "script" || tag == "style" {
                excluded.extend(*start..=*end);
            }
        }
        let mut markup: Vec<Section> = Vec::new();
        let mut start: Option<u32> = None;
        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx as u32 + 1;
            let available = !excluded.contains(&line_no) && !line.trim().is_empty();
            if available && start.is_none() {
                start = Some(line_no);
            }
            if let Some(s) = start {
                if excluded.contains(&line_no) || line_no == lines.len() as u32 {
                    let mut end = if excluded.contains(&line_no) {
                        line_no - 1
                    } else {
                        line_no
                    };
                    while end >= s && lines[end as usize - 1].trim().is_empty() {
                        end -= 1;
                    }
                    if end >= s {
                        markup.push(("markup".into(), s, end));
                    }
                    start = None;
                }
            }
        }
        markup
    }

    fn emit(&self, sections: &[Section], lines: &[&str], path: &str, language: &str) -> Vec<Chunk> {
        let styles: Vec<&Section> = sections.iter().filter(|(t, _, _)| t == "style").collect();
        let primary: Vec<&Section> = sections.iter().filter(|(t, _, _)| t != "style").collect();

        let mut groups: Vec<(u32, u32, String)> =
            self.primary_groups(&primary, &styles, lines, language);
        groups.extend(styles.iter().map(|(_, s, e)| (*s, *e, "style".to_string())));
        groups.sort_by_key(|g| g.0);

        let mut chunks = Vec::new();
        for (start, end, section_type) in groups {
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
                    Some(format!("{language}:{section_type}")),
                ) {
                    chunks.push(chunk);
                }
            }
        }
        chunks
    }

    /// 主块合并规则：区间不与 style 交叉且总字符 ≤ 预算时合并为一个 chunk。
    fn primary_groups(
        &self,
        primary: &[&Section],
        styles: &[&Section],
        lines: &[&str],
        language: &str,
    ) -> Vec<(u32, u32, String)> {
        if primary.is_empty() {
            return vec![];
        }
        let start = primary.iter().map(|(_, s, _)| *s).min().unwrap();
        let end = primary.iter().map(|(_, _, e)| *e).max().unwrap();
        let crosses_style = styles.iter().any(|(_, ss, se)| *ss <= end && *se >= start);
        let tags: std::collections::HashSet<&str> =
            primary.iter().map(|(t, _, _)| t.as_str()).collect();
        let combined_type = if tags.len() == 1 {
            tags.into_iter().next().unwrap().to_string()
        } else if language == "vue" {
            "template+script".to_string()
        } else {
            "markup+script".to_string()
        };
        let combined_chars: usize = lines[start as usize - 1..end as usize]
            .iter()
            .map(|l| char_len(l) + 1)
            .sum::<usize>()
            - 1;
        if !crosses_style && combined_chars <= self.max_chunk_chars {
            return vec![(start, end, combined_type)];
        }
        primary
            .iter()
            .map(|(t, s, e)| (*s, *e, t.clone()))
            .collect()
    }
}

fn count_le(offsets: &[usize], value: usize) -> usize {
    offsets.partition_point(|&o| o <= value)
}

#[async_trait]
impl Chunker for VueChunker {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        if !is_meaningful(content) {
            return vec![];
        }
        let lines = split_lines(content);
        if lines.is_empty() {
            return vec![];
        }
        let language = Self::language(path);
        let sections = match Self::locate_sections(content) {
            Ok(s) => s,
            Err(()) => return self.fallback.chunk(content, path),
        };
        let mut sections = sections;
        if language == "svelte" {
            sections = sections
                .iter()
                .filter(|(t, _, _)| t == "script" || t == "style")
                .cloned()
                .collect();
            sections.extend(Self::locate_svelte_markup(&lines, &sections));
        }
        if sections.is_empty() {
            return self.fallback.chunk(content, path);
        }
        let chunks = self.emit(&sections, &lines, path, language);
        if chunks.is_empty() {
            self.fallback.chunk(content, path)
        } else {
            chunks
        }
    }
}
