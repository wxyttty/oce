//! 查询侧词法概念投影表。借鉴 semble_rs `CJK_QUERY_ALIAS_DATA` / `semantic_query`：
//! 只扩展 BM25 查询文本（查询侧追加通用代码词汇），不动索引与向量。
//!
//! 约束与 path_doc 的 EXTENSION_SEMANTICS 同源：条目必须是跨仓库成立的通用
//! 世界知识（概念 → 真实代码里高频出现的词法），不注入单仓先验。
//! 实测针对的盲区：error_handling 等"概念型查询"——查询词（错误处理/重试）
//! 与代码词法（try/catch/exception/backoff）零重叠，dense 也常够不着。

/// 概念短语 → 代码词汇别名。中英双语条目；匹配大小写不敏感。
const CONCEPT_ALIASES: &[(&str, &str)] = &[
    ("错误处理", "try catch exception throw raise error handler logger"),
    ("error handling", "try catch exception throw raise errorhandler"),
    ("异常", "exception throw catch panic"),
    ("重试", "retry backoff attempts max_retries"),
    ("重连", "reconnect retry backoff"),
    ("超时", "timeout deadline elapsed expire"),
    ("日志", "log logger logging tracing debug"),
    ("鉴权", "auth authentication authorization token session login jwt"),
    ("权限", "permission role access control acl"),
    ("配置", "config configuration settings options env"),
    ("数据库", "database db sql query migration pool transaction"),
    ("缓存", "cache caching ttl invalidate evict"),
    ("队列", "queue worker task job consumer producer"),
    ("定时", "cron scheduler timer interval job"),
    ("分页", "pagination page limit offset cursor"),
    ("校验", "validate validation schema assert check"),
    ("上传", "upload file storage multipart"),
    ("下载", "download stream attachment"),
    ("搜索", "search query index filter match"),
    ("排序", "sort order rank"),
    ("过滤", "filter where predicate"),
    ("加密", "encrypt decrypt cipher aes rsa hash"),
    ("国际化", "i18n l10n locale translation language"),
    ("通知", "notification push webhook alert"),
    ("打印", "print template report export"),
    ("汇总", "sum aggregate total group"),
    ("统计", "statistics count aggregate metrics"),
    ("导入", "import parse ingest load"),
    ("导出", "export dump serialize download"),
    ("登录", "login signin authentication session token"),
    ("注册", "register signup account create"),
    ("订单", "order payment checkout cart"),
    ("库存", "inventory stock sku warehouse"),
    ("发货", "ship delivery logistics package"),
    ("调拨", "transfer allocation inventory"),
    ("期号", "times catalog batch issue period"),
    ("retry logic", "backoff attempts exponential"),
    ("input validation", "validate schema sanitize check"),
    ("rate limit", "throttle quota bucket limiter"),
    ("concurrency", "mutex lock atomic channel async thread"),
    ("connection pool", "pool datasource connections maxidle"),
    ("state management", "store reducer state atom signal"),
];

/// 追加概念别名后的查询文本（去重：别名 token 已在原查询出现则跳过该 token）。
/// 无任何短语命中时原样返回。
/// 中文短语语序容错匹配：子串命中，或短语的全部二字段都出现
/// （“调用错误”≠“错误处理”子串，但“错误”+“处理”都在 → 视为命中）。
fn phrase_matches(lower: &str, phrase: &str) -> bool {
    if lower.contains(phrase) {
        return true;
    }
    let chars: Vec<char> = phrase.chars().collect();
    if chars.iter().any(|c| !is_cjk(*c)) || chars.len() < 3 {
        return false;
    }
    (0..chars.len() - 1).step_by(2).all(|i| {
        let seg: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        lower.contains(&seg)
    })
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

pub fn enrich_lexical_query(query: &str) -> String {
    let lower = query.to_lowercase();
    let mut extra: Vec<&str> = Vec::new();
    for (phrase, tokens) in CONCEPT_ALIASES {
        if !phrase_matches(&lower, phrase) {
            continue;
        }
        for token in tokens.split_whitespace() {
            // token 已出现在原查询（大小写不敏感）则不重复
            if !lower.split_whitespace().any(|w| w == token)
                && !extra.contains(&token)
            {
                extra.push(token);
            }
        }
    }
    if extra.is_empty() {
        return query.to_string();
    }
    format!("{query} {}", extra.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enriches_error_handling_query() {
        let q = "前端如何处理 API 调用错误？";
        let e = enrich_lexical_query(q);
        assert!(e.contains("exception"), "got: {e}");
        assert!(e.contains("catch"), "got: {e}");
        assert!(e.starts_with(q));
    }

    #[test]
    fn enriches_english_concept_query() {
        let e = enrich_lexical_query("how is rate limit implemented?");
        assert!(e.contains("throttle"), "got: {e}");
    }

    #[test]
    fn no_alias_no_change() {
        assert_eq!(enrich_lexical_query("proxyPackageQty 在哪里定义"), "proxyPackageQty 在哪里定义");
    }
}
