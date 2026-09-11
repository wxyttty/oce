//! 覆盖度与预算感知的结果选择。与 Python `selector/coverage_selector.py` 对齐。
//!
//! 贪心填充：优先保证仓库覆盖度（第一轮每个文件各选一个）；
//! 字符预算为硬限制，跳过放不下的大片段继续尝试小片段；
//! top_k 为软上限，实际返回数量可能更少。

use crate::search::{search_hit_key, SearchHit};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub struct CoverageSelector {
    max_per_path: usize,
    max_chars: usize,
    overlap_threshold: f32,
}

impl CoverageSelector {
    pub fn new(
        max_per_path: usize,
        max_chars: usize,
        overlap_threshold: f32,
    ) -> Result<Self, String> {
        if max_per_path < 1 {
            return Err("max_per_path must be positive".into());
        }
        if max_chars < 1 {
            return Err("max_chars must be positive".into());
        }
        if !(0.0..=1.0).contains(&overlap_threshold) {
            return Err("overlap_threshold must be between zero and one".into());
        }
        Ok(Self {
            max_per_path,
            max_chars,
            overlap_threshold,
        })
    }

    pub fn select(&self, hits: &[SearchHit], top_k: usize) -> Vec<SearchHit> {
        if top_k == 0 || hits.is_empty() {
            return vec![];
        }

        let mut selected: Vec<SearchHit> = Vec::new();
        let mut path_counts: HashMap<&str, usize> = HashMap::new();
        let mut seen: HashSet<(String, String, u32, u32, String)> = HashSet::new();
        let mut used_chars = 0usize;

        for prefer_new_path in [true, false] {
            for hit in hits {
                if selected.len() >= top_k {
                    continue;
                }
                let path_count = path_counts.get(hit.path.as_str()).copied().unwrap_or(0);
                if prefer_new_path != (path_count == 0) {
                    continue;
                }
                if path_count >= self.max_per_path {
                    continue;
                }
                let key = search_hit_key(hit);
                if seen.contains(&key) || self.overlaps_selected(hit, &selected) {
                    continue;
                }

                // 字符预算检查：放不下就跳过，继续尝试后面的小片段。
                // Python len() 是码点数，CJK 内容按字节会虚胖 ~3 倍，必须用 char_len。
                let hit_chars = crate::chunk::spans::char_len(&hit.content);
                if !selected.is_empty() && used_chars + hit_chars > self.max_chars {
                    continue;
                }

                selected.push(hit.clone());
                seen.insert(key);
                *path_counts.entry(hit.path.as_str()).or_insert(0) += 1;
                used_chars += hit_chars;
            }
        }
        selected
    }

    fn overlaps_selected(&self, candidate: &SearchHit, selected: &[SearchHit]) -> bool {
        for hit in selected {
            if hit.path != candidate.path {
                continue;
            }
            // 与 Python 的 max(0, min(end) - max(start) + 1) 对齐
            let overlap = (hit.end_line.min(candidate.end_line) as i64
                - hit.start_line.max(candidate.start_line) as i64
                + 1)
            .max(0) as f32;
            let shorter = (hit.end_line - hit.start_line + 1)
                .min(candidate.end_line - candidate.start_line + 1)
                as f32;
            if overlap / shorter >= self.overlap_threshold {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, start: u32, end: u32, content: &str, score: f32) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}-{start}-{end}"),
            path: path.into(),
            content: content.into(),
            score,
            content_hash: String::new(),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn prefers_coverage_across_paths() {
        let s = CoverageSelector::new(2, 32_000, 0.6).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "aaaa", 0.9),
            hit("a.rs", 1, 12, "bbbb", 0.8),
            hit("b.rs", 1, 10, "cccc", 0.7),
        ];
        let out = s.select(&hits, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[1].path, "b.rs");
    }

    #[test]
    fn budget_is_hard_limit() {
        let s = CoverageSelector::new(2, 10, 0.6).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "0123456789", 0.9),
            hit("b.rs", 1, 10, "abcdefghij", 0.8),
        ];
        let out = s.select(&hits, 2);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn overlapping_spans_suppressed() {
        let s = CoverageSelector::new(3, 32_000, 0.6).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "aaaa", 0.9),
            hit("a.rs", 5, 15, "bbbb", 0.8), // 与第一个重叠 6/11
            hit("a.rs", 20, 30, "cccc", 0.7),
        ];
        let out = s.select(&hits, 3);
        assert_eq!(out.len(), 2);
    }
}
