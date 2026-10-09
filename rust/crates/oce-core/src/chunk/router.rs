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
/// cAST 分窗的非空白字符预算（对方引擎 unit ≤65 行的近似）。
/// v2 时代是 1500：窗口合并后 chunk 平均 45 行、最大 232 行，答案埋在
/// chunk 中部时 reranker 打分被稀释、选择器成本归一被体量惩罚（行级
/// 覆盖实测：36 个缺失 unit 中 21 个答案位于 chunk 1600 字符之后）。
/// v3 收紧到 650（≈45 行），intact 规则仍允许适度超限的完整声明整块。
pub const CAST_MAX_CHUNK_SIZE: usize = 650;

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

/// 切块器版本指纹。
///
/// 索引按 chunk 内容寻址，行号与 chunk 边界由切块器决定：只要**边界语义**变了
/// （分块规则、合并规则、字符预算、跨度修正），旧索引的行号就不再与 chunk 对应，
/// 必须 fail-closed 重建，而不是新旧混用。所以任何切块器改动都要递增这个版本号。
///
/// - v1：初版（cAST + 递归 + markdown/vue/jsp 专用切块器）。
/// - v2：修两处实测缺陷——cAST 相邻块共享边界行（重叠 1 行）、递归切块器丢弃
///   纯标点尾部片段导致漏行（真语料实测：flask 6 处重叠、cc-switch 122 处重叠 +
///   1 文件漏 3 行）。边界语义变了，旧索引必须重建。
pub const CHUNKER_VERSION: &str = "chunk=v3";

/// 供模型指纹拼装使用的切块器指纹（与 `CHUNKER_VERSION` 同源，避免两处漂移）。
pub fn chunker_fingerprint() -> &'static str {
    CHUNKER_VERSION
}
