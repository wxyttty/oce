//! OpenContextEngine 领域核心。
//!
//! 与 Python 版 `src/oce/domain` + `shared/errors` 语义对齐：
//! - [`chunk`]: 切块值对象与切块器协议/实现
//! - [`search`]: SearchHit 与存储/检索协议
//! - [`retrieval`]: 检索编排管道
//! - [`classifier`]/[`planner`]/[`strategy`]/[`selector`]: 查询理解与结果选择
//!
//! 本 crate 只依赖纯逻辑；I/O（存储、HTTP、LLM）全部经 trait 注入。

pub mod blob;
pub mod broad;
pub mod chain;
pub mod chunk;
pub mod classifier;
pub mod cooldown;
pub mod error;
pub mod file_desc;
pub mod formatter;
pub mod indexing;
pub mod lexical;
pub mod metrics;
pub mod path_doc;
pub mod planner;
pub mod priority;
pub mod related;
pub mod retrieval;
pub mod retrieval_settings;
pub mod search;
pub mod selector;
pub mod source_filter;
pub mod span_merge;
pub mod strategy;
pub mod symbol;

pub use error::{OceError, OceResult};
