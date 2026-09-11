//! OpenContextEngine 应用层：用例编排（容器 + 命令/查询处理器 + worker）。
//!
//! 传输层（axum router）只做 DTO 映射与鉴权；业务流程全部在本层。
//! 与 Python `application/` 层对齐：service.py（RetrievalApplication）、
//! checkpoint/gc/ingest/status/credentials 处理器、worker。

pub mod container;
pub mod service;
pub mod worker;
pub mod workspace;

pub use container::Container;
pub use service::RetrievalApplication;
