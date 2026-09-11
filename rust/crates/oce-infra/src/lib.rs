//! OpenContextEngine 基础设施层。
//!
//! 依赖方向：只实现 oce-core 的协议；只能由 composition root（oce-app/oce-server）装配。
//! - [`sqlite`]: 关系元数据（blob/chunk/symbol/chain/credentials/metrics，与 Python schema 一致）
//! - [`trivium`]: TriviumDB 向量引擎（个人模式，替代 Milvus Lite；chunk+path 节点）
//! - [`openai`]: OpenAI 兼容 embedding / rerank / chat 客户端
//! - [`credentials`]: 凭据解析运行时（DB 凭据 → env 回落，热重载）
//! - [`settings`]: 环境变量配置（与 Python 版前缀兼容）

pub mod credentials;
pub mod openai;
pub mod settings;
pub mod static_embed;
pub mod sqlite;
pub mod trivium;

/// 同 run_sql，但闭包直接返回 OceError（需要区分唯一约束冲突时使用）。
pub(crate) async fn run_sql_oce<T: Send + 'static>(
    db: sqlite::SqlDb,
    f: impl FnOnce(&mut rusqlite::Connection) -> Result<T, oce_core::error::OceError> + Send + 'static,
) -> oce_core::error::OceResult<T> {
    tokio::task::spawn_blocking(move || {
        let mut guard = db.lock_conn();
        f(&mut guard)
    })
    .await
    .map_err(|e| oce_core::error::OceError::new(e.to_string(), "JoinError"))?
}
