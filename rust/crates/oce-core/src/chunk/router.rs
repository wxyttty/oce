//! 语言感知切块分发。与 Python `domain/chunk/router.py` 对齐：
//! 按检测语言分发到自声明语言的切块器；未命中走 RecursiveChunker 兜底。

use super::cast::CastChunker;
use super::jsp::JspChunker;
use super::lang::detect_language;
use super::markdown::MarkdownChunker;
use super::recursive::RecursiveChunker;
use super::vue::VueChunker;
use super::{Chunk, Chunker};
use async_trait::async_trait;
use std::collections::HashMap;

/// 默认切块参数（与 Python `factories/chunker.py` 一致）。
pub const CHUNK_SIZE: usize = 6_000;
pub const CHUNK_OVERLAP: usize = 200;
pub const CAST_MAX_CHUNK_SIZE: usize = 1_500;

/// 按语言分发的切块器集合（对应 `build_chunker()` 装配）。
pub struct LanguageChunkerRouter {
    by_language: HashMap<&'static str, Box<dyn Chunker>>,
    fallback: RecursiveChunker,
}

impl LanguageChunkerRouter {
    pub fn build_default() -> Result<Self, String> {
        let recursive = RecursiveChunker::new(CHUNK_SIZE, CHUNK_OVERLAP)?;
        let shared = || RecursiveChunker::new(CHUNK_SIZE, CHUNK_OVERLAP);
        let cast = CastChunker::new(
            std::sync::Arc::new(shared()?),
            CAST_MAX_CHUNK_SIZE,
            super::cast::DEFAULT_MAX_CHUNK_CHARS,
            super::cast::DEFAULT_MIN_CHUNK_CHARS,
        )?;
        let markdown = MarkdownChunker::new(
            std::sync::Arc::new(shared()?),
            super::markdown::DEFAULT_MAX_CHUNK_CHARS,
            super::markdown::DEFAULT_MIN_CHUNK_CHARS,
        )?;
        let vue = VueChunker::new(
            std::sync::Arc::new(shared()?),
            super::vue::DEFAULT_MAX_CHUNK_CHARS,
        )?;
        let jsp = JspChunker::new(
            std::sync::Arc::new(shared()?),
            super::jsp::DEFAULT_MAX_CHUNK_CHARS,
        )?;

        let mut by_language: HashMap<&'static str, Box<dyn Chunker>> = HashMap::new();
        for lang in super::cast::cast_languages() {
            by_language.insert(lang, Box::new(cast.clone()));
        }
        for lang in ["markdown"] {
            by_language.insert(lang, Box::new(markdown.clone()));
        }
        for lang in ["vue", "svelte"] {
            by_language.insert(lang, Box::new(vue.clone()));
        }
        for lang in ["jsp"] {
            by_language.insert(lang, Box::new(jsp.clone()));
        }
        Ok(Self {
            by_language,
            fallback: recursive,
        })
    }
}

#[async_trait]
impl Chunker for LanguageChunkerRouter {
    fn chunk(&self, content: &str, path: &str) -> Vec<Chunk> {
        match detect_language(path).and_then(|l| self.by_language.get(l)) {
            Some(chunker) => chunker.chunk(content, path),
            None => self.fallback.chunk(content, path),
        }
    }
}

/// 便捷工厂：默认装配。
pub fn build_chunker() -> Result<LanguageChunkerRouter, String> {
    LanguageChunkerRouter::build_default()
}
