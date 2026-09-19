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

use crate::related::RelatedSymbol;
use crate::search::SearchHit;

pub const HEADER: &str = "The following code sections were retrieved:";

/// 检索结果的质量提示（借鉴 BCE 的 weak/degraded note）：
/// 弱匹配被 padding 进窗口和「真的找到了实现」对外观相同，提示让 agent
/// 能区分「仓库里没有」和「找到了」；语义路缺席同理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetrievalNotes {
    /// 头部命中分低于阈值：本仓库可能没有实现，可能在别的仓库/服务里
    pub weak: bool,
    /// 语义通路未参与（嵌入服务冷却/故障）：结果仅词法/结构匹配
    pub degraded: bool,
    /// broad regime（探索型查询宽窗口）：长摘录已骨架化，省略段以标记行
    /// 引用真实行号区间，agent 据此跟进读取全文
    pub broad: bool,
}

impl RetrievalNotes {
    pub fn is_empty(&self) -> bool {
        !self.weak && !self.degraded && !self.broad
    }
}

/// 拼装带行号的检索结果文本。行号从 hit.start_line 起，逐行配 content 的内容。
pub fn format_retrieval(hits: &[SearchHit]) -> String {
    format_retrieval_full(hits, &RetrievalNotes::default(), &[])
}

/// 带质量提示的拼装：提示以 Note: 前缀置于头部之后，正文之前。
pub fn format_retrieval_with_notes(hits: &[SearchHit], notes: &RetrievalNotes) -> String {
    format_retrieval_full(hits, notes, &[])
}

/// 完整拼装：sections + related_symbols 块（借鉴 BCE format）。
/// `<related_symbols>` 追加在 sections 之后——评测脚本只解析 `Path: ` 行，
/// 不影响评分；价值在 agent 消费面（grep leads）。
pub fn format_retrieval_full(
    hits: &[SearchHit],
    notes: &RetrievalNotes,
    related: &[RelatedSymbol],
) -> String {
    let mut sections: Vec<String> = Vec::new();

    for hit in hits {
        let lines: Vec<&str> = hit.content.split('\n').collect();
        let mut formatted_lines: Vec<String> = Vec::with_capacity(lines.len());
        let mut offset = 0usize;
        for line_content in &lines {
            if let Some((_omitted, _first, last)) =
                crate::broad::elided_run(line_content, &hit.path)
            {
                // broad 骨架化的省略标记行：原样渲染（无行号），之后每行的真实
                // 行号从引用区间末端恢复——骨架内容行数少于原 span，若不重同步，
                // 首个标记之后的所有行号都会漂移
                formatted_lines.push((*line_content).to_string());
                offset = offset.max((last + 1).saturating_sub(hit.start_line as usize));
            } else {
                let lineno = hit.start_line as usize + offset;
                // Python f"{lineno:>6}\t{line}"
                formatted_lines.push(format!("{lineno:>6}\t{line_content}"));
                offset += 1;
            }
        }
        sections.push(format!(
            "Path: {}\nLines: {}-{}\n{}",
            hit.path,
            hit.start_line,
            hit.end_line,
            formatted_lines.join("\n")
        ));
    }

    if sections.is_empty() && related.is_empty() {
        return if notes.degraded {
            format!(
                "{HEADER}\nNote: the semantic index did not participate in this search (embedding backend unavailable); no keyword/structure match was found either. Retrying later may give better results."
            )
        } else {
            HEADER.to_string()
        };
    }
    let mut prefix = String::new();
    if notes.degraded {
        // 语义路静默缺席（嵌入服务故障/冷却中）从外面看和检索质量差无法区分；
        // 提示让调用方能区分平台故障和真的不匹配，稍后重试可能更好
        prefix.push_str(
            "Note: the semantic index did not participate in this search (embedding backend unavailable) — results are keyword/structure matches only and may miss conceptually related code. Retrying later may give better results.\n",
        );
    }
    if notes.weak {
        // 弱匹配被 padding 进窗口会被误读为「实现已找到」；提示真实代码可能在别的仓库/服务里
        prefix.push_str(
            "Note: no strongly matching code was found for this query. The fragments below are weak matches — the functionality you are looking for may not be implemented in this codebase (it could live in a separate repository or service).\n",
        );
    }
    if notes.broad {
        // broad 模式用覆盖换深度：不做此提示，被省略的骨架会被误读为「展示的行就是全部」
        prefix.push_str(
            "Note: exploratory query — the excerpts below are trimmed candidates from across the codebase. Long sections are elided; read the cited file paths and line ranges for full detail.\n",
        );
    }
    let mut out = if sections.is_empty() {
        // 无 sections 但有 hints（罕见：窗口全被 confidence floor 滤空）——
        // 仍输出 hints 块，它们不依赖 sections 存在
        format!("{HEADER}\n{prefix}")
    } else {
        format!("{HEADER}\n{prefix}{}", sections.join("\n\n"))
    };
    if !related.is_empty() {
        // grep leads：上下文引用但定义未入窗的符号，agent 可据此跟进检索
        out.push_str(
            "\n<related_symbols hint=\"referenced by the context above and defined in this codebase, but not shown; search for them to explore further\">\n",
        );
        for r in related {
            out.push_str(&format!(
                "  <symbol name=\"{}\" kind=\"{}\" path=\"{}\"/>\n",
                escape_attr(&r.name),
                escape_attr(&r.kind),
                escape_attr(&r.path)
            ));
        }
        out.push_str("</related_symbols>");
    }
    out
}

/// XML 属性转义（BCE html.EscapeString 的最小子集：路径/符号名可能含 & " < >）。
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

    #[test]
    fn weak_note_prepended() {
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "main.py".into(),
            content: "xxx".into(),
            score: 0.1,
            content_hash: String::new(),
            start_line: 1,
            end_line: 1,
        };
        let out = format_retrieval_with_notes(
            &[hit],
            &RetrievalNotes {
                weak: true,
                degraded: false,
                broad: false,
            },
        );
        assert!(out.contains("weak matches"));
        assert!(out.contains("separate repository or service"));
        assert!(out.contains("Path: main.py"));
        // 无提示时不注入任何 Note
        assert!(!format_retrieval(&[]).contains("Note:"));
    }

    #[test]
    fn degraded_note_prepended() {
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "main.py".into(),
            content: "xxx".into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 1,
        };
        let out = format_retrieval_with_notes(
            &[hit],
            &RetrievalNotes {
                weak: false,
                degraded: true,
                broad: false,
            },
        );
        assert!(out.contains("semantic index did not participate"));
        assert!(out.contains("keyword/structure matches only"));
    }

    #[test]
    fn empty_hits_with_degraded_note() {
        let out = format_retrieval_with_notes(
            &[],
            &RetrievalNotes {
                weak: false,
                degraded: true,
                broad: false,
            },
        );
        assert!(out.starts_with(HEADER));
        assert!(out.contains("semantic index did not participate"));
    }

    #[test]
    fn related_symbols_block_appended_after_sections() {
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "main.py".into(),
            content: "xxx".into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 1,
        };
        let related = vec![crate::related::RelatedSymbol {
            name: "TokenRefresher".into(),
            kind: "definition".into(),
            path: "src/token.rs".into(),
        }];
        let out = format_retrieval_full(&[hit.clone()], &RetrievalNotes::default(), &related);
        // sections 在前，hints 在后；Path: 行仍是唯一的路径信号源（评测兼容）
        let path_pos = out.find("Path: main.py").unwrap();
        let rel_pos = out.find("<related_symbols").unwrap();
        assert!(rel_pos > path_pos);
        assert!(out.contains(
            "<symbol name=\"TokenRefresher\" kind=\"definition\" path=\"src/token.rs\"/>"
        ));
        assert!(out.ends_with("</related_symbols>"));
        // 无 hints 时不追加块（与既有输出完全一致）
        assert!(!format_retrieval(&[hit.clone()]).contains("related_symbols"));
    }

    #[test]
    fn related_symbols_attrs_escaped() {
        let related = vec![crate::related::RelatedSymbol {
            name: "a<&>b".into(),
            kind: "definition".into(),
            path: "src/a&b.rs".into(),
        }];
        let out = format_retrieval_full(&[], &RetrievalNotes::default(), &related);
        assert!(out.contains("name=\"a&lt;&amp;&gt;b\""));
        assert!(out.contains("path=\"src/a&amp;b.rs\""));
    }

    #[test]
    fn broad_note_prepended() {
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "main.py".into(),
            content: "xxx".into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 1,
        };
        let out = format_retrieval_with_notes(
            &[hit],
            &RetrievalNotes {
                weak: false,
                degraded: false,
                broad: true,
            },
        );
        assert!(out.contains("exploratory query"));
        assert!(out.contains("read the cited file paths and line ranges"));
        assert!(out.contains("Path: main.py"));
        // 默认 notes 不注入 broad 提示
        assert!(!format_retrieval(&[]).contains("exploratory query"));
    }

    #[test]
    fn elision_marker_resyncs_line_numbers() {
        // 骨架化后的 content：头 2 行 + 标记（省略 3 行：3-5）+ 第 6 行。
        // 若不重同步，"pub fn six" 会被编成 3；重同步后应编成 6。
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "src/api.rs".into(),
            content: "line one\nline two\n... (3 lines omitted, read src/api.rs:3-5)\nline six"
                .into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 1,
            end_line: 6,
        };
        let out = format_retrieval(&[hit]);
        assert!(out.contains("     1\tline one"));
        assert!(out.contains("     2\tline two"));
        // 标记行原样渲染，不带行号前缀
        assert!(out.contains("\n... (3 lines omitted, read src/api.rs:3-5)"));
        // 省略段之后的行恢复真实行号
        assert!(out.contains("     6\tline six"));
        assert!(!out.contains("     3\tline six"));
    }

    #[test]
    fn ordinary_content_lines_are_never_treated_as_markers() {
        // 内容行恰好含省略样式文本但引用不同路径/算术不自洽：按普通行编号
        let hit = SearchHit {
            blob_name: "b".into(),
            path: "src/api.rs".into(),
            content: "x\n... (3 lines omitted, read src/other.rs:3-5)\ny".into(),
            score: 0.5,
            content_hash: String::new(),
            start_line: 7,
            end_line: 9,
        };
        let out = format_retrieval(&[hit]);
        // 第二行按连续行号 8 编号（未识别为标记）
        assert!(out.contains("     8\t... (3 lines omitted, read src/other.rs:3-5)"));
        assert!(out.contains("     9\ty"));
    }
}
