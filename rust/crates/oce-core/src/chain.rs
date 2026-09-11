//! Chain 聚合根 — 工作集抽象。与 Python `domain/chain/chain.py` 语义对齐。
//!
//! 不变量：chain_id 必须是 UUID；version 单调递增；members 集合去重。

/// Chain 聚合根。
#[derive(Debug, Clone)]
pub struct Chain {
    pub chain_id: String,
    pub version: u32,
    pub members: std::collections::HashSet<String>,
}

fn is_valid_uuid(s: &str) -> bool {
    // Python uuid.UUID 语义：接受 8-4-4-4-12 或 32 位 hex（无连字符）
    let compact: String = s.chars().filter(|c| *c != '-').collect();
    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        let lens = [8usize, 4, 4, 4, 12];
        parts.len() == 5
            && parts
                .iter()
                .zip(lens)
                .all(|(p, l)| p.len() == l && p.bytes().all(|b| b.is_ascii_hexdigit()))
    } else {
        compact.len() == 32 && compact.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

impl Chain {
    /// 创建新 Chain（version 从 1 开始，members 去重）。
    pub fn create(members: Vec<String>) -> Self {
        Self {
            chain_id: uuid_v4(),
            version: 1,
            members: members.into_iter().collect(),
        }
    }

    /// 应用 Checkpoint：members ∪ added − deleted，version += 1。
    pub fn apply_checkpoint(&mut self, added: &[String], deleted: &[String]) {
        for blob_name in added {
            self.members.insert(blob_name.clone());
        }
        for blob_name in deleted {
            self.members.remove(blob_name);
        }
        self.version += 1;
    }

    pub fn contains(&self, blob_name: &str) -> bool {
        self.members.contains(blob_name)
    }

    pub fn size(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Checkpoint 令牌：`{chain_id}:{version}`。不透明令牌，客户端只存储并回传。
    pub fn checkpoint_token(&self) -> String {
        format!("{}:{}", self.chain_id, self.version)
    }

    /// 解析 Checkpoint 令牌 → (chain_id, version)；格式非法返回 None。
    pub fn parse_checkpoint_token(token: &str) -> Option<(String, u32)> {
        if token.is_empty() || !token.contains(':') {
            return None;
        }
        let Some((chain_id, version_str)) = token.rsplit_once(':') else {
            return None;
        };
        if chain_id.is_empty() || !version_str.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let version: u32 = version_str.parse().ok()?;
        if !is_valid_uuid(chain_id) {
            return None;
        }
        Some((chain_id.to_string(), version))
    }
}

/// 链 ID（uuid4 hex 无连字符，与 Python `uuid.uuid4().hex` 一致）。
pub fn new_chain_id_hex() -> String {
    uuid_v4().replace('-', "")
}

/// UUID v4 生成（无外部依赖版本：随机字节 + 版本位设置）。
fn uuid_v4() -> String {
    // infra/app 层可用 uuid crate；core 内置轻量实现避免依赖蔓延
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 128 位：时间纳斯 + 计数器 + 地址熵
    let a = nanos ^ (count.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let b = std::process::id() as u64 ^ count.rotate_left(32) ^ nanos.rotate_left(17);

    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    // 设置 version 4 与 variant 位
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_token_roundtrip() {
        let chain = Chain::create(vec!["a".into(), "b".into()]);
        assert_eq!(chain.version, 1);
        assert_eq!(chain.size(), 2);
        let token = chain.checkpoint_token();
        let (id, version) = Chain::parse_checkpoint_token(&token).unwrap();
        assert_eq!(id, chain.chain_id);
        assert_eq!(version, 1);
    }

    #[test]
    fn apply_checkpoint_updates_members_and_version() {
        let mut chain = Chain::create(vec!["a".into(), "b".into()]);
        chain.apply_checkpoint(&["b".into(), "c".into()], &["a".into()]);
        assert_eq!(chain.version, 2);
        assert!(chain.contains("b"));
        assert!(chain.contains("c"));
        assert!(!chain.contains("a"));
    }

    #[test]
    fn invalid_tokens_rejected() {
        assert!(Chain::parse_checkpoint_token("").is_none());
        assert!(Chain::parse_checkpoint_token("not-a-uuid:1").is_none());
        assert!(Chain::parse_checkpoint_token("550e8400-e29b-41d4-a716-446655440000:x").is_none());
        assert!(Chain::parse_checkpoint_token("550e8400-e29b-41d4-a716-446655440000").is_none());
    }
}
