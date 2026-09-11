//! 切块值对象：与 Python `domain/chunk/types.py` 对齐。

use sha2::{Digest, Sha256};

/// 1-based 闭区间的源码行区间引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    pub content_hash: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl ChunkRef {
    pub fn new(content_hash: String, start_line: u32, end_line: u32) -> Result<Self, String> {
        if !is_sha256(&content_hash) {
            return Err(format!("Invalid content_hash: {content_hash}"));
        }
        if start_line < 1 {
            return Err(format!("Invalid start_line: {start_line}"));
        }
        if end_line < start_line {
            return Err(format!("Invalid line range: {start_line} - {end_line}"));
        }
        Ok(Self {
            content_hash,
            start_line,
            end_line,
        })
    }
}

/// 内容寻址的代码 chunk：1-based 闭区间行跨度。
///
/// `content` 是所报区间的逐字源文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub content_hash: String,
    pub path: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub chunk_type: Option<String>,
}

impl Chunk {
    pub fn new(
        content_hash: String,
        path: impl Into<String>,
        content: impl Into<String>,
        start_line: u32,
        end_line: u32,
        chunk_type: Option<String>,
    ) -> Result<Self, String> {
        if !is_sha256(&content_hash) {
            return Err(format!("Invalid content_hash: {content_hash}"));
        }
        if start_line < 1 || end_line < start_line {
            return Err(format!("Invalid line range: {start_line} - {end_line}"));
        }
        Ok(Self {
            content_hash,
            path: path.into(),
            content: content.into(),
            start_line,
            end_line,
            chunk_type,
        })
    }

    pub fn compute_hash(content: &str) -> String {
        let digest = Sha256::digest(content.as_bytes());
        hex::encode(digest)
    }

    pub fn to_ref(&self) -> ChunkRef {
        ChunkRef {
            content_hash: self.content_hash.clone(),
            start_line: self.start_line,
            end_line: self.end_line,
        }
    }

    pub fn line_count(&self) -> u32 {
        self.end_line - self.start_line + 1
    }

    /// 嵌入输入文本（与 Python `LocatedChunk.embedding_text` 一致）。
    pub fn embedding_text(&self) -> String {
        format!("File: {}\n\n{}", self.path, self.content)
    }
}

/// 持久化 chunk 出现位置：写入向量索引前的完整定位（与 Python `LocatedChunk` 对齐）。
#[derive(Debug, Clone)]
pub struct LocatedChunk {
    pub blob_name: String,
    pub content_hash: String,
    pub path: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl LocatedChunk {
    /// chunk_id：出现位置级别的内容寻址 ID（sha256）。
    pub fn chunk_id(&self) -> String {
        let raw = format!(
            "{}\n{}\n{}:{}",
            self.blob_name, self.content_hash, self.start_line, self.end_line
        );
        let digest = Sha256::digest(raw.as_bytes());
        hex::encode(digest)
    }
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
