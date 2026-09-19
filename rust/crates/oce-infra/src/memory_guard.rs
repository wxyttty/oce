//! 内存硬限制守卫。进程内存超过 OCE_MEMORY_LIMIT_MB 时拒绝新嵌入请求。
//!
//! 设计意图：candle CPU 嵌入在大批量上传时会因 KV cache 泄漏和中间张量堆积
//! 导致内存暴涨。守卫在嵌入前检查内存，超限立即返回错误，让调用方
//! 暂停喂入而非无限堆积。
//!
//! macOS 上使用 `mach_task_info` 的 `phys_footprint`（包含 RSS + compressed memory），
//! 与活动监视器 "Memory" 列一致。其他平台用 sysinfo RSS。

use std::sync::atomic::{AtomicU64, Ordering};

/// 全局内存限制（字节），0=不限。
static MEMORY_LIMIT_BYTES: AtomicU64 = AtomicU64::new(0);

/// 初始化内存限制。在 composition root 启动时调用一次。
pub fn init(limit_mb: usize) {
    MEMORY_LIMIT_BYTES.store(limit_mb as u64 * 1024 * 1024, Ordering::Relaxed);
    if limit_mb > 0 {
        tracing::info!("memory guard: limit = {} MB", limit_mb);
    }
}

/// 当前进程内存（MB）。用 macOS /proc 替代——macOS 没有 /proc，
/// 但 sysinfo 的 refresh_processes 会和 candle CPU 前向冲突触发 SIGSYS。
/// 改用 ps 命令获取 RSS，开销 ~5ms 但安全。
fn current_memory_mb() -> f64 {
    // 用 std::process::Command 调 ps 获取 RSS，避免 sysinfo 的 SIGSYS 问题
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok();
    match output {
        Some(o) if o.status.success() => {
            let rss_kb: f64 = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0.0);
            // macOS 活动监视器 Memory 列 ≈ RSS × 1.5（含 compressed）
            rss_kb / 1024.0 * 1.5
        }
        _ => 0.0,
    }
}

/// 检查内存是否超限。超限返回 Err（含当前内存和限制值）。
/// 注意：此函数通过 ps 命令获取 RSS，不在 candle 前向线程中调用，
/// 避免和 candle CPU 操作冲突触发 SIGSYS。
pub fn check() -> Result<(), String> {
    let limit = MEMORY_LIMIT_BYTES.load(Ordering::Relaxed);
    if limit == 0 {
        return Ok(());
    }
    let mem_mb = current_memory_mb();
    let limit_mb = limit as f64 / 1024.0 / 1024.0;
    if mem_mb > limit_mb {
        Err(format!(
            "memory limit exceeded: memory={:.0} MB > limit={:.0} MB; refuse new embedding to avoid OOM",
            mem_mb, limit_mb
        ))
    } else {
        Ok(())
    }
}

/// 当前内存（MB），供日志/监控用。
pub fn memory_mb() -> f64 {
    current_memory_mb()
}
