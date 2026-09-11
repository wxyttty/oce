# Agent Note: 吸收上游 /admin/reports/* 报表套件与监控前置缺口

Status: implemented

## Problem

上游 Python 版合入 `92fe1b8 feat(reports)`：六套 `/admin/reports/*` 聚合报表（api-calls /
retrieval 含 slow/empty-queries 明细 / tokens / index-inventory / resources / storage）。
Rust 版对齐评估发现：报表本身可以移植，但其**三个数据前置在 Rust 版是断的**——
直接移植只会得到永远为空的报表：

1. `api_call_metrics` 无写入方（Python 有 HTTP 中间件；Rust 版 `record_api_call`
   存在但零调用，且同步单条写）
2. `resource_samples` 无写入方（Python 有 psutil 采样器；Rust 版建了表、清理任务
   会删它，但没人写）
3. 监控清理任务（MonitoringCleaner）未装配——监控行只增不减
4. 顺带发现：`MONITORING_STORE_QUERY_TEXT` / `retrieval_audit_enabled` 配置项
   解析了但从未接线（service.rs 硬编码不落 query 原文，audit 无开关判断）

## Decision

前置补齐 + 六套报表全量移植，只做个人模式（SQLite）：

- **HTTP 监控中间件**（`routes.rs`）：axum `middleware::from_fn_with_state`，
  记录 endpoint/method/status/latency 到缓冲 sink；`/health` 豁免；monitoring
  关闭（sink None）时直接放行。endpoint 取请求路径而非路由模板——axum 0.8
  无 route 模板暴露，个人模式端点无动态段，路径即模板。
- **资源采样器**（`resource_sampler.rs` 新增，psutil → sysinfo + libc statvfs）：
  后台周期采集磁盘/内存/CPU；采集在 `spawn_blocking`（目录递归可能慢）；
  drop 时随 JoinHandle 终止。CPU% 依赖采样间隔（sysinfo 语义），报表只看趋势可接受。
- **清理任务**：`SqlMetricsSink::spawn_cleanup_task` 按 retention_days 周期清四张监控表。
- **`record_api_call`/`record_resource_sample` 改走缓冲**：与 token/retrieval 同一
  `SinkBuffer` 批量 flush（原来 api_call 是同步单条 INSERT，且 `MetricsSink` trait
  没这两个方法）。
- **配置接线**：`RetrievalApplication` 增加 `retrieval_audit_enabled` +
  `store_query_text` 字段（容器装配时从 monitoring 设置解析），audit 关闭时零开销跳过。
- **报表 reader**（`sqlite/reports.rs` 新增 ~1100 行）：沿用 Python 版设计——
  SQL 只做窗口过滤与基础聚合，分桶（hour/day 截断）与分位数在应用层算（Python 为
  跨 SQLite/PG 可移植性如此设计，Rust 版只有 SQLite 但沿用，避免 SQL 方言分位函数）；
  旁路只读，`read()` 封装任何查询失败降级为默认值（报表端点不 5xx）。
- **storage 报表的向量库统计**：Milvus 双 collection → TriviumDB TQL
  `FIND {kind: "chunk"} RETURN count(*) AS rows`（kind 属性索引路径，已写探针验证
  返回正确行数）+ `.tdb` 文件体积 stat。est_bytes = rows × dim × 4（与 Python 一致）。
- **`/admin/stats` 补 resource 快照**：`stats()` 查询最新 `resource_samples` 行，
  routes 层映射到已有的 `ResourceSnapshotResponse`（原来恒 None）。

## 与 Python 版的差异（有意为之）

- endpoint 无路由模板（axum 限制，见上）
- `dbstat` 表空间：bundled SQLite 未编译 dbstat 虚表时逐表降级 `approximate=true`
  （Python 同样兜底）
- 报表 DTO 直接 `Serialize`（Rust 无 pydantic 双层映射，读模型即响应模型）
- CQRS query handler 层省略：Python 的 Query/Handler/Bus 在 Rust 版由 routes
  直接调 `ReportsReader`（个人模式无跨方言需求，多一层只增加间接）

## Alternatives considered

- **只移植报表不管前置**：得到六套永远为空的端点，纯负资产。
- **等前置自然补齐再移植**：前置没有其他消费方驱动，不会"自然"补。
- **报表在 SQL 里算分位数**（SQLite 无原生 percentile，需自拼窗口函数）：
  Python 版已验证"应用层算"在窗口内行数有限时代价可接受，保持两侧算法一致
  更重要（同一份报表数字可对照）。
- **TriviumDB 行数用 `node_count()` 全库计数**：不分 kind，无法对应 Milvus
  chunk/path 双 collection 语义；TQL FIND + COUNT 走属性索引，代价相同。
- **sysinfo 的 disk_usage**：sysinfo 0.33 的 `Disk` API 需枚举全部挂载点再匹配，
  statvfs 直接探测目标路径所在卷更直接；非 Unix 平台降级为 0（个人模式目标平台）。

## Consequences

- 收益：Rust 版监控面与 Python 版功能对齐（六报表 + stats resource 快照 +
  监控数据闭环）；`MONITORING_STORE_QUERY_TEXT` 开启后慢查询/空回报表可展示原文。
- 代价：`oce-infra` 新增 `libc` 依赖（statvfs）；每请求多一次内存 push（缓冲，
  批量落库，微秒级）。
- 验证：`cargo test --workspace` 66 通过（api_contract 新增报表端点 200 形状 /
  bucket 422 / 错误 key 401 断言）；`kind_stats` TQL 探针实测
  `[("chunk", 2), ("path", 0)]`；clippy 三 crate 无告警。
- 未验证：资源采样器的真实数值合理性（需要长时间运行才有意义；CPU% 首个
  采样周期恒 0 是 sysinfo 已知语义，报表只看趋势）。
