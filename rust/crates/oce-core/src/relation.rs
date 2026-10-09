//! 关系抽取：从源文本抽出**静态可解析**的调用关系。
//!
//! 只做 1 跳、只认静态名：`foo(...)`、`self.bar(...)`、`obj.method(...)` 这类调用点
//! 能给出确定的被调标识符；`handlers["k"](...)`、`getattr(o, "m")(...)` 这类动态分派
//! 给不出确定名字，标 `resolution = "unresolved"`，且**不进 hints、不进图扩展**
//! （2026-09-21 Note：允许不知道，但不假装知道）。
//!
//! 设计取舍（实施时收窄，理由见计划 STEP-14 的修订说明）：
//!
//! - **不落 `chunk_relations` 表**：AGENTS.md 要求 Rust 的 PG schema 与 Python alembic
//!   head 逐列一致，而本轮不改 Python；加表会直接违反这条约束（STEP-6 的
//!   `retrieval_metrics` 新列就是因此放弃的）。关系改为**查询期**从已选中的
//!   chunk 正文现算——种子是 ≤20 段内容，正则成本可忽略。
//! - **只抽 `Calls`**：`SameSymbol` / `MemberOf` 不是从源码文本能算出来的（需要
//!   `symbol_occurrences` 符号表），而它们服务的 hints 增强已由
//!   `ident_tokens + find_definitions` 覆盖；图扩展只需要 `Calls`。

use regex::Regex;
use std::sync::OnceLock;

/// 静态调用关系的置信度。低于精确符号召回（0.95），高于弱语义兜底。
pub const CALL_CONFIDENCE: f32 = 0.8;

/// 关系种类。当前只有调用（见模块注释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Calls,
}

impl RelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationKind::Calls => "calls",
        }
    }
}

/// 一条抽取出的关系。`resolution == "unresolved"` 时 `dst_identifier` 为空。
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub dst_identifier: String,
    pub kind: RelationKind,
    pub confidence: f32,
    /// `"static-name"` | `"unresolved"`
    pub resolution: String,
}

impl Relation {
    /// 是否可用于 hints / 图扩展：只有解析出确定名字的才算数。
    pub fn is_resolved(&self) -> bool {
        self.resolution == "static-name" && !self.dst_identifier.is_empty()
    }
}

/// 调用点：标识符或点号链紧跟左括号。
fn call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)\s*\(")
            .expect("valid call regex")
    })
}

/// 动态分派调用点：`](`、`)(`、`"..."(` 这类没有静态被调名的形态。
fn dynamic_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"["'\]\)]\s*\("#).expect("valid dynamic call regex"))
}

/// 声明关键字：`def foo(` / `fn foo(` / `function foo(` / `class Foo(` 是定义不是调用。
fn declaration_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[^\w])(?:def|fn|function|class|struct|enum|trait|impl|interface|type)\s+$")
            .expect("valid declaration regex")
    })
}

/// 控制流/语言关键字：`if (`、`for (`、`return (` 不是调用。
const KEYWORDS: &[&str] = &[
    "if", "elif", "else", "for", "while", "return", "match", "switch", "case", "catch", "except",
    "with", "assert", "lambda", "async", "await", "not", "in", "is", "and", "or", "import", "from",
    "raise", "yield", "del", "global", "pass", "break", "continue", "try", "finally", "new",
    "sizeof", "typeof", "print", "println", "unwrap", "expect",
];

/// 从源码文本抽取调用关系（同一被调名去重，保留最高置信度）。
///
/// `path` 只用于将来的语言相关微调，当前各语言共用同一套调用形态。
pub fn extract_relations(path: &str, content: &str) -> Vec<Relation> {
    let _ = path;
    let mut out: Vec<Relation> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for caps in call_re().captures_iter(content) {
        let whole = caps.get(0).expect("group 0");
        let identifier = caps.get(1).expect("group 1").as_str();
        // 动态分派形态（`handlers["k"](` 之类）由下面的专门分支处理，这里跳过
        let prefix = &content[..whole.start()];
        if prefix.ends_with(']') || prefix.ends_with(')') || prefix.ends_with('"') || prefix.ends_with('\'') {
            continue;
        }
        if declaration_re().is_match(prefix) {
            continue;
        }
        let last = identifier.rsplit('.').next().unwrap_or(identifier);
        if KEYWORDS.contains(&last) {
            continue;
        }
        let relation = Relation {
            dst_identifier: identifier.to_string(),
            kind: RelationKind::Calls,
            confidence: CALL_CONFIDENCE,
            resolution: "static-name".to_string(),
        };
        match seen.get(identifier) {
            Some(index) => {
                if out[*index].confidence < relation.confidence {
                    out[*index] = relation;
                }
            }
            None => {
                seen.insert(identifier.to_string(), out.len());
                out.push(relation);
            }
        }
    }

    // 动态分派：能看见"这里有一次调用"，但给不出确定名字。
    for _ in dynamic_call_re().find_iter(content) {
        out.push(Relation {
            dst_identifier: String::new(),
            kind: RelationKind::Calls,
            confidence: 0.0,
            resolution: "unresolved".to_string(),
        });
    }

    out
}

/// 动态分派调用表达式的文本跨度。
///
/// hints 用它排除"只在动态调用里出现"的 token：`handlers["refresh"](1)` 里的
/// `refresh` 是字符串键、不是被调符号，不该出现在 hints 里。
pub fn unresolved_call_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for m in dynamic_call_re().find_iter(content) {
        // 向前找表达式起点：最近的语句/参数分隔符之后
        let mut start = m.start();
        for (idx, ch) in content[..m.start()].char_indices().rev() {
            if matches!(ch, ';' | '{' | '}' | '\n' | '=' | ',' | '(') {
                start = idx + ch.len_utf8();
                break;
            }
            start = idx;
        }
        spans.push((start, m.end()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_relations_python_calls() {
        let content = "\
import os


def helper(value):
    return value


def run(client):
    data = compute(1, 2)
    client.refresh()
    return data
";
        let relations = extract_relations("src/app.py", content);
        let resolved: Vec<&str> = relations
            .iter()
            .filter(|r| r.is_resolved())
            .map(|r| r.dst_identifier.as_str())
            .collect();
        // `def helper(` / `def run(` 是定义不是调用；`import os` 不是调用
        assert!(resolved.contains(&"compute"), "{resolved:?}");
        assert!(resolved.contains(&"client.refresh"), "{resolved:?}");
        assert!(!resolved.contains(&"helper"), "定义不应算调用: {resolved:?}");
        assert!(!resolved.contains(&"run"), "定义不应算调用: {resolved:?}");
        assert!(!resolved.contains(&"import"), "{resolved:?}");
        assert!(
            relations.iter().all(|r| r.resolution == "static-name"),
            "{relations:?}"
        );
        assert!(relations.iter().all(|r| r.kind == RelationKind::Calls));
    }

    #[test]
    fn extract_relations_marks_dynamic_dispatch_unresolved() {
        let content = "fn main() {\n    handlers[\"k\"](1);\n    let f = make();\n    f(2);\n}\n";
        let relations = extract_relations("src/main.rs", content);
        let unresolved: Vec<&Relation> = relations.iter().filter(|r| !r.is_resolved()).collect();
        assert!(!unresolved.is_empty(), "动态分派必须标 unresolved: {relations:?}");
        assert!(unresolved.iter().all(|r| r.dst_identifier.is_empty()));
        assert!(relations.iter().any(|r| r.dst_identifier == "make"));
    }

    #[test]
    fn extract_relations_dedupes_same_target() {
        let content = "fn a() { b(); b(); b(); }\n";
        let relations = extract_relations("src/a.rs", content);
        let calls: Vec<&str> = relations
            .iter()
            .filter(|r| r.is_resolved())
            .map(|r| r.dst_identifier.as_str())
            .collect();
        assert_eq!(calls, vec!["b"], "同名调用只留一条: {relations:?}");
    }

    #[test]
    fn unresolved_call_spans_cover_the_dynamic_expression() {
        let content = "let x = handlers[\"k\"](1);\n";
        let spans = unresolved_call_spans(content);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (start, end) = spans[0];
        let covered = &content[start..end];
        assert!(covered.contains("handlers"), "跨度应覆盖表达式: {covered:?}");
        assert!(covered.ends_with('('), "跨度应到左括号: {covered:?}");
    }
}
