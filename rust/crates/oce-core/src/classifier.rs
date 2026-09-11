//! 启发式查询意图分类器。与 Python `domain/services/query_classifier.py` 逐条对齐。
//!
//! 判定优先级：
//! 1. 有符号锚点（反引号/snake_case/::）：调用动词→CALL_CHAIN，引用动词→REFERENCE，其余→SYMBOL
//! 2. 无符号锚点：文件名 token 或路径词（非功能类）→PATH；概览词→OVERVIEW；其余→FEATURE

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    Symbol,
    CallChain,
    Reference,
    Path,
    Feature,
    Overview,
    Compound,
}

impl Intent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Symbol => "symbol",
            Intent::CallChain => "call_chain",
            Intent::Reference => "reference",
            Intent::Path => "path",
            Intent::Feature => "feature",
            Intent::Overview => "overview",
            Intent::Compound => "compound",
        }
    }
}

/// 符号锚点：反引号包裹、snake_case、路径限定符 ::
fn symbol_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`]+`|[a-z][a-z0-9]*_[a-z0-9_]+|\w+::\w+").unwrap())
}

fn identifier_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z_$][A-Za-z0-9_$]*(?:::[A-Za-z_$][A-Za-z0-9_$]*)*$").unwrap()
    })
}

fn snake_identifier_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z][a-z0-9]*_[a-z0-9_]+").unwrap())
}

fn qualified_identifier_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]*(?:::[A-Za-z_$][A-Za-z0-9_$]*)+").unwrap()
    })
}

fn type_identifier_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Z][A-Za-z0-9_$]*)\s*(?:的)?(?:前后端)?(?:类型|类|接口|结构|定义|(?:type|interface|struct|enum|trait|class|definition)\b)",
        )
        .unwrap()
    })
}

/// 带扩展名的文件名 token（config.json / lib.rs）：「找文件」的强结构信号。
fn filename_token_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9_\-]+\.[A-Za-z][A-Za-z0-9]{0,7}").unwrap())
}

const CALL_VERBS: [&str; 19] = [
    "调用", "触发", "执行", "从", "到", "路径", "流程", "完整", "如何被", "如何从", "call",
    "invoke", "trigger", "execute", "from", "to", "path", "flow", "pipeline",
];

const REFERENCE_VERBS: [&str; 11] = [
    "使用", "引用", "导入", "依赖", "消费", "接收", "use", "import", "depend", "consume",
    "receive",
];

const OVERVIEW_KEYWORDS: [&str; 15] = [
    "架构", "实现", "事件处理", "状态管理", "调度", "机制", "流程", "architecture",
    "implementation", "event handling", "state management", "scheduling", "dispatch",
    "mechanism", "workflow",
];

const PATH_KEYWORDS: [&str; 12] = [
    "文件", "配置", "在哪里", "在哪", "哪个文件", "翻译文件", "依赖", "file", "config",
    "where", "location", "dependency",
];

const FEATURE_MARKERS: [&str; 18] = [
    "功能", "实现", "逻辑", "代码", "机制", "策略", "如何", "怎样", "怎么", "feature",
    "implement", "implementation", "logic", "code", "mechanism", "strategy", "behavior",
    "how",
];

/// 英文（ASCII）词用前缀词边界匹配：避免子串误命中，又覆盖词形变化。
/// 中文无词边界，按子串匹配。
fn terms_pattern(terms: &[&str]) -> Regex {
    let parts: Vec<String> = terms
        .iter()
        .map(|t| {
            if t.is_ascii() {
                format!(r"\b{}", regex::escape(t))
            } else {
                regex::escape(t)
            }
        })
        .collect();
    Regex::new(&parts.join("|")).unwrap()
}

fn call_verbs_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| terms_pattern(&CALL_VERBS))
}

fn reference_verbs_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| terms_pattern(&REFERENCE_VERBS))
}

fn overview_keywords_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| terms_pattern(&OVERVIEW_KEYWORDS))
}

fn path_keywords_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| terms_pattern(&PATH_KEYWORDS))
}

fn feature_markers_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| terms_pattern(&FEATURE_MARKERS))
}

/// 按意图分类查询，用于派发检索策略。
pub fn classify_query_intent(query: &str) -> Intent {
    let query_lower = query.to_lowercase();
    let has_symbol = symbol_pattern().is_match(query);

    // 分支1：有符号锚点
    if has_symbol {
        // 提取反引号外的文本，避免符号名本身被动词误匹配
        static BACKTICKS: OnceLock<Regex> = OnceLock::new();
        let re = BACKTICKS.get_or_init(|| Regex::new(r"`[^`]+`").unwrap());
        let text_outside_backticks = re.replace_all(&query_lower, "");

        if call_verbs_re().is_match(&text_outside_backticks) {
            return Intent::CallChain;
        }
        if reference_verbs_re().is_match(&text_outside_backticks) {
            return Intent::Reference;
        }
        return Intent::Symbol;
    }

    // 分支2：无符号锚点。用结构信号判定，不枚举技术栈。
    let has_path_kw = path_keywords_re().is_match(&query_lower);
    let has_feature_marker = feature_markers_re().is_match(&query_lower);

    if filename_token_pattern().is_match(query) {
        return Intent::Path;
    }
    if has_path_kw && !has_feature_marker {
        return Intent::Path;
    }
    if overview_keywords_re().is_match(&query_lower) {
        return Intent::Overview;
    }
    Intent::Feature
}



/// 查询是否包含代码标识符。
pub fn has_code_identifier(query: &str) -> bool {
    symbol_pattern().is_match(query)
}

/// 提取适合精确词法召回的代码标识符，保持查询中的出现顺序。
pub fn extract_code_identifiers(query: &str) -> Vec<String> {
    let mut identifiers: Vec<String> = Vec::new();

    let add = |value: &str, out: &mut Vec<String>| {
        let value = value.trim();
        if identifier_pattern().is_match(value) && !out.iter().any(|v| v == value) {
            out.push(value.to_string());
        }
    };

    static BACKTICKS: OnceLock<Regex> = OnceLock::new();
    let re = BACKTICKS.get_or_init(|| Regex::new(r"`([^`]+)`").unwrap());
    for caps in re.captures_iter(query) {
        add(&caps[1], &mut identifiers);
    }
    for pattern in [
        qualified_identifier_pattern(),
        snake_identifier_pattern(),
        type_identifier_pattern(),
    ] {
        for caps in pattern.captures_iter(query) {
            // 有捕获组用组 1，否则整体
            let m = caps.get(1).unwrap_or_else(|| caps.get(0).unwrap());
            add(m.as_str(), &mut identifiers);
        }
    }
    identifiers
}

/// 是否文件名查询（兼容接口，内部用意图分类）。
pub fn is_filename_query(query: &str) -> (bool, f32) {
    match classify_query_intent(query) {
        Intent::Path => (true, 0.9),
        _ => (false, 0.0),
    }
}

/// 是否应该使用路径索引。符号查询不路由到 path index：
/// 带符号锚点的查询（即便含扩展名）优先判为 SYMBOL。
pub fn should_use_path_index(query: &str) -> bool {
    classify_query_intent(query) == Intent::Path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_intent_with_backticks() {
        assert_eq!(
            classify_query_intent("`parse_config` 函数在哪里定义？"),
            Intent::Symbol
        );
    }

    #[test]
    fn call_chain_intent() {
        assert_eq!(
            classify_query_intent("前端如何调用后端的 `parse_config`？"),
            Intent::CallChain
        );
    }

    #[test]
    fn path_intent_for_filename_token() {
        assert_eq!(classify_query_intent("config.json 在哪里？"), Intent::Path);
    }

    #[test]
    fn symbol_beats_extension_token() {
        // 符号优先，不因扩展名改判为 PATH
        assert_eq!(
            classify_query_intent("`parse_config` 在 server.py 中注册了哪些路由？"),
            Intent::Symbol
        );
    }

    #[test]
    fn feature_default() {
        assert_eq!(classify_query_intent("登录功能是怎么做的"), Intent::Feature);
    }

    #[test]
    fn extract_identifiers_dedup_and_order() {
        let ids = extract_code_identifiers("参考 `delete_profile` 和 delete_profile 与 DeleteProfileConfig::new");
        assert_eq!(ids.first().map(String::as_str), Some("delete_profile"));
        assert!(ids.contains(&"DeleteProfileConfig::new".to_string()));
    }

    #[test]
    fn verb_inside_backticks_does_not_trigger_call_chain() {
        // invoke_handler 中的 invoke 不应触发 CALL_CHAIN
        assert_eq!(
            classify_query_intent("`invoke_handler` 是什么"),
            Intent::Symbol
        );
    }




    #[test]
    fn should_use_path_index_semantics() {
        assert!(should_use_path_index("config.json 在哪里？"));
        assert!(!should_use_path_index(
            "`parse_config` 在 server.py 中注册了哪些路由？"
        ));
    }
}
