//! 相邻 span 合并 + 小片段补全（semble_rs M1 移植，源自 BCE curate.go 的
//! adjacent-span merging / small-span padding）。
//!
//! 同文件的两个选中片段相距 ≤ [`MERGE_GAP_LINES`] 行时合并成一个连续段；
//! 合并后（或单个）片段不足 [`PAD_MIN_LINES`] 行时两侧各补 [`PAD_LINES`] 行
//! 上下文。内容从 blob 的 chunk 行重构——**行号永不撒谎**：请求区间内任何
//! 一行缺失（chunk 覆盖有洞，实测 70/221 blob 存在内部空洞）整组回退原样
//! 保留，绝不用猜测的行文本充数。
//!
//! 回填机制（M1 教训）：合并让窗口缩短、补全让内容变长，两者都必须在
//! **截断之前**发生——调用方用 `select_k + MERGE_POOL_EXTRA` 的池 select，
//! 合并后按预算 + `select_k` 收口，融合腾出的席位由次优排名回填。

use crate::search::SearchHit;
use std::collections::{BTreeMap, HashMap};

/// 同文件片段视作相邻的最大间隔行数（BCE mergeGapLines）。
pub const MERGE_GAP_LINES: u32 = 2;
/// 不足此行数的（合并后）片段触发两侧补全（BCE expandMinLines）。
pub const PAD_MIN_LINES: u32 = 6;
/// 补全时两侧各扩的行数（BCE expandPadLines）。
pub const PAD_LINES: u32 = 3;
/// select 池相对最终窗口的加宽量：合并缩窗后由次优排名回填（semble
/// DEDUP_POOL_EXTRA 同值）。
pub const MERGE_POOL_EXTRA: usize = 8;

/// blob 的行文本（行号 1 起，按行号升序）。由调用方从存储层重构。
pub type BlobLines = Vec<(u32, String)>;

/// 合并 + 补全。`lines_by_blob` 提供需要重构内容的 blob 行文本；某 blob 缺行
/// 或未提供时，该 blob 的组回退为原样保留。返回按 best rank 排序的合并结果
/// （顺序与入参中的最佳排名一致），**不做**条数/预算收口（调用方做）。
pub fn merge_and_pad_spans(
    hits: &[SearchHit],
    lines_by_blob: &HashMap<String, BlobLines>,
) -> Vec<SearchHit> {
    if hits.is_empty() {
        return Vec::new();
    }
    // 逐 blob 聚类，同时记住组内最佳 rank（输出顺序与入选顺序对齐）
    let mut by_blob: HashMap<&str, Vec<(usize, &SearchHit)>> = HashMap::new();
    for (rank, hit) in hits.iter().enumerate() {
        by_blob
            .entry(hit.blob_name.as_str())
            .or_default()
            .push((rank, hit));
    }
    let mut jobs: Vec<(&str, Vec<(usize, &SearchHit)>)> = by_blob.into_iter().collect();
    for (_, ranked) in jobs.iter_mut() {
        ranked.sort_by_key(|(rank, hit)| (hit.start_line, *rank));
    }

    let mut out: Vec<(usize, SearchHit)> = Vec::new();
    for (blob_name, ranked) in jobs {
        // 按相邻性聚组
        let mut clusters: Vec<Vec<(usize, &SearchHit)>> = vec![vec![ranked[0]]];
        for entry in &ranked[1..] {
            let last = clusters
                .last()
                .expect("non-empty")
                .last()
                .expect("non-empty")
                .1;
            let adjacent = entry.1.start_line <= last.end_line.saturating_add(1 + MERGE_GAP_LINES);
            if adjacent {
                clusters.last_mut().expect("non-empty").push(*entry);
            } else {
                clusters.push(vec![*entry]);
            }
        }
        // 需要内容重构的情形：任何簇含 ≥2 个片段（合并），或存在 <6 行的
        // 片段（补全）。单片段且行数达标的 blob 不做行查询。
        let needs_lines = clusters
            .iter()
            .any(|c| c.len() > 1 || (c[0].1.end_line - c[0].1.start_line + 1) < PAD_MIN_LINES);
        if !needs_lines {
            for (rank, hit) in clusters.into_iter().flatten() {
                out.push((rank, hit.clone()));
            }
            continue;
        }
        let Some(lines) = lines_by_blob.get(blob_name) else {
            for (rank, hit) in clusters.into_iter().flatten() {
                out.push((rank, hit.clone()));
            }
            continue;
        };
        let line_map: BTreeMap<u32, &str> = lines
            .iter()
            .map(|(no, text)| (*no, text.as_str()))
            .collect();
        for cluster in clusters {
            let best_rank = cluster
                .iter()
                .map(|(rank, _)| *rank)
                .min()
                .expect("non-empty");
            let best = cluster
                .iter()
                .min_by_key(|(rank, _)| *rank)
                .expect("non-empty")
                .1;
            let start = cluster
                .iter()
                .map(|(_, h)| h.start_line)
                .min()
                .expect("non-empty");
            let end = cluster
                .iter()
                .map(|(_, h)| h.end_line)
                .max()
                .expect("non-empty");
            // 补全：合并后仍过短则两侧各扩 PAD_LINES，钳到文件边界
            let (padded_start, padded_end) = if end - start + 1 < PAD_MIN_LINES {
                (start.saturating_sub(PAD_LINES).max(1), end + PAD_LINES)
            } else {
                (start, end)
            };
            // 行完整性：区间内任何一行缺失（chunk 覆盖有洞/文件越界）→ 整组回退
            let complete = padded_end >= padded_start
                && (padded_start..=padded_end).all(|no| line_map.contains_key(&no));
            if !complete {
                for (rank, hit) in &cluster {
                    out.push((*rank, (*hit).clone()));
                }
                continue;
            }
            let content: Vec<&str> = (padded_start..=padded_end)
                .map(|no| *line_map.get(&no).expect("checked complete"))
                .collect();
            let mut merged = best.clone();
            merged.start_line = padded_start;
            merged.end_line = padded_end;
            merged.content = content.join("\n");
            out.push((best_rank, merged));
        }
    }
    out.sort_by_key(|(rank, _)| *rank);
    out.into_iter().map(|(_, hit)| hit).collect()
}

/// 收口：按顺序保留前 `keep_k` 条、且总字符数不超 `max_chars`（预算硬限制，
/// 放不下的整条跳过、继续尝试后面的小条——与 selector 的预算语义一致）。
pub fn enforce_budget(hits: Vec<SearchHit>, keep_k: usize, max_chars: usize) -> Vec<SearchHit> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for hit in hits {
        if out.len() >= keep_k {
            break;
        }
        let chars = crate::chunk::spans::char_len(&hit.content);
        if !out.is_empty() && used + chars > max_chars {
            continue;
        }
        used += chars;
        out.push(hit);
    }
    out
}

/// 调用方预取内容时需要的 blob 名单：有 ≥2 个片段的同 blob，或存在
/// 不足 PAD_MIN_LINES 行的片段（可能触发补全）。
pub fn blobs_needing_lines(hits: &[SearchHit]) -> Vec<String> {
    let mut by_blob: HashMap<&str, Vec<&SearchHit>> = HashMap::new();
    for hit in hits {
        by_blob.entry(hit.blob_name.as_str()).or_default().push(hit);
    }
    let mut names: Vec<String> = Vec::new();
    for (blob, group) in by_blob {
        let needs = group.len() > 1
            || group
                .iter()
                .any(|h| h.end_line - h.start_line + 1 < PAD_MIN_LINES);
        if needs {
            names.push(blob.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(blob: &str, path: &str, start: u32, end: u32, score: f32) -> SearchHit {
        SearchHit {
            blob_name: blob.into(),
            path: path.into(),
            content: format!("lines {start}..{end}"),
            score,
            content_hash: String::new(),
            start_line: start,
            end_line: end,
        }
    }

    fn lines_map(blob: &str, total: u32) -> HashMap<String, BlobLines> {
        let mut m = HashMap::new();
        m.insert(
            blob.to_string(),
            (1..=total)
                .map(|i| (i, format!("line {i}")).into())
                .collect::<Vec<(u32, String)>>(),
        );
        m
    }

    #[test]
    fn adjacent_spans_merge_into_one_contiguous_section() {
        // 同文件两段相距 2 行（50-52 空档 ≤ MERGE_GAP_LINES）：合并为一个连续段
        let hits = vec![
            hit("b1", "src/app.py", 40, 49, 0.9),
            hit("b1", "src/app.py", 52, 60, 0.7),
            hit("b2", "src/other.py", 1, 10, 0.5),
        ];
        let out = merge_and_pad_spans(&hits, &lines_map("b1", 100));
        // b1 两段合一（40..60，行全有）；b2 十行 ≥ PAD_MIN_LINES 原样
        assert_eq!(out.len(), 2);
        let merged = out.iter().find(|h| h.blob_name == "b1").unwrap();
        assert_eq!((merged.start_line, merged.end_line), (40, 60));
        assert_eq!(merged.content.lines().count(), 21);
        // 顺序：b1 的 best rank 0 在 b2（rank 2）前
        assert_eq!(out[0].blob_name, "b1");
        // gap 3 行不合并
        let hits = vec![
            hit("b1", "a.rs", 40, 49, 0.9),
            hit("b1", "a.rs", 53, 60, 0.7),
        ];
        let out = merge_and_pad_spans(&hits, &lines_map("b1", 100));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn small_span_padded_both_sides_clamped() {
        let hits = vec![hit("b1", "a.rs", 2, 3, 0.9)];
        let out = merge_and_pad_spans(&hits, &lines_map("b1", 100));
        // 2 行 < 6：两侧各补 3 → 1..6（起点钳到第 1 行）
        let merged = &out[0];
        assert_eq!((merged.start_line, merged.end_line), (1, 6));
        assert!(merged.content.starts_with("line 1"));
        // 文件尾部钳位
        let hits = vec![hit("b1", "a.rs", 96, 97, 0.9)];
        let out = merge_and_pad_spans(&hits, &lines_map("b1", 100));
        assert_eq!((out[0].start_line, out[0].end_line), (93, 100));
    }

    #[test]
    fn missing_lines_fall_back_to_original_spans() {
        // chunk 覆盖有洞（49-52 缺）：合并区间不完整 → 回退原样，行号不撒谎
        let mut m = HashMap::new();
        m.insert(
            "b1".to_string(),
            (1..=100)
                .filter(|i| !(49..=52).contains(i))
                .map(|i| (i, format!("line {i}")))
                .collect::<Vec<(u32, String)>>(),
        );
        let hits = vec![
            hit("b1", "a.rs", 40, 49, 0.9),
            hit("b1", "a.rs", 52, 60, 0.7),
        ];
        let out = merge_and_pad_spans(&hits, &m);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_line, 40);
        assert_eq!(out[1].start_line, 52);
    }

    #[test]
    fn pool_backfill_restores_window_size() {
        // 池 5 条、合并缩为 3 条；enforce_budget 收口 keep_k=4 时从池的次优回填
        // ——本函数只收口，回填由"池含更多候选"保证：这里验证收口语义
        let pool = vec![
            hit("b1", "a.rs", 1, 10, 0.9),
            hit("b1", "a.rs", 11, 20, 0.85),
            hit("b2", "b.rs", 1, 10, 0.8),
            hit("b3", "c.rs", 1, 10, 0.7),
            hit("b4", "d.rs", 1, 10, 0.6),
        ];
        let merged = merge_and_pad_spans(&pool, &lines_map("b1", 30));
        assert_eq!(merged.len(), 4, "b1 两段合一，其余原样");
        let kept = enforce_budget(merged, 4, 32_000);
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[3].blob_name, "b4", "池内第 5 条回填进窗口");
    }

    #[test]
    fn budget_is_hard_limit_after_padding() {
        // 补全让内容变长：收口时放不下的整条跳过（首条不受预算约束，与
        // selector 语义一致）。构造：小 hit 居首，补全后的大 hit 其次。
        let mut m = HashMap::new();
        m.insert(
            "b1".to_string(),
            (1..=10)
                .map(|i| (i, "x".repeat(10)))
                .collect::<Vec<(u32, String)>>(),
        );
        let hits = vec![hit("b2", "b.rs", 1, 2, 0.95), hit("b1", "a.rs", 2, 3, 0.9)];
        let padded = merge_and_pad_spans(&hits, &m);
        // b2 无行数据原样（2 行）；b1 补成 1..6 = 60 字符
        assert_eq!(padded.len(), 2);
        assert_eq!(padded[0].blob_name, "b2");
        assert!(padded[1].content.len() > 40, "补全后内容变长");
        let kept = enforce_budget(padded, 2, 40);
        // b2(2行≈20c) 入窗；b1 补全后 60c 放不下（40-20 < 60）→ 跳过
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].blob_name, "b2");
    }

    #[test]
    fn blobs_needing_lines_targets_only_merge_candidates() {
        let hits = vec![
            hit("b1", "a.rs", 40, 49, 0.9),
            hit("b1", "a.rs", 52, 60, 0.7), // b1 两段 → 需要
            hit("b2", "b.rs", 1, 5, 0.8),   // b2 单段 < 6 行 → 需要（补全）
            hit("b3", "c.rs", 1, 30, 0.7),  // b3 单段 30 行 → 不需要
        ];
        assert_eq!(
            blobs_needing_lines(&hits),
            ["b1".to_string(), "b2".to_string()]
        );
    }
}
