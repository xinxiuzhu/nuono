//! 管理 HTTP 开放后，等待首次账号设置，再启动业务与平台连接。
//!
//! 设置请求只发送通知；独立监督任务读取已提交的账号状态，因此请求取消、通知合并或
//! 重复提交不会漏启动或重复启动。停止状态不可恢复，重启必须创建新的控制器。

use crate::{background_tasks::HostBackgroundTasks, bot_instances::BotInstanceRuntimeManager};
use nuono_core::{config::ConfigManager, core::ConnectionManager, db::Database};
use serde::Serialize;
use std::{
    future::{Future, IntoFuture, poll_fn},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::{Notify, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusinessPhase {
    WaitingHttp,
    WaitingSetup,
    Starting,
    Running,
    Failed,
    Stopped,
}

pub(crate) struct BusinessStartup {
    phase: watch::Sender<BusinessPhase>,
    setup_changed: Notify,
}

impl BusinessStartup {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: watch::channel(BusinessPhase::WaitingHttp).0,
            setup_changed: Notify::new(),
        })
    }

    pub(crate) fn phase(&self) -> BusinessPhase {
        *self.phase.borrow()
    }

    fn transition(&self, next: BusinessPhase) -> bool {
        self.phase.send_if_modified(|phase| {
            if *phase == BusinessPhase::Stopped || *phase == next {
                return false;
            }
            *phase = next;
            true
        })
    }

    pub(crate) fn setup_changed(&self) {
        self.setup_changed.notify_one();
    }

    pub(crate) fn close(&self) {
        self.phase.send_replace(BusinessPhase::Stopped);
        self.setup_changed.notify_one();
    }
}

pub(crate) fn register(
    tasks: &mut HostBackgroundTasks,
    control: Arc<BusinessStartup>,
    http_started: oneshot::Receiver<()>,
    manager: Arc<ConnectionManager>,
    db: Database,
    config: Arc<ConfigManager>,
    instances: Arc<BotInstanceRuntimeManager>,
) {
    let stop = tasks.token();
    tasks.spawn("business-startup", async move {
        let start = async {
            if http_started.await.is_err() {
                return;
            }
            if !wait_for_setup(&control, &db, &stop).await {
                return;
            }
            if manager.framework_control.last_signal().is_some() {
                manager.request_business_shutdown();
                control.close();
                return;
            }
            if !control.transition(BusinessPhase::Starting) {
                return;
            }
            tracing::info!(target: "nuono::startup", "管理员设置已完成，正在启动业务…");
            if let Err(error) = manager.start_business().await {
                control.transition(BusinessPhase::Failed);
                tracing::error!(target: "nuono::startup", %error,
                    "业务启动失败，管理服务继续运行；请检查插件和配置后重启");
                return;
            }
            if !control.transition(BusinessPhase::Running) {
                return;
            }
            tracing::info!(target: "nuono::startup",
                "业务已启动；外部服务的连接状态由各插件和适配器分别报告");
            start_adapters(&config, &instances).await;
        };
        tokio::select! {
            biased;
            _ = stop.cancelled() => {},
            _ = start => {},
        }
    });
}

async fn wait_for_setup(
    control: &BusinessStartup,
    db: &Database,
    stop: &CancellationToken,
) -> bool {
    let mut reported_error = false;
    loop {
        if stop.is_cancelled() || control.phase() == BusinessPhase::Stopped {
            return false;
        }
        match db.owner_count().await {
            Ok(count) if count > 0 => return true,
            Ok(_) => {
                reported_error = false;
                if control.transition(BusinessPhase::WaitingSetup) {
                    tracing::info!(target: "nuono::startup",
                        "等待首次管理员设置：管理服务可用，插件业务、平台连接与定时任务尚未启动");
                }
            }
            Err(error) => {
                if !reported_error {
                    tracing::warn!(target: "nuono::startup", %error,
                        "读取管理员设置状态失败，业务保持暂停");
                }
                reported_error = true;
            }
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => return false,
            _ = control.setup_changed.notified() => {},
            // 数据库提交与 HTTP 请求生命周期相互独立，定期复查补偿被取消的请求。
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

/// 只有 serve 已经实际 poll 到等待状态，才能宣布 HTTP 可用并通知业务监督任务。
pub(crate) async fn serve_with_readiness<F>(
    serve: F,
    started: oneshot::Sender<()>,
    readiness: Arc<std::sync::atomic::AtomicBool>,
    stop: CancellationToken,
) -> std::io::Result<()>
where
    F: IntoFuture<Output = std::io::Result<()>>,
{
    let serve = serve.into_future();
    tokio::pin!(serve);
    let mut started = Some(started);
    poll_fn(move |cx| {
        let result = serve.as_mut().poll(cx);
        if result.is_pending()
            && !stop.is_cancelled()
            && let Some(started) = started.take()
        {
            readiness.store(true, Ordering::Release);
            tracing::info!(target: "nuono::startup", "管理服务已启动");
            let _ = started.send(());
        }
        result
    })
    .await
}

async fn start_adapters(config: &ConfigManager, instances: &BotInstanceRuntimeManager) {
    let cfg = config.snapshot().await;
    // 外部连接互相独立，一个平台不可达不阻塞其他平台，也不占用 HTTP 启动流程。
    tokio::join!(
        async {
            if cfg.feishu.enabled {
                report_adapter_start("飞书", instances.source.start_legacy(
                    "feishu", "feishu", "飞书（运行配置）",
                    serde_json::json!({"app_id":cfg.feishu.app_id.as_deref().unwrap_or_default(),"api_base_url":"https://open.feishu.cn/open-apis"}),
                    serde_json::json!({"app_secret":cfg.feishu.app_secret.as_deref().unwrap_or_default(),"verification_token":cfg.feishu.verification_token.as_deref().unwrap_or_default(),"encrypt_key":cfg.feishu.encrypt_key.as_deref().unwrap_or_default()}),
                )).await;
            }
        },
        async {
            if cfg.qq_official.enabled {
                if cfg.qq_official.app_id.is_some() && cfg.qq_official.app_secret.is_some() {
                    report_adapter_start(
                        "QQ 官方 Bot",
                        instances.start_legacy_qq(&cfg.qq_official),
                    )
                    .await;
                } else {
                    tracing::warn!("QQ 官方 Bot 适配器已启用但缺少 app_id/app_secret，等待配置");
                }
            }
        },
        async {
            if cfg.telegram.enabled
                && let Some(token) = cfg.telegram.resolve_token()
            {
                report_adapter_start(
                    "Telegram",
                    instances.source.start_legacy(
                        "telegram",
                        "telegram",
                        "Telegram（运行配置）",
                        serde_json::json!({"api_base_url":cfg.telegram.api_base_url}),
                        serde_json::json!({"token":token}),
                    ),
                )
                .await;
            }
        },
        async {
            if cfg.discord.enabled
                && let Some(token) = cfg.discord.token.as_deref()
            {
                report_adapter_start(
                    "Discord",
                    instances.source.start_legacy(
                        "discord",
                        "discord",
                        "Discord（运行配置）",
                        serde_json::json!({}),
                        serde_json::json!({"token":token}),
                    ),
                )
                .await;
            }
        },
        instances.start_enabled(),
    );
}

async fn report_adapter_start(
    adapter: &str,
    start: impl Future<Output = nuono_core::error::Result<()>>,
) {
    if let Err(error) = start.await {
        tracing::warn!(
            adapter,
            kind = error.kind(),
            "平台连接未启动，管理服务和其他业务继续运行"
        );
    }
}

#[cfg(test)]
#[path = "../tests/unit/business_startup.rs"]
mod tests;
