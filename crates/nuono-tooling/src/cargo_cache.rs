//! 在执行故障恢复或缓存清理前，对 Cargo 的构建产物目录与中间目录加排他锁。
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    fs,
    path::{Path, PathBuf},
};

const LOCK_NAMES: [&str; 2] = [".cargo-lock", ".cargo-build-lock"];
const MAX_ENTRIES: usize = 10_000;

/// 在守卫生命周期内持有所有已发现 Cargo profile 目录的文件锁。
#[derive(Debug)]
pub struct BuildLocks {
    profiles: Vec<PathBuf>,
    _files: Vec<fs::File>,
}

impl BuildLocks {
    /// 当前已锁定产物与中间目录的 profile 路径列表。
    pub fn profiles(&self) -> &[PathBuf] {
        &self.profiles
    }
}

fn lock_present(directory: &Path) -> Result<bool> {
    for name in LOCK_NAMES {
        match fs::symlink_metadata(directory.join(name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("无法检查 Cargo 构建锁"),
        }
    }
    Ok(false)
}

fn profiles(target: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    let mut pending = vec![(target.to_path_buf(), 0)];
    let mut scanned = 0;
    while let Some((directory, depth)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("无法检查 Cargo 构建目录"),
        };
        for entry in entries {
            scanned += 1;
            ensure!(scanned <= MAX_ENTRIES, "Cargo 构建目录过多，已停止操作");
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "构建缓存含符号链接，已停止操作：{}",
                entry.path().display()
            );
            if !kind.is_dir() {
                continue;
            }
            let path = entry.path();
            if lock_present(&path)? {
                result.push(path);
            } else if depth == 0 {
                // Cargo 的公开产物目录结构为 `<profile>` 或 `<target>/<profile>`。
                // 直接通过锁文件标识识别各编译 profile（包括自定义 profile），无需深入扫描具体的编译单元目录。
                pending.push((path, 1));
            }
        }
    }
    result.sort();
    Ok(result)
}

/// 获取 target 下所有已存在 profile（含指定 target 三元组子目录）的 Cargo 构建锁。
///
/// # Errors
/// 当存在正在运行的构建、发现符号链接、目录不可访问或超出目录扫描上限时返回错误。
pub fn lock_build_directories(target: &Path) -> Result<BuildLocks> {
    let profiles = profiles(target)?;
    let mut files = Vec::new();
    for profile in &profiles {
        for name in LOCK_NAMES {
            let path = profile.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) => ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "构建锁文件不是普通文件：{}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("无法检查 Cargo 构建锁"),
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("无法读取构建锁：{}", path.display()))?;
            file.try_lock_exclusive()
                .with_context(|| format!("构建正在运行，请完成后再操作：{}", path.display()))?;
            files.push(file);
        }
    }
    Ok(BuildLocks {
        profiles,
        _files: files,
    })
}

#[cfg(test)]
#[path = "../tests/unit/cargo_cache.rs"]
mod tests;
