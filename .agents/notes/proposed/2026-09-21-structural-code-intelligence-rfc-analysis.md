# Structural Code Intelligence RFC 分析与 Rust 版吸收决策

- 日期：2026-09-21
- 状态：已分析，待排期
- 输入：colbymchenry/codegraph 方向 RFC（graph 作为 recall primitive）
- 结论：**大方向不吸收（无 headroom 证据），三个具体小点吸收，其一为现成不一致修复**

---

## 一、RFC 核心主张 vs Rust 版实测

| RFC 主张 | Rust 版已有近似 | 实测结果 | 判定 |
|---|---|---|---|
| 图扩展召回（anchor → bounded expansion） | TriviumDB SA-PPR `expand_depth=1` | **-32.89 分**（159.60→126.71） | 已验证有害 |
| 结构信号进融合 | BM25 标识符门控 + boost 0.3 | +4.4 分（已优化到位） | 已做 |
| 结构 hints（不动排序） | `related_symbols`（fanout 门控 + 停用词） | 已实现，不影响评分 | 已做 |
| typed edges + workspace resolution | 无 | 未验证 | 无 headroom 证据 |

### headroom 分析（flask benchmark，当前最优配置）

| 类别 | 得分/20 | graph headroom |
|---|---:|---|
| call_chain | 17.86-18.29 | **很小**（已 89%+） |
| error_handling | 18.63 | 很小 |
| api_usage | 18.42 | 很小 |
| cross_language | 9.24 | **大**（但需 frontend→backend heuristic edge，MVP 之后） |
| file_exact_match | 4.54 | 中（graph 帮助有限） |

**关键结论：现有 100-query benchmark 上 graph recall 大概率测出假阴性。**
验证 graph 价值前必须先建 graph-specific query set（multi-hop / cross_language /
interface impact 类问题）。

---

## 二、三个可吸收点（按性价比排序）

### 1. exact 召回 fanout 降权 —— 现成的不一致（性价比最高）

代码事实（`crates/oce-infra/src/sqlite/chains.rs`，同一个 `SqlExactStore`）：

- `find_definitions`（related hints 用）：**有 fanout 处理**
  `COUNT(*) OVER (PARTITION BY identifier)` 窗口函数，fanout 升序（低 = 更具体）
- `search_exact`（检索主链路用）：**没有 fanout 处理**
  纯 kind 打分（endpoint 1.0 / definition 0.95 / 其他 0.85），
  同名定义多文件出现时不区分

修复：`search_exact` SQL 加 fanout 窗口函数，score 乘衰减因子。
SQL 级改动，flask + cc-switch 各跑一轮即可验证。

### 2. tree-sitter 取代 regex 提取 symbol（中期）

`crates/oce-core/src/symbol.rs` 是纯 regex。已知缺陷在
`related.rs` 停用词表注释里自己承认：docstring 行首的
"Subclass and has..." 被宽松定义正则误提取——related 侧维护了
100+ 词停用词表来过滤。

- 方向正确：chunker 已有 tree-sitter（cAST），parse 基础设施现成
- 代价：换提取器 = `symbol_occurrences` 数据变 = exact 召回行为变
  = 全量重索引 + 回归验证
- 必须以 model fingerprint / fail-closed 方式切换（参照 file_desc 的 etext=v2 先例）

### 3. import 关系进 related_symbols hints（可选实验）

RFC"允许不知道但不假装知道"原则的最便宜落点：
- 不做 edge 图，只在 `<related_symbols>` 块加 import 信息
  （"此文件 import 了 X / 被 Y import"）
- regex 提取 import 行成本极低
- hints 模式零评分风险（不动排序，评测只解析 `Path:` 行）
- 是验证"结构信号对 agent 有没有用"的最小实验，比建 graph 便宜两个数量级

---

## 三、明确不吸收的

| 项 | 理由 |
|---|---|
| Candidate + Evidence envelope | SearchHit 干净；reranker 已承担统一裁决；没有 graph 就没有证据链需求 |
| GraphStore Protocol / 两阶段 indexing | 没有 edge 数据前是空壳 |
| 任何默认开启的结构融合 | BM25 教训：结构信号必须从关/低权重开始调；graph recall 若做须 intent 门控（仅 REFERENCE/CALL_CHAIN）+ 权重 0.1 起调 |
| 专用 Graph DB | 1-2 hop 在 SQLite/PostgreSQL 无本质问题 |

---

## 四、RFC 中值得保留的设计原则（做 graph 时必须遵守）

1. **hub 抑制**：高频 target 的边信息量低（IDF 思想）。`A calls jsonify`
   对召回零信息。related.rs 的 FANOUT_MAX=15 是现成实现。
2. **允许不知道，但不假装知道**：无法 disambiguate 的 reference 保留
   unresolved，不强行连错误边。错误边污染 flow 比漏边严重。
3. **provenance/confidence**：heuristic edge 必须带证据链
   （resolution method + supporting metadata）。
4. **两阶段 indexing**：per-file extraction → workspace resolution。
   跨文件 edge 必须在 workspace 级 resolve。
5. **bounded expansion 四约束**：max_depth / max_nodes / fanout cap /
   edge kind allowlist——expand_depth=-33 分的教训。

---

## 五、执行顺序

1. **现在**：`search_exact` 加 fanout 降权（对齐 `find_definitions`），
   flask + cc-switch benchmark 验证
2. **下个迭代**：tree-sitter symbol 提取 + fingerprint 隔离 + 全量回归
3. **graph 价值验证前**：先建 graph-specific query set
   （multi-hop / cross_language / interface impact）——
   没有这个 query set，任何 graph 实现都是盲做
