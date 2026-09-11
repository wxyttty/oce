//! 切块领域：值对象、协议与实现。
//!
//! 与 Python 版 `domain/chunk` + `infrastructure/astchunk|chunkers` 对齐。
//! 核心不变量：chunk 的 `content` 必须逐字等于其声明的行区间源文本，
//! 因为 formatter 按 `start_line + offset` 逐行打印。

pub mod cast;
pub mod jsp;
pub mod lang;
pub mod markdown;
pub mod recursive;
pub mod router;
pub mod spans;
pub mod types;
pub mod vue;

pub use cast::CastChunker;
pub use jsp::JspChunker;
pub use lang::{detect_language, supported_languages};
pub use markdown::MarkdownChunker;
pub use recursive::{is_meaningful, RecursiveChunker};
pub use router::{build_chunker, LanguageChunkerRouter};
pub use spans::char_len;
pub use types::{Chunk, ChunkRef, LocatedChunk};
pub use vue::VueChunker;

use async_trait::async_trait;

/// 切块器最小接口（对应 Python `Chunker` Protocol）。
#[async_trait]
pub trait Chunker: Send + Sync {
    /// 将源内容切分为行对齐的 chunk。
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk>;
}

/// 共享切块器实例（router 把 CastChunker 注册到多个语言）。
#[async_trait]
impl<T: Chunker + ?Sized> Chunker for std::sync::Arc<T> {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        (**self).chunk(content, path)
    }
}
