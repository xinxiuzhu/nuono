//! 数据目录进程级独占文件锁，贯穿整个服务端生命周期（含启动前的数据恢复阶段）。
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub(crate) struct ServiceLease {
    _file: File,
}

impl ServiceLease {
    pub(crate) fn acquire(data: &Path) -> anyhow::Result<Self> {
        if nuono_core::db::portable::cutover::startup_guard(data).exists() {
            anyhow::bail!(
                "部署切换尚未恢复完成，请通过 nuono-database-transfer recover 或 rollback 处理持久日志"
            );
        }
        std::fs::create_dir_all(data)?;
        if nuono_core::db::portable::pending_operation(data)?
            || data.join(".portable-transfer-pending").exists()
            || data
                .join(nuono_core::db::portable::external::PENDING)
                .exists()
            || data.join(nuono_core::db::portable::purge::PENDING).exists()
        {
            anyhow::bail!(
                "离线数据库迁移尚未完成；请使用原操作日志运行 resume-import、recover 或 purge-recover，核对数据库和资产后再启动，不能直接删除恢复标记"
            );
        }
        let path = data.join(".nuono-service.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            anyhow::anyhow!(
                "无法独占数据目录 {}：可能已有 Nuono 服务运行（{}）",
                data.display(),
                error
            )
        })?;
        // 切勿删除（unlink）该锁文件：其他进程可能已打开同一个文件描述符，删除会导致新进程锁在另一个 inode 上而使互斥失效。
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_host_is_rejected_before_recovery_and_drop_releases_ownership() {
        let data = tempfile::tempdir().unwrap();
        let first = ServiceLease::acquire(data.path()).unwrap();
        assert!(ServiceLease::acquire(data.path()).is_err());
        drop(first);
        let restarted = ServiceLease::acquire(data.path()).unwrap();
        assert!(ServiceLease::acquire(data.path()).is_err());
        drop(restarted);
    }

    #[test]
    fn independent_data_directories_do_not_block_each_other() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let _first = ServiceLease::acquire(first.path()).unwrap();
        let _second = ServiceLease::acquire(second.path().join("data").as_path()).unwrap();
    }

    #[test]
    fn interrupted_portable_import_cannot_start_the_host() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(".portable-transfer-pending"),
            b"snapshot",
        )
        .unwrap();
        assert!(ServiceLease::acquire(directory.path()).is_err());
        assert!(directory.path().join(".portable-transfer-pending").exists());
    }
    #[test]
    fn cutover_guard_blocks_startup_even_between_directory_renames() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let guard = nuono_core::db::portable::cutover::startup_guard(&data);
        std::fs::write(&guard, b"operation").unwrap();
        assert!(ServiceLease::acquire(&data).is_err());
        assert!(!data.exists());
    }
}
