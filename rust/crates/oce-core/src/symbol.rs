//! 从 chunk content 提取标识符（函数名、类名、endpoint）。
//! 与 Python `infrastructure/persistence/symbol_extractor.py` 逐条对齐。

use regex::Regex;
use std::sync::OnceLock;

/// 单个标识符出现位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOccurrence {
    pub identifier: String,
    /// 'endpoint' | 'definition'
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

fn endpoint_patterns() -> &'static Vec<Regex> {
    static PATS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            // Tauri command: #[tauri::command] 或 #[pytauri::command]
            Regex::new(
                r"(?s)#\[(?:tauri::command|pytauri::command)[^\]]*\]\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\b",
            )
            .unwrap(),
            // Python FastAPI/Flask: @app.get() @router.post() 等
            Regex::new(
                r"(?m)@(?:app|router)\.(?:get|post|put|patch|delete|websocket)\([^\n]*\)\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\b",
            )
            .unwrap(),
            // TypeScript/JavaScript decorators (Express/NestJS)
            Regex::new(
                r"(?m)@(?:Get|Post|Put|Patch|Delete|Controller)\([^\n]*\)\s*(?:async\s+)?(?:function\s+)?([A-Za-z_$][A-Za-z0-9_$]*)\b",
            )
            .unwrap(),
        ]
    })
}

fn definition_patterns() -> &'static Vec<Regex> {
    static PATS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            // Rust: pub fn, async fn, struct, enum, trait
            Regex::new(
                r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)\b",
            )
            .unwrap(),
            // Python: def, class, async def
            Regex::new(r"(?m)^\s*(?:async\s+)?(?:def|class)\s+([A-Za-z_][A-Za-z0-9_]*)\b")
                .unwrap(),
            // TypeScript/JavaScript: function, class, interface, type
            Regex::new(
                r"(?m)^\s*(?:export\s+)?(?:async\s+)?(?:default\s+)?(?:function|class|interface|type|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)\b",
            )
            .unwrap(),
            // const/let/var assignment
            Regex::new(
                r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
            )
            .unwrap(),
        ]
    })
}

pub struct SymbolExtractor;

impl SymbolExtractor {
    /// 从 chunk content 提取所有标识符（按 (identifier, kind) 去重；
    /// endpoint 不被 definition 覆盖）。
    pub fn extract_symbols(content: &str, start_line: u32, end_line: u32) -> Vec<SymbolOccurrence> {
        let mut symbols: Vec<SymbolOccurrence> = Vec::new();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        // 1. endpoint 定义（最高优先级）
        for pattern in endpoint_patterns() {
            for caps in pattern.captures_iter(content) {
                let identifier = &caps[1];
                if identifier.len() >= 2 && seen.insert((identifier.into(), "endpoint".into())) {
                    symbols.push(SymbolOccurrence {
                        identifier: identifier.to_string(),
                        kind: "endpoint".into(),
                        start_line,
                        end_line,
                    });
                }
            }
        }

        // 2. 普通定义：已是 endpoint 的不覆盖
        for pattern in definition_patterns() {
            for caps in pattern.captures_iter(content) {
                let identifier = &caps[1];
                if identifier.len() < 2 {
                    continue;
                }
                let key = (identifier.to_string(), "definition".to_string());
                let endpoint_key = (identifier.to_string(), "endpoint".to_string());
                if !seen.contains(&endpoint_key) && seen.insert(key) {
                    symbols.push(SymbolOccurrence {
                        identifier: identifier.to_string(),
                        kind: "definition".into(),
                        start_line,
                        end_line,
                    });
                }
            }
        }
        symbols
    }

    /// 从查询中提取标识符（用于检索）。支持反引号、snake_case、Rust 路径、类型名。
    pub fn extract_identifiers_from_query(query: &str) -> Vec<String> {
        let mut identifiers = std::collections::BTreeSet::new();

        static BACKTICK: OnceLock<Regex> = OnceLock::new();
        static SNAKE: OnceLock<Regex> = OnceLock::new();
        static PASCAL: OnceLock<Regex> = OnceLock::new();
        let backtick = BACKTICK.get_or_init(|| Regex::new(r"`([A-Za-z_][A-Za-z0-9_:]*)`").unwrap());
        let snake = SNAKE
            .get_or_init(|| Regex::new(r"\b([a-z_][a-z0-9_]*(?:::[a-z_][a-z0-9_]*)*)\b").unwrap());
        let pascal = PASCAL.get_or_init(|| {
            Regex::new(r"\b([A-Z][A-Za-z0-9]*(?:::[A-Z][A-Za-z0-9]*)*)\b").unwrap()
        });

        for caps in backtick.captures_iter(query) {
            identifiers.insert(caps[1].to_string());
        }
        for caps in snake.captures_iter(query) {
            let candidate = &caps[1];
            if candidate.contains('_') || candidate.contains("::") {
                identifiers.insert(candidate.to_string());
            }
        }
        for caps in pascal.captures_iter(query) {
            identifiers.insert(caps[1].to_string());
        }
        identifiers.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_python_defs_and_endpoints() {
        let content = "@app.get(\"/x\")\ndef handler_x():\n    pass\n\nclass Widget:\n    pass\n";
        let syms = SymbolExtractor::extract_symbols(content, 1, 6);
        let endpoint = syms.iter().find(|s| s.kind == "endpoint").unwrap();
        assert_eq!(endpoint.identifier, "handler_x");
        let def = syms.iter().find(|s| s.identifier == "Widget").unwrap();
        assert_eq!(def.kind, "definition");
    }

    #[test]
    fn extracts_rust_items() {
        let content = "pub fn run_all() {}\nstruct Config;\nasync fn fetch() {}\n";
        let syms = SymbolExtractor::extract_symbols(content, 1, 3);
        let names: Vec<&str> = syms.iter().map(|s| s.identifier.as_str()).collect();
        assert!(names.contains(&"run_all"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"fetch"));
    }

    #[test]
    fn query_identifiers() {
        let ids = SymbolExtractor::extract_identifiers_from_query(
            "`delete_profile` 和 module::fn_name 与 DeleteProfile 在哪里",
        );
        assert!(ids.contains(&"delete_profile".to_string()));
        assert!(ids.contains(&"module::fn_name".to_string()));
        assert!(ids.contains(&"DeleteProfile".to_string()));
    }

    #[test]
    fn endpoint_not_overwritten_by_definition() {
        let content = "@app.post(\"/y\")\ndef create_y():\n    pass\n";
        let syms = SymbolExtractor::extract_symbols(content, 1, 3);
        let y = syms.iter().find(|s| s.identifier == "create_y").unwrap();
        assert_eq!(y.kind, "endpoint");
        assert_eq!(
            syms.iter().filter(|s| s.identifier == "create_y").count(),
            1
        );
    }
}
