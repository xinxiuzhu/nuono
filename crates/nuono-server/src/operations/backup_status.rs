//! 备份健康度评估：仅以磁盘上实际落盘发布的有效备份文件为准，不将「未到期而空转跳过」的维护周期算作已备份。
use crate::backup::{self, BackupFormat};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{io, path::Path};

#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
pub struct BackupHealth {
    pub auto_enabled: Option<bool>,
    pub last_success_at: Option<String>,
    pub next_due_at: Option<String>,
    /// 备份健康状态：`unknown`、`missing`、`overdue`、`awaiting_auto`、`ready` 或 `manual`。
    pub status: String,
}

pub(super) fn inspect(
    data: &Path,
    auto_enabled: bool,
    sqlite: bool,
    now: DateTime<Utc>,
) -> io::Result<BackupHealth> {
    let directory = data.join(backup::BACKUP_DIR);
    let mut latest = None::<DateTime<Utc>>;
    let mut latest_auto = None::<DateTime<Utc>>;
    match std::fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(io::Error::other("备份目录不是普通目录"));
            }
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                if !backup::validate_backup_filename(&entry.file_name().to_string_lossy())
                    || !entry.file_type()?.is_file()
                {
                    continue;
                }
                let compatible = matches!(
                    (backup::detect_backup_format(&entry.path()), sqlite),
                    (Ok(BackupFormat::Sqlite), true) | (Ok(BackupFormat::PostgresCustom), false)
                );
                if compatible {
                    let modified = DateTime::<Utc>::from(entry.metadata()?.modified()?);
                    if modified > now + chrono::Duration::minutes(5) {
                        return Err(io::Error::other("备份文件时间晚于服务器时间"));
                    }
                    latest = Some(latest.map_or(modified, |old| old.max(modified)));
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(backup::AUTO_BACKUP_PREFIX)
                    {
                        latest_auto = Some(latest_auto.map_or(modified, |old| old.max(modified)));
                    }
                }
            }
        }
    }
    let due = latest_auto.map(|time| time + chrono::Duration::hours(24));
    // 自动备份维护循环每小时运行一次；预留 1 小时调度窗口，超时后才判定为逾期（overdue）。
    let overdue = auto_enabled && due.is_some_and(|due| now > due + chrono::Duration::hours(1));
    Ok(BackupHealth {
        auto_enabled: Some(auto_enabled),
        last_success_at: latest.map(|time| time.to_rfc3339()),
        next_due_at: due.filter(|_| auto_enabled).map(|time| time.to_rfc3339()),
        status: if latest.is_none() {
            "missing"
        } else if overdue {
            "overdue"
        } else if auto_enabled && latest_auto.is_none() {
            "awaiting_auto"
        } else if auto_enabled {
            "ready"
        } else {
            "manual"
        }
        .into(),
    })
}
