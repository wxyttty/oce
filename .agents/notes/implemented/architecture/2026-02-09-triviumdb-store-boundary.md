# Agent Note: Rust 版个人模式用 TriviumDB 承接向量+路径索引，关系元数据保留 SQLite

Status: implemented

## Problem

Python 版个人模式的检索存储是「SQLite 元数据 + Milvus Lite 向量」两套引擎：向量检索要走
Milvus Lite 子进程 gRPC 往返，路径索引是独立 collection；同时 SQLite 里还有 checkpoint 链、
凭据、审计等纯关系数据。Rust 重写需要决定新存储引擎的边界——全部迁到 TriviumDB，还是只迁
检索路径。

## Decision

个人模式 Rust 版（`rust/crates/oce-infra/src/trivium.rs`）用 **TriviumDB 单文件 `oce.tdb`**
承接全部检索路径存储：

- `kind="chunk"` 节点：vector = embedding，payload 携带
  `chunk_id/content_hash/blob_name/path/start_line/end_line/content`。
  节点 ID = sha256(chunk_id) 前 8 字节，碰撞线性探测、读回校验 payload——upsert
  天然幂等，对齐 Python Milvus collection 的 chunk_id 主键语义。
- `kind="path"` 节点：vector = 路径文档 embedding，payload `{blob_name, path, path_id}`。
- `create_index("kind"/"blob_name"/"identifier")` 做 Hash 属性索引，检索用
  `SearchConfig.payload_filter` 前置过滤；引擎侧不过滤相似度（对齐 Milvus 语义），
  `vector_threshold` 后过滤在应用层。
- 批量导入 `SyncMode::Off`，批次结束恢复原同步级别并 flush。
- **QuIVer ANN 默认关闭**（`TRIVIUM_AUTO_BUILD_QUIVER=false`）：实测（Apple Silicon，
  dim=256）3 万节点 exact 14.8ms vs ANN 21.6ms，20 万节点 exact 89ms vs ANN 129ms，
  recall@10 均为 1.000——本机内存带宽下 BruteForce 在个人库全规模占优；QuIVer 的
  公布优势在百万级 + mmap 冷向量场景，需要时显式开启。

**关系元数据保留 SQLite（同 Python schema）**：blob 注册表、blob_staging、chunks、
blob_chunks、chains/chain_members、symbol_occurrences、model_credentials、监控四表。
`VectorStore` trait 留出服务模式 Milvus gRPC 后端的扩展点。

## Alternatives considered

- **全部数据进 TriviumDB**（用户原始提议「SQLite+Milvus Lite 都换成 TriviumDB」）：
  checkpoint 链成员增删、凭据 (kind, model, api_key_hash) 唯一约束、监控 TTL 清理都是
  关系操作；TriviumDB 的文档/图模型表达这些需要应用层维护索引和事务语义，收益为负。
  实测后把边界定在「检索路径全部迁、关系数据保留」。
- **符号 occurrences 也迁 TriviumDB**：symbol 节点没有真实向量，塞零向量会在余弦
  分数里产生 NaN 风险；SQLite 的 identifier 倒排 + kind 打分逐字保留即可，精确召回
  不是性能瓶颈（scope ≤ 2000 blob、2s 超时上限）。
- **rusqlite 换 sqlx**：个人模式单连接 StaticPool 语义，rusqlite + spawn_blocking
  更轻；服务模式迁 PostgreSQL 时再引入 sqlx。

## Consequences

- 收益：向量检索进程内完成（消除子进程 gRPC 往返）；单文件 `oce.tdb` 可整体拷贝；
  QuIVer ANN 在 ≥1 万节点自动激活，无手工配置。
- 代价：TriviumDB 写锁是进程级排他，多进程同时写同一 `.tdb` 不可行（个人模式单进程
  写入，可接受）；`.tdb` 维度创建后不可变，换 embedding 模型需要重建或 migrate。
- 保留 SQLite 意味着两个存储文件（`oce.db` + `oce.tdb`），删除 blob 时两侧都要清理
  （`RetrievalApplication::delete_blob` 已处理）。
