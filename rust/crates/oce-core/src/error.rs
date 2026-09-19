//! 错误类型：与 Python 版 `shared/errors` 语义对齐。
//!
//! API 层按变体映射 HTTP 状态码（与 Python router.py 一致）：
//! `InvalidCheckpointToken` → 400，`NeedsReset` → 404，`ServiceNotReady` → 503，
//! `ScopeRequired` → 400。`Display` 输出 `[CODE] message`，与 Python `OCEError.__str__` 一致。

#[derive(Debug, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct OceError {
    pub message: String,
    pub code: String,
}

impl OceError {
    pub fn new(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
        }
    }

    pub fn domain(message: impl Into<String>) -> Self {
        Self::new(message, "DomainError")
    }

    /// 检查点令牌格式非法（HTTP 400）。
    pub fn invalid_checkpoint_token(token: &str) -> Self {
        Self::new(
            format!("Invalid checkpoint token format: {token}"),
            "INVALID_CHECKPOINT_TOKEN",
        )
    }

    /// 服务未就绪：无可用的 embedding 凭据（HTTP 503 + Retry-After: 0）。
    pub fn service_not_ready(reason: Option<&str>) -> Self {
        Self::new(
            reason.unwrap_or("Service not ready: no embedding credential is configured"),
            "SERVICE_NOT_READY",
        )
    }

    /// checkpoint 链不存在，客户端必须重置（HTTP 404）。
    pub fn needs_reset(reason: impl Into<String>) -> Self {
        Self::new(reason, "NEEDS_RESET")
    }

    /// 检索未声明工作集（HTTP 400）。
    pub fn scope_required() -> Self {
        Self::new(
            "检索必须声明工作集：提供 checkpoint_id 或 added_blobs",
            "SCOPE_REQUIRED",
        )
    }

    /// 凭据唯一约束冲突 (kind, model, api_key_hash)。
    pub fn credential_conflict(reason: Option<&str>) -> Self {
        Self::new(
            reason.unwrap_or("该 kind + model 下已存在相同 api_key 的凭据"),
            "CREDENTIAL_CONFLICT",
        )
    }
}

pub type OceResult<T> = Result<T, OceError>;
