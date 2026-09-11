//! 有界查询规划。与 Python `domain/services/query_planner.py` 对齐。
//!
//! 保留完整请求，只添加显式句子级 facet。

use regex::Regex;
use std::sync::OnceLock;

pub trait QueryPlanner: Send + Sync {
    fn plan(&self, query: &str) -> Vec<String>;
}

pub struct HeuristicQueryPlanner {
    max_queries: usize,
    min_facet_chars: usize,
}

fn bullet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:[-*+]\s+|\d+[.)]\s*)").unwrap())
}

/// Python `re.split(r"(?:\r?\n+|[!?;。！？；]+|\.(?=\s|$))", query)` 的手工等价实现。
/// Rust regex 不支持前瞻：`.` 后跟空白/结尾才算边界。
fn split_on_boundaries(query: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let bytes = query.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    let is_break = |b: u8| {
        b == b'!' || b == b'?' || b == b';'
            || matches!(bytes_get_context(b), true)
    };
    let _ = is_break;
    while i < bytes.len() {
        let b = bytes[i];
        let break_len = if b == b'\r' || b == b'\n' {
            // \r?\n+ ：吞掉连续换行（含 CR LF 序列）
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b'\r' || bytes[j] == b'\n') {
                j += 1;
            }
            Some(j - i)
        } else if matches!(b, b'!' | b'?' | b';')
            || b == 0xEF /* 。！？； UTF-8 首字节 */ || b == 0xE3
        {
            // ASCII 标点或中文标点起始：吞掉连续的标点字符
            let mut j = i;
            while j < bytes.len() {
                let c = bytes[j];
                let is_punct = matches!(c, b'!' | b'?' | b';')
                    || bytes[j..].starts_with("。".as_bytes())
                    || bytes[j..].starts_with("！".as_bytes())
                    || bytes[j..].starts_with("？".as_bytes())
                    || bytes[j..].starts_with("；".as_bytes());
                if !is_punct {
                    break;
                }
                j += if c == 0xEF || c == 0xE3 { 3 } else { 1 };
            }
            if j > i {
                Some(j - i)
            } else {
                None
            }
        } else if b == b'.' {
            // 前瞻：`.` 后是空白或结尾才算边界（边界本身不消耗空白）
            let next_is_ws_or_end = i + 1 >= bytes.len()
                || bytes[i + 1] == b' '
                || bytes[i + 1] == b'\t'
                || bytes[i + 1] == b'\r'
                || bytes[i + 1] == b'\n';
            if next_is_ws_or_end {
                Some(1)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(len) = break_len {
            parts.push(&query[start..i]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    parts.push(&query[start..]);
    parts
}

#[inline]
fn bytes_get_context(_b: u8) -> bool {
    false
}

impl HeuristicQueryPlanner {
    pub fn new(max_queries: usize, min_facet_chars: usize) -> Result<Self, String> {
        if max_queries < 1 {
            return Err("max_queries must be positive".into());
        }
        if min_facet_chars < 1 {
            return Err("min_facet_chars must be positive".into());
        }
        Ok(Self {
            max_queries,
            min_facet_chars,
        })
    }
}

impl QueryPlanner for HeuristicQueryPlanner {
    fn plan(&self, query: &str) -> Vec<String> {
        let normalized: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return vec![];
        }
        if self.max_queries == 1 {
            return vec![normalized];
        }

        let mut facets: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        seen.insert(normalized.to_lowercase());
        for raw in split_on_boundaries(query) {
            let stripped = bullet_re().replace(raw, "");
            let facet: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
            let key = facet.to_lowercase();
            if facet.chars().count() < self.min_facet_chars || seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            facets.push(facet);
        }

        // 单个 fragment 相对完整请求没有增量信息
        if facets.len() < 2 {
            return vec![normalized];
        }
        facets.truncate(self.max_queries - 1);
        let mut out = Vec::with_capacity(facets.len() + 1);
        out.push(normalized);
        out.extend(facets);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_sentence_returns_normalized() {
        let p = HeuristicQueryPlanner::new(4, 8).unwrap();
        assert_eq!(p.plan("  how does   auth work? "), vec!["how does auth work?"]);
    }

    #[test]
    fn multi_sentence_adds_facets() {
        let p = HeuristicQueryPlanner::new(4, 8).unwrap();
        let out = p.plan("How does auth work. Where is the token refresh. Short");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "How does auth work. Where is the token refresh. Short");
        assert_eq!(out[1], "How does auth work");
        assert_eq!(out[2], "Where is the token refresh");
    }

    #[test]
    fn dedupes_facets_equal_to_full_query() {
        let p = HeuristicQueryPlanner::new(4, 8).unwrap();
        // Python 行为：facet "same question" 与 normalized 不同（不重复），
        // 但单 facet 无增量信息 → 返回 [normalized]
        assert_eq!(
            p.plan("same question. same question"),
            vec!["same question. same question"]
        );
    }
}
