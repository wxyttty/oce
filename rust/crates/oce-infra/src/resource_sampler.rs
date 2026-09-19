//! 资源采样器：后台周期采集磁盘 / 内存 / CPU 到 MetricsSink，落 resource_samples
//!（对应 Python `resource_sampler.py`；psutil → sysinfo）。
//!
//! 采样与写库都走旁路，异常只记日志；sink 未装配时 start() 直接跳过。

use oce_core::metrics::{MetricsSink, ResourceSampleRecord};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// 递归累加目录内文件字节数；单个文件不可读则跳过，整体不可达返回 0。
fn dir_size(path: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_file() => {
                    if let Ok(md) = entry.metadata() {
                        *total += md.len();
                    }
                }
                Ok(t) if t.is_dir() => walk(&entry.path(), total),
                _ => {}
            }
        }
    }
    let mut total = 0;
    walk(path, &mut total);
    total
}

/// 采集一次快照。sysinfo 的 CPU% 需要两次采样间隔才有意义，这里由调用方的
/// 周期间隔保证（MIN_CPU_INTERVAL 以下读数恒为 0，可接受——报表只看趋势）。
fn collect(data_dir: Option<&Path>) -> ResourceSampleRecord {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.refresh_cpu_usage();

    let mem_rss_bytes = {
        let mut p_sys = sysinfo::System::new();
        match sysinfo::get_current_pid() {
            Ok(pid) => {
                p_sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                if let Some(proc) = p_sys.process(pid) {
                    proc.memory()
                } else {
                    sys.used_memory()
                }
            }
            Err(_) => sys.used_memory(),
        }
    };
    let mem_percent = if sys.total_memory() > 0 {
        mem_rss_bytes as f64 / sys.total_memory() as f64 * 100.0
    } else {
        0.0
    };

    // 磁盘：目标卷取 data_dir（缺省当前目录）；data 目录体积单独递归统计
    let target = data_dir.unwrap_or_else(|| Path::new("."));
    let (disk_free, disk_total) = target
        .ancestors()
        .find_map(|p| {
            // 找到实际存在的最近祖先作为挂载点探测目标
            if p.exists() {
                Some(p)
            } else {
                None
            }
        })
        .and_then(statvfs_free_total)
        .unwrap_or((0, 0));

    ResourceSampleRecord {
        disk_data_bytes: data_dir.map(|d| dir_size(d)).unwrap_or(0),
        disk_free_bytes: disk_free,
        disk_total_bytes: disk_total,
        mem_rss_bytes,
        mem_percent,
        cpu_percent: sys.global_cpu_usage() as f64,
    }
}

#[cfg(unix)]
fn statvfs_free_total(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let free = stat.f_bavail as u64 * stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * stat.f_frsize as u64;
    Some((free, total))
}

#[cfg(not(unix))]
fn statvfs_free_total(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// 后台周期采样器。drop 时随任务句柄终止。
pub struct ResourceSampler {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ResourceSampler {
    /// 启动周期采样；sink 为 None 时返回 None（调用方据此禁用）。
    pub fn start(
        sink: Option<Arc<dyn MetricsSink>>,
        interval_seconds: f64,
        data_dir: Option<String>,
    ) -> Option<Self> {
        let sink = sink?;
        let interval = Duration::from_secs_f64(interval_seconds.max(1.0));
        let data_dir = data_dir.filter(|d| !d.is_empty());
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // 首个 tick 立即触发，跳过后再进入周期
            loop {
                ticker.tick().await;
                // 采集在阻塞线程执行（目录递归可能较慢），上报走旁路
                let dir = data_dir.as_deref().map(std::path::PathBuf::from);
                let record = tokio::task::spawn_blocking(move || collect(dir.as_deref())).await;
                match record {
                    Ok(record) => sink.record_resource_sample(record),
                    Err(e) => tracing::warn!("resource sample join error: {e}"),
                }
            }
        });
        Some(Self { task: Some(task) })
    }

    /// 停止采样任务。
    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for ResourceSampler {
    fn drop(&mut self) {
        self.stop();
    }
}
