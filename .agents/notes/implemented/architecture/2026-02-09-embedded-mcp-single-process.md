# Agent Note: 嵌入式 MCP 单进程模式（对齐 semble_rs 使用形态）

Status: implemented

## Problem

原形态需要「`oce serve` 后台 HTTP 服务 + `uv tool install` 的 oce-client-mcp 客户端 + mcp.json 配 URL/KEY」三件套；客户端还自带一份 state.sqlite3 清单。真实测试暴露：跨实例切换时客户端本地清单与服务端状态漂移（链重建不上传内容 → 检索恒返回单文件）、413、锁误报等一串运维摩擦。用户要求像 semble_rs 一样：mcp.json 一行命令直接起，引擎就在 MCP 进程里。

## Decision

`oce mcp --workspace <dir>`（`crates/oce-server/src/mcp.rs` + `crates/oce-app/src/workspace.rs`）：

- 单进程内嵌 Container（SQLite + TriviumDB + 静态嵌入器）；MCP stdio 协议（initialize/tools/list/tools/call/ping）手写 JSON-RPC——协议面小，避免 rmcp 3.x 的 API 演进风险
- 工作区扫描用 `ignore` crate（ripgrep 同源）：.gitignore + `.oceignore` + 默认忽略集，`require_git(false)` 让非 git 仓库也生效；`.gitignore/.oceignore` 自身与 `.oce` 数据目录不进索引
- 同步语义：惰性增量——search/status 前做轻量 walk（mtime/size 比对，干净时毫秒级），变化文件才读/哈希/入库，消失路径连向量一并删除；全量重建走 `oce_reindex`
- 数据落 `<workspace>/.oce/`（oce.db + oce.tdb + .model 指纹 sidecar），删除即重置
- 无 checkpoint 机制（索引即工作区），检索 scope = 全部已索引 blob
- 无 LLM key/凭据时 LLM 组件整体不装配（原来每次检索白试一次失败调用）

HTTP `serve` 模式保留（ACE 兼容、评测与远端场景），两模式共享全部引擎代码。

## Alternatives considered

- **rmcp 3.2 官方 SDK**：能力全但依赖重、API 演进快；本服务只需 4 个方法，手写约 200 行且可全量测试
- **守护进程 + 本地 socket**：仍是两进程，未解决用户痛点
- **复用 oce-client-mcp 但省掉 serve**：客户端是 Python 包需单独安装，且其状态文件正是漂移问题根源
- **watcher 常驻监听文件变化**：MCP 进程生命周期由宿主管理，常驻线程收益低；惰性同步已满足新鲜度

## Consequences

- 收益：mcp.json 一行接入；无端口/无 key/无客户端安装；数据随工作区走（.oce/ 可整体删除重置）；首查全量索引、后续增量毫秒级
- 代价：MCP 进程退出即引擎退出（无跨会话缓存之外的状态）；多会话同时打开同一工作区会竞争 .tdb 写锁（TriviumDB 单写者）——同一时刻应只有一个 agent 会话持锁；HTTP 模式与 MCP 模式使用不同数据目录，索引不共享
- 客户端 oce-client-mcp 保留可用（连 serve），但推荐路径不再是它
