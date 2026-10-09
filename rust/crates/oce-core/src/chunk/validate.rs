//! 切块不变式校验：坐标无损、span 不重叠、非空行不静默丢失。
//!
//! 目的不是"检查切块器写得对不对"，而是让**静默丢代码**变成一次可见的索引失败：
//! 只要某个 chunk 的正文与它声明的源码行不一致、两个 span 实质重叠、或一段非空源码
//! 行谁都没覆盖，就把该 blob 置 error，而不是安静地少索引一段实现。
//!
//! 分级（真实语料实测后确定，2026-10-06）：
//!
//! - **硬错误**（`validate_chunks` 返回 Err，索引失败）：正文与 span 不符、实质重叠
//!   （重叠宽度 > 1 行）、非空短行无人覆盖。
//! - **告警**（`inspect_chunks().boundary_overlaps`）：相邻两块恰好共享 1 行边界
//!   （前块 `end_line` == 后块 `start_line`），同一行会被索引两次。它是回归探针：
//!   STEP-11b 修完 cAST 边界后真语料已归零，再次出现说明切块器又退化了。
//!
//! 行模型与 `chunk::spans::slice_lines` / `formatter` 完全一致：`content.split('\n')`，
//! 1-based 闭区间，用 `join("\n")` 复现文本（因此以空行结尾的区间其正文行数少于
//! 声明行数是预期行为，不算不一致）。
//!
//! 覆盖检查放行一类行：**比所有已覆盖行都更长的非空行**。`cap_span` 会按字符预算
//! 跳过单行超预算的行（见 `chunk::spans::cap_span` 注释），这类行本来就不该被任何
//! span 覆盖；用"比已覆盖行都长"作为判据不需要把各切块器的预算一路透传进来。

use crate::chunk::spans::{char_len, slice_lines};
use crate::chunk::types::Chunk;

/// 校验结论：硬错误 + 允许存在的 1 行边界共享。
#[derive(Debug, Default, Clone)]
pub struct ChunkInspection {
    /// 必须让索引失败的问题（正文与 span 不符 / 实质重叠 ≥2 行）。
    pub hard: Vec<String>,
    /// 未被任何 span 覆盖的非空短行（1-based）。真语料实测 cc-switch 1/1031 命中，
    /// 属切块器漏行缺陷；修好之前只告警——把整个文件判 error 会让该文件从索引消失，
    /// 比少两行更糟（STEP-11b）。
    pub uncovered: Vec<usize>,
    /// 相邻 chunk 共享的边界行号（1 行重叠，仅告警）。
    pub boundary_overlaps: Vec<u32>,
}

impl ChunkInspection {
    pub fn is_ok(&self) -> bool {
        self.hard.is_empty()
    }
}

/// 逐条收集问题，不做短路——一次就能看到该文件所有坏点。
pub fn inspect_chunks(content: &str, chunks: &[Chunk]) -> ChunkInspection {
    let mut result = ChunkInspection::default();
    if chunks.is_empty() {
        return result;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let mut covered = vec![false; lines.len()];
    let mut longest_covered = 0usize;

    for chunk in chunks {
        let start = chunk.start_line as usize;
        let end = chunk.end_line as usize;
        if start < 1 || end < start || end > lines.len() {
            result.hard.push(format!(
                "chunk span {start}-{end} out of range (file has {} lines)",
                lines.len()
            ));
            continue;
        }
        let expected = slice_lines(&lines, chunk.start_line, chunk.end_line);
        if chunk.content != expected {
            result.hard.push(format!(
                "chunk {start}-{end} text does not match its source span"
            ));
        }
        for line_no in start..=end {
            covered[line_no - 1] = true;
            longest_covered = longest_covered.max(char_len(lines[line_no - 1]));
        }
    }

    // 重叠：按起点排序后逐对比较。恰好共享 1 行（prev.end == next.start）是良性边界，
    // 其余（真实交集 ≥ 2 行，或包含关系）是硬错误。
    let mut ordered: Vec<&Chunk> = chunks.iter().collect();
    ordered.sort_by_key(|chunk| (chunk.start_line, chunk.end_line));
    for pair in ordered.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        if next.start_line <= prev.end_line {
            let overlap = prev.end_line.min(next.end_line) - next.start_line + 1;
            if overlap == 1 && next.start_line == prev.end_line {
                result.boundary_overlaps.push(prev.end_line);
            } else {
                result.hard.push(format!(
                    "overlapping spans: [{}-{}] and [{}-{}] share {overlap} line(s)",
                    prev.start_line, prev.end_line, next.start_line, next.end_line
                ));
            }
        }
    }

    let uncovered: Vec<usize> = (1..=lines.len())
        .filter(|&line_no| {
            let line = lines[line_no - 1];
            !covered[line_no - 1]
                && !line.trim().is_empty()
                && char_len(line) <= longest_covered
        })
        .collect();
    if !uncovered.is_empty() {
        let sample: Vec<usize> = uncovered.iter().take(5).copied().collect();
        result.hard.push(format!(
            "{} non-blank source line(s) not covered by any span, first: {sample:?}",
            uncovered.len()
        ));
    }
    result.uncovered = uncovered;
    result
}

/// 硬错误校验：返回第一条不满足的不变式。1 行边界共享不算失败（见模块注释）。
pub fn validate_chunks(content: &str, chunks: &[Chunk]) -> Result<(), String> {
    let inspection = inspect_chunks(content, chunks);
    match inspection.hard.first() {
        Some(reason) => Err(reason.clone()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_for(content: &str, start: u32, end: u32) -> Chunk {
        let lines: Vec<&str> = content.split('\n').collect();
        Chunk::new(
            "a".repeat(64),
            "src/lib.rs",
            slice_lines(&lines, start, end),
            start,
            end,
            Some("function".to_string()),
        )
        .expect("valid chunk")
    }

    #[test]
    fn validate_accepts_python_source() {
        let content = "import os\n\n\ndef f():\n    return os.getcwd()\n\n\nclass A:\n    def m(self):\n        return 1\n";
        let chunks = vec![chunk_for(content, 1, 2), chunk_for(content, 3, 10)];
        assert_eq!(validate_chunks(content, &chunks), Ok(()));
    }

    #[test]
    fn validate_rejects_text_mismatch() {
        let content = "alpha\nbeta\ngamma\n";
        let mut bad = chunk_for(content, 1, 2);
        bad.content = "alpha\nBETA".to_string();
        let err = validate_chunks(content, &[bad]).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn validate_rejects_overlapping_spans() {
        // 交集 2 行（2-3）：实质重叠，必须失败
        let content = "a\nb\nc\nd\n";
        let chunks = vec![chunk_for(content, 1, 3), chunk_for(content, 2, 4)];
        let inspection = inspect_chunks(content, &chunks);
        assert!(!inspection.is_ok(), "2 行交集必须判硬错误");
        assert!(inspection.hard[0].contains("overlapping"), "{:?}", inspection.hard);
        assert!(inspection.boundary_overlaps.is_empty());
    }

    #[test]
    fn boundary_overlap_is_reported_but_not_fatal() {
        // 相邻两块恰好共享 1 行边界（cAST 实测形态）：只告警，不判失败
        let content = "header\nbody\nnext\n";
        let chunks = vec![chunk_for(content, 1, 2), chunk_for(content, 2, 3)];
        let inspection = inspect_chunks(content, &chunks);
        assert!(inspection.is_ok(), "{:?}", inspection.hard);
        assert_eq!(inspection.boundary_overlaps, vec![2]);
        assert_eq!(validate_chunks(content, &chunks), Ok(()));
    }

    #[test]
    fn validate_rejects_uncovered_nonblank_line() {
        // 第 2 行是非空短行却没人覆盖（且已有更长的覆盖行）→ 硬错误：
        // 这正是 STEP-11b 修的递归切块器尾部丢行（真语料曾 1/1031 命中）
        let content = "a longer covered line here\nDROPPED\nc\n";
        let chunks = vec![chunk_for(content, 1, 1), chunk_for(content, 3, 3)];
        let inspection = inspect_chunks(content, &chunks);
        assert_eq!(inspection.uncovered, vec![2]);
        assert!(!inspection.is_ok(), "漏行必须判索引失败");
        let err = validate_chunks(content, &chunks).unwrap_err();
        assert!(err.contains("not covered"), "{err}");
    }

    #[test]
    fn validate_tolerates_skipped_overlong_line() {
        // cap_span 按预算跳过的单行超长内容：未覆盖但比所有已覆盖行都长 → 放行
        let long = "x".repeat(500);
        let content = format!("short\n{long}\nalso short\n");
        let chunks = vec![chunk_for(&content, 1, 1), chunk_for(&content, 3, 3)];
        assert_eq!(validate_chunks(&content, &chunks), Ok(()));
    }
}
