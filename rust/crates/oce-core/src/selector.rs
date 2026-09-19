//! 覆盖度与预算感知的结果选择。与 Python `selector/coverage_selector.py` 对齐。
//!
//! 贪心填充：优先保证仓库覆盖度（第一轮每个文件各选一个）；
//! 字符预算为硬限制，跳过放不下的大片段继续尝试小片段；
//! top_k 为软上限，实际返回数量可能更少。

use crate::search::{search_hit_key, SearchHit};
use std::collections::{HashMap, HashSet};

/// 同 basename 近重复限席（BCE dupBasenameCap）：同名且行集合 Jaccard ≥ 0.7
/// 的拷贝变体最多占 2 席，把窗口让给不同内容。
const DUP_BASENAME_CAP: usize = 2;
const DUP_LINE_JACCARD_MIN: f32 = 0.7;

/// select 循环内的近重复门控状态：按 basename 分桶记录已入席内容的行集合。
/// 字节级相同内容（content_hash 相同）直接跳过；同名但内容不同（mod.rs/
/// provider.rs 这类真实现）不受影响——Jaccard 门控只压「内容重叠」不压「同名不同文」。
#[derive(Default)]
struct DupGuard {
    /// basename（小写）→ 已入席内容的 (content_hash, 行集合)
    seated: HashMap<String, Vec<(String, HashSet<String>)>>,
}

impl DupGuard {
    /// 字节级相同、或同桶已满且行集合高度重叠的候选被门控（返回 true）。
    fn blocked(&self, hit: &SearchHit, basename: &str) -> bool {
        let Some(bucket) = self.seated.get(basename) else {
            return false;
        };
        for (hash, lines) in bucket {
            if !hash.is_empty() && hash == &hit.content_hash {
                return true; // 字节级相同：拷贝文件/vendored 重复，永不占第二席
            }
            if bucket.len() >= DUP_BASENAME_CAP
                && crate::retrieval::line_jaccard_public(&hit.content, lines)
                    >= DUP_LINE_JACCARD_MIN
            {
                return true;
            }
        }
        false
    }

    fn seat(&mut self, hit: &SearchHit, basename: &str) {
        let lines = crate::retrieval::line_set_public(&hit.content);
        self.seated
            .entry(basename.to_string())
            .or_default()
            .push((hit.content_hash.clone(), lines));
    }
}

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
        // 近重复门控：两轮共用（第二轮补齐时同样不放宽——回填同质内容是纯浪费）
        let mut dup_guard = DupGuard::default();

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
                // 同 basename 近重复门控（BCE dupBasenameCap）：字节级相同
                // 或行集合高度重叠的拷贝变体限席
                let basename = basename_of(&hit.path);
                if dup_guard.blocked(hit, &basename) {
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
                dup_guard.seat(hit, &basename);
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

/// basename（小写，路径分隔符两种）。
fn basename_of(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_lowercase()
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

    /// 带 content_hash 的命中构造（字节级相同判定用）。
    fn hit_hashed(
        path: &str,
        start: u32,
        end: u32,
        content: &str,
        score: f32,
        content_hash: &str,
    ) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}-{start}-{end}"),
            path: path.into(),
            content: content.into(),
            score,
            content_hash: content_hash.into(),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn byte_identical_copies_never_take_a_second_seat() {
        // flask Q52 场景：根 LICENSE.txt 与 examples/*/LICENSE.txt 字节级相同
        let s = CoverageSelector::new(2, 32_000, 0.6).unwrap();
        let content = "Flask License\n\nCopyright 2010 Pallets\n";
        let hash = "h1";
        let hits = vec![
            hit_hashed("LICENSE.txt", 1, 3, content, 0.9, hash),
            hit_hashed("examples/tutorial/LICENSE.txt", 1, 3, content, 0.8, hash),
            hit_hashed("examples/javascript/LICENSE.txt", 1, 3, content, 0.7, hash),
            hit("src/flask/app.py", 1, 5, "app = Flask(__name__)\n", 0.6),
        ];
        let out = s.select(&hits, 4);
        // 三份相同 LICENSE 只占一席，第四个不同内容候选入窗
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "LICENSE.txt");
        assert_eq!(out[1].path, "src/flask/app.py");
    }

    #[test]
    fn near_copy_variants_capped_at_two_seats() {
        // 行集合 Jaccard ≥ 0.7 的同名变体（微改拷贝）：限 2 席
        let s = CoverageSelector::new(3, 32_000, 0.6).unwrap();
        let base = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        let variant_a =
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10-changed\n";
        let variant_b =
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10-other\n";
        let variant_c =
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10-third\n";
        let hits = vec![
            hit("pkg_a/pom.xml", 1, 10, base, 0.9),
            hit("pkg_b/pom.xml", 1, 10, variant_a, 0.85),
            hit("pkg_c/pom.xml", 1, 10, variant_b, 0.8),
            hit("pkg_d/pom.xml", 1, 10, variant_c, 0.75), // 第三个近拷贝被门控
            hit("src/main.rs", 1, 5, "fn main() {}\n", 0.7),
        ];
        let out = s.select(&hits, 5);
        let pom_seats = out.iter().filter(|h| h.path.ends_with("pom.xml")).count();
        assert_eq!(
            pom_seats, 2,
            "near-copy variants must cap at DUP_BASENAME_CAP"
        );
        assert!(out.iter().any(|h| h.path == "src/main.rs"));
    }

    #[test]
    fn same_basename_different_content_not_suppressed() {
        // cc-switch 场景：mod.rs × N / provider.rs × N 是同名不同文的真实现
        let s = CoverageSelector::new(3, 32_000, 0.6).unwrap();
        let hits = vec![
            hit(
                "src-tauri/src/proxy/mod.rs",
                1,
                10,
                "pub mod provider;\npub mod usage;\n",
                0.9,
            ),
            hit(
                "src-tauri/src/database/mod.rs",
                1,
                10,
                "pub mod dao;\npub mod models;\n",
                0.85,
            ),
            hit(
                "src-tauri/src/mcp/mod.rs",
                1,
                10,
                "pub mod server;\npub mod tools;\n",
                0.8,
            ),
            hit(
                "src-tauri/src/session_manager/mod.rs",
                1,
                10,
                "pub mod providers;\npub mod terminal;\n",
                0.75,
            ),
        ];
        let out = s.select(&hits, 4);
        // 同名但行集合零重叠：全部入席，Jaccard 门控只压「内容重叠」
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn dup_guard_holds_in_backfill_round() {
        // 第二轮（补齐）同样不放宽近重复 cap：回填同质内容是纯浪费。
        // 构造：pkg_a/pom.xml 首块第一轮入席；pkg_b/pom.xml 是它的字节级拷贝
        // （第一轮入席，新路径）；pkg_a 的第二块（同路径，只能等第二轮）
        // 与 pkg_b 入席内容相同 → 第二轮被字节级门控拦截。
        let s = CoverageSelector::new(2, 32_000, 0.6).unwrap();
        let content_a = "[project]\nname = \"a\"\ndeps = []\n";
        let content_b = "[project]\nname = \"b\"\nextra = true\n";
        let hash = "h-same";
        let hits = vec![
            hit_hashed("pkg_a/pom.xml", 1, 3, content_a, 0.9, "h-a1"),
            hit_hashed("pkg_b/pom.xml", 1, 3, content_b, 0.85, "h-b"),
            // pkg_a 第二块：与 pkg_b 入席内容字节级相同，只能进第二轮
            hit_hashed("pkg_a/pom.xml", 10, 12, content_b, 0.8, hash),
        ];
        let out = s.select(&hits, 3);
        assert_eq!(out.len(), 2, "backfill must not seat a byte-identical copy");
        assert!(out
            .iter()
            .all(|h| h.path != "pkg_a/pom.xml" || h.start_line == 1));
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
