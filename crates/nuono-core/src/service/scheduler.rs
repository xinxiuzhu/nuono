//! 有界的计划任务准入，以及对已校验 cron 快照的原子替换。
use super::tasks::ScheduledTask;
use crate::db::Database;
use crate::error::{NuonoError, Result};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::sync::{Arc, Mutex as SyncMutex, Weak};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{error, info, warn};

mod delivery;
#[cfg(test)]
#[path = "../../tests/unit/service/scheduler/fault_tests.rs"]
mod fault_tests;
#[cfg(test)]
#[path = "../../tests/unit/service/scheduler/management_tests.rs"]
mod management_tests;
use delivery::{push_scheduled_subscription, send_scheduled_message};

pub use nuono_app::scheduled_tasks::ScheduledAction as SchedulerAction;

#[derive(Debug, Clone)]
pub struct SchedulerEvent {
    pub task_id: i64,
    pub task_name: String,
    /// 为 `None` 时维持原有的全量在线 Bot 广播语义；若显式填写了无法解析的非法 Bot ID 则直接拒绝。
    pub bot_id: Option<String>,
    pub action: SchedulerAction,
}

#[derive(Debug, Clone)]
pub struct ScheduledDelivery {
    pub event: SchedulerEvent,
    pub task: ScheduledTask,
    pub request_id: String,
    pub trigger_kind: String,
}
impl std::ops::Deref for ScheduledDelivery {
    type Target = SchedulerEvent;
    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

const MAX_SCHEDULED_TASKS: usize = 4096;
const EVENT_CAPACITY: usize = 256;

struct ScheduledJob {
    event: SchedulerEvent,
    task: ScheduledTask,
    cron: nuono_app::scheduled_tasks::ZonedSchedule,
    next_tick: Option<DateTime<Utc>>,
}
#[derive(Default)]
struct ScheduleSnapshot {
    jobs: Vec<ScheduledJob>,
}
impl ScheduleSnapshot {
    fn prepare(tasks: Vec<ScheduledTask>, now: DateTime<Utc>) -> Result<Self> {
        if tasks.len() > MAX_SCHEDULED_TASKS {
            return Err(NuonoError::Validation(format!(
                "启用的定时任务不能超过 {MAX_SCHEDULED_TASKS} 项"
            )));
        }
        let mut jobs = Vec::with_capacity(tasks.len());
        for task in tasks.into_iter().filter(|task| task.enabled) {
            if task.action_type == "run_sop" {
                warn!(
                    task_id = task.id,
                    "历史 SOP 定时任务已停用，保留记录但不注册"
                );
                continue;
            }
            nuono_app::scheduled_tasks::validate_bot_id(task.bot_id.as_deref())?;
            let action = nuono_app::scheduled_tasks::parse_action(
                &task.action_type,
                task.action_params.as_deref(),
            )?;
            let cron =
                nuono_app::scheduled_tasks::ZonedSchedule::parse(&task.cron_expr, &task.timezone)?;
            // 任务创建后，有限次数的定时表达式可能已经不再有未来触发点；过期任务直接置为 None，绝不重放已过去的历史触发点。
            let next_tick = cron.next(now);
            if next_tick.is_none() {
                continue;
            }
            jobs.push(ScheduledJob {
                event: SchedulerEvent {
                    task_id: task.id,
                    task_name: task.name.clone(),
                    bot_id: task.bot_id.clone(),
                    action,
                },
                task,
                cron,
                next_tick,
            });
        }
        Ok(Self { jobs })
    }

    /// 快照替换与到期触发准入共用同一把短临界区同步锁：新快照发布后，旧计划不再入队新事件，已入队的在途事件仍可正常执行完毕。
    fn trigger_due(&mut self, now: DateTime<Utc>, tx: &mpsc::Sender<ScheduledDelivery>) {
        for job in &mut self.jobs {
            if job.next_tick.is_some_and(|next| next <= now) {
                // 即使通道已满也要推进：错过的触发可观测，且永不堆积。
                job.next_tick = job.cron.next(now);
                if let Err(error) = tx.try_send(ScheduledDelivery {
                    event: job.event.clone(),
                    task: job.task.clone(),
                    request_id: uuid::Uuid::new_v4().to_string(),
                    trigger_kind: "scheduled".into(),
                }) {
                    metrics::counter!("nuono_scheduler_rejected_total").increment(1);
                    warn!(task_id=job.event.task_id, %error, "定时任务队列不可用，本次触发未入队，不自动补发");
                }
            }
        }
    }
}

/// 单一定时器统一驱动最多 4096 个定时任务并向有界通道投递，避免每次触发都动态创建协程。
/// 重载（reload）会先在锁外构造完整的候选计划再同步切换；若中途失败或被取消，当前快照与定时器均保持不变。实例一旦关闭（shutdown）即不可重新启动。
pub struct Scheduler {
    db: Database,
    business_gate: nuono_app::business_runtime::BusinessGate,
    event_tx: mpsc::Sender<ScheduledDelivery>,
    snapshot: SyncMutex<ScheduleSnapshot>,
    lifecycle: Mutex<()>,
    recovered: std::sync::atomic::AtomicBool,
    ticker: SyncMutex<Option<tokio::task::JoinHandle<()>>>,
    stopped: watch::Sender<bool>,
}
impl Scheduler {
    pub fn new(db: Database) -> (Self, mpsc::Receiver<ScheduledDelivery>) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (stopped, _) = watch::channel(false);
        (
            Self {
                db,
                business_gate: nuono_app::business_runtime::BusinessGate::running(),
                event_tx,
                snapshot: SyncMutex::new(ScheduleSnapshot::default()),
                lifecycle: Mutex::new(()),
                recovered: std::sync::atomic::AtomicBool::new(false),
                ticker: SyncMutex::new(None),
                stopped,
            },
            event_rx,
        )
    }
    pub(crate) fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.stopped.subscribe()
    }
    pub(crate) fn with_business_gate(
        mut self,
        gate: nuono_app::business_runtime::BusinessGate,
    ) -> Self {
        self.business_gate = gate;
        self
    }
    pub(crate) async fn wait_for_business(&self) -> bool {
        self.business_gate.wait_until_running().await
    }
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        {
            let _guard = self.lifecycle.lock().await;
            if !self.recovered.load(std::sync::atomic::Ordering::Acquire) {
                nuono_storage::task_runs::interrupt_unfinished(&self.db).await?;
                self.recovered
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        self.reload().await
    }

    /// 重复请求返回既有收据，不会再次发送。
    pub async fn run_once(
        &self,
        task_id: i64,
        request_id: &str,
    ) -> Result<nuono_storage::task_runs::TaskRun> {
        let _lifecycle = self.lifecycle.lock().await;
        if *self.stopped.borrow() {
            return Err(NuonoError::Conflict("调度器已关闭".into()));
        }
        if !self.business_gate.is_running() {
            return Err(NuonoError::Conflict(
                "业务尚未启动，不能执行定时任务".into(),
            ));
        }
        let task = super::tasks::get(&self.db, task_id).await?;
        let action = nuono_app::scheduled_tasks::parse_action(
            &task.action_type,
            task.action_params.as_deref(),
        )?;
        let (mut run, inserted) =
            nuono_storage::task_runs::reserve(&self.db, request_id, &task, "manual").await?;
        if !inserted {
            return Ok(run);
        }
        let delivery = ScheduledDelivery {
            event: SchedulerEvent {
                task_id,
                task_name: task.name.clone(),
                bot_id: task.bot_id.clone(),
                action,
            },
            task,
            request_id: request_id.into(),
            trigger_kind: "manual".into(),
        };
        // 执行记录落库提交后，在同步锁内立即检查停机屏障并完成通道入队（中间不跨越 .await）。
        let admitted = {
            let _snapshot = self.snapshot.lock().unwrap();
            !*self.stopped.borrow() && self.event_tx.try_send(delivery).is_ok()
        };
        if !admitted {
            let detail = "调度器已关闭或队列已满，本次请求没有执行";
            nuono_storage::task_runs::finish(&self.db, request_id, "rejected", detail).await?;
            run.status = "rejected".into();
            run.detail = Some(detail.into());
        }
        Ok(run)
    }
    pub async fn reload(self: &Arc<Self>) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        if *self.stopped.borrow() {
            return Err(NuonoError::Conflict("调度器已关闭".into()));
        }
        let tasks =
            super::tasks::list_for_scheduler(&self.db, MAX_SCHEDULED_TASKS as u64 + 1).await?;
        let candidate = ScheduleSnapshot::prepare(tasks, Utc::now())?;
        let count = candidate.jobs.len();
        {
            let mut snapshot = self.snapshot.lock().unwrap();
            if *self.stopped.borrow() {
                return Err(NuonoError::Conflict("调度器已关闭".into()));
            }
            *snapshot = candidate;
            // 在同一同步代码块内完成快照发布与定时器协程状态检查，中间不留异步空隙。
            let mut ticker = self.ticker.lock().unwrap();
            if ticker.is_none() {
                *ticker = Some(tokio::spawn(Self::tick_loop(Arc::downgrade(self))));
            }
        }
        info!(count, "定时任务快照已发布");
        Ok(())
    }
    async fn tick_loop(owner: Weak<Self>) {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(owner) = owner.upgrade() else {
                return;
            };
            let mut snapshot = owner.snapshot.lock().unwrap();
            if *owner.stopped.borrow() {
                return;
            }
            if !owner.business_gate.is_running() {
                continue;
            }
            snapshot.trigger_due(Utc::now(), &owner.event_tx);
        }
    }
    /// 立即关闭新任务准入（即使此时正有异步 reload 在等待数据库查询返回）。
    pub(crate) fn request_shutdown(&self) {
        self.stopped.send_replace(true);
        self.snapshot.lock().unwrap().jobs.clear();
        if let Some(ticker) = self.ticker.lock().unwrap().take() {
            ticker.abort();
        }
    }
    pub async fn shutdown(&self) -> Result<()> {
        // stopped 标志与快照锁会立即拦截后续发布；正在等待数据库的 reload 不会阻塞停机，并会在尝试切换快照前直接失败退出。
        self.request_shutdown();
        Ok(())
    }
    pub fn job_count(&self) -> usize {
        self.snapshot.lock().unwrap().jobs.len()
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

/// 执行一条已准入的定时任务事件（进入传输队列不代表平台已最终送达）。
/// `last_run_at` 仅以尽力而为（best-effort）方式记录本次触发时间，更新失败会打印告警日志。
pub async fn handle_scheduler_event(
    event: &SchedulerEvent,
    db: &Database,
    bot_txs: &DashMap<i64, mpsc::Sender<nuono_app::transport::TransportFrame>>,
) -> Result<()> {
    handle_scheduler_event_with_router(event, db, bot_txs, None)
        .await
        .map(|_| ())
}

async fn handle_scheduler_event_with_router(
    event: &SchedulerEvent,
    db: &Database,
    bot_txs: &DashMap<i64, mpsc::Sender<nuono_app::transport::TransportFrame>>,
    router: Option<&crate::core::runtime_action_router::RuntimeActionRouter>,
) -> Result<Option<crate::core::runtime_action_router::RuntimeActionReceipt>> {
    if let Err(error) = super::tasks::record_last_run(db, event.task_id).await {
        error!(task_id=event.task_id, %error, "定时任务尝试时间保存失败");
    }
    match &event.action {
        SchedulerAction::SendNativeMessage { request } => {
            let router =
                router.ok_or_else(|| NuonoError::Connection("定时消息路由不可用".into()))?;
            if let Some(group_id) = scheduled_group_id(&request.target)?
                && !db.is_group_enabled(&group_id).await?
            {
                return Err(NuonoError::PermissionDenied(
                    "目标群已停用或不可服务".into(),
                ));
            }
            super::managed_messages::send(
                router,
                request,
                nuono_app::action::ActionAuthority::Runtime {
                    stage: "scheduler".into(),
                },
            )
            .await
            .map(Some)
        }
        SchedulerAction::SendMessage {
            target_type,
            target_id,
            message,
        } => {
            let id = target_id
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| NuonoError::Validation("定时消息目标 ID 必须是正整数".into()))?;
            let api_call = match target_type.as_str() {
                "group" => {
                    serde_json::json!({"action":"send_group_msg","params":{"group_id":id,"message":[{"type":"text","data":{"text":message}}]}})
                }
                "user" | "private" => {
                    serde_json::json!({"action":"send_private_msg","params":{"user_id":id,"message":[{"type":"text","data":{"text":message}}]}})
                }
                _ => return Err(NuonoError::Validation("未知的定时消息目标类型".into())),
            };
            send_scheduled_message(db, event, target_type, target_id, &api_call, bot_txs)
                .await
                .map(|_| None)
        }
        SchedulerAction::PushSubscribe { topic, message } => {
            push_scheduled_subscription(db, event, topic, message, bot_txs)
                .await
                .map(|_| None)
        }
    }
}

// 与运行时群发现使用相同的规范化群组 ID（含频道）；带前导零或正号的 OneBot 数字写法会被归一化，防止绕过目标群的策略检查。
fn scheduled_group_id(target: &crate::domain::DeliveryTarget) -> Result<Option<String>> {
    use crate::domain::{ConversationRef, DeliveryTarget, Platform};
    let identity = match target {
        DeliveryTarget::Conversation {
            conversation: ConversationRef::Group { group },
        } => group,
        DeliveryTarget::Conversation {
            conversation: ConversationRef::Channel { channel, .. },
        }
        | DeliveryTarget::Channel { channel, .. } => channel,
        _ => return Ok(None),
    };
    if *identity.platform() == Platform::OneBot {
        let id = identity
            .value()
            .as_str()
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| NuonoError::Validation("OneBot 群 ID 必须是正整数".into()))?;
        Ok(Some(id.to_string()))
    } else {
        Ok(Some(identity.value().as_str().to_owned()))
    }
}

pub async fn handle_scheduled_delivery(
    delivery: &ScheduledDelivery,
    db: &Database,
    bot_txs: &DashMap<i64, mpsc::Sender<nuono_app::transport::TransportFrame>>,
    router: &crate::core::runtime_action_router::RuntimeActionRouter,
) -> Result<()> {
    nuono_storage::task_runs::reserve(
        db,
        &delivery.request_id,
        &delivery.task,
        &delivery.trigger_kind,
    )
    .await?;
    if !nuono_storage::task_runs::claim(db, &delivery.request_id).await? {
        return Ok(());
    }
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        handle_scheduler_event_with_router(&delivery.event, db, bot_txs, Some(router)),
    )
    .await;
    let (status, detail, result) = match result {
        Ok(Ok(Some(receipt))) => {
            let detail = if receipt.degradations.is_empty() {
                "平台调用已返回".into()
            } else {
                format!(
                    "平台调用已返回；降级提示：{}",
                    receipt.degradations.join("; ")
                )
            };
            ("succeeded", detail, Ok(()))
        }
        Ok(Ok(None)) => ("enqueued", "消息已入队，入队不代表平台送达".into(), Ok(())),
        Ok(Err(error)) => ("failed", error.to_string(), Err(error)),
        Err(_) => (
            "unknown",
            "投递等待超时，平台可能已经执行；请核对后再补发".into(),
            Err(NuonoError::Timeout("定时投递结果未确认".into())),
        ),
    };
    nuono_storage::task_runs::finish(db, &delivery.request_id, status, &detail).await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    use crate::service::tasks;

    async fn test_db() -> Database {
        crate::test_support::test_db(1).await
    }

    #[tokio::test]
    async fn retired_workflow_tasks_are_preserved_but_not_scheduled() {
        let db = test_db().await;
        use sea_orm::{ActiveModelTrait, Set};
        let legacy = crate::entity::scheduled_tasks::ActiveModel {
            name: Set("legacy workflow".into()),
            cron_expr: Set("0 0 0 * * *".into()),
            action_type: Set("run_sop".into()),
            enabled: Set(Some(true)),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();
        tasks::create(
            &db,
            "send message",
            "0 0 0 * * *",
            "send_message",
            Some(r#"{"target_type":"group","target_id":"1","message":"hello"}"#),
            None,
        )
        .await
        .unwrap();
        let (scheduler, _event_rx) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler);
        scheduler.start().await.unwrap();
        assert_eq!(scheduler.job_count(), 1);
        assert!(
            scheduler
                .snapshot
                .lock()
                .unwrap()
                .jobs
                .iter()
                .all(|job| job.event.task_id != legacy.id)
        );
        assert!(
            tasks::list(&db)
                .await
                .unwrap()
                .iter()
                .any(|task| task.id == legacy.id)
        );
        scheduler.shutdown().await.unwrap();
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reload_reflects_created_toggled_and_deleted_tasks() {
        let db = test_db().await;
        let first = tasks::create(
            &db,
            "first",
            "0 0 0 * * *",
            "send_message",
            Some(r#"{"target_type":"group","target_id":"1","message":"first"}"#),
            None,
        )
        .await
        .unwrap();
        let disabled = tasks::create(
            &db,
            "disabled",
            "0 0 0 * * *",
            "send_message",
            Some(r#"{"target_type":"group","target_id":"1","message":"disabled"}"#),
            None,
        )
        .await
        .unwrap();
        tasks::toggle(&db, disabled.id, false).await.unwrap();

        let (scheduler, _event_rx) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler);
        scheduler.start().await.unwrap();
        assert_eq!(scheduler.job_count(), 1);

        let second = tasks::create(
            &db,
            "second",
            "0 0 0 * * *",
            "push_subscribe",
            Some(r#"{"topic":"test","message":"second"}"#),
            None,
        )
        .await
        .unwrap();
        scheduler.reload().await.unwrap();
        assert_eq!(scheduler.job_count(), 2);

        tasks::toggle(&db, first.id, false).await.unwrap();
        scheduler.reload().await.unwrap();
        assert_eq!(scheduler.job_count(), 1);

        tasks::remove(&db, second.id).await.unwrap();
        scheduler.reload().await.unwrap();
        assert_eq!(scheduler.job_count(), 0);

        scheduler.shutdown().await.unwrap();
        db.shutdown().await.unwrap();
    }

    #[test]
    fn unsupported_actions_are_not_schedulable() {
        assert!(nuono_app::scheduled_tasks::parse_action("http_request", None).is_err());
        assert!(nuono_app::scheduled_tasks::parse_action("plugin_hook", None).is_err());
    }

    fn event(action: SchedulerAction, bot_id: Option<&str>) -> SchedulerEvent {
        SchedulerEvent {
            task_id: 1,
            task_name: "test".to_string(),
            bot_id: bot_id.map(str::to_string),
            action,
        }
    }

    fn group_action(target: &str) -> SchedulerAction {
        SchedulerAction::SendMessage {
            target_type: "group".to_string(),
            target_id: target.to_string(),
            message: "hi".to_string(),
        }
    }

    fn group_api_call(target: &str) -> Value {
        serde_json::json!({
            "action": "send_group_msg",
            "params": {
                "group_id": target.parse::<i64>().unwrap_or(0),
                "message": [{"type": "text", "data": {"text": "hi"}}]
            }
        })
    }

    fn future_expiry() -> String {
        (chrono::Local::now() + chrono::Duration::days(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    async fn seed_group(db: &Database, group_id: &str, _expire_at: Option<&str>) {
        db.ensure_group(&nuono_storage::groups::GroupIdentity::onebot(group_id), "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn bound_bot_alone_receives_paid_group_message() {
        let db = test_db().await;
        seed_group(&db, "100", Some(&future_expiry())).await;

        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let bot_txs = DashMap::new();
        bot_txs.insert(11, tx_a);
        bot_txs.insert(22, tx_b);

        let ev = event(group_action("100"), Some("11"));
        send_scheduled_message(&db, &ev, "group", "100", &group_api_call("100"), &bot_txs)
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx_a.recv())
                .await
                .is_ok(),
            "绑定的 Bot 应收到消息"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), rx_b.recv())
                .await
                .is_err(),
            "未绑定的 Bot 不应收到消息"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn offline_bound_bot_is_an_observable_error() {
        let db = test_db().await;
        seed_group(&db, "100", Some(&future_expiry())).await;

        let bot_txs = DashMap::new();
        let ev = event(group_action("100"), Some("99"));
        let err =
            send_scheduled_message(&db, &ev, "group", "100", &group_api_call("100"), &bot_txs)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("不在线"), "实际: {err}");
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_unbound_task_still_broadcasts_but_gated() {
        let db = test_db().await;
        seed_group(&db, "100", Some(&future_expiry())).await;

        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let bot_txs = DashMap::new();
        bot_txs.insert(11, tx_a);
        bot_txs.insert(22, tx_b);

        let ev = event(group_action("100"), None);
        send_scheduled_message(&db, &ev, "group", "100", &group_api_call("100"), &bot_txs)
            .await
            .unwrap();

        for rx in [&mut rx_a, &mut rx_b] {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .is_ok(),
                "老任务广播语义应保留"
            );
        }
        db.shutdown().await.unwrap();
    }
}
