//! 查询期子窗口切分（RETRIEVAL_SPAN_WINDOW_LINES，默认 0=关闭）。
//!
//! 动机（行级覆盖实测，2026-10-08）：cAST chunk 是「窗口」不是「定义」——
//! 平均 45 行、最大 232 行，一个 chunk 常含多个定义，答案 span 埋在 chunk
//! 中部（36 个缺失 unit 中 21 个答案位于 chunk 1600 字符之后、多数在
//! 2000-4500 字符处）。reranker 拿到整段文本时，打分被无关前缀稀释：
//! 「异步校验」查询下 ZodEffects._parse（答案在 166 行 chunk 的 2007 字符处）
//! 稳定输给 ZodPromise._parse（async 词汇密集但不是答案）。
//!
//! 解法：召回后把每个 chunk 按语句边界切成 ≤`max_lines` 的子窗口，子窗口
//! 作为独立候选流经 rerank / 融合 / 选择 / 渲染。效果上逼近对手引擎的
//! 「每定义一个 unit」粒度，但**不需要重建索引**——向量仍是 chunk 级
//! （召回已饱和，recall@24=100%，粒度只影响打分与打包，不影响召回）。
//!
//! 切分规则（与对方 languages adapter 的 max_lines 语义对齐）：
//! - 行数 ≤ max_lines 的 chunk 原样返回（多数 chunk：中位数 44-52 行）
//! - 超长 chunk 在**空行**处切（空行是语句边界的可靠代理）；无空行可用时
//!   按固定 max_lines 硬切
//! - 每个子窗口继承 chunk 的 blob_name / content_hash / 分数，行号区间
//!   重算为窗口自身的 1-based 闭区间——`search_hit_key` 含行号，子窗口
//!   天然是不同 key，融合去重 / facet 对齐 / 选择器重叠抑制全部按窗口生效

use crate::search::SearchHit;
use regex::Regex;
use std::sync::OnceLock;

/// 定义起始行的判定：切点必须落在定义边界上，否则答案 span 会被窗口
/// 边界切开（实测：consume_value 2281-2286 被 2281/2282 空行切点劈成两半，
/// 两窗都不完整包含 → unit 失败）。对方引擎的 unit 本就是定义对齐的，
/// 不存在这个问题；我们用定义正则近似定义边界。
static DEFINITION_START: OnceLock<Regex> = OnceLock::new();

fn definition_start() -> &'static Regex {
    DEFINITION_START.get_or_init(|| {
        Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:def|class|function|interface|type|enum|struct|trait|const|let|var|fn)\b",
        )
        .unwrap()
    })
}

/// 行是否是定义起始行。
fn is_definition_boundary(line: &str) -> bool {
    definition_start().is_match(line)
}

/// 把命中列表按行数切成子窗口。`max_lines` ≤ 0 时原样返回（开关关闭语义）。
pub fn split_hits_into_windows(hits: Vec<SearchHit>, max_lines: usize) -> Vec<SearchHit> {
    if max_lines == 0 {
        return hits;
    }
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        out.extend(split_hit_into_windows(hit, max_lines));
    }
    out
}

/// 单个命中的子窗口切分。行数不超限的命中原样返回（零拷贝路径）。
fn split_hit_into_windows(hit: SearchHit, max_lines: usize) -> Vec<SearchHit> {
    let lines: Vec<&str> = hit.content.split('\n').collect();
    if lines.len() <= max_lines {
        return vec![hit];
    }
    // 语句边界：空行（trim 后为空）。切点必须落在窗口中段之后，
    // 否则会出现「每 3 行切一刀」的碎片化窗口。
    let blank: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().is_empty())
        .map(|(i, _)| i)
        .collect();
    // 定义边界：切点优先取定义起始行（对方 unit 的对齐方式）。空行切点
    // 会把跨空行的答案 span 劈开；定义边界保证窗口与定义对齐。
    let def_boundary: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_definition_boundary(l))
        .map(|(i, _)| i)
        .collect();

    // 微窗下限：小于它的窗口会被选择器的成本归一偏好成「性价比之王」，
    // 挤掉真正含答案的实质窗口（实测：3-23 行碎片窗口把 43 行答案窗口
    // 挤出 per_path 席位）。对方引擎的 unit 是完整定义，不存在微窗；
    // 我们用「切点两侧都 ≥ min_lines」近似这个性质。
    let min_lines = (max_lines / 3).max(8);

    let mut windows: Vec<(usize, usize)> = Vec::new(); // [start_idx, end_idx) 0-based
    let mut start = 0usize;
    while start < lines.len() {
        let hard_end = (start + max_lines).min(lines.len());
        if hard_end == lines.len() {
            windows.push((start, hard_end));
            break;
        }
        // 切点优先级：定义边界 > 空行 > 硬切。切点必须让两侧都 ≥ min_lines
        // （微窗挤掉实质窗口）；定义边界切点让窗口与定义对齐（答案 span
        // 不会被窗口边界劈开）。
        let min_cut = start + max_lines / 2;
        let remaining = lines.len() - start;
        let valid = |b: usize| {
            b >= min_cut && b < hard_end && b - start >= min_lines && remaining.saturating_sub(b) >= min_lines
        };
        let cut = def_boundary
            .iter()
            .rev()
            .find(|&&b| valid(b))
            .copied()
            .or_else(|| blank.iter().rev().find(|&&b| valid(b)).copied())
            .unwrap_or(hard_end);
        windows.push((start, cut));
        start = cut;
    }
    // 尾窗合并：最后一窗不足 min_lines 时并入前一窗（允许略超 max_lines）。
    // 微窗会被选择器的成本归一偏好成「性价比之王」，挤掉实质答案窗口
    // （实测：2-9 行尾窗把 43 行答案窗口挤出 per_path 席位）。
    if windows.len() >= 2 {
        let last = windows.last().copied().unwrap();
        if last.1 - last.0 < min_lines {
            let prev = windows[windows.len() - 2];
            let merged = (prev.0, last.1);
            windows.truncate(windows.len() - 2);
            windows.push(merged);
        }
    }

    windows
        .into_iter()
        .filter_map(|(a, b)| {
            if a >= b {
                return None;
            }
            let text = lines[a..b].join("\n");
            if text.trim().is_empty() {
                return None;
            }
            Some(SearchHit {
                blob_name: hit.blob_name.clone(),
                path: hit.path.clone(),
                content: text,
                score: hit.score,
                content_hash: hit.content_hash.clone(),
                start_line: hit.start_line + a as u32,
                end_line: hit.start_line + (b - 1) as u32,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, start: u32, content: &str) -> SearchHit {
        SearchHit {
            blob_name: format!("b-{path}"),
            path: path.into(),
            content: content.into(),
            score: 0.5,
            content_hash: "h".into(),
            start_line: start,
            end_line: start + content.split('\n').count() as u32 - 1,
        }
    }

    #[test]
    fn short_hit_passes_through() {
        let h = hit("a.rs", 10, "l1\nl2\nl3");
        let out = split_hits_into_windows(vec![h], 65);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_line, 10);
        assert_eq!(out[0].end_line, 12);
    }

    #[test]
    fn zero_disables() {
        let h = hit("a.rs", 1, "l1\nl2");
        let out = split_hits_into_windows(vec![h], 0);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn long_hit_splits_at_blank_lines() {
        // 130 行，第 40 行（索引 39）是空行且落在切分搜索区 [32,65) 内
        // → 第一刀取它；剩余 91 行仍超 65，无空行可切 → 第二刀硬切
        // 窗口 = [1-39]（39 行）、[40-104]（65 行）、[105-130]（26 行）
        let mut content = String::new();
        for i in 1..=130 {
            if i == 40 {
                content.push('\n'); // 空行（索引 39）
            } else {
                content.push_str(&format!("line{i}\n"));
            }
        }
        let content = content.trim_end_matches('\n').to_string();
        let h = hit("a.rs", 100, &content);
        let out = split_hits_into_windows(vec![h], 65);
        assert_eq!(out.len(), 3, "空行切一刀 + 剩余硬切一刀");
        // 空行归下一段：第一段止于空行前一行
        assert_eq!(out[0].end_line, 100 + 38, "空行归下一段：第一段止于行 138");
        assert_eq!(out[1].start_line, 100 + 39);
        // 行号连续且覆盖全部行
        assert_eq!(out[0].end_line + 1, out[1].start_line);
        assert_eq!(out[1].end_line + 1, out[2].start_line);
        assert_eq!(out[2].end_line, 100 + 129);
        // 每窗不超 max_lines
        for w in &out {
            assert!(w.end_line - w.start_line + 1 <= 65);
        }
    }

    #[test]
    fn no_blank_lines_hard_cuts() {
        let content: String = (0..130).map(|i| format!("x{i}\n")).collect();
        let content = content.trim_end_matches('\n').to_string();
        let h = hit("a.rs", 1, &content);
        let out = split_hits_into_windows(vec![h], 65);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end_line, 65);
        assert_eq!(out[1].start_line, 66);
    }

    #[test]
    fn windows_share_hash_but_differ_in_lines() {
        let content: String = (0..130).map(|i| format!("x{i}\n")).collect();
        let content = content.trim_end_matches('\n').to_string();
        let h = hit("a.rs", 1, &content);
        let out = split_hits_into_windows(vec![h], 65);
        assert_eq!(out[0].content_hash, out[1].content_hash);
        assert_ne!(out[0].start_line, out[1].start_line);
    }

    #[test]
    fn content_matches_declared_lines() {
        // 不变量：窗口 content 必须逐字等于其声明行区间的文本
        // （formatter 按 start_line + 逐行打印）
        let content: String = (0..200)
            .map(|i| if (i + 1) % 50 == 0 { String::new() } else { format!("code {i}") })
            .collect::<Vec<_>>()
            .join("\n");
        let h = hit("a.rs", 1, &content);
        let out = split_hits_into_windows(vec![h], 65);
        let all_lines: Vec<&str> = content.split('\n').collect();
        for w in &out {
            let expect: Vec<&str> = all_lines
                [(w.start_line - 1) as usize..w.end_line as usize]
                .to_vec();
            assert_eq!(w.content, expect.join("\n"), "窗口内容必须等于声明区间");
        }
    }
}
