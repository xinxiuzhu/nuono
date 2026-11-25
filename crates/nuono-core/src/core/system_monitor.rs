use std::sync::Arc;
use std::sync::RwLock;

/// 仅静态操作系统详情；无需进程枚举或监视器刷新。
pub fn operating_system_info() -> (Option<String>, Option<String>) {
    (
        sysinfo::System::long_os_version().ok().or_else(|| {
            let name = sysinfo::System::name().ok()?;
            Some(match sysinfo::System::os_version().ok() {
                Some(version) => format!("{name} {version}"),
                None => name,
            })
        }),
        sysinfo::System::kernel_version().ok(),
    )
}

/// 路径所在文件系统的空间；无法识别挂载点时返回 None，避免把未知显示成零。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct DiskSpace {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

pub fn disk_space(path: &std::path::Path) -> Option<DiskSpace> {
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        if !existing.pop() {
            return None;
        }
    }
    let canonical = existing.canonicalize().ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    let disk = disks
        .iter()
        .filter(|disk| canonical.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())?;
    Some(DiskSpace {
        mount: disk.mount_point().display().to_string(),
        total_bytes: disk.total_space(),
        available_bytes: disk.available_space(),
    })
}

/// 系统状态快照
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct SystemStatus {
    pub cpu_percent: f32,
    /// 系统总内存，单位 MiB。保留历史字段名称以兼容现有客户端。
    pub total_memory_mb: u64,
    /// 系统已用内存，单位 MiB。
    pub used_memory_mb: u64,
    /// 当前进程驻留内存（RSS），单位 MiB，含共享及可回收页，并非独占内存。
    pub process_memory_mb: u64,
    /// 系统对当前进程统计的物理内存负担，单位 MiB。
    /// macOS 使用 physical footprint（含压缩内存，排除部分可回收页）；
    /// 不支持或采集失败时为 null，不以 RSS 替代。
    pub process_physical_memory_mb: Option<u64>,
    pub timestamp_secs: u64,
}

impl Default for SystemStatus {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            total_memory_mb: 0,
            used_memory_mb: 0,
            process_memory_mb: 0,
            process_physical_memory_mb: None,
            timestamp_secs: 0,
        }
    }
}

static GLOBAL_MONITOR: std::sync::OnceLock<Arc<SystemMonitor>> = std::sync::OnceLock::new();

/// 初始化全局系统监控（应在程序启动时调用一次）
pub fn init_global_monitor() -> Arc<SystemMonitor> {
    let monitor = SystemMonitor::new();
    let _ = GLOBAL_MONITOR.set(monitor.clone());
    monitor
}

/// 获取全局系统监控实例（host function / API 共用）
pub fn global_monitor() -> Option<Arc<SystemMonitor>> {
    GLOBAL_MONITOR.get().cloned()
}

/// 系统监控服务 — 后台定期刷新，供前端仪表盘和插件共用。
pub struct SystemMonitor {
    status: Arc<RwLock<SystemStatus>>,
}

impl SystemMonitor {
    pub fn new() -> Arc<Self> {
        let status = Arc::new(RwLock::new(SystemStatus::default()));
        let monitor = Arc::new(Self {
            status: status.clone(),
        });

        // 启动后台刷新任务（每 1 秒）
        tokio::spawn(Self::refresh_loop(status));

        monitor
    }

    pub fn get_status(&self) -> SystemStatus {
        self.status.read().unwrap().clone()
    }

    /// 序列化为 JSON 字符串；与状态 API 使用相同内存口径。
    pub fn status_json(&self) -> String {
        let s = self.status.read().unwrap();
        serde_json::json!({
            "cpu_percent": s.cpu_percent,
            "total_memory_mb": s.total_memory_mb,
            "used_memory_mb": s.used_memory_mb,
            "process_memory_mb": s.process_memory_mb,
            "process_physical_memory_mb": s.process_physical_memory_mb,
        })
        .to_string()
    }

    async fn refresh_loop(status: Arc<RwLock<SystemStatus>>) {
        // 仅需系统 CPU/内存及当前进程 RSS，避免读取并长期保留其它进程的
        // 命令行、环境变量、路径等信息。
        let mut sys = match sysinfo::System::new() {
            Ok(sys) => sys,
            Err(error) => {
                tracing::warn!(%error, "初始化系统监控失败，跳过后台资源采集");
                return;
            }
        };
        // 获取一次当前进程 PID，后续循环中复用
        let current_pid = sysinfo::get_current_pid().ok();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            sys.refresh_cpu_all();
            sys.refresh_memory();
            // 刷新当前进程信息（跨平台：Linux / macOS / Windows 均可用）
            if let Some(pid) = current_pid {
                sys.refresh_processes_specifics(
                    sysinfo::ProcessesToUpdate::Some(&[pid]),
                    true,
                    sysinfo::ProcessRefreshKind::nothing()
                        .with_memory()
                        .without_tasks(),
                );
            }

            let total_mem_mb = sys.total_memory() / 1024 / 1024;
            let used_mem_mb = sys.used_memory() / 1024 / 1024;
            let cpu_percent = sys.global_cpu_usage();
            // Process::memory() 返回 RSS 字节数，历史 *_mb 字段实际单位为 MiB。
            let process_mem_mb = current_pid
                .and_then(|pid| sys.process(pid))
                .map(|p| p.memory() / 1024 / 1024)
                .unwrap_or(0);
            let process_physical_mem_mb = process_physical_memory_bytes().map(|n| n / 1024 / 1024);

            let mut s = status.write().unwrap();
            s.cpu_percent = cpu_percent;
            s.total_memory_mb = total_mem_mb;
            s.used_memory_mb = used_mem_mb;
            s.process_memory_mb = process_mem_mb;
            s.process_physical_memory_mb = process_physical_mem_mb;
            s.timestamp_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }
}

#[cfg(target_os = "macos")]
fn process_physical_memory_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
    // SAFETY: getpid() 仅读取当前进程号，无额外前置条件。
    let pid = unsafe { libc::getpid() };
    // SAFETY: pid 指向当前进程；rusage_info_v0 是 repr(C) 缓冲区，其大小与对齐方式
    // 与传入的 RUSAGE_INFO_V0 版本号严格对应（切勿换成 RUSAGE_INFO_CURRENT）。
    // 该缓冲区由当前栈帧独占并在同步系统调用期间保持有效；尽管 libproc 声明的参数类型是 void**，
    // XNU 内核实际上直接把 V0 结构体拷贝到该缓冲区地址，不会将其当作二级指针解引用，也不会在调用后保留指针。
    // 参见 Apple XNU 源码 proc_get_rusage：
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()) };
    if result != 0 {
        return None;
    }
    // Safety: 调用成功返回 0 时，内核已完整填充整个 rusage_info_v0 结构体；
    // 其字段全为 u8/u64 原始整数类型，任意已初始化位模式均合法。若调用失败已在上方提前返回，不会读取未初始化数据。
    Some(unsafe { usage.assume_init() }.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
fn process_physical_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_physical_memory_is_null_and_rss_remains_compatible() {
        let status = SystemStatus {
            process_memory_mb: 459,
            ..Default::default()
        };
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["process_memory_mb"], 459);
        assert!(value["process_physical_memory_mb"].is_null());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_can_read_own_physical_footprint() {
        assert!(process_physical_memory_bytes().is_some_and(|bytes| bytes > 0));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_system_does_not_substitute_rss() {
        assert_eq!(process_physical_memory_bytes(), None);
    }
}
