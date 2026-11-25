//! 聊天命令和管理网页共用的运行时维护。
use crate::{backup, source_build, startup_progress};
use nuono_core::{
    core::{FrameworkControl, Paths},
    core_commands::{PreparedRestart, RestartReplyTarget, RestartService},
    db::Database,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(crate) struct ArtifactMaintenance {
    cancelled: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ArtifactMaintenance {
    pub(crate) fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub(crate) async fn stop(mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ArtifactMaintenance {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

use nuono_tooling::launcher::managed_source_entry;

/// HTTP 监听就绪后，在后台闲时检查并清理旧的历史构建候选产物。
/// 这只是后台清理任务：每次启动时仍会严格校验当前选中的运行二进制。
pub(crate) fn start_artifact_maintenance(paths: &Paths) -> Option<ArtifactMaintenance> {
    let root = paths
        .root
        .canonicalize()
        .ok()
        .and_then(|root| crate::find_nuono_repository_root(&root))?;
    let executable = source_build::running_executable().or_else(|| std::env::current_exe().ok())?;
    if !managed_source_entry(&executable, &source_build::target_dir(&root).ok()?) {
        return None;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let task = tokio::spawn(async move {
        let blocking_cancelled = worker_cancelled.clone();
        let result = tokio::task::spawn_blocking(move || {
            maintain_artifacts_when_idle(&root, source_build::operation_lock(), &|| {
                blocking_cancelled.load(Ordering::Acquire)
            })
        })
        .await;
        match result {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) if worker_cancelled.load(Ordering::Acquire) => {}
            Ok(Ok(false)) => tracing::debug!("源码操作正在运行，暂缓历史构建产物维护"),
            Ok(Err(error)) => tracing::warn!(%error, "保留无法校验的历史构建产物"),
            Err(error) => tracing::warn!(%error, "历史构建产物后台维护未完成"),
        }
    });
    Some(ArtifactMaintenance {
        cancelled,
        task: Some(task),
    })
}

fn maintain_artifacts_when_idle(
    root: &Path,
    operation: Arc<Mutex<()>>,
    cancelled: &impl Fn() -> bool,
) -> anyhow::Result<bool> {
    if cancelled() {
        return Ok(false);
    }
    let Ok(_operation) = operation.try_lock_owned() else {
        return Ok(false);
    };
    let Some(build) = source_build::try_application_lock(root)? else {
        return Ok(false);
    };
    let stage = startup_progress::Stage::new("历史构建产物后台维护");
    let completed = source_build::maintain_artifacts_cancellable(root, &build.target, cancelled)?;
    if completed {
        stage.finish();
    } else if cancelled() {
        stage.cancel();
    }
    Ok(completed)
}

pub(crate) struct ServerRestartService {
    data: PathBuf,
    source_root: Option<PathBuf>,
    control: FrameworkControl,
    operation_lock: Arc<Mutex<()>>,
    receipt_db: Option<Database>,
}

struct RestartGuards {
    _operation: OwnedMutexGuard<()>,
    _build: Option<source_build::ApplicationBuildLock>,
}

impl ServerRestartService {
    pub(crate) fn new(paths: &Paths, control: FrameworkControl) -> Self {
        let source_root = paths
            .root
            .canonicalize()
            .ok()
            .and_then(|root| crate::find_nuono_repository_root(&root));
        Self {
            data: paths.data.clone(),
            source_root,
            control,
            operation_lock: source_build::operation_lock(),
            receipt_db: None,
        }
    }

    pub(crate) fn with_restart_receipts(mut self, db: Database) -> Self {
        self.receipt_db = Some(db);
        self
    }

    async fn prepare(&self) -> Result<RestartGuards, String> {
        let operation = self
            .operation_lock
            .clone()
            .try_lock_owned()
            .map_err(|_| "还有源码操作、恢复安排或缓存维护正在运行，请稍后操作".to_string())?;
        if self.control.last_signal().is_some() {
            return Err("服务已经在停止或重启，请等待操作完成".into());
        }
        // 数据恢复安排同样受 `operation_lock` 互斥保护；在持锁期间检查待恢复标记，
        // 可防止在「前置检查通过」到「发出停机信号」之间插入新的恢复任务。
        if backup::transfer::is_pending(&self.data)
            || std::fs::symlink_metadata(self.data.join(backup::RESTORE_MARKER)).is_ok()
        {
            return Err("已有数据恢复安排，请按恢复流程重启".into());
        }
        let root = self.source_root.clone();
        tokio::task::spawn_blocking(move || {
            let build = match root {
                Some(root) => Some(
                    source_build::try_application_lock(&root)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "还有源码构建正在运行，请稍后操作".to_string())?,
                ),
                None => None,
            };
            Ok(RestartGuards {
                _operation: operation,
                _build: build,
            })
        })
        .await
        .map_err(|e| format!("检查维护条件失败: {e}"))?
    }

    /// 停机与重启同样需要排斥源码构建和数据恢复操作，但停机不要求必须处于受管进程（supervisor）下。
    pub(crate) async fn shutdown(&self, reason: &str) -> Result<(), String> {
        let guards = self.prepare().await?;
        let control = self.control.clone();
        let reason = reason.to_string();
        tokio::spawn(async move {
            let _guards = guards;
            control.request_shutdown(reason);
            // 与重启一致，进程停止前继续占有维护锁，避免重复提交和新任务。
            std::future::pending::<()>().await;
        });
        Ok(())
    }

    async fn reserve(
        &self,
        reason: &str,
        supervised: bool,
        reply: Option<RestartReplyTarget>,
    ) -> Result<PreparedRestart, String> {
        if !supervised {
            return Err(
                "当前没有声明进程托管，请在终端手动重启 Nuono，避免服务退出后无法自动启动。".into(),
            );
        }
        let guards = self.prepare().await?;
        let control = self.control.clone();
        let reason = reason.to_string();
        let db = self.receipt_db.clone();
        Ok(PreparedRestart::new(move || async move {
            // 一旦开始提交重启，放入独立任务执行，避免调用方中途取消导致「已落库保存重启回执，却未发出重启信号」。
            tokio::spawn(async move {
                if let (Some(db), Some(reply)) = (db, reply) {
                    crate::restart_receipt::record(&db, reply)
                        .await
                        .map_err(|e| format!("保存重启完成通知失败：{e}"))?;
                }
                tokio::spawn(async move {
                    let _guards = guards;
                    control.request_restart(reason);
                    // 在进程真正退出前持续持有维护锁，防止停机期间再插入新任务。
                    std::future::pending::<()>().await;
                });
                Ok(())
            })
            .await
            .map_err(|e| format!("提交重启失败：{e}"))?
        }))
    }
}

#[async_trait::async_trait]
impl RestartService for ServerRestartService {
    async fn prepare_restart(
        &self,
        reason: &str,
        reply: Option<RestartReplyTarget>,
    ) -> Result<PreparedRestart, String> {
        self.reserve(
            reason,
            std::env::var("NUONO_SUPERVISED").is_ok_and(|v| v == "1"),
            reply,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn service(root: &std::path::Path) -> ServerRestartService {
        let mut service =
            ServerRestartService::new(&Paths::with_root(root), FrameworkControl::new());
        service.operation_lock = Arc::new(Mutex::new(()));
        service
    }

    #[tokio::test]
    async fn background_artifact_maintenance_never_interferes_with_source_operations() {
        let directory = tempfile::tempdir().unwrap();
        let operation = Arc::new(Mutex::new(()));
        let guard = operation.lock().await;
        assert!(
            !maintain_artifacts_when_idle(directory.path(), operation.clone(), &|| false).unwrap()
        );
        assert!(!directory.path().join("target").exists());
        drop(guard);

        let build = source_build::application_lock(directory.path()).unwrap();
        assert!(
            !maintain_artifacts_when_idle(directory.path(), operation.clone(), &|| false).unwrap()
        );
        assert!(operation.try_lock().is_ok());
        drop(build);

        assert!(
            maintain_artifacts_when_idle(directory.path(), operation.clone(), &|| false).unwrap()
        );
        assert!(operation.try_lock().is_ok());
        assert!(
            source_build::try_application_lock(directory.path())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn exported_programs_cannot_maintain_source_artifacts_in_a_lookalike_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("crates/nuono-server")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/nuono-server\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/nuono-server/Cargo.toml"),
            "[package]\nname=\"nuono-server\"\n",
        )
        .unwrap();
        assert!(crate::find_nuono_repository_root(root).is_some());
        let target = root.join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::create_dir_all(root.join("export")).unwrap();
        for name in ["nuono", "nuono-application"] {
            let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            std::fs::write(target.join("debug").join(&name), b"source program").unwrap();
            std::fs::write(root.join("export").join(&name), b"exported program").unwrap();
            assert!(managed_source_entry(
                &target.join("debug").join(&name),
                &target
            ));
            assert!(!managed_source_entry(
                &root.join("export").join(&name),
                &target
            ));
        }
        std::fs::write(
            target.join("debug").join(format!(
                "nuono-application-candidate{}",
                std::env::consts::EXE_SUFFIX
            )),
            b"candidate program",
        )
        .unwrap();
        assert!(!managed_source_entry(
            &target.join("debug").join(format!(
                "nuono-application-candidate{}",
                std::env::consts::EXE_SUFFIX
            )),
            &target
        ));
    }

    #[tokio::test]
    async fn cancelled_background_scan_releases_operation_and_build_locks() {
        let directory = tempfile::tempdir().unwrap();
        let operation = Arc::new(Mutex::new(()));
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };
        assert!(
            !maintain_artifacts_when_idle(directory.path(), operation.clone(), &cancelled).unwrap()
        );
        assert!(operation.try_lock().is_ok());
        assert!(
            source_build::try_application_lock(directory.path())
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn shutdown_refuses_active_work_and_pending_restore_without_signalling() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        let operation = service.operation_lock.lock().await;
        assert!(
            service
                .shutdown("test")
                .await
                .unwrap_err()
                .contains("正在运行")
        );
        drop(operation);
        std::fs::create_dir_all(&service.data).unwrap();
        let marker = service.data.join(backup::RESTORE_MARKER);
        std::fs::write(&marker, "pending").unwrap();
        assert!(
            service
                .shutdown("test")
                .await
                .unwrap_err()
                .contains("恢复安排")
        );
        std::fs::remove_file(marker).unwrap();
        let transfer = backup::transfer::directory(&service.data);
        std::fs::create_dir_all(&transfer).unwrap();
        std::fs::write(transfer.join("pending.json"), "{}").unwrap();
        assert!(
            service
                .shutdown("test")
                .await
                .unwrap_err()
                .contains("恢复安排")
        );
        assert!(service.control.last_signal().is_none());
        assert!(service.operation_lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn shutdown_sends_stop_once_and_reserves_maintenance_until_exit() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        let mut signals = service.control.subscribe();
        service.shutdown("dashboard shutdown").await.unwrap();
        let signal = tokio::time::timeout(Duration::from_secs(3), signals.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!signal.is_restart());
        assert_eq!(signal.reason(), "dashboard shutdown");
        assert!(service.operation_lock.try_lock().is_err());
        assert!(service.shutdown("duplicate").await.is_err());
        assert!(service.reserve("restart", true, None).await.is_err());
        assert!(signals.try_recv().is_err());
    }

    #[tokio::test]
    async fn restart_rejects_unmanaged_process_and_maintenance_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        assert!(
            service
                .reserve("test", false, None)
                .await
                .unwrap_err()
                .contains("进程托管")
        );
        let _operation = service.operation_lock.lock().await;
        assert!(
            service
                .reserve("test", true, None)
                .await
                .unwrap_err()
                .contains("正在运行")
        );
        assert!(service.control.last_signal().is_none());
    }

    #[tokio::test]
    async fn restart_rejects_pending_restore_and_transfer() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        std::fs::create_dir_all(&service.data).unwrap();
        let marker = service.data.join(backup::RESTORE_MARKER);
        std::fs::write(&marker, "pending").unwrap();
        assert!(
            service
                .reserve("test", true, None)
                .await
                .unwrap_err()
                .contains("恢复安排")
        );
        std::fs::remove_file(marker).unwrap();
        let transfer = backup::transfer::directory(&service.data);
        std::fs::create_dir_all(&transfer).unwrap();
        std::fs::write(transfer.join("pending.json"), "{}").unwrap();
        assert!(
            service
                .reserve("test", true, None)
                .await
                .unwrap_err()
                .contains("恢复安排")
        );
        assert!(service.control.last_signal().is_none());
    }

    #[tokio::test]
    async fn exported_runtime_schedules_restart_without_source_and_keeps_operation_lock() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        assert!(service.source_root.is_none());
        let mut signals = service.control.subscribe();
        let reservation = service
            .reserve("chat maintenance", true, None)
            .await
            .unwrap();
        assert!(service.control.last_signal().is_none());
        assert!(service.operation_lock.try_lock().is_err());
        assert!(
            service
                .reserve("duplicate request", true, None)
                .await
                .is_err()
        );
        reservation.commit().await.unwrap();
        let signal = tokio::time::timeout(Duration::from_secs(3), signals.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(signal.is_restart());
        assert_eq!(signal.reason(), "chat maintenance");
        assert!(service.operation_lock.try_lock().is_err());
        assert!(service.reserve("after signal", true, None).await.is_err());
        assert!(!directory.path().join("target").exists());
    }

    #[tokio::test]
    async fn dropping_restart_reservation_releases_maintenance_without_stopping() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(directory.path());
        let reservation = service
            .reserve("cancelled caller", true, None)
            .await
            .unwrap();
        assert!(service.operation_lock.try_lock().is_err());
        drop(reservation);
        assert!(service.operation_lock.try_lock().is_ok());
        assert!(service.control.last_signal().is_none());
    }

    #[tokio::test]
    async fn receipt_is_durable_before_restart_signal_and_storage_failure_cancels_restart() {
        use nuono_core::domain::{
            ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform,
        };
        let directory = tempfile::tempdir().unwrap();
        let db = nuono_core::test_support::test_db(2).await;
        let route = nuono_core::core::RuntimeBotRoute::new("onebot", "fixture", "42");
        let reply = RestartReplyTarget {
            bot: nuono_plugin::core::runtime_transport::route_identity(&route).unwrap(),
            route,
            conversation: ConversationRef::direct(
                IdentityRef::new(
                    Platform::OneBot,
                    IdentityKind::User,
                    IdentityScheme::Native,
                    "123",
                    None::<String>,
                )
                .unwrap(),
                None,
            )
            .unwrap(),
        };
        let running = service(directory.path()).with_restart_receipts(db.clone());
        drop(
            running
                .reserve("cancelled caller", true, Some(reply.clone()))
                .await
                .unwrap(),
        );
        assert!(crate::restart_receipt::load(&db).await.is_none());
        let mut signals = running.control.subscribe();
        running
            .reserve("receipt test", true, Some(reply.clone()))
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), signals.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(crate::restart_receipt::load(&db).await.is_some());
        db.shutdown().await.unwrap();
        let failed = service(directory.path()).with_restart_receipts(db);
        assert!(
            failed
                .reserve("failed write", true, Some(reply))
                .await
                .unwrap()
                .commit()
                .await
                .is_err()
        );
        assert!(failed.control.last_signal().is_none());
        assert!(failed.operation_lock.try_lock().is_ok());
    }
}
