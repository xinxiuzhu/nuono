use super::{AppState, OperationsMonitor};
use fs2::FileExt;
use nuono_core::core::system_monitor::{DiskSpace, disk_space};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 24;
const MAX_SCAN: Duration = Duration::from_secs(3);
const CACHE_SECONDS: u64 = 60;
const REBUILDABLE: &[&str] = &["deps", "build", "incremental", ".fingerprint", "examples"];

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DiskCategory {
    pub id: String,
    pub label: String,
    pub bytes: u64,
    pub files: usize,
    pub partial: bool,
    pub note: Option<String>,
}
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DiskSnapshot {
    pub checked_at: String,
    pub volumes: Vec<DiskSpace>,
    pub categories: Vec<DiskCategory>,
    pub clean_available: bool,
    pub clean_unavailable_reason: Option<String>,
    #[serde(skip)]
    #[schema(ignore)]
    pub scanned: Option<Instant>,
}
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CleanResult {
    pub removed_directories: usize,
    pub message: String,
}

pub(super) async fn snapshot(
    monitor: &OperationsMonitor,
    state: &AppState,
) -> Result<DiskSnapshot, String> {
    let mut cache = monitor.disk.clone().lock_owned().await;
    if let Some(value) = cache.as_ref().filter(|value| {
        value
            .scanned
            .is_some_and(|t| t.elapsed().as_secs() < CACHE_SECONDS)
    }) {
        return Ok(value.clone());
    }
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let snapshot = scan(&paths);
        *cache = Some(snapshot.clone());
        snapshot
    })
    .await
    .map_err(|_| "磁盘扫描未完成，请稍后重试".to_string())
}

struct Budget {
    started: Instant,
    entries: usize,
}
impl Budget {
    fn exhausted(&self) -> bool {
        self.entries >= MAX_ENTRIES || self.started.elapsed() >= MAX_SCAN
    }
}
fn category(id: &str, label: &str, paths: Vec<PathBuf>) -> DiskCategory {
    let mut result = DiskCategory {
        id: id.into(),
        label: label.into(),
        bytes: 0,
        files: 0,
        partial: false,
        note: None,
    };
    let mut budget = Budget {
        started: Instant::now(),
        entries: 0,
    };
    let mut stack: Vec<_> = paths.into_iter().map(|p| (p, 0)).collect();
    while let Some((path, depth)) = stack.pop() {
        if budget.exhausted() || depth > MAX_DEPTH {
            result.partial = true;
            break;
        }
        budget.entries += 1;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                result.partial = true;
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            result.partial = true;
            continue;
        }
        if meta.is_file() {
            result.bytes = result.bytes.saturating_add(meta.len());
            result.files += 1;
        } else if meta.is_dir() {
            let Ok(entries) = fs::read_dir(path) else {
                result.partial = true;
                continue;
            };
            for entry in entries {
                if budget.exhausted() || stack.len() >= MAX_ENTRIES {
                    result.partial = true;
                    break;
                }
                budget.entries += 1;
                match entry {
                    Ok(entry) => stack.push((entry.path(), depth + 1)),
                    Err(_) => result.partial = true,
                }
            }
        }
    }
    if result.partial {
        result.note = Some(
            "只统计可访问的普通文件；扫描达到上限或遇到链接/权限限制，当前为已统计大小。".into(),
        );
    }
    result
}

fn scan(paths: &nuono_core::core::Paths) -> DiskSnapshot {
    let target =
        crate::source_build::target_dir(&paths.root).unwrap_or_else(|_| paths.root.join("target"));
    let mut volumes = Vec::new();
    for path in [&paths.root, &paths.data, &paths.logs, &target] {
        if let Some(volume) = disk_space(path)
            && !volumes.iter().any(|v: &DiskSpace| v.mount == volume.mount)
        {
            volumes.push(volume);
        }
    }
    let mut assets = vec![paths.hub()];
    if let Ok(entries) = fs::read_dir(&paths.data) {
        for entry in entries.take(10_000).flatten() {
            if entry.file_name() != "hub" && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                assets.push(entry.path().join("assets"));
            }
        }
    }
    let check = clean_target(&paths.root);
    DiskSnapshot {
        checked_at: chrono::Utc::now().to_rfc3339(),
        volumes,
        categories: vec![
            category("build", "构建产物", vec![target]),
            category("logs", "日志文件", vec![paths.logs.clone()]),
            category(
                "backups",
                "数据库与实例备份",
                vec![paths.data.join("backup")],
            ),
            category("assets", "插件素材与管理界面资产", assets),
        ],
        clean_available: check.is_ok(),
        clean_unavailable_reason: check.err(),
        scanned: Some(Instant::now()),
    }
}

fn no_symlink_directory(path: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(path).map_err(|_| "构建缓存目录不存在或不可访问".to_string())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("构建缓存必须是实际目录，不能清理符号链接".into());
    }
    Ok(())
}
fn clean_target(root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|_| "无法定位应用目录".to_string())?;
    if !root.join("crates/nuono-server/Cargo.toml").is_file() {
        return Err("当前为部署目录，没有可清理的框架构建缓存".into());
    }
    let target =
        crate::source_build::target_dir(&root).map_err(|_| "无法定位构建缓存".to_string())?;
    no_symlink_directory(&target)?;
    let canonical = target
        .canonicalize()
        .map_err(|_| "无法定位构建缓存".to_string())?;
    if canonical != root.join("target") {
        return Err("外部或自定义 target 可能与其他项目共用，请在服务器手动清理".into());
    }
    Ok(canonical)
}
fn acquire(path: &Path) -> Result<fs::File, String> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink() || !meta.is_file())
    {
        return Err("构建锁文件不是普通文件，已取消清理".into());
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "无法读取构建锁，已取消清理".to_string())?;
    lock.try_lock_exclusive()
        .map_err(|_| "构建正在运行，请完成后再清理缓存".to_string())?;
    Ok(lock)
}

pub(super) fn clean(root: &Path) -> Result<CleanResult, String> {
    let target = clean_target(root)?;
    let generated = target.join("nuono-app");
    if !generated.exists() {
        fs::create_dir(&generated).map_err(|_| "无法取得构建锁".to_string())?;
    }
    no_symlink_directory(&generated)?;
    let _application_lock = acquire(&generated.join("build.lock"))?;
    let cargo_locks = nuono_tooling::cargo_cache::lock_build_directories(&target)
        .map_err(|error| format!("清理未执行：{error:#}"))?;
    // 先取得所有 Cargo profile 锁，再删除任何文件，避免部分清理后才发现构建进行中。
    let mut candidates = Vec::new();
    for folder in cargo_locks.profiles() {
        no_symlink_directory(folder)?;
        for name in REBUILDABLE {
            let candidate = folder.join(name);
            if candidate.exists() {
                no_symlink_directory(&candidate)?;
                candidates.push(candidate);
            }
        }
    }
    let mut removed = 0;
    for candidate in candidates {
        // remove_dir_all 不遍历内部符号链接；根、profile 与候选目录已逐层校验。
        fs::remove_dir_all(candidate)
            .map_err(|_| "部分缓存未能删除，请检查文件权限后刷新占用".to_string())?;
        removed += 1;
    }
    Ok(CleanResult {
        removed_directories: removed,
        message:
            "构建缓存已清理。现有程序、配置、插件源码和业务数据均已保留；下次构建会重新生成依赖。"
                .into(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/operations/disk.rs"]
mod tests;
