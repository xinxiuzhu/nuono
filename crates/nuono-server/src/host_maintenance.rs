//! 宿主进程级周期性后台维护任务（记忆整理、历史压缩、过期会话与令牌清理、自动备份、审计日志留存清理）。
use crate::{background_tasks::HostBackgroundTasks, backup};
use nuono_core::{
    config::ConfigManager,
    core::{ConnectionManager, Paths},
    db::Database,
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub(crate) fn start(
    tasks: &mut HostBackgroundTasks,
    manager: &Arc<ConnectionManager>,
    db: &Database,
    config: &Arc<ConfigManager>,
    paths: &Paths,
    backup_lock: Arc<Mutex<()>>,
) {
    let stop = tasks.token();
    let runtime = manager.event_kernel.agent_runtime.clone();
    let memory_business = manager.clone();
    tasks.spawn(
        "memory-maintenance",
        periodic(
            stop.clone(),
            Duration::ZERO,
            Duration::from_secs(60),
            move || {
                let runtime = runtime.clone();
                let stop = stop.clone();
                let business = memory_business.clone();
                async move {
                    if !business.business_is_running() {
                        return;
                    }
                    // 记忆整理支持在收到停机信号时随时中断退出。
                    if let Some(Err(error)) =
                        until_cancelled(&stop, runtime.maintain_memories(true)).await
                    {
                        warn!(%error, "闲时记忆整理失败，稍后重试");
                    }
                }
            },
        ),
    );

    let stop = tasks.token();
    let assistant = manager.event_kernel.assistant.clone();
    let companion_business = manager.clone();
    let retention_cursor = Arc::new(Mutex::new(None::<String>));
    tasks.spawn(
        "companion-retention",
        periodic(
            stop.clone(),
            Duration::from_secs(60),
            Duration::from_secs(60),
            move || {
                let assistant = assistant.clone();
                let stop = stop.clone();
                let cursor = retention_cursor.clone();
                let business = companion_business.clone();
                async move {
                    if !business.business_is_running() {
                        return;
                    }
                    let mut cursor = cursor.lock().await;
                    if let Some(Err(error)) =
                        until_cancelled(&stop, assistant.maintain_companion_history(&mut cursor))
                            .await
                    {
                        warn!(
                            error_kind = error.kind(),
                            "个人历史压缩暂未完成，保留原始内容"
                        );
                    }
                }
            },
        ),
    );

    let wait_manager = manager.wait_manager.clone();
    let matcher_sessions = manager.session_manager.clone();
    let interactive_sessions = nuono_core::api::session::global_session_manager();
    tasks.spawn(
        "session-cleanup",
        periodic(
            tasks.token(),
            Duration::ZERO,
            Duration::from_secs(60),
            move || {
                let wait_manager = wait_manager.clone();
                let matcher_sessions = matcher_sessions.clone();
                let interactive_sessions = interactive_sessions.clone();
                async move {
                    wait_manager.cleanup_expired();
                    matcher_sessions.cleanup_expired();
                    interactive_sessions.cleanup_expired(30 * 60).await;
                }
            },
        ),
    );

    let cleanup_db = db.clone();
    tasks.spawn(
        "mail-token-cleanup",
        periodic(
            tasks.token(),
            Duration::ZERO,
            Duration::from_secs(600),
            move || {
                let db = cleanup_db.clone();
                async move {
                    if let Err(error) = db.delete_expired_plugin_state(1000).await {
                        warn!(%error,"清理到期插件状态失败");
                    }
                    match db.delete_stale_user_tokens().await {
                        Ok(deleted) if deleted > 0 => info!(count = deleted, "过期邮件令牌已清理"),
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "清理过期邮件令牌失败"),
                    }
                    match db.delete_stale_registration_otps().await {
                        Ok(deleted) if deleted > 0 => {
                            info!(count = deleted, "过期注册验证码已清理")
                        }
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "清理过期注册验证码失败"),
                    }
                    match db.delete_stale_account_challenges().await {
                        Ok(deleted) if deleted > 0 => {
                            info!(count = deleted, "过期账号绑定请求已清理")
                        }
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "清理过期账号绑定请求失败"),
                    }
                }
            },
        ),
    );

    let stop = tasks.token();
    let backup_config = config.clone();
    let backup_data = paths.data.clone();
    tasks.spawn(
        "automatic-backup",
        periodic(
            stop.clone(),
            Duration::from_secs(30),
            Duration::from_secs(60 * 60),
            move || {
                let stop = stop.clone();
                let config = backup_config.clone();
                let data = backup_data.clone();
                let lock = backup_lock.clone();
                async move {
                    let Some(cfg) = until_cancelled(&stop, config.snapshot()).await else {
                        return;
                    };
                    if !cfg.backup.enabled {
                        return;
                    }
                    locked_round(&stop, &lock, || async {
                        // 一旦进入备份流程，绝不能用 select 中途取消该 Future：
                        // 必须等待 PostgreSQL 的 `pg_dump` 子进程或 SQLite 的备份连接完整结束后才能释放备份互斥锁。
                        let result = backup::run_backup_cycle(
                            &cfg.database.url,
                            &data,
                            cfg.backup.retention_days.max(1),
                        )
                        .await;
                        crate::operations::record_backup_result(
                            &data,
                            result.as_ref().map(|_| ()).map_err(ToString::to_string),
                        )
                        .await;
                        match result {
                            Ok(backup::BackupOutcome::Created(path)) => {
                                info!(path = %path.display(), "🗄️ 自动备份循环完成")
                            }
                            Ok(backup::BackupOutcome::CreatedAndPruned(path, pruned)) => {
                                info!(path = %path.display(), pruned, "🗄️ 自动备份循环完成")
                            }
                            Ok(backup::BackupOutcome::Pruned(pruned)) => {
                                info!(pruned, "🗑️ 自动备份清理完成")
                            }
                            Ok(_) => {}
                            Err(e) => warn!(error = %e, "自动备份循环失败"),
                        }
                    })
                    .await;
                }
            },
        ),
    );

    let stop = tasks.token();
    let audit_db = db.clone();
    let audit_config = config.clone();
    tasks.spawn(
        "audit-retention",
        periodic(
            stop.clone(),
            Duration::from_secs(5 * 60),
            Duration::from_secs(24 * 60 * 60),
            move || {
                let stop = stop.clone();
                let db = audit_db.clone();
                let config = audit_config.clone();
                async move {
                    let Some(cfg) = until_cancelled(&stop, config.snapshot()).await else {
                        return;
                    };
                    let days = cfg.audit.retention_days;
                    if days == 0 || stop.is_cancelled() {
                        return;
                    }
                    match nuono_core::audit::delete_command_logs_older_than(&db, days).await {
                        Ok(n) if n > 0 => info!(removed = n, "🧹 命令日志留存清理完成"),
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "命令日志留存清理失败"),
                    }
                    match nuono_core::audit::delete_event_logs_older_than(&db, days).await {
                        Ok(n) if n > 0 => info!(removed = n, "🧹 事件日志留存清理完成"),
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "事件日志留存清理失败"),
                    }
                    match db.delete_notifications_older_than(days).await {
                        Ok(n) if n > 0 => info!(removed = n, "🧹 站内通知留存清理完成"),
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "站内通知留存清理失败"),
                    }
                }
            },
        ),
    );
    tasks.seal();
}

async fn until_cancelled<F: Future>(stop: &CancellationToken, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        value = future => Some(value),
    }
}

// 触发取消令牌只会阻止开启下一轮定时任务，不会中途打断已经开始执行的当前轮次。
async fn periodic<F, Fut>(stop: CancellationToken, delay: Duration, period: Duration, mut round: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    if !delay.is_zero()
        && until_cancelled(&stop, tokio::time::sleep(delay))
            .await
            .is_none()
    {
        return;
    }
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while until_cancelled(&stop, interval.tick()).await.is_some() {
        if stop.is_cancelled() {
            break;
        }
        round().await;
    }
}

async fn locked_round<F, Fut>(stop: &CancellationToken, lock: &Mutex<()>, round: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(_guard) = until_cancelled(stop, lock.lock()).await else {
        return;
    };
    if stop.is_cancelled() {
        return;
    }
    // 互斥锁守卫 `_guard` 覆盖整轮任务执行与结果落盘全过程。
    round().await;
}

#[cfg(test)]
#[path = "../tests/unit/host_maintenance/tests.rs"]
mod tests;
