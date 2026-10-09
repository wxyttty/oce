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
    /// rank 分离权重（RETRIEVAL_SELECTOR_RANK_WEIGHT，0=关）：gain 混入
    /// exp(-rank/8) 项，打破 rerank 分数饱和的平局
    rank_weight: f32,
}

impl CoverageSelector {
    pub fn new(
        max_per_path: usize,
        max_chars: usize,
        overlap_threshold: f32,
        rank_weight: f32,
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
        if !(0.0..=1.0).contains(&rank_weight) {
            return Err("rank_weight must be between zero and one".into());
        }
        Ok(Self {
            max_per_path,
            max_chars,
            overlap_threshold,
            rank_weight,
        })
    }

    pub fn select(&self, hits: &[SearchHit], top_k: usize) -> Vec<SearchHit> {
        self.select_with_bundles(hits, &[], top_k)
    }

    /// 默认两趟填充 + 可选 bundle：命中锚点时把它所在 bundle 的其余成员一起带入
    /// （`RETRIEVAL_CONTEXT_BUNDLE_ENABLED`）。bundle 为空时逐字等于原 `select`。
    pub fn select_with_bundles(
        &self,
        hits: &[SearchHit],
        bundles: &[Vec<usize>],
        top_k: usize,
    ) -> Vec<SearchHit> {
        if top_k == 0 || hits.is_empty() {
            return vec![];
        }
        let bundle_of = bundle_index(hits, bundles);

        let mut selected: Vec<SearchHit> = Vec::new();
        let mut path_counts: HashMap<String, usize> = HashMap::new();
        let mut seen: HashSet<(String, String, u32, u32, String)> = HashSet::new();
        let mut used_chars = 0usize;
        // 近重复门控：两轮共用（第二轮补齐时同样不放宽——回填同质内容是纯浪费）
        let mut dup_guard = DupGuard::default();

        for prefer_new_path in [true, false] {
            for (index, hit) in hits.iter().enumerate() {
                if selected.len() >= top_k {
                    continue;
                }
                let path_count = path_counts.get(&hit.path).copied().unwrap_or(0);
                if prefer_new_path != (path_count == 0) {
                    continue;
                }
                let allow_over_budget = selected.is_empty();
                self.seat_with_bundle(
                    index,
                    hits,
                    &bundle_of,
                    &mut selected,
                    &mut seen,
                    &mut path_counts,
                    &mut dup_guard,
                    &mut used_chars,
                    top_k,
                    allow_over_budget,
                );
            }
        }
        selected
    }

    /// 尝试入席 `anchor`，并把它所在 bundle 的其余成员尽可能一起带入。
    ///
    /// 每个成员仍逐个过 per-path 上限、重叠、近重复与预算检查；预算按**新增**成员的
    /// 字符数累加（bundle 只买新东西）。返回新入席的成员数（0 = anchor 本身不合格）。
    #[allow(clippy::too_many_arguments)]
    fn seat_with_bundle(
        &self,
        anchor: usize,
        hits: &[SearchHit],
        bundle_of: &HashMap<usize, Vec<usize>>,
        selected: &mut Vec<SearchHit>,
        seen: &mut HashSet<(String, String, u32, u32, String)>,
        path_counts: &mut HashMap<String, usize>,
        dup_guard: &mut DupGuard,
        used_chars: &mut usize,
        top_k: usize,
        allow_over_budget: bool,
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = vec![anchor];
        if let Some(members) = bundle_of.get(&anchor) {
            candidates.extend(members.iter().copied().filter(|member| *member != anchor));
        }
        // 先算总成本再落座：预算放不下就整组不落（宁可少补，不半补）
        let mut fresh: Vec<usize> = Vec::new();
        let mut fresh_hits: Vec<SearchHit> = Vec::new();
        let mut cost = 0usize;
        for index in candidates {
            if selected.len() + fresh.len() >= top_k {
                break;
            }
            let hit = &hits[index];
            let key = search_hit_key(hit);
            if seen.contains(&key)
                || self.overlaps_selected(hit, selected)
                || self.overlaps_selected(hit, &fresh_hits)
            {
                continue;
            }
            let path_count = path_counts.get(&hit.path).copied().unwrap_or(0);
            if path_count >= self.max_per_path {
                continue;
            }
            let basename = basename_of(&hit.path);
            if dup_guard.blocked(hit, &basename) {
                continue;
            }
            cost += crate::chunk::spans::char_len(&hit.content);
            fresh.push(index);
            fresh_hits.push(hit.clone());
        }
        if fresh.is_empty() {
            return Vec::new();
        }
        // 预算硬限制；唯一的例外是"第一个片段总能入席"（沿用旧 select 的语义），
        // 且这个例外只对**单个**片段生效——整组超预算时先退化为锚点，
        // 不能因为空列表就整组挤进预算。
        let over_budget = *used_chars + cost > self.max_chars;
        if over_budget && (fresh.len() > 1 || !allow_over_budget) {
            if fresh.len() > 1 {
                // 整组放不下 → 退化为只入席锚点：bundle 是"尽量"，不是"必须"
                fresh.truncate(1);
                fresh_hits.truncate(1);
                let anchor_cost = crate::chunk::spans::char_len(&fresh_hits[0].content);
                if *used_chars + anchor_cost > self.max_chars && !allow_over_budget {
                    return Vec::new();
                }
            } else {
                return Vec::new();
            }
        }
        for hit in &fresh_hits {
            let key = search_hit_key(hit);
            let basename = basename_of(&hit.path);
            *path_counts.entry(hit.path.clone()).or_insert(0) += 1;
            dup_guard.seat(hit, &basename);
            seen.insert(key);
            *used_chars += crate::chunk::spans::char_len(&hit.content);
            selected.push(hit.clone());
        }
        fresh
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

    /// 边际覆盖增益 + 成本归一的贪心选择（`RETRIEVAL_MARGINAL_COVERAGE_ENABLED`）。
    ///
    /// 与 [`Self::select`] 的差别只在**排序依据**：固定顺序两趟填充换成了
    ///   `gain = 0.7 * mean_j(value_j / (1 + covered_j)) + 0.3 * base_score`
    /// 再除以 `(max(120, cost) / 300) ** 0.35`——多 facet 共同满足的片段优先于
    /// 单点高分，长片段不能靠体量吃预算（OCE cascade 的目标函数形状）。
    ///
    /// 不变量与 `select` 完全一致：字符预算硬限制、per-path 上限、重叠抑制、
    /// 近重复门控、`top_k` 软上限。同分并列按 `search_hit_key` 字典序取，保证
    /// 同一输入两次调用结果逐条相同（不依赖传入顺序）。
    ///
    /// `facet_scores[i][j]` 是第 i 个候选在第 j 路召回上的归一化亲和度（缺失 0）。
    pub fn select_with_coverage(
        &self,
        hits: &[SearchHit],
        facet_scores: &[Vec<f32>],
        top_k: usize,
    ) -> Vec<SearchHit> {
        self.select_with_coverage_and_bundles(hits, facet_scores, &[], top_k)
    }

    /// 边际覆盖贪心 + 可选 bundle：锚点入席时把它所在 bundle 的其余成员一起带入。
    pub fn select_with_coverage_and_bundles(
        &self,
        hits: &[SearchHit],
        facet_scores: &[Vec<f32>],
        bundles: &[Vec<usize>],
        top_k: usize,
    ) -> Vec<SearchHit> {
        if top_k == 0 || hits.is_empty() {
            return vec![];
        }
        let bundle_of = bundle_index(hits, bundles);
        let facet_count = facet_scores.first().map(|s| s.len()).unwrap_or(0);
        let base_score = |hit: &SearchHit| hit.score.clamp(0.0, 1.0);
        let cost_of = |hit: &SearchHit| crate::chunk::spans::char_len(&hit.content).max(1);

        let mut selected: Vec<SearchHit> = Vec::new();
        let mut selected_order: Vec<usize> = Vec::new();
        let mut path_counts: HashMap<String, usize> = HashMap::new();
        let mut seen: HashSet<(String, String, u32, u32, String)> = HashSet::new();
        let mut dup_guard = DupGuard::default();
        let mut used_chars = 0usize;
        let mut covered = vec![0.0f32; facet_count];

        loop {
            if selected.len() >= top_k {
                break;
            }
            let mut best: Option<(f32, (String, String, u32, u32, String), usize)> = None;
            for (index, hit) in hits.iter().enumerate() {
                let key = search_hit_key(hit);
                if seen.contains(&key) || self.overlaps_selected(hit, &selected) {
                    continue;
                }
                let path_count = path_counts.get(&hit.path).copied().unwrap_or(0);
                if path_count >= self.max_per_path {
                    continue;
                }
                let basename = basename_of(&hit.path);
                if dup_guard.blocked(hit, &basename) {
                    continue;
                }
                let cost = cost_of(hit);
                if !selected.is_empty() && used_chars + cost > self.max_chars {
                    continue;
                }
                let values = facet_scores.get(index);
                let coverage_gain = match values {
                    Some(values) if facet_count > 0 => values
                        .iter()
                        .zip(covered.iter())
                        .map(|(value, already)| value / (1.0 + already))
                        .sum::<f32>()
                        / facet_count as f32,
                    _ => 0.0,
                };
                let mut gain = 0.7 * coverage_gain + 0.3 * base_score(hit);
                // rank 分离（RETRIEVAL_SELECTOR_RANK_WEIGHT > 0 时）：rerank
                // 分数饱和时多个候选几乎同分，用组内 rank 的指数衰减打破
                // 平局——排名靠前者优先入席。对方 EvidenceEngine value() 的
                // 同款机制，替代逆成本奖励做「性价比」区分
                if self.rank_weight > 0.0 {
                    let rank_term = (-(index as f32) / 8.0).exp();
                    gain = (1.0 - self.rank_weight) * gain
                        + self.rank_weight * rank_term * gain.max(0.0);
                }
                gain /= (cost.max(120) as f32 / 300.0).powf(0.35);
                let better = match &best {
                    None => true,
                    Some((best_gain, best_key, _)) => {
                        gain > *best_gain || (gain == *best_gain && key < *best_key)
                    }
                };
                if better {
                    best = Some((gain, key, index));
                }
            }
            let Some((gain, _key, index)) = best else {
                break;
            };
            if gain < 0.015 {
                break;
            }
            let allow_over_budget = selected.is_empty();
            let seated = self.seat_with_bundle(
                index,
                hits,
                &bundle_of,
                &mut selected,
                &mut seen,
                &mut path_counts,
                &mut dup_guard,
                &mut used_chars,
                top_k,
                allow_over_budget,
            );
            if seated.is_empty() {
                break;
            }
            // 覆盖度按**实际入席**的成员累加：bundle 带入的成员同样满足其 facet
            for member in seated {
                if let (Some(values), true) = (facet_scores.get(member), facet_count > 0) {
                    for (already, value) in covered.iter_mut().zip(values.iter()) {
                        *already += value;
                    }
                }
                selected_order.push(member);
            }
        }
        // 贪心序只用于「选哪些」，不作为返回顺序：评测按返回顺序读 Top-1/nDCG，
        // 贪心序会把「多 facet 但分低」的片段顶到头部，直接打穿头部精度。
        // 集合不变，输出回到输入顺序（= 融合/重排分数序）。
        selected_order.sort_unstable();
        selected_order
            .into_iter()
            .filter_map(|index| hits.get(index).cloned())
            .collect()
    }
}

/// 同一符号的多个命中聚成一个实体（短函数 bundle 的输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub symbol: String,
    pub kind: String,
    /// 该符号在候选列表里的命中下标（升序）。
    pub members: Vec<usize>,
}

/// 按注解表把候选聚成实体：同 `(symbol, kind)` 的命中归一个实体，成员下标升序。
///
/// 没有注解的命中（取不到 definition 行）不参与任何实体——bundle 只对"知道是什么
/// 符号"的候选生效，不做猜测。
pub fn build_entities(
    hits: &[SearchHit],
    annotations: &HashMap<(String, String, u32, u32, String), (String, String)>,
) -> Vec<Entity> {
    let mut grouped: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (index, hit) in hits.iter().enumerate() {
        if let Some((symbol, kind)) = annotations.get(&search_hit_key(hit)) {
            grouped
                .entry((symbol.clone(), kind.clone()))
                .or_default()
                .push(index);
        }
    }
    let mut entities: Vec<Entity> = grouped
        .into_iter()
        .map(|((symbol, kind), mut members)| {
            members.sort_unstable();
            Entity {
                symbol,
                kind,
                members,
            }
        })
        .collect();
    // 稳定输出：符号名 + 首成员下标，便于测试与复现
    entities.sort_by(|a, b| {
        a.symbol
            .cmp(&b.symbol)
            .then_with(|| a.members.first().cmp(&b.members.first()))
    });
    entities
}

/// 把实体转成 bundle：整组字符数 ≤ `max_chars` 时作为一个 action 整体取舍；
/// 超过则**退化为成员逐个取舍**（每个成员各自成一个单元素 bundle）。
///
/// 空实体（无成员）与单成员实体都返回单元素 bundle——单元素 bundle 与普通候选
/// 行为一致，保留它们可以让调用方统一按 bundle 处理。
pub fn build_bundles(entities: &[Entity], hits: &[SearchHit], max_chars: usize) -> Vec<Vec<usize>> {
    let mut bundles: Vec<Vec<usize>> = Vec::new();
    for entity in entities {
        let total: usize = entity
            .members
            .iter()
            .filter_map(|index| hits.get(*index))
            .map(|hit| crate::chunk::spans::char_len(&hit.content))
            .sum();
        if entity.members.len() > 1 && total <= max_chars {
            bundles.push(entity.members.clone());
        } else {
            for member in &entity.members {
                bundles.push(vec![*member]);
            }
        }
    }
    bundles
}

/// 成员下标 → 它所属 bundle 的全部成员（锚点与成员共用同一份列表）。
fn bundle_index(hits: &[SearchHit], bundles: &[Vec<usize>]) -> HashMap<usize, Vec<usize>> {
    let mut index: HashMap<usize, Vec<usize>> = HashMap::new();
    for bundle in bundles {
        let members: Vec<usize> = bundle
            .iter()
            .copied()
            .filter(|member| *member < hits.len())
            .collect();
        if members.len() < 2 {
            continue;
        }
        for member in &members {
            index.insert(*member, members.clone());
        }
    }
    index
}

/// basename（小写，路径分隔符两种）。
fn basename_of(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_lowercase()
}

/// 由各路召回列表构造 facet 亲和度矩阵：`result[i][j]` = 第 i 个候选在第 j 路召回上的亲和度。
///
/// 逐列按该列最大值做 soft-max（`exp(min(0, raw - column_max) / temperature)`）——
/// 余弦分数不可跨列比较，减自身列最大值后才是"相对该 facet 有多近"。某路没召回该
/// 候选时记 0（不是负分，避免"没被检索到"被当成"负相关"）。
///
/// `hits` 与 `result_lists` 是两次独立构造的列表（中间经过 rerank/合并），因此按
/// `search_hit_key` 对齐，而不是按下标。
pub fn facet_affinity(
    hits: &[SearchHit],
    result_lists: &[Vec<SearchHit>],
    temperature: f32,
) -> Vec<Vec<f32>> {
    let facets = result_lists.len();
    if hits.is_empty() || facets == 0 {
        return vec![];
    }
    let temperature = if temperature > 0.0 { temperature } else { 0.06 };
    let mut raw = vec![vec![0.0f32; facets]; hits.len()];
    for (column, list) in result_lists.iter().enumerate() {
        let by_key: HashMap<_, f32> = list
            .iter()
            .map(|hit| (search_hit_key(hit), hit.score))
            .collect();
        for (row, hit) in hits.iter().enumerate() {
            if let Some(score) = by_key.get(&search_hit_key(hit)) {
                raw[row][column] = *score;
            }
        }
    }
    let mut column_max = vec![0.0f32; facets];
    for row in &raw {
        for (column, value) in row.iter().enumerate() {
            column_max[column] = column_max[column].max(*value);
        }
    }
    for row in raw.iter_mut() {
        for (column, value) in row.iter_mut().enumerate() {
            if *value > 0.0 {
                *value = ((*value - column_max[column]).min(0.0) / temperature).exp();
            }
        }
    }
    raw
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
        let s = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
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
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
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
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
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
        let s = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
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
        let s = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
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
        let s = CoverageSelector::new(2, 10, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "0123456789", 0.9),
            hit("b.rs", 1, 10, "abcdefghij", 0.8),
        ];
        let out = s.select(&hits, 2);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn overlapping_spans_suppressed() {
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "aaaa", 0.9),
            hit("a.rs", 5, 15, "bbbb", 0.8), // 与第一个重叠 6/11
            hit("a.rs", 20, 30, "cccc", 0.7),
        ];
        let out = s.select(&hits, 3);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn coverage_respects_char_budget() {
        // 预算 10 码点：两个各 10 码点的候选只能入一个
        let s = CoverageSelector::new(2, 10, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "0123456789", 0.9),
            hit("b.rs", 1, 10, "abcdefghij", 0.8),
        ];
        let facets = vec![vec![1.0], vec![1.0]];
        let out = s.select_with_coverage(&hits, &facets, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "a.rs");
    }

    #[test]
    fn coverage_still_suppresses_overlap() {
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "aaaa", 0.9),
            hit("a.rs", 5, 15, "bbbb", 0.8), // 与第一个重叠 6/11 ≥ 0.6
            hit("a.rs", 20, 30, "cccc", 0.7),
        ];
        let facets = vec![vec![1.0], vec![1.0], vec![1.0]];
        let out = s.select_with_coverage(&hits, &facets, 3);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn coverage_prefers_two_facet_hit_over_single_top() {
        // A/C 各只满足一个 facet 且 base 更高；B 满足两个 facet、base 最低。
        // top_k=2 时纯按分数会选 A+C，边际覆盖应选 B，再补 A。
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
        let content = "x".repeat(100);
        let hits = vec![
            hit("a.rs", 1, 10, &content, 0.9),
            hit("b.rs", 1, 10, &content, 0.5),
            hit("c.rs", 1, 10, &content, 0.7),
        ];
        let facets = vec![vec![1.0, 0.0], vec![1.0, 1.0], vec![0.0, 1.0]];
        let out = s.select_with_coverage(&hits, &facets, 2);
        let paths: Vec<&str> = out.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"b.rs"), "覆盖两个 facet 的候选必须入选: {paths:?}");
        assert!(
            !paths.contains(&"c.rs"),
            "同覆盖度的次高分候选应被边际增益挤掉: {paths:?}"
        );
    }

    #[test]
    fn coverage_output_preserves_input_order() {
        // 选择器只决定集合：返回顺序必须回到输入（分数）序，否则贪心序会
        // 打穿按返回顺序读的 Top-1/nDCG。
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
        let content = "y".repeat(100);
        let hits = vec![
            hit("a.rs", 1, 10, &content, 0.9),
            hit("b.rs", 1, 10, &content, 0.5),
            hit("c.rs", 1, 10, &content, 0.7),
        ];
        let facets = vec![vec![1.0], vec![1.0], vec![1.0]];
        let out = s.select_with_coverage(&hits, &facets, 3);
        let paths: Vec<&str> = out.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn coverage_is_deterministic() {
        let s = CoverageSelector::new(3, 32_000, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 10, "aaaa", 0.5),
            hit("b.rs", 1, 10, "bbbb", 0.5),
            hit("c.rs", 1, 10, "cccc", 0.5),
        ];
        let facets = vec![vec![1.0, 1.0], vec![1.0, 1.0], vec![1.0, 1.0]];
        let first = s.select_with_coverage(&hits, &facets, 3);
        // 打乱输入顺序：并列项按 search_hit_key 字典序取，选出的**集合**必须一致
        let shuffled = vec![hits[2].clone(), hits[0].clone(), hits[1].clone()];
        let second = s.select_with_coverage(&shuffled, &facets, 3);
        let mut keys = |v: &[SearchHit]| {
            let mut k: Vec<_> = v.iter().map(search_hit_key).collect();
            k.sort();
            k
        };
        assert_eq!(keys(&first), keys(&second));
    }

    #[test]
    fn facet_affinity_softmax_per_column_and_missing_is_zero() {
        let hits = vec![
            hit("a.rs", 1, 10, "a", 0.0),
            hit("b.rs", 1, 10, "b", 0.0),
        ];
        let column0 = vec![hit("b.rs", 1, 10, "b", 0.9), hit("a.rs", 1, 10, "a", 0.5)];
        let affinity = facet_affinity(&hits, &[column0], 0.06);
        // 列内最大者（b）为 exp(0)=1；a 相对它衰减；未被召回的列记 0
        assert!((affinity[1][0] - 1.0).abs() < 1e-6);
        assert!(affinity[0][0] < affinity[1][0]);
        assert!(affinity[0][0] > 0.0);
        assert!(affinity[0][0] <= 1.0);
    }

    // ── STEP-10：短函数 bundle / 符号级实体聚合 ──

    fn annotate(hit: &SearchHit, symbol: &str) -> ((String, String, u32, u32, String), (String, String)) {
        (search_hit_key(hit), (symbol.to_string(), "definition".to_string()))
    }

    #[test]
    fn build_entities_groups_by_symbol_and_skips_unannotated() {
        let hits = vec![
            hit("a.rs", 1, 5, "aaa", 0.9),
            hit("a.rs", 6, 12, "bbb", 0.8),   // 同符号的第二段
            hit("b.rs", 1, 3, "ccc", 0.7),    // 无注解
        ];
        let mut annotations = std::collections::HashMap::new();
        let (k0, v0) = annotate(&hits[0], "alpha");
        let (k1, v1) = annotate(&hits[1], "alpha");
        annotations.insert(k0, v0);
        annotations.insert(k1, v1);
        let entities = build_entities(&hits, &annotations);
        assert_eq!(entities.len(), 1, "只有 alpha 有注解");
        assert_eq!(entities[0].symbol, "alpha");
        assert_eq!(entities[0].members, vec![0, 1]);
    }

    #[test]
    fn bundle_selects_small_function_atomically() {
        // alpha 的两段共 6 字符 ≤ 阈值 → 整体入席；只按新增字符计费
        let s = CoverageSelector::new(3, 1_000, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 5, "aaa", 0.9),
            hit("a.rs", 6, 12, "bbb", 0.8),
            hit("b.rs", 1, 3, "ccc", 0.7),
        ];
        let bundles = vec![vec![0usize, 1usize]];
        let out = s.select_with_bundles(&hits, &bundles, 3);
        let paths: Vec<&str> = out.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(out.len(), 3, "bundle 成员一起入席");
        assert!(paths.contains(&"b.rs"));
    }

    #[test]
    fn bundle_falls_back_when_oversized() {
        // 整组 3 段超出 max_chars → 退化为成员逐个取舍（仍按分数序尽量多选）
        let s = CoverageSelector::new(3, 8, 0.6, 0.0).unwrap();
        let big = "x".repeat(10);
        let hits = vec![
            hit("a.rs", 1, 5, &big, 0.9),
            hit("a.rs", 6, 12, &big, 0.8),
        ];
        let bundles = vec![vec![0usize, 1usize]];
        let out = s.select_with_bundles(&hits, &bundles, 2);
        assert_eq!(out.len(), 1, "整组放不下 → 只入席锚点");
        assert_eq!(out[0].path, "a.rs");
        assert_eq!(out[0].content, big);
    }

    #[test]
    fn bundle_works_on_default_selector_path() {
        // 默认两趟填充路径（边际覆盖关闭）也必须能带入 bundle 成员：
        // b.rs 是另一个文件，若 bundle 不生效就只会各选一个（这里断言 a.rs 两段都在）
        let s = CoverageSelector::new(3, 1_000, 0.6, 0.0).unwrap();
        let hits = vec![
            hit("a.rs", 1, 5, "aaa", 0.9),
            hit("b.rs", 1, 5, "bbb", 0.8),
            hit("a.rs", 6, 12, "ccc", 0.7),
        ];
        let without = s.select(&hits, 3);
        let with = s.select_with_bundles(&hits, &[vec![0usize, 2usize]], 3);
        assert_eq!(without.len(), 3);
        assert_eq!(with.len(), 3);
        // 两者集合相同（3 席够放），但 bundle 让同符号的 a.rs 两段同时在场
        let a_count = |v: &[SearchHit]| v.iter().filter(|h| h.path == "a.rs").count();
        assert_eq!(a_count(&with), 2);
    }

    #[test]
    fn build_bundles_degrades_oversized_entities() {
        let big = "x".repeat(500);
        let hits = vec![
            hit("a.rs", 1, 5, &big, 0.9),
            hit("a.rs", 6, 12, &big, 0.8),
            hit("b.rs", 1, 3, "small", 0.7),
        ];
        let entities = vec![
            Entity { symbol: "alpha".into(), kind: "definition".into(), members: vec![0, 1] },
            Entity { symbol: "beta".into(), kind: "definition".into(), members: vec![2] },
        ];
        let bundles = build_bundles(&entities, &hits, 768);
        assert_eq!(bundles, vec![vec![0usize], vec![1usize], vec![2usize]]);
        let roomy = build_bundles(&entities, &hits, 10_000);
        assert_eq!(roomy, vec![vec![0usize, 1usize], vec![2usize]]);
    }

    // ── rank 分离（RETRIEVAL_SELECTOR_RANK_WEIGHT）──

    #[test]
    fn rank_weight_zero_keeps_baseline_selection() {
        // 权重 0：选择结果与基线完全一致（关闭态逐字等价）
        let hits = vec![
            hit("a.rs", 1, 10, &"x".repeat(200), 0.95),
            hit("b.rs", 1, 10, &"y".repeat(200), 0.94),
            hit("c.rs", 1, 10, &"z".repeat(200), 0.93),
        ];
        let facets = vec![vec![0.9f32], vec![0.9], vec![0.9]];
        let base = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
        let off = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
        let a = base.select_with_coverage(&hits, &facets, 3);
        let b = off.select_with_coverage(&hits, &facets, 3);
        assert_eq!(
            a.iter().map(search_hit_key).collect::<Vec<_>>(),
            b.iter().map(search_hit_key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rank_weight_breaks_score_saturation_toward_head() {
        // 分数饱和（全部 0.95）：rank 项让头部候选优先入席。
        // 构造：3 个同分候选，per_path=2 只能选 2 个——
        // rank 分离开启时选 rank 0/1（头部），关闭时同 gain 由 key 字典序决定
        let hits = vec![
            hit("c.rs", 1, 10, &"x".repeat(200), 0.95),
            hit("a.rs", 1, 10, &"y".repeat(200), 0.95),
            hit("b.rs", 1, 10, &"z".repeat(200), 0.95),
        ];
        // per-hit facet 行向量（facet_scores[i] = hit i 的各 facet 分）
        let facets = vec![vec![0.9f32], vec![0.9], vec![0.9]];
        let off = CoverageSelector::new(2, 32_000, 0.6, 0.0).unwrap();
        let on = CoverageSelector::new(2, 32_000, 0.6, 1.0).unwrap();
        let sel_off: Vec<String> = off
            .select_with_coverage(&hits, &facets, 2)
            .iter()
            .map(|h| h.path.clone())
            .collect();
        let sel_on: Vec<String> = on
            .select_with_coverage(&hits, &facets, 2)
            .iter()
            .map(|h| h.path.clone())
            .collect();
        // 开启时 rank 项主导：选头部两个（rank 0=c.rs, rank 1=a.rs）
        assert_eq!(sel_on, vec!["c.rs", "a.rs"]);
        // 关闭时同 gain 字典序 tie-break 选 a.rs, b.rs（不同结果证明开关生效）
        assert_eq!(sel_off, vec!["a.rs", "b.rs"]);
    }
}
