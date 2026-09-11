//! Formatter — 拼装 formatted_retrieval。与 Python `domain/services/formatter.py` 对齐。
//!
//! 输出形态：
//! ```text
//! The following code sections were retrieved:
//! Path: main.py
//! Lines: 1-3
//!      1\txxx
//!      2\t...
//! ```
//!
//! chunk 原文直接取自 SearchHit.content；每个 hit 独立片段，保留 score 排序。

use crate::search::SearchHit;

pub const HEADER: &str = "The following code sections were retrieved:";

/// 拼装带行号的检索结果文本。行号从 hit.start_line 起，逐行配 content 的内容。
pub fn format_retrieval(hits: &[SearchHit]) -> String {
    let mut sections: Vec<String> = Vec::new();

    for hit in hits {
        let lines: Vec<&str> = hit.content.split('\n').collect();
        let mut formatted_lines: Vec<String> = Vec::with_capacity(lines.len());
        for (offset, line_content) in lines.iter().enumerate() {
            let lineno = hit.start_line as usize + offset;
            // Python f"{lineno:>6}\t{line}"
            formatted_lines.push(format!("{lineno:>6}\t{line_content}"));
        }
        sections.push(format!(
            "Path: {}\nLines: {}-{}\n{}",
            hit.path,
            hit.start_line,
            hit.end_line,
            formatted_lines.join("\n")
        ));
    }

    if sections.is_empty() {
        return HEADER.to_string();
    }
    format!("{HEADER}\n{}", sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hits_return_header_only() {
        assert_eq!(format_retrieval(&[]), HEADER);
    }

    #[test]
    fn renders_numbered_sections() {
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "main.py".into(),
            content: "xxx\n...".into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 2,
        };
        let out = format_retrieval(&[hit]);
        assert!(out.starts_with(HEADER));
        assert!(out.contains("Path: main.py\nLines: 1-2\n"));
        assert!(out.contains("     1\txxx"));
        assert!(out.contains("     2\t..."));
    }
}
