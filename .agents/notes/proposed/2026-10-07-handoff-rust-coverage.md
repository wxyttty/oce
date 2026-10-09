# 交接文档：Rust 版行级证据覆盖（cov@4000）差距攻坚

Status: handoff（2026-10-07）
接手对象: pi
相关 Note: `.agents/notes/proposed/2026-10-07-rust-span-entity-gap.md`（诊断全文，含 9 条错误结论撤回）

---

## 0. 一句话任务

在 **OpenContextEngine 自己的数据集与评分器**下，把 Rust 版 oce 的
**`cov@4000` 从 76.4% 提到 ≥90%**（对方引擎 94.8%，oracle 上限 93.8%）。

**不要**用文件级口径（flask/cc-switch 的 Top-1/nDCG）判断这项工作——两套口径在这件事上
结论相反（见 §6.4）。

---

## 1. 度量方法（先跑通，再改代码）

```bash
cd oce-benchmark

# 1) 语料：按 snapshot 的 pinned commit 拉取 + 逐文件 sha256 校验（52/52）
uv run python scripts/fetch_eval_corpus.py --out /tmp/oce-eval-corpus

# 2) 启动服务（配置见 §5），然后跑双口径评测
uv run python scripts/eval_dual.py \
  --dataset-root ../OpenContextEngine/eval/expanded-v1 \
  --datasets click httpx zod \
  --corpus-root /tmp/oce-eval-corpus \
  --base-url http://127.0.0.1:8990 \
  --output results/dual-all-<臂名>.md \
  --json-output results/dual-all-<臂名>.json \
  --budgets 1000 2000 4000 \
  --upload
```

报告里要看的是 **`cov@4000`**（以及 `cov@1000`——头部指标，见 §4）。

数据集：3 仓（click/httpx/zod）× 10 case × zh/en = **60 题**，共 **190 个 unit**。
评分器是对方 `src/eval/evidence.mjs` 的**逐字移植**（tokenizer 用 `tiktoken` cl100k_base，
与对方 protocol 的 `js-tiktoken@1.0.21` 同编码）。

---

## 2. 当前数字（全部落盘可查）

| 配置 | Top-1 | nDCG | rawTok | cov@1000 | cov@2000 | cov@4000 | 落盘文件 |
|---|---:|---:|---:|---:|---:|---:|---|
| 用户原始配置 | 47/60 | 0.901 | 3948 | 38.8% | 63.9% | **71.4%** | `dual-all-realnollm.json` |
| **当前最优**（§5） | 47/60 | 0.886 | 4400 | **40.4%** | 67.2% | **76.4%** | `dual-all-margcov.json` |
| 同上 + 图扩展 | 47/60 | 0.886 | 4399 | 39.9% | 67.2% | 76.4% | `dual-all-graph.json` |
| `select_k=300`（池仍被 `top_k=24` 截断） | 47/60 | 0.901 | 8328 | — | — | 73.6% | `dual-all-pool.json` |
| rerank OFF | 46/60 | 0.892 | 4808 | 33.8% | 52.2% | 55.8% | `dual-all-norerank.json` |
| 全池 `top_k=500` + `sel=300`（**仅 click，20 题**） | 16/20 | 0.920 | 9419 | — | — | 75.8% | `dual-click-pool500.json` |
| **对方引擎（公开数字）** | — | — | 3736 | **57.2%** | — | **94.8%** | — |
| **oracle 上限**（离线，§3） | — | — | — | — | — | **93.8%** | 脚本见 `scripts/diagnostics/` |

---

## 3. 已验证事实（不要重复验证）

### 3.1 差距 100% 在"选择与打包"，不在召回

| 环节 | 结论 | 证据 |
|---|---|---|
| 索引里有答案 span | ✓ 不是问题 | 本仓 chunk 完整包含答案 span：click 39/39、httpx 49/50（**对方 37/39、48/50**） |
| 稠密召回 | ✓ 不是问题 | 离线复算（Qwen3-Embedding-8B）：chunk 级 **recall@24 = 100%**，unit 级同为 100% |
| chunk 粒度 | ✓ 不是问题 | contained 率高于对方 |
| API rerank | ✓ 在工作 | ON 73.6% vs OFF 55.8% → **值 +17.8 点** |
| 窗口大小 | ✓ 不是问题 | `select_k` 300→500 无变化 |
| 池子大小 | 部分问题 | `top_k` 24→500：缺失 23.2%→10%，净收益 +3.3 |
| **选择与打包** | **主要问题** | oracle 93.8% vs 本仓 76.4% |

### 3.2 oracle 上限 = 93.8%

离线：候选 = 语料**全部 chunk**，用答案键贪心打包到 4000 token。

**对方引擎（94.8%）已贴着这个上限** —— 它没有特殊机制，就是把"选哪些 chunk、
怎么塞进 4000 token"做到接近最优。**本仓的 17.4 点差距全在这一步。**

### 3.3 打包预算仍在超支

当前最优配置 `rawTok = 4400 > 4000` —— `RETRIEVAL_MAX_CONTEXT_CHARS=14000` 是**字符**口径，
没有计入 `Path:`/`Lines:` 头和行号开销。约 10% 的输出被 `budgetPrefix` 丢掉。

---

## 4. 下一步（唯一方向，有 oracle 背书）

### 4.1 把打包的覆盖率代理换成 **span 级语义相似度**

对方的机制（`src/retrieval/entities.py`，`entity-cascade-v3`）：

```python
POLICY = {'candidateEntities': 80, 'maxAtomicBudgetFraction': .65,
          'facetTemperature': .06, 'documentChars': 5000}

# 稠密检索是 span 级（每 unit 一个向量），用 np.maximum.at 聚合到实体
dense = self.aggregate(unit_dense)

# 打包时的覆盖率代理：facet 来自 span 级 dense 分数
value = scores[eid] * (.35 + .65 * facet)
gain  = .7 * mean(value / (1 + covered)) + .3 * scores[eid]
gain /= (max(120, cost) / 300) ** .35          # cost = 该实体的 token 成本
if entity['cost'] <= budget * .65: 整组原子入席 else 逐 span
```

本仓现状：`select_with_coverage` 已有同款增益公式（STEP-7 移植），但
**facet 代理的粒度是 result-list 级**（"哪一路召回了它"），不是 span 级语义相似度。
**76.4% 与 93.8% 之间的距离主要就是这个粒度差。**

> **重要澄清**：span 级向量的用途是**打包时的覆盖率代理**，**不是召回**。
> 先前把它当召回手段已被实测否定（recall@24 已是 100%）。不要搞错用途。

### 4.2 验收标准

- **主指标**：`cov@4000` 相对 **76.4%** 有提升；目标 ≥90%
- **不得回退**：`cov@1000` 不低于 40.4%；文件级 Top-1 不低于 47/60
- 每改一处都要跑 §1 的命令，把 JSON 落盘后再引用数字

### 4.3 其他候选（优先级低）

| 优先级 | 改动 | 依据 |
|---|---|---|
| 2 | 打包预算改 **token** 口径（现在字符口径，§3.3） | rawTok 4400 > 4000 |
| 3 | 池 500 + `ctx` 对齐预算 | +2.2（已验证） |
| 4 | 边际覆盖设为该口径推荐配置 | +2.8（已验证） |
| 5 | 2 跳图扩展（对方 `graphHops=2` + 置信度传播 + 语义兼容门控，rerank **之前**） | 对方有，本仓未试；本仓 1 跳 RRF 实现无效 |
| 6 | rerank 输入用实体最强 span 拼装（对方 `representation()` ≤5000 字符，源码注释 "not always their prefix"） | 对方有，本仓喂 chunk 原文 |

---

## 5. 环境与配置

### 5.1 服务启动

```bash
cd rust
cargo build --release -p oce-server
./target/release/oce serve --data-dir /tmp/oce-xeval/data-real --host 127.0.0.1 --port 8990
```

数据目录 `/tmp/oce-xeval/data-real/.env`（**用户真实配置**，密钥不在此文档中）：

```ini
# 检索栈（决定一切的前提）
EMBED_PROVIDER=openai
EMBED_MODEL=Qwen3-Embedding-8B
EMBED_DIMENSIONS=1024
EMBED_ENDPOINT=https://ai.gitee.com/v1/embeddings
RERANK_ENABLED=true
RERANK_MODEL=Qwen3-Reranker-8B
RERANK_ENDPOINT=https://ai.gitee.com/v1/rerank
LLM_RERANK_ENABLED=false          # 用户明确要求：不开 LLM rerank

# 当前最优的检索旋钮
RETRIEVAL_DEFAULT_TOP_K=500
RERANK_MAX_DOCS=500
RETRIEVAL_FINAL_SELECT_K=20
RETRIEVAL_MAX_CHUNKS_PER_PATH=6
RETRIEVAL_MAX_CONTEXT_CHARS=14000
RETRIEVAL_MARGINAL_COVERAGE_ENABLED=true
RETRIEVAL_GRAPH_EXPANSION_ENABLED=false   # 无效，关掉
```

索引指纹（证明配置生效）：`openai:Qwen3-Embedding-8B:1024 dim=1024 etext=v1 chunk=v2`

### 5.2 索引

- 换嵌入模型/维度必须换数据目录（维度不可变）
- 切块器改动会让 `chunk=vN` 变化 → 旧索引 fail-closed，需重建
- **检索旋钮不影响指纹**，改旋钮只需重启服务，可复用索引

---

## 6. 陷阱（这一轮用大量返工换来的，务必遵守）

### 6.1 每次跑之前必须证明配置生效

这一轮有 **3 次**因为配置没生效而测出"两臂完全相同"的假对照：
- `.env` 被 shell heredoc 截断成 0 字节
- 同一 key 在文件里出现两次（后写的没生效）
- 改完 `.env` 但服务没重启

**三步验证法**（缺一不可）：

```bash
# ① 文件内容与大小
grep -E "^(RETRIEVAL_XXX|EMBED_MODEL)=" <data-dir>/.env && wc -c <data-dir>/.env
# ② 服务进程指向同一数据目录
ps aux | grep "oce serve" | grep -- "--data-dir"
# ③ 索引指纹（嵌入侧）
cat <data-dir>/oce.tdb.model
```

**再加一条**：两臂数字**完全相同**时，先怀疑配置没生效，不要先下"无效果"的结论。

### 6.2 数字落盘后再引用

这一轮我引用过一个**输出目录并不存在**的表格（`cov@4000 57.5%`），
因为它"恰好"等于别人给的数字就没去核。

**规则**：只引用 `results/*.json` 里存在、且能追溯到某条命令的输出。

### 6.3 双口径必须一起看

**同一个改动在两个口径下结论相反**（实测）：

| 改动 | 文件级（flask/cc-switch） | 行级（本任务） |
|---|---|---|
| `RETRIEVAL_MARGINAL_COVERAGE_ENABLED` | nDCG **−0.015**（微负） | cov@4000 **+2.8**（正） |

我 STEP-7 就是因为只看文件级口径而关掉了这个正确机制。
**本任务的所有判断以行级 `cov@4000` 为准，但必须确认文件级 Top-1 不塌。**

### 6.4 文件级口径接近饱和，会掩盖行级问题

本仓文件命中率 **98.4%**（答案文件出现在输出里），但同一批响应的行级覆盖只有 22–76%。
`Path: src/click/core.py` 一行在文件级能拿分，在行级几乎不值钱。
**用文件级口径评估本任务会得出"已经很好"的错误结论。**

### 6.5 已排除的路径（不要重做）

| 假设 | 实测 | 结论 |
|---|---|---|
| chunk 粒度太粗 | contained 率**高于**对方 | 排除 |
| 稠密召回不足 | recall@24 = **100%** | 排除 |
| 池子太小是主因 | 24→500 净收益 +3.3 | 收益有限 |
| 窗口太窄 | `select_k` 300→500 无变化 | 排除 |
| bundle 阈值太小 | 768→10000 只 +0.5 | 排除 |
| reranker 坏了 | 值 **+17.8** | 排除 |
| 扩展到完整定义 | **74.2% → 71.9%** | **负收益** |
| 词法查询词覆盖当代理 | **10.9%**（oracle 93.8%） | **比不做还差** |
| 图扩展（1 跳 RRF） | 无变化 | 本仓实现无效 |

### 6.6 nollm 档是确定性的

`LLM_RERANK_ENABLED=false` 时同一配置两次运行**逐位相同**（已验证）。
所以不需要多次取中位；**但这也意味着任何数字差异都必然来自配置或代码，不是噪声**——
这反过来要求 §6.1 的配置验证必须严格。

---

## 7. 文件清单

### 评测（`oce-benchmark`）

| 文件 | 作用 |
|---|---|
| `scripts/eval_dual.py` | **双口径评测主脚本**（文件级 + 行级，多数据集一份报告） |
| `scripts/fetch_eval_corpus.py` | 按 pinned commit 拉语料 + sha256 校验 |
| `docs/dual-metric-eval.md` | 两套口径的差异与用法 |
| `scripts/diagnostics/` | 6 个离线诊断脚本 + README（oracle 上限、损失分解、代理对比…） |
| `results/dual-all-*.json` | 各臂落盘数据 |

### 诊断

| 文件 | 作用 |
|---|---|
| `.agents/notes/proposed/2026-10-07-rust-span-entity-gap.md` | **诊断全文**：oracle 上限、对方机制源码分析、落地清单、9 条错误结论撤回、4 条负面结果 |

### 参考（对方仓库，只读）

| 路径 | 内容 |
|---|---|
| `../OpenContextEngine/src/retrieval/entities.py` | 实体聚合、`pack_entities` 打包、`complete` 图扩展（**核心参考**） |
| `../OpenContextEngine/src/retrieval/languages/python.py` | unit 切分（每定义一个 span、`same_symbol` 边、静态调用解析） |
| `../OpenContextEngine/src/retrieval/engine.py` | unit 脚手架、词法索引、token 成本 |
| `../OpenContextEngine/src/eval/evidence.mjs` | 评分器原件（本仓 Python 版是它的逐字移植） |
| `../OpenContextEngine/eval/expanded-v1/{click,httpx,zod}/` | 数据集（snapshot + queries + answers，**不含语料内容**） |

---

## 8. 未决问题（需要用户或 pi 判断）

1. **`cov@1000` 的 18.4 点差距**（本仓 40.4% vs 对方 57.2%）
   在所有查询期旋钮下**恒定不变**，说明它由初始排序决定。是否值得单独攻？
2. **是否把边际覆盖设为默认开**（行级 +2.8 / 文件级 nDCG −0.015）——
   这是产品取舍，需要用户拍板。
3. **Rust 不再与 Python 对齐**（用户 2026-10-07 明确）——
   §4.1 的索引期改动可以自由改 schema，不再受"PG schema 与 alembic head 逐列一致"约束。
4. **对方 2 跳图扩展与实体最强 span 拼装**（§4.3 第 5、6 条）尚未在本仓试过，
   机制与现有实现不同，不能据"本仓 1 跳无效"否定。

---

## 9. 给接手者的最后提醒

这一轮的所有权结论是：**差距不在"能不能拿到"，而在"怎么选、怎么塞"。**
oracle 93.8% 已经证明 chunk 粒度足够、召回足够；对方 94.8% 已经证明这个上限可达。

**不要在没有 oracle/上限测量的情况下反复试错** —— 这一轮我试了 9 个假设，
其中 8 个被实测否定，全部因为缺少"上限是多少"这个参照。
`scripts/diagnostics/oracle_ceiling.py` 就是为此保留的。
