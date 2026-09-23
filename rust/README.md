# OpenContextEngine — Rust 版（oce-rs）

Python 版 [`src/oce`](../src/oce) 的 Rust 重写。目标：**能力对齐（ACE 兼容）+ 索引/查询性能提升**，
个人模式存储引擎由「SQLite 元数据 + Milvus Lite 向量」迁移到 **TriviumDB**。

> 当前状态：**个人模式 + 服务模式全链路可用**（API/切块/索引/检索/凭据/监控/GC/CLI）。
> 服务模式：PostgreSQL 元数据（sqlx，schema 与 alembic head 逐列一致）+ Redis 队列
> （Lua 去重防幽灵消息）+ 向量后端可选 TriviumDB（默认）或 pgvector（PG 一体化）。

## 快速开始

```bash
cd rust
cargo build --release

# 初始化个人模式数据目录（生成 .env）
./target/release/oce init --data-dir ~/.oce/data

# 启动服务（默认 127.0.0.1:8986）——零配置：默认向量化为本地静态查表模型
# （Model2Vec potion-base-8M，256 维；首次自动下载缓存，之后完全离线）
./target/release/oce serve --data-dir ~/.oce/data

# 可选：切换到 OpenAI 兼容 API 嵌入（EMBED_PROVIDER=auto 时配置了 key 即自动切换）
EMBED_API_KEY=sk-xxx ./target/release/oce serve --data-dir ~/.oce/data

# 体检：数据文件/schema/向量引擎/嵌入提供方逐项自检
./target/release/oce doctor --data-dir ~/.oce/data
```

环境变量与 Python 版**完全兼容**（同前缀同语义：`API_KEY` / `EMBED_*` / `LLM_*` /
`RETRIEVAL_*` / `MILVUS_DENSE_DIM` / `MONITORING_*`…），新增 `TRIVIUM_*` 前缀
（`TRIVIUM_PATH` / `TRIVIUM_SYNC_MODE` / `TRIVIUM_STORAGE_MODE` / `TRIVIUM_AUTO_BUILD_QUIVER`）。

## 工作区结构

```
rust/
├── crates/
│   ├── oce-core/     # 领域层：切块（recursive/cAST/markdown/vue/jsp）、检索编排、
│   │                 #   查询分类/规划/融合/覆盖度选择、formatter、symbol 提取、协议 trait
│   ├── oce-infra/    # 基础设施：TriviumDB 向量引擎、SQLite 元数据、OpenAI 兼容客户端、
│   │                 #   凭据解析运行时、监控 sink
│   ├── oce-app/      # 应用层：Container（组合根）、RetrievalApplication、EmbedWorker
│   ├── oce-server/   # API 层：axum 路由（ACE 兼容 + /admin）、鉴权、CLI 二进制 `oce`
│   └── oce-bench/    # 基准：chunk / index / query 阶段耗时
└── README.md
```

依赖方向：`core ← infra ← app ← server`（与 Python `shared <- domain <- application <- api` 对齐）。

## 存储决策：TriviumDB 承接什么

| 数据 | Python 版 | Rust 版 | 说明 |
|---|---|---|---|
| chunk 向量 | Milvus Lite（子进程 gRPC） | **TriviumDB** `kind="chunk"` 节点 | 进程内检索，消除 IPC |
| 路径索引 | Milvus 独立 collection | **TriviumDB** `kind="path"` 节点 | 同库不同 kind，`payload_filter` 分流 |
| blob 注册表 / staging / checkpoint 链 | SQLite | SQLite（同 schema） | 关系工作负载（UPSERT/JOIN/TTL 清理） |
| symbol_occurrences 精确召回 | SQLite | SQLite（同 schema 同查询语义） | identifier 倒排 + kind 打分逐字保留 |
| model_credentials | SQLite | SQLite（同 schema） | CRUD/唯一约束/热重载，尾 4 位脱敏 |
| 监控指标 | SQLite | SQLite（同 schema） | 批量缓冲 + 周期 flush |

**为什么关系元数据保留 SQLite**：checkpoint 链、凭据唯一约束、监控 TTL 清理是典型关系
工作负载；TriviumDB 的文档/图模型表达 JOIN 类查询需要绕行，收益为负。TriviumDB 的价值
在向量检索路径（进程内 + QuIVer ANN ≥1 万节点自动激活），已完整兑现。

### TriviumDB 节点模型

- `kind="chunk"`：vector = embedding；payload 携带
  `chunk_id/content_hash/blob_name/path/start_line/end_line/content`。
  节点 ID = sha256(chunk_id) 前 8 字节（碰撞线性探测，读回校验）→ **upsert 天然幂等**，
  与 Python Milvus collection 的 chunk_id 主键语义一致。
- `kind="path"`：vector = 路径文档 embedding；payload `{blob_name, path, path_id}`。
- `create_index("kind"/"blob_name"/"identifier")`：Hash 属性索引，检索前置过滤 O(1)。
- 写入用 `SyncMode::Off` 批量导入 + 结束恢复并 flush（SSD 友好，与 WAL 设计对齐）。

## 能力对齐清单

- [x] ACE 数据面：`/find-missing` `/batch-upload` `/agents/codebase-retrieval`
      `/checkpoint-blobs` `/agents/blob-status`（Bearer 鉴权、401 OpenAI 风格错误体、
      400/404/503 与 FastAPI 语义一致、CORS 白名单、`/health` `/version`）
- [x] 内容寻址 `sha256(path+content)`、READY 即可检索不变量、staging 补嵌、
      EMBED_ENABLED=false 保持 pending（历史事故语义保留）
- [x] 切块器：RecursiveChunker（LangChain RecursiveCharacterTextSplitter 忠实移植：
      分隔符表/合并算法/start_index）、cAST（分窗/intact 声明/React 顶层/小区间合并）、
      markdown（标题层级/围栏原子/短节合并）、vue/svelte、jsp（tree-sitter-html）
- [x] 检索编排：意图策略表（LLM 分类 S/C/R/P/F/O/M）、启发式分类、查询分解（RRF 融合）、
      LLM 重排/改写（prompt 与回显拦截一致）、路径 boost、精确标识符召回（scope 上限）、
      source priority、置信度门槛、覆盖度选择、formatter
- [x] 凭据：model_credentials CRUD/复制/热重载（`/admin/credentials*`），DB 解析 → env 回落，
      kind 专属字段逐项回落，维度校验
- [x] Admin：queue 状态/reset（个人模式队列禁用语义）、requeue-stale、GC（dry_run/链+blob 过期）、
      stats（窗口聚合）
- [x] 监控：token usage / retrieval 审计（阶段耗时）落库，旁路非阻塞
- [x] 向量化双提供方：**Model2Vec 静态查表默认**（`EMBED_PROVIDER=auto|static|openai`）。
      默认模型 `potion-multilingual-128M`（多语言）：真实工作集探针显示中文业务查询
      margin +0.23（纯英文 potion-base-8M 为 **-0.10**，无关块得分反而更高）。
      索引带模型指纹 sidecar（`oce.tdb.model`），换模型 fail-closed 强制重建，
      杜绝跨模型向量静默污染
- [x] `dist-*` 构建产物目录准入过滤（实测 dist-prod 混入索引稀释结果）
- [x] 符号提取 rayon 并行化（≥8 块并行预提取，SQLite 事务内只批量写入）
- [x] `RETRIEVAL_FILE_DESC_ENABLED`：清单/配置文件规则层描述注入 embedding_text 与
      rerank 文档（BCE filedesc.go 移植；nollm 双仓 A/B +30.46 分，默认关）
- [x] `RETRIEVAL_RELATED_SYMBOLS_ENABLED`：`<related_symbols>` grep leads 输出层追加
      （BCE relatedSymbolHints 移植，fanout 门控 + 停用词过滤，默认关）
- [x] `RETRIEVAL_BROAD_MODE_ENABLED`：架构/概览探索型查询专用 regime（BCE broad.go
      移植：宽窗口 20 hits / per-path 2、manifest 结构先验进 RRF、超 28 行摘录骨架化
      且省略段行号重同步、定位语气查询一票否决；基准 200 题无探索型查询故 A/B 零
      差异，机制探针见 note，默认关）
- [x] `RETRIEVAL_META_DIR_PENALTY_ENABLED`：.github 等 CI/模板目录 ×0.5 降权，CI
      意图查询豁免（semble_rs M2 移植；.github 入窗 24→14，默认关）
- [x] `RETRIEVAL_SPAN_MERGE_ENABLED`：相邻 span 合并 + 小片段补全（semble_rs M1
      移植：≤2 行间隔合并、<6 行补全、chunk 行重构缺行回退、池加宽回填；大库下
      覆盖选择器已防碎片化、基准面中性，受益面为小 scope 工作区，默认关）
- [x] `oce doctor` 迁移/排障自检（只读，逐项报告 .env/SQLite/TriviumDB/嵌入提供方）
- [ ] 服务模式：PostgreSQL（sqlx）、Redis 队列、Milvus gRPC 后端（trait 已留扩展点）
- [ ] 资源采样器（sysinfo 磁盘/CPU 采样）、p50/p95 延迟聚合

## 性能

`cargo run -p oce-bench -- index <dir> --dim 1024`（本仓库 rust/ 树，58 文件 41.9 万字符）：

```
chunk     235.1 ms   (1741 Kchars/s)   ← cAST+递归切块（tree-sitter 原生）
embed      90.1 ms   (fake, 218 chunks)← 确定性假向量，隔离本地处理与外部 API
tdb        70.1 ms   (3111 upserts/s)  ← TriviumDB 批量导入（含 flush）
sqlite    493.5 ms                     ← 元数据+符号提取（正则密集，未并行）
total     888.8 ms   nodes=218

query avg 2.04 ms/查询（embed + 向量检索 top-50 + blob 过滤，dim=1024）
```

### 检索质量基准（oce-benchmark，Top-1 + nDCG@10，每基准 100 题）

| 配置 | flask Top-1 | flask nDCG@10 | flask 总分 | cc-switch Top-1 | cc-switch nDCG@10 | cc-switch 总分 |
|---|---:|---:|---:|---:|---:|---:|
| 静态多语言，无混合检索 | 39% | 0.567 | 47.8% | 28% | 0.342 | 31.1% |
| 静态多语言 + BM25 混合，无 LLM | 52% | 0.686 | 60.3% | 28% | 0.371 | 32.5% |
| potion-code-16M-v2 + 混合检索 | 41% | 0.597 | 50.3% | 23% | 0.317 | 27.3% |
| **API 嵌入 Qwen3-4B + 混合 + 指令，无 LLM** | 65-66% | 0.801 | 72.5-73.0% | 40% | 0.577 | 48.8% |
| **API 嵌入 + 混合 + LLM 全开（完全体）** | **94%** | **0.947** | **94.3%** | **82%** | **0.864** | **84.2%** |
| （参照）ACE Finnian 云服务 | 69% | 0.816 | 75.3% | 64% | 0.707 | 67.4% |

增益分解（完全体 vs 静态混合基线）：
- 嵌入升级（potion-multilingual-128M 256 维 → Qwen3-Embedding-4B 1024 维 API）：flask +20.2 / cc +30.2 分
- LLM 层（rerank 为主，消融实测 rerank 单开即拿走几乎全部增益；rewrite 对跨语言查询 +0.15 nDCG；intent 单开为负收益）：flask +49.6 / cc +62.0 分
- 消融细节：rerank 是唯一大杠杆；rewrite 价值在把相关文件拉进窗口（召回）；intent 分类错会带偏策略权重

- BM25 混合检索（CJK 2-gram 词法兜底）是静态嵌入路线下的最大单项增益：flask +12.5 分，Top-1 +13
- `potion-code-16M-v2` 英文字段名区分度最高但中文弱，整体不敌多语言模型——保持多语言默认
- 三处 LLM 调用点（rerank/rewrite/intent）的模型回落链：显式设置 > LLM_MODEL > 内置默认，换模型不会局部失效
- 召回预算（借鉴 semble_rs 实测校准）：default_top_k 50→120、per_query_top_k 20→60，
  完全体 cc-switch +9.6 分（变体每路浅池曾是瓶颈），flask 持平（LLM 噪声带内），200 饱和
- 已实测否定的召回思路：查询侧 camelCase 拆词变体（三种语义均 ±噪声）、
  按意图自适应 text_boost（flask -1.4~-4.3）——启发式注入 RRF 是噪声不是信号
- **Qwen3-8B（Nebius API，4096 原生维）**：flask 141.89 (70.9%) /
  cc-switch 104.87 (52.4%) —— flask 低于 4B 约 4-5 分、**cc 反超 +5.2**；
  1024/4096 维质量持平（MRL 截断无感）；8B token 单价 ~2×、速度 -20%
  （Nebius vs SiliconFlow）。结论：与 4B 总分打平，分布互补——
  跨语言重仓用 8B、英文代码仓用 4B
- **voyage-4-large（API，OpenAI 兼容端点已适配）**：flask 142.06 (71.0%) /
  cc-switch 102.08 (51.0%) —— flask 低于 Qwen3-4B 约 4.5 分，**cc 反超 +2.4**
  （cross_language 10/10 满分）。要点：output_dimension 缺省返回 1024（MRL 档
  [256,512,1024,2048]，原生 2048 需显式传）；input_type=query/document 已按
  官方推荐注入； Voyage 契约差异（encoding_format 仅 base64、无 dimensions 字段）
  已在嵌入器内按模型名自动适配
- **自训静态模型 ft-sprint（80M，中英）**：flask 128.41 (64.2%) / cc-switch
  75.59 (37.8%) —— **双仓反超 potion-multilingual-128M**（121.51/71.70），
  +6.9/+3.9 分；速度同级（flask 全量 3s / cc 35s，静态查表）
- potion-code-16M-v2 当前代码重测：flask 108.41 (54.2%) / cc-switch 55.09
  (27.5%) —— 代码专项蒸馏反而双仓垫底（比 multilingual 低 13/17 分），
  16M 参数量在双语词汇覆盖上先天不足
- **llama.cpp 路线**（EMBED_LOCAL_BACKEND=llama，GGUF + Metal 全算子）：
  Qwen3-Embedding-0.6B Q8_0 137.53 (68.8%) / f16 137.38 (68.7%) —— 两者同分，
  -5 分是运行时差异（llama.cpp tokenizer/pooling vs HF）而非量化；速度
  单条 0.15s（candle 3.5s 的 **23×**）、全量 173s（2.4×）。两路线并存：
  EMBED_LOCAL_BACKEND=candle（质量优先）| llama（速度优先）
- **batch 批量化结论**：candle 批量前向（b>1）有上游 bug——batch=8 panic、
  batch=50 静默错值（141→15 分）；已实现"批量 vs 逐条等价性自检"（含
  catch_unwind 拦 panic），batch>1 触发即拒绝启动并提示回退。
  EMBED_LOCAL_BATCH_SIZE 默认 1；本地提速路线=ort/ONNX
- 权重精度扫描（Qwen3-0.6B + 代码指令，flask nollm）：f32 142.64 (2273MB) /
  f16 142.68 (1136MB) / bf16 142.97 (1136MB) / int8 141.63 (568MB) / int4 87.00
  (284MB)。**f16 半内存零损失，int8 -1 分省 75%，int4 崩盘**；各档计算均 F32
  （candle CPU 无 int 内核），速度无差异——换速度需 ort/GGUF
- 本地神经嵌入（EMBED_PROVIDER=local，candle 跑 Qwen3 骨干，新增能力）：
  **EOS 池化位修复后**（漏加 EOS 时 0.6B 只有 106.69——lasttoken 模型的
  EOS 追加是生死攸关的实现细节）：
  - Qwen3-Embedding-0.6B + **代码专用指令** = **142.68 (71.3%)**（官方通用指令
    137.32，+5.4）—— 距 API Qwen3-4B 仅 3.4 分，完全离线免费；全量 7 分钟
  - F2LLM-v2-80M + 官方 QA 指令 = 138.72 (69.4%)（代码指令反而 -2.4，
    指令效果是模型相关的）；全量 82 秒
  - 两者均反超静态 potion (121.51) 约 17 分。代码指令覆盖：
    EMBED_LOCAL_QUERY_PROMPT env（完整 "Instruct: ...\nQuery: " 前缀）
  基础设施：CPU+Accelerate、官方 query prompt 自动读取、MRL 截断、
  vendored qwen3 修复 KV cache 复位
- 静态嵌入模型横评（flask nollm，同代码同参）：potion-multilingual-128M 121.51 (60.8%)
  卫冕；用户推荐候选全败——alikia2x/jina-v3-m2v-256 115.78、hs-hf/jina-v3-distilled
  112.61、giangndm/F2LLM-v2-330M 107.82、jeadie/octen-8b 86.64、jina-code-static 78.78；
  Qwen3-8B-M2V-Entropy-RAG 无法加载（model2vec-rs 0.2.1 不支持 I8 权重 dtype）
- 查询侧词法概念投影表（lexical.rs，借鉴 semble_rs）：意图门控（仅 Feature/Overview/
  Compound）后 nollm flask +1.3 / cc +2.0；无门控全量投影 -10.7（PATH 查询被灌噪声）
- 改写∥原查询召回 tokio 并行化：无回归，完全体省 ~1-2s/条
- Qwen3-Embedding 官方 query instruction（指令感知训练）：查询侧加
  "Given a code search query..." 实测 nollm 双仓 +1.1/+2.4 分；模型条件默认
  （Qwen3 系列自动启用，其他模型保持空，EMBED_QUERY_INSTRUCTION 显式覆盖）。
  修复：指令此前在凭据回落路径被硬编码 "" 丢弃
- 图扩散（TriviumDB SA-PPR expand_depth + 同文件边）：flask -12 分，已否决（默认 0）
- HyDE（RETRIEVAL_HYDE_ENABLED，默认关）：cc +0.84 分但延迟 5 倍，留给离线场景

### 嵌入模型选型探针（margin = 相关块余弦 − 无关块余弦）

| 模型 | 中文业务查询 | 字段名查询 | 下载/内存 | 结论 |
|---|---|---|---|---|
| potion-base-8M | **-0.10**（反相关） | +0.31 | ~35MB | 中文场景不可用（默认已弃） |
| **potion-multilingual-128M（默认）** | **+0.23** | +0.34 | ~500MB | 中英均衡，推荐 |
| potion-code-16M-v2 | -0.02 | **+0.38** | ~64MB | 仅英文字段名场景 |

吞吐（potion-base-8M，dim=256）：**4057 texts/s**（零网络，确定性可复现）。

QuIVer ANN 实测结论（`oce-bench quiver`，Apple Silicon，dim=256）：

| 节点数 | exact (BruteForce) | QuIVer ANN | recall@10 |
|---|---|---|---|
| 30 000 | 14.8 ms | 21.6 ms（另构建 6.4s） | 1.000 |
| 200 000 | 89.3 ms | 129.3 ms（另构建 67s） | 1.000 |

本机内存带宽下 BruteForce 在个人库全规模占优且 recall 相同——**默认关闭 ANN**
（`TRIVIUM_AUTO_BUILD_QUIVER=false`）；百万级 + mmap 冷向量场景再显式开启。

与 Python 版的对比说明：

- **查询速度**：个人模式 Rust 版在进程内完成向量检索（TriviumDB），消除了
  Milvus Lite 的子进程 gRPC 往返；SQL 精确召回同 schema 同索引。
- **索引速度**：切块从 Python 递归切分器（pyo3 往返 + 单线程）变为原生
  tree-sitter + 零拷贝行切片；写入路径合并为批量 WAL-Off 导入。
- embedding/rerank/LLM 均为外部网络调用，与实现语言无关，两端耗时由 API 服务主导。
- Python 侧对照基准：`.venv` 在本机不可用（pydantic_core 缺失）未能运行；
  `oce-bench` 的阶段口径可直接对齐 Python 版同阶段埋点补测。

## 从 Python 版迁移（个人模式）

1. **.env 直接复用**：环境变量完全兼容。`oce serve --data-dir <Python 数据目录>` 会
   读取其中的 `.env`；`EMBED_API_KEY` 等原样生效。
2. **SQLite 文件直接复用**：schema 与 alembic head 一致（blobs/chunks/blob_chunks/
   symbol_occurrences/chains/model_credentials/监控表），既有 `oce.db` 可直接挂给
   Rust 版，`oce doctor` 会验证 schema 可打开与凭据可解析。
3. **向量数据需重传**：Milvus Lite 的 `.db` 文件无独立读取工具，向量部分由客户端
   重新 batch-upload（内容寻址 sha256(path+content) 幂等，已存在的 blob 只补嵌入）。
   配合 `EMBED_PROVIDER=static` 重嵌零 API 成本。
4. **自检**：迁移前后各跑一次 `oce doctor`，任何一项 [fail] 都会给出具体原因。

## 测试

```bash
cargo test --workspace        # 52 个测试：单元 + TriviumDB 集成 + E2E + API 契约
```

- `oce-core`：spans/切块器/分类器/规划器/选择器/formatter/symbol/chain/checkpoint 令牌
- `oce-infra`：TriviumDB 往返（upsert 幂等/blob 过滤/持久化/删除）、path 索引
- `oce-app`：batch_upload → checkpoint → retrieve 全链路（假嵌入器）
- `oce-server`：路由契约（鉴权/错误形状/凭据脱敏/409/400 语义/admin CRUD）

## 服务模式

服务模式面向多用户/多机共享索引：PostgreSQL 元数据 + Redis 任务队列 + 后台 worker。
向量后端二选一：

| 后端 | 配置 | 适用 |
|---|---|---|
| TriviumDB（默认） | `VECTOR_BACKEND=trivium` + `TRIVIUM_PATH` | 单进程部署；零额外容器 |
| pgvector | `VECTOR_BACKEND=pgvector` | PG 一体化：向量与元数据同库同事务，pg_dump 单备份 |

```bash
# 编排启动（pgvector 镜像 + redis；无 etcd/minio/milvus）
docker compose -f docker-compose.service.yml up -d

# 或本机运行：依赖 docker-compose.dev.yml 的 postgres(25432)/redis(26379)
./target/release/oce init --service --data-dir ~/.oce/data   # 生成服务模式 .env
# 编辑 .env：DB_URL / REDIS_URL / EMBED_API_KEY / TRIVIUM_PATH
./target/release/oce serve --data-dir ~/.oce/data
```

关键语义（与 Python 版对齐）：

- **元数据 schema 逐列一致**：Rust 版幂等建表（`CREATE TABLE IF NOT EXISTS`），
  Python alembic head 建的库可直接挂载，反之亦然
- **队列**：Redis Lua 脚本原子去重（SADD pending → 新加才 LPUSH），防客户端
  重复上传造成的幽灵消息；BLMOVE 处理中队列，崩溃残留启动时恢复
- **worker**：`WORKER_ENABLED=true` 时容器装配即启动；消费 blob → 嵌入 → ack，
  失败 retry_count++ 超限 mark_error
- **模型指纹 fail-closed**：换嵌入模型（或维度）拒绝启动，提示重建索引——
  trivium 走 `.tdb.model` sidecar，pgvector 走 `vector_index_meta` 表
- **pgvector 已知限制**：无 BM25 词法混合（词法信号由 symbol_occurrences
  exact 路承担）、HNSW 删除不收缩（膨胀靠 VACUUM 缓解）

集成测试（需真实实例，`#[ignore]` 门控）：

```bash
OCE_PG_URL="postgres://oce:oce@localhost:25432/oce"   cargo test -p oce-infra --test pg_integration -- --ignored
OCE_REDIS_URL="redis://:pass@localhost:26379/0"   cargo test -p oce-infra --test redis_queue_integration -- --ignored
OCE_PG_URL="postgres://oce:oce@localhost:25432/oce"   cargo test -p oce-infra --test pgvector_integration -- --ignored
```

## 阶段路线

1. ✅ 领域核心 + 切块器（tree-sitter 23 语言，其余 RecursiveChunker 兜底）
2. ✅ TriviumDB 存储 + SQLite 元数据 + 凭据运行时
3. ✅ 检索编排 + 应用服务 + ACE API + CLI
4. ✅ 集成测试 + 基准
5. ✅ 服务模式：PostgreSQL 元数据（sqlx）+ Redis 队列 + 向量后端
   （trivium 默认 / pgvector 可选；端口抽象 Phase 0 完成）
6. ✅ pgvector vs TriviumDB 检索质量 A/B（nollm 档，Qwen3-8B API 嵌入）：
   双仓合计 154.3 vs 156.6（差 2.3 分，噪声带边缘）——质量持平，pgvector 达到
   服务模式可用标准；默认仍 trivium（零依赖），pgvector 为 PG 一体化一等选项。
   详见 oce-benchmark/results/ab-pgvector-vs-trivium-nollm.md
   ⬜ QuIVer 大库基准（≥1 万节点）
7. ⬜ 数据迁移工具：Python 版 .env / db 文件兼容说明
