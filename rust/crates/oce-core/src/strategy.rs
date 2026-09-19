//! 意图驱动的检索策略决策表。与 Python `domain/services/retrieval_strategy.py` 对齐。

use serde::{Deserialize, Serialize};

/// LLM 意图分类的 7 类标签（对应 Python `llm.intent.QueryIntent`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LlmIntent {
    /// S：符号定义
    Symbol,
    /// C：调用链
    CallChain,
    /// R：引用位置
    Reference,
    /// P：文件路径
    Path,
    /// F：功能实现
    Feature,
    /// O：架构概览
    Overview,
    /// M：复合查询
    Compound,
}

impl LlmIntent {
    pub fn label(&self) -> &'static str {
        match self {
            LlmIntent::Symbol => "S",
            LlmIntent::CallChain => "C",
            LlmIntent::Reference => "R",
            LlmIntent::Path => "P",
            LlmIntent::Feature => "F",
            LlmIntent::Overview => "O",
            LlmIntent::Compound => "M",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label.trim().to_uppercase().as_str() {
            "S" => LlmIntent::Symbol,
            "C" => LlmIntent::CallChain,
            "R" => LlmIntent::Reference,
            "P" => LlmIntent::Path,
            "F" => LlmIntent::Feature,
            "O" => LlmIntent::Overview,
            "M" => LlmIntent::Compound,
            _ => LlmIntent::Feature,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LlmIntent::Symbol => "symbol",
            LlmIntent::CallChain => "call_chain",
            LlmIntent::Reference => "reference",
            LlmIntent::Path => "path",
            LlmIntent::Feature => "feature",
            LlmIntent::Overview => "overview",
            LlmIntent::Compound => "compound",
        }
    }
}

/// 检索策略配置。
#[derive(Debug, Clone)]
pub struct RetrievalStrategy {
    pub enable_path_index: bool,
    pub enable_query_rewrite: bool,
    pub enable_llm_rerank: bool,
    pub boost_definitions: bool,
    pub boost_docs: bool,
    pub max_chunks_per_path: usize,
    /// broad regime（架构/概览类探索查询：宽窗口 + manifest prior + 骨架化）。
    /// 触发是「意图判为 Overview ∨ 启发式词表」的 OR 之一，启发式在 pipeline 里
    /// 独立判定（无 LLM 分类时的 fallback，词表见 `broad::query_wants_structure`）。
    pub broad: bool,
}

const fn strategy(
    enable_path_index: bool,
    enable_query_rewrite: bool,
    enable_llm_rerank: bool,
    boost_definitions: bool,
    boost_docs: bool,
    max_chunks_per_path: usize,
) -> RetrievalStrategy {
    RetrievalStrategy {
        enable_path_index,
        enable_query_rewrite,
        enable_llm_rerank,
        boost_definitions,
        boost_docs,
        max_chunks_per_path,
        broad: false,
    }
}

/// 决策表：意图 → 检索策略。
pub fn get_strategy(intent: LlmIntent) -> RetrievalStrategy {
    match intent {
        // S：符号名应在正文中定位；路径语义会把同名引用、模型和 DAO 提到定义前面
        LlmIntent::Symbol => strategy(false, true, true, true, false, 2),
        // C：保留原查询中的方向和边界信息，交给 LLM 判断调用关系
        LlmIntent::CallChain => strategy(false, false, true, false, false, 3),
        // R：查询改写 + 中等块数（多个引用位置）
        LlmIntent::Reference => strategy(false, true, false, false, false, 4),
        // P：文件语义改写补足中英文差异，路径索引负责召回，LLM 决定最终顺序
        LlmIntent::Path => strategy(true, true, true, false, false, 2),
        // F：功能描述需要跨中英文术语召回，再由正文相关性确定实现文件
        LlmIntent::Feature => strategy(false, true, true, false, false, 3),
        // O：文档提升 + LLM 重排（理解架构描述）+ broad regime（答案散布多文件）
        LlmIntent::Overview => RetrievalStrategy {
            broad: true,
            ..strategy(false, false, true, false, true, 3)
        },
        // M：查询改写 + LLM 重排（处理多条件）
        LlmIntent::Compound => strategy(false, true, true, false, false, 3),
    }
}

impl Default for RetrievalStrategy {
    fn default() -> Self {
        strategy(false, false, false, false, false, 3)
    }
}
