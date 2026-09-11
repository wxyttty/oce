//! OpenContextEngine 领域核心。
//!
//! 与 Python 版 `src/oce/domain` + `shared/errors` 语义对齐：
//! - [`chunk`]: 切块值对象与切块器协议/实现
//! - [`search`]: SearchHit 与存储/检索协议
//! - [`retrieval`]: 检索编排管道
//! - [`classifier`]/[`planner`]/[`strategy`]/[`selector`]: 查询理解与结果选择
//!
//! 本 crate 只依赖纯逻辑；I/O（存储、HTTP、LLM）全部经 trait 注入。

pub mod chunk;
pub mod classifier;
pub mod error;
pub mod formatter;
pub mod lexical;
pub mod path_doc;
pub mod planner;
pub mod priority;
pub mod retrieval;
pub mod retrieval_settings;
pub mod search;
pub mod selector;
pub mod blob;
pub mod chain;
pub mod indexing;
pub mod metrics;
pub mod source_filter;
pub mod strategy;
pub mod symbol;

pub use error::{OceError, OceResult};
