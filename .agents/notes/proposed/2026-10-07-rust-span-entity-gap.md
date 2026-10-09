# Agent Note: Rust 版行级证据覆盖差距——诊断与索引期改造方案

Status: proposed（诊断已完成并实测，改造未开始）

## Problem

用 OpenContextEngine 自己的数据集与评分器（`eval/expanded-v1/{click,httpx,zod}`，
3 仓 × 10 case × zh/en = 60 题，95 个 unit）测量 Rust 版，行级证据覆盖显著落后：

| | 对方（公开数字） | oce Rust（用户配置，LLM rerank 关） |
|---|---:|---:|
| avg tokens | 3736 | **3948** |
| cov@1000 | 57.2% | **38.8%** |
| cov@4000 | 94.8% | **71.4%** |

同口径、同评分器、同语料。**token 用量基本持平，前 1000 token 差 18.4 点** ——
不是"给得不够多"，是"同样预算下的证据密度"。

双口径同时看（`results/dual-all-realnollm.md`）：

```
文件级  Top-1 47/60   nDCG@10 0.9012   无效行号 0
行级    raw tokens 3948
        cov@1000 38.8%   cov@2000 63.9%   cov@4000 71.4%   cov@8000 75.8%
```

**两套口径在这批题上接近负相关**：`click/value-precedence`（Top-1=1、nDCG=1.0）行级覆盖
**0%**；`zod/intersection-merging`（Top-1=0、nDCG=0.5）行级覆盖 **100%**。
说明本仓"能找对文件，但返回的是该文件里语义最像的片段，而不是答案要的那几行"。

## 已排除的原因（都实测过，别再试）

| 假设 | 实测 | 结论 |
|---|---|---|
| 选择窗口太窄 | `select_k` 10→20、`per_path` 2→6 | token 3949→7342（+86%），cov@1000 只 37.6%→38.8% |
| bundle 阈值太小 | `BUNDLE_MAX_CHARS` 768→10000 | cov@4000 71.4%→71.4%（+0.5 以内） |
| 检索栈降级 | 静态 256 维 + rerank 关 vs Qwen3-8B + rerank | 后者 cov@4000 从 19.6%→71.4%，**配置是前提不是差距** |
| 评分脚本错 | 用对方 `evidence.mjs`（sha256 `76cb50cf…` 逐字副本）+ js-tiktoken 1.0.21 | 语料 52/52 sha256 校验通过；无效行号 0 |

**查询期旋钮解决不了索引期的问题**：上面三个旋钮合计只动了 1-2 点。

## 实测：损失分解（2026-10-07 修正版）

用与评分器**同一套函数**（`budget_prefix` + `parse_and_verify`）把 190 个 unit 分三类：

| 配置 | cov@1000 | cov@4000 | cov@不截断 | 预算外丢失(排序) | 缺失(absent) |
|---|---:|---:|---:|---:|---:|
| `top_k=24, sel=10, ctx=32000`（用户配置） | 38.8% | 71.4% | — | 4.7% | **23.2%** |
| `top_k=500, sel=300, ctx=20M` | — | 75.8% | **90.0%** | 14.2% | 10.0% |
| `top_k=500, sel=20/6, ctx=14000` | 38.8% | 73.6% | 76.4% | — | — |

**两个瓶颈同时存在，且此消彼长：**

1. **池子小（24）→ 23.2% 的答案根本没进池**；池子开到 500，缺失降到 10%
2. **池子大 → 多出的内容全堆在后面**，排序丢失从 4.7% 涨到 14.2%
3. 两者净效果只有 **+3.3 点**（71.4% → 75.8%）

**最强的单一信号：`cov@1000` 在所有臂里都是 38.8%，一个 token 都没动**（对方 57.2%）。
前 1000 token（≈2 段）完全由初始排序决定，而所有查询期旋钮（池子/窗口/预算/bundle/图扩展）
只影响尾部。**头部 18.4 点的差距是排序顺序问题，旋钮碰不到。**

### Oracle 上限：差距 100% 在选择与打包（2026-10-07 决定性测量）

离线计算：候选 = 语料**全部 chunk**（不是召回池），用答案键做 oracle 贪心
（每次选"能新覆盖最多答案行"的 chunk，直到 token 预算满），再按 formatter 格式渲染打分。

| | cov@4000 |
|---|---:|
| **oracle（chunk 粒度 + 完美选择/打包）** | **93.8%**（click 100%、httpx 87.5%） |
| **对方引擎** | **94.8%** |
| 本仓当前最优（margcov + 池 500） | 76.4% |
| 本仓用户原始配置 | 71.4% |

**三条结论：**

1. **chunk 粒度不是限制**——完美打包能到 93.8%
2. **对方已贴着 oracle 上限**（94.8% vs 93.8%）：它没有魔法，就是把选择与打包做到接近最优
3. **本仓的 17.4 点差距 100% 在"选哪些 chunk、怎么塞进 4000 token"**——
   不在召回（recall@24 = 100%）、不在粒度（contained 更高）、不在重排（rerank 值 +17.8）

### 重新定义"该借什么"：span 级向量的正确用途

先前把"span 级向量"当**召回**手段，已被实测否定（稠密 recall@24 = 100%）。
但 oracle 显示瓶颈在**打包**，而对方打包用的覆盖率代理正是 **span 级稠密相似度**：

```python
value = scores[eid] * (.35 + .65 * facet)   # facet 来自 span 级 dense 分数
```

本仓现在的代理是 **result-list 级 facet**（"哪一路召回了它"），粒度粗得多。
**本仓选择器 76.4% 与 oracle 93.8% 之间的距离，主要就是这个代理的粒度差。**

→ **span 级向量的正确用途是"打包时的覆盖率代理"，不是召回。** 与先前被否定的理由完全不同。

### 覆盖率代理必须是语义的：词法代理实测失败（2026-10-07 补测）

离线对比两种打包代理（候选 = 语料全部 chunk，贪心打包到 4000 token）：

| 代理 | cov@4000 |
|---|---:|
| oracle（按**答案行**覆盖贪心） | **93.8%** |
| **lexical（按查询词覆盖贪心）** | **10.9%** |

**词法代理比不做还差**：它挑中"提到查询词"的 chunk——docstring、注释、文档、配置——
而不是**实现该行为的代码**。查询是自然语言描述（"怎样限制跳转次数"），
答案代码里往往不出现那些词。

**结论：覆盖率代理必须是语义的**（对方的 `facet` 来自 span 级稠密分数）。
这条排除了一条看起来便宜的实现路径（查询词覆盖），避免在 Rust 里白做一遍。

### 落地清单（按证据强度排序）

| 优先级 | 改动 | 依据 |
|---|---|---|
| 1 | 边际覆盖选择器设为该口径推荐配置 | 实测 +2.8，唯一正收益 |
| 2 | **打包的覆盖率代理换成 span 级稠密相似度** | oracle 93.8% vs 本仓 76.4%；对方用同一机制达 94.8% |
| 3 | 打包预算改成 **token** 口径（现在 `max_context_chars` 是字符，渲染头开销未计） | rawTok 4400 > 4000，仍在超预算 |
| 4 | 池 500 + `ctx` 对齐预算 | +2.2 |
| 5 | 2 跳图扩展 / rerank 输入用实体最强 span | 对方有，本仓未试 |

### 图扩展：行级口径下同样无效（2026-10-07 补测）

在最优配置上只切换 `RETRIEVAL_GRAPH_EXPANSION_ENABLED`：

| | Top-1 | nDCG | rawTok | cov@1000 | cov@2000 | cov@4000 |
|---|---:|---:|---:|---:|---:|---:|
| OFF | 47/60 | 0.886 | 4400 | 40.4% | 67.2% | 76.4% |
| ON（1 跳，RRF 权重 0.1） | 47/60 | 0.886 | 4399 | 39.9% | 67.2% | 76.4% |

**无效果**，与此前文件级口径的 +0.00 一致。本仓的图扩展是"独立一路低权重进 RRF"，
而对方是 **2 跳置信度传播 + 语义兼容门控，且在 rerank 之前**扩候选集——机制不同，
不能据此否定对方那条路线，只能说明本仓当前实现无效。

### 当前最优配置（行级口径）

| 开关 | 值 | 相对默认 |
|---|---|---|
| `RETRIEVAL_MARGINAL_COVERAGE_ENABLED` | **true** | cov@4000 +2.8（**唯一正收益**） |
| `RETRIEVAL_DEFAULT_TOP_K` | 500 | +2.2 |
| `RETRIEVAL_MAX_CHUNKS_PER_PATH` | 6 | 同上 |
| `RETRIEVAL_MAX_CONTEXT_CHARS` | 14000（≈4000 token） | 同上 |
| `RETRIEVAL_FINAL_SELECT_K` | 20 | 同上 |
| `RERANK_ENABLED` | true | **+17.8**（必开） |
| `RETRIEVAL_GRAPH_EXPANSION_ENABLED` | false | 无效果 |
| `RETRIEVAL_CONTEXT_BUNDLE_ENABLED` | false | +0.5，不值得 |

**最优：cov@1000 40.4% / cov@2000 67.2% / cov@4000 76.4%（对方 57.2% / — / 94.8%）。**

### 边际覆盖选择器：两个口径给出相反结论（2026-10-07 关键补测）

同一配置（池 500、`sel=20/6`、`ctx=14000`、rerank ON）只切换
`RETRIEVAL_MARGINAL_COVERAGE_ENABLED`（STEP-7 从对方 `pack_entities` 移植的公式）：

| | Top-1 | nDCG | rawTok | cov@1000 | cov@2000 | cov@4000 |
|---|---:|---:|---:|---:|---:|---:|
| OFF（基线） | 47/60 | 0.901 | 4470 | 38.8% | 64.4% | 73.6% |
| **ON** | 47/60 | 0.886 | 4400 | **40.4%** | **67.2%** | **76.4%** |

**行级 +2.8（含头部 +1.6），文件级 nDCG −0.015。**

这是本 Note 里唯一一个正收益，也是**唯一一个两个口径结论相反**的改动：

- STEP-7 当初在 flask / cc-switch 的**文件级**口径上测出 −15/−14 分 → 关闭
- 在**行级**口径上是 +2.8 → 应该开

**教训：只有文件级口径时，这个正确的机制会被否掉。** 这也说明"文件级 98% 命中率"的
口径会系统性地低估覆盖率感知的选择策略——因为它优化的是"证据是否齐"，
而文件级只看"文件名对不对"。

**注意**：本 Note 先前写的 STEP-20"头部排序"方向，与这条结果一致——
边际覆盖正是按 facet 覆盖度加权，优先排多路共同命中的内容。

### rerank 的作用（2026-10-07 补测）

同一配置（池 500、`sel=20/6`、`ctx=14000`）只切换 `RERANK_ENABLED`：

| | Top-1 | nDCG | rawTok | cov@1000 | cov@2000 | cov@4000 |
|---|---:|---:|---:|---:|---:|---:|
| **rerank ON** | 47/60 | 0.901 | 4470 | **38.8%** | 64.4% | **73.6%** |
| rerank OFF | 46/60 | 0.892 | 4808 | 33.8% | 52.2% | **55.8%** |

**`Qwen3-Reranker-8B` 值 +17.8 点（cov@4000）**。重排通路正常、无降级——
差距不在"有没有重排"，而在**重排看到什么**与**重排后怎么打包**：

| | 对方 | 本仓 |
|---|---|---|
| rerank 输入 | 实体**最强 span 拼装**（`representation()` ≤5000 字符；源码注释明确 "not always their prefix"） | chunk 原文（超 `RERANK_DOC_CHARS=5000` 截断） |
| 候选单位 | **80 个实体**（同符号全部 span = 一个文档） | 最多 500 个 chunk（同一函数多段各自竞争名次） |
| 打包 | 实体原子（选中即输出全部 span） | 单 chunk 逐个 |

**同一函数的多段在本仓各自当文档竞争，在对方是一个文档**——既切碎重排信号，
又导致选中后只输出一段、覆盖不了多段答案（`allOf` 需要全部 span）。

### 各环节实测结论（每条都有实验支撑）

| 环节 | 结论 | 证据 |
|---|---|---|
| 索引里有答案 span | ✓ 不是问题 | 本仓 chunk 完整包含答案 span：click 39/39、httpx 49/50（对方 37/39、48/50） |
| 稠密召回 | ✓ 不是问题 | 用 Qwen3-Embedding-8B 离线复算：chunk 级 recall@24 = **100%**（对方 unit 级同为 100%） |
| 池子大小 | 部分问题 | `top_k` 24→500：缺失 23.2%→10%，但净收益仅 +3.3 点 |
| 窗口大小 | 不是问题 | `select_k` 300→500 无变化 |
| chunk 粒度 | 不是问题 | 本仓 contained 率**高于**对方 |
| **响应内排序** | **主要问题** | `cov@1000` 恒为 38.8%；不截断时 90% vs 截断后 75.8% |

### 已排除的查询期旋钮（别再试）

| 旋钮 | 实测 |
|---|---|
| `select_k` 10→300 | cov@4000 +2.2（全部来自尾部） |
| `max_chunks_per_path` 2→6 | 同上 |
| `bundle` 阈值 768→10000 | +0.5 |
| `top_k` 24→500 | +3.3 |
| 图扩展 | 默认关，未在此口径下验证 |

**结论：查询期调参解决不了头部排序问题。**

## 对方的机制（读 `src/retrieval/{engine,entities,languages/python}.py`）

### 索引期：unit = AST span

- `max_lines=65`；超出按**语句边界**切（`ast.stmt` 行号，`start + max_lines//2 <= b <= stop+1`）
- 每个 unit 带 `symbol`（`module.name`）、`owner`、`kind`（`function`/`class-body`/`module`）、
  `start/end`、`calls`、`edges`、`relations`、`scope`
- **嵌套定义各自成 unit，外层保留剩余行**（`cursor = n.end_lineno + 1`）
- 边静态按名解析：`calls`（`self.`/`cls.`→owner、import 别名、module 前缀）、`inherits`、
  `member_of`、**`same_symbol`**（把被切开的片段链回同一符号），各带 `confidence` 与 `resolution`
- **`cost = len(tiktoken.encode(render(u))) + 2`——索引期算好 token 成本**

### 选择期：实体原子打包

```python
POLICY = {'candidateEntities': 80, 'anchorEntities': 8, 'graphHops': 2,
          'maxAssembledEntities': 128, 'maxAtomicBudgetFraction': .65,
          'facetTemperature': .06, 'documentChars': 5000}
```

1. 稠密检索是 **span 级**（每 unit 一个向量），用 `np.maximum.at` 聚合到实体
2. 实体 = `groups[(symbol, kind)]` 的**全部 span**（索引期预分组）
3. rerank 输入是实体的**最强 span 拼装**（`representation()`，≤5000 字符），不是前缀
4. `pack_entities`：`entity['cost'] <= budget*.65` 时**整个实体作为一个 action**；
   超出才退化为逐 span。增益 `gain = .7*mean(value/(1+covered)) + .3*score`，
   再 `/(max(120,cost)/300)**.35`，阈值 `.015`
5. `complete()`：2 跳静态图扩展，`proposal = confidence * link_weight * (.4 + .6*compatibility)`，
   `compatibility = exp(min(0, dense[target]-dense[source])/.12)`

**本仓 STEP-7 抄对了第 4 条的增益公式，但漏了它依赖的前提：实体必须在索引期就存在。**
查询期现凑的实体，候选池里没有的 span 永远凑不进来——这是 STEP-10 bundle 零收益的真因。

## 本仓现状对照

| | 对方 | 本仓 Rust |
|---|---|---|
| unit | 每个定义一个 span，≤65 行，语句边界切 | cAST 块，**会合并小定义**（`min_chunk_chars`） |
| symbol/owner | **索引期**由 AST 给出 | 查询期正则查 `symbol_occurrences`（`annotate_symbols`） |
| `same_symbol` | **索引期**建立 | 无 |
| token 成本 | **索引期**算好 | 查询期用 `char_len` 近似 |
| 实体 | 索引期 `(symbol,kind)` 预分组 | 查询期从候选池现凑（`build_entities`） |
| 图扩展 | 2 跳，rerank 前按置信度传播 | 1 跳，独立 RRF 一路（`graph_candidates`，默认关） |
| rerank 输入 | 实体最强 span 拼装 | chunk 原文 |

## Decision（待执行，已按实测修正）

**Rust 不再与 Python 对齐**（用户 2026-10-07 明确：Python 是原版，要的是 Rust 版），
索引结构可以自由改。

原方案的"span 级向量"**已被实测否定**（稠密 recall@24 本就是 100%），改为针对
**头部排序 + 预算感知打包**：

### STEP-19：候选池扩大 + 预算感知打包

- `default_top_k` 从 24 提到 ≥200（缺失 23.2% → 10%）
- 选择器按**token 预算**打包（不是字符预算）：`max_context_chars` 与评测预算对齐，
  或直接引入 token 计数
- 验收：`cov@4000` 相对 71.4% 有提升，且 `cov@1000` 不得下降、文件级 Top-1/nDCG 不降

### STEP-20：头部排序（预期收益最大）

- `cov@1000` 在所有臂里恒为 38.8%（对方 57.2%）——**前 1000 token 的排序是主战场**
- 可借鉴对方的做法：`value = scores[eid] * (.35 + .65 * facet)`（按 facet 覆盖度加权，
  多路召回共同命中的排前）；以及 `pack_entities` 的**原子实体**（同符号全部 span 一起输出，
  减少 `Path:`/`Lines:` 头开销、提高单位 token 的证据密度）
- 验收：`cov@1000` 相对 38.8% 有提升（这是唯一还没被任何旋钮影响过的指标）

### STEP-21：缩短单段体积

- 本仓 chunk 平均 ~45 行 ≈ 600-800 token，4000 token 只装 5-6 段
- 对方 `representation()` 只取**最强 span**（≤5000 字符），不是整个函数
- 验收：同 STEP-20

## 验收基线（复现命令）

```bash
cd oce-benchmark
uv run python scripts/fetch_eval_corpus.py --out /tmp/oce-eval-corpus   # 52/52 校验
uv run python scripts/eval_dual.py \
  --dataset-root ../OpenContextEngine/eval/expanded-v1 \
  --datasets click httpx zod --corpus-root /tmp/oce-eval-corpus \
  --base-url http://127.0.0.1:8986 --output results/dual-all.md \
  --json-output results/dual-all.json --upload
```

当前基线：`results/dual-all-realnollm.{md,json}`（Top-1 47/60、cov@4000 71.4%）。

## 本轮的错误结论（已撤回，勿引用）

1. **"cov@4000 57.5%"**：我引用过一个输出目录并不存在的表格。该数字来自用户/pi 的测量，
   不是我测的；我的实测是 71.4%（用户配置）/ 19.6%（静态降级档）。
2. **"STEP-12 指纹错误信息会打印两个相同指纹仍报不符"**：实测推翻——同一数据目录连续
   启动两次都成功。该 bug 不存在。
3. **"bundle 零收益是因为阈值 768 太小"**：实测推翻——768→10000 只 +0.5 点。

## 附带发现（值得单独吸收）

- 对方查询嵌入带指令前缀：`'Instruct: Retrieve source code implementing the requested behavior.\nQuery: '`
  （`Qwen3-Embedding-4B`）。本仓未见同等前缀，值得验证是否影响召回质量。
- 词法侧对 `path + name` 做 **3 倍字段加权**（`terms((path + ' ' + name + ' ') * 3 + text)`）。

## 本轮的错误结论（全部撤回，勿引用）

| # | 我说过的 | 实际 | 发现方式 |
|---|---|---|---|
| 1 | `cov@4000 57.5%` | 输出目录不存在，从未落盘 | 用户指出 |
| 2 | `19.6%` 当基线结论 | `.env` 0 字节，两臂同配置自比 | 自查 |
| 3 | STEP-12 指纹错误信息自相矛盾 | 同目录连续启动两次都成功，**bug 不存在** | 实测推翻 |
| 4 | bundle 零收益因阈值 768 太小 | 768→10000 只 +0.5 | 实测推翻 |
| 5 | chunker 合并小定义导致 span 对不齐 | 本仓 contained 率**高于**对方 | 实测推翻 |
| 6 | "全池 57 倍 token，cov 纹丝不动" | 漏了 `top_k=24` 截断；实际 2.1 倍、+2.2 点 | 用户追问 |
| 7 | "正确答案不在候选池里" | 池开大后 absent 仅 10% | 实测推翻 |
| 8 | "chunk recall@24=30%、unit=45%，支持 span 级向量" | 读错输出行；实际两者都是 100% | 重跑推翻 |
| 9 | "absent = 0%，损失全在排序" | 实际 absent 23.2%（池=24 时） | 重跑推翻 |

**教训：本轮的每一个数字，第一次都是错的。** 凡引用未落盘/未重跑的输出，一律不可信。
评测脚本必须（a）配置在打分时刻可证明，（b）结果落盘后再引用，（c）对照臂之间逐项确认
配置差异真的生效（本次有 3 次因配置未生效而测出"完全相同"的假对照）。
