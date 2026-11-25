//! 独立于可恢复数据库快照之外的持久化无正文隐私删除日志（防止备份回滚后已删除的个人数据复活）。

use crate::db::Database;
use crate::error::{NuonoError, Result};
use nuono_app::companion_data::CompanionDeletion;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const DIRECTORY: &str = ".companion-deletions";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// 仅追加写入的隐私删除日志，其记录在数据库备份恢复或数据迁移回滚后依然保留生效。
#[derive(Debug, Clone)]
pub struct PrivacyJournal {
    directory: PathBuf,
}

impl PrivacyJournal {
    /// 将隐私删除日志存放在运行时数据目录下，绝不打入数据库备份包中。
    pub fn new(data_dir: &Path) -> Self {
        Self {
            directory: data_dir.join(DIRECTORY),
        }
    }

    /// 在执行数据库删除事务之前，先将隐私删除记录持久化写入日志。
    ///
    /// 相同条目的幂等重试是安全的，但同一 ID 携带不同的删除范围参数会直接报错；
    /// 一旦写入日志失败，调用方绝不能对外宣告隐私删除已完成。
    pub async fn append(&self, entry: &CompanionDeletion) -> Result<()> {
        entry.validate()?;
        let bytes = serde_json::to_vec(entry).map_err(NuonoError::from)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(NuonoError::InvalidInput("删除记录超过大小限制".into()));
        }
        let directory = self.directory.clone();
        let id = entry.id.clone();
        tokio::task::spawn_blocking(move || append_record(&directory, &id, &bytes))
            .await
            .map_err(|_| NuonoError::Internal("删除日志写入任务中断".into()))?
    }

    /// 检查日志中是否尚未包含任何持久化隐私删除记录。
    pub async fn is_empty(&self) -> Result<bool> {
        Ok(self.entries().await?.is_empty())
    }

    /// 在进程启动或完成受管备份恢复后、正式放行流量前，重放所有隐私删除记录。
    ///
    /// 重放成功后日志条目仍继续保留，以防后续再次恢复更早的旧备份快照。
    pub async fn replay(&self, db: &Database) -> Result<()> {
        for entry in self.entries().await? {
            db.apply_companion_deletion(&entry).await?;
        }
        Ok(())
    }

    async fn entries(&self) -> Result<Vec<CompanionDeletion>> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || read_entries(&directory))
            .await
            .map_err(|_| NuonoError::Internal("删除日志读取任务中断".into()))?
    }
}

fn failure(path: &Path, error: impl std::fmt::Display) -> NuonoError {
    NuonoError::FileWrite {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

fn plain_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory).map_err(|error| failure(directory, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(failure(directory, "删除日志必须是普通目录"));
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| failure(directory, error))?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn append_record(directory: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    match fs::create_dir(directory) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                    .map_err(|error| failure(directory, error))?;
            }
            if let Some(parent) = directory.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failure(directory, error)),
    }
    plain_directory(directory)?;
    let target = directory.join(format!("{id}.json"));
    let pending = directory.join(format!(".pending-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&pending)
            .map_err(|error| failure(&pending, error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| failure(&pending, error))?;
        match fs::hard_link(&pending, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_record_bytes(&target)? != bytes {
                    return Err(failure(&target, "删除记录编号已被不同内容使用"));
                }
            }
            Err(error) => return Err(failure(&target, error)),
        }
        sync_directory(directory)
    })();
    let _ = fs::remove_file(pending);
    result
}

fn read_record_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| failure(path, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_RECORD_BYTES
    {
        return Err(failure(path, "删除记录文件类型或大小无效"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| failure(path, error))?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| failure(path, error))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(failure(path, "删除记录过大"));
    }
    Ok(bytes)
}

fn read_entries(directory: &Path) -> Result<Vec<CompanionDeletion>> {
    match fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(failure(directory, error)),
        Ok(_) => plain_directory(directory)?,
    }
    let mut entries = Vec::new();
    for file in fs::read_dir(directory).map_err(|error| failure(directory, error))? {
        let file = file.map_err(|error| failure(directory, error))?;
        let name = file.file_name();
        if name.to_string_lossy().starts_with(".pending-") {
            continue;
        }
        let path = file.path();
        let entry: CompanionDeletion = serde_json::from_slice(&read_record_bytes(&path)?)
            .map_err(|_| failure(&path, "删除日志损坏，已停止恢复"))?;
        entry.validate()?;
        if name != std::ffi::OsStr::new(&format!("{}.json", entry.id)) {
            return Err(failure(&path, "删除记录编号与文件名不一致"));
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| {
        left.through
            .cmp(&right.through)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

#[cfg(test)]
#[path = "../../tests/unit/service/privacy_journal.rs"]
mod tests;
