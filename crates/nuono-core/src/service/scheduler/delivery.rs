//! 定时任务消息投递：仅负责将出站帧送入底层传输队列，不等待远端平台最终接收或送达回执。

use super::SchedulerEvent;
use crate::db::Database;
use crate::error::{NuonoError, Result};
use dashmap::DashMap;
use nuono_app::transport::TransportFrame;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

type BotSenders = DashMap<i64, mpsc::Sender<TransportFrame>>;

/// 若显式配置了 bot_id 但格式非法，必须直接报错，绝不能静默退化为全量 Bot 广播。
fn bound_bot_id(event: &SchedulerEvent) -> Result<Option<i64>> {
    event
        .bot_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| positive_id(value, "投递 Bot"))
        .transpose()
}

fn positive_id(value: &str, label: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| NuonoError::InvalidInput(format!("{label} ID 必须为正整数")))
}

/// 在进入任何异步操作前先克隆出所有目标发送通道句柄，确保不跨 `.await` 持有 DashMap 分片锁（否则会阻塞连接断开与重连对同一映射表的修改）。
fn recipients(
    event: &SchedulerEvent,
    bot_txs: &BotSenders,
) -> Result<Vec<(i64, mpsc::Sender<TransportFrame>)>> {
    match bound_bot_id(event)? {
        Some(bot_id) => bot_txs
            .get(&bot_id)
            .map(|entry| vec![(bot_id, entry.value().clone())])
            .ok_or_else(|| {
                NuonoError::MessageSend(format!("任务绑定的 Bot {bot_id} 不在线，无法投递定时消息"))
            }),
        None => {
            warn!(task = %event.task_name, "定时任务未绑定 Bot，正在向全部在线 Bot 广播；请在任务配置中补选投递 Bot");
            let senders = bot_txs
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
                .collect::<Vec<_>>();
            if senders.is_empty() {
                Err(NuonoError::MessageSend(
                    "无在线 Bot，无法投递定时消息".into(),
                ))
            } else {
                Ok(senders)
            }
        }
    }
}

#[derive(Default)]
struct DeliveryCounts {
    enqueued: usize,
    failed: usize,
    skipped: usize,
}

impl DeliveryCounts {
    /// 使用非阻塞有界入队（`try_send`），防止单个发送缓慢的 Bot 拖慢整个调度器或其他 Bot；
    /// 队列满载时直接计入过载失败，绝不生成无限等待的后台任务，也不自动重试。
    fn enqueue(&mut self, senders: &[(i64, mpsc::Sender<TransportFrame>)], payload: &str) {
        for (bot_id, sender) in senders {
            match sender.try_send(TransportFrame::Text(payload.to_owned())) {
                Ok(()) => self.enqueued += 1,
                Err(error) => {
                    self.failed += 1;
                    let reason = match error {
                        mpsc::error::TrySendError::Full(_) => "queue_full",
                        mpsc::error::TrySendError::Closed(_) => "queue_closed",
                    };
                    metrics::counter!("nuono_scheduler_delivery_failures_total", "reason" => reason).increment(1);
                    warn!(bot_id, reason, "定时任务消息未入队");
                }
            }
        }
    }

    fn finish(&self, event: &SchedulerEvent) -> Result<()> {
        info!(task = %event.task_name, enqueued = self.enqueued, failed = self.failed, skipped = self.skipped, "定时任务消息入队结果");
        if self.failed == 0 {
            Ok(())
        } else {
            Err(NuonoError::MessageSend(format!(
                "定时任务消息入队未完成：已入队 {}，失败 {}，跳过 {}；入队不代表平台送达",
                self.enqueued, self.failed, self.skipped
            )))
        }
    }
}

pub(super) async fn send_scheduled_message(
    db: &Database,
    event: &SchedulerEvent,
    target_type: &str,
    target_id: &str,
    api_call: &Value,
    bot_txs: &BotSenders,
) -> Result<()> {
    let canonical_target = positive_id(target_id, "消息目标")?.to_string();
    if !matches!(target_type, "group" | "user" | "private") {
        return Err(NuonoError::InvalidInput("定时消息目标类型无效".into()));
    }
    let senders = recipients(event, bot_txs)?;
    if target_type == "group" && !db.is_group_enabled(&canonical_target).await? {
        return Err(NuonoError::PermissionDenied(format!(
            "定时任务目标群 {target_id} 当前不可服务，已拒绝投递"
        )));
    }

    let payload = serde_json::to_string(api_call)?;
    let mut counts = DeliveryCounts::default();
    counts.enqueue(&senders, &payload);
    counts.finish(event)
}

pub(super) async fn push_scheduled_subscription(
    db: &Database,
    event: &SchedulerEvent,
    topic: &str,
    message: &str,
    bot_txs: &BotSenders,
) -> Result<()> {
    let senders = recipients(event, bot_txs)?;
    let subscribers = super::super::pubsub::subscriptions::push(db, topic, None).await?;
    let mut counts = DeliveryCounts::default();

    for sub in &subscribers {
        let target = sub.target_id.as_deref().unwrap_or(&sub.subscriber_id);
        let target_id = match positive_id(target, "订阅目标") {
            Ok(id) => id,
            Err(error) => {
                counts.failed += 1;
                warn!(task = %event.task_name, subscription_id = sub.id, error = %error, "订阅目标无效，跳过投递");
                continue;
            }
        };
        // 权限校验与平台发送路由统一使用归一化后的十进制群号字符串，防止 `0100` 或 `+100` 等写法绕过群 `100` 的策略控制。
        let canonical_target = target_id.to_string();
        match db.is_group_enabled(&canonical_target).await {
            Ok(true) => {}
            Ok(false) => {
                counts.skipped += 1;
                continue;
            }
            Err(error) => {
                warn!(task = %event.task_name, enqueued = counts.enqueued, failed = counts.failed, skipped = counts.skipped, error = %error, "订阅权限查询失败，推送中断");
                return Err(error);
            }
        }

        let payload = serde_json::to_string(&serde_json::json!({
            "action": "send_group_msg",
            "params": {
                "group_id": target_id,
                "message": [{"type": "text", "data": {"text": message}}]
            }
        }))?;
        counts.enqueue(&senders, &payload);
    }
    counts.finish(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::scheduler::SchedulerAction;
    use std::time::Duration;

    fn event(bot_id: Option<&str>) -> SchedulerEvent {
        SchedulerEvent {
            task_id: 1,
            task_name: "fault test".into(),
            bot_id: bot_id.map(str::to_owned),
            action: SchedulerAction::SendMessage {
                target_type: "private".into(),
                target_id: "100".into(),
                message: "hi".into(),
            },
        }
    }

    fn api_call() -> Value {
        serde_json::json!({"action": "send_private_msg", "params": {"user_id": 100}})
    }

    #[tokio::test]
    async fn full_bot_queue_does_not_block_healthy_bot_or_disconnect() {
        let db = crate::test_support::test_db(1).await;
        let (full_tx, mut full_rx) = mpsc::channel(1);
        full_tx
            .try_send(TransportFrame::Text("occupied".into()))
            .unwrap();
        let (healthy_tx, mut healthy_rx) = mpsc::channel(1);
        let senders = DashMap::new();
        senders.insert(11, full_tx);
        senders.insert(22, healthy_tx);

        let error = tokio::time::timeout(
            Duration::from_millis(200),
            send_scheduled_message(&db, &event(None), "private", "100", &api_call(), &senders),
        )
        .await
        .expect("a full Bot queue must not block the scheduler")
        .unwrap_err();
        assert!(matches!(error, NuonoError::MessageSend(_)));
        assert!(error.to_string().contains("已入队 1，失败 1"));
        assert!(
            healthy_rx.try_recv().is_ok(),
            "healthy Bot must still receive its frame"
        );
        assert!(
            senders.remove(&11).is_some(),
            "delivery must release all map guards"
        );
        assert!(
            matches!(full_rx.try_recv(), Ok(TransportFrame::Text(value)) if value == "occupied")
        );
        assert!(
            full_rx.try_recv().is_err(),
            "failed frames must not be retried later"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn closed_bound_queue_is_an_error() {
        let db = crate::test_support::test_db(1).await;
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let senders = DashMap::new();
        senders.insert(11, sender);
        let error = send_scheduled_message(
            &db,
            &event(Some("11")),
            "private",
            "100",
            &api_call(),
            &senders,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, NuonoError::MessageSend(_)));
        assert!(error.to_string().contains("已入队 0，失败 1"));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_explicit_bot_never_falls_back_to_broadcast() {
        let db = crate::test_support::test_db(1).await;
        let (sender, mut receiver) = mpsc::channel(1);
        let senders = DashMap::new();
        senders.insert(11, sender);
        for value in ["invalid", "0", "-11", "9223372036854775808"] {
            let error = send_scheduled_message(
                &db,
                &event(Some(value)),
                "private",
                "100",
                &api_call(),
                &senders,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, NuonoError::InvalidInput(_)),
                "{value}: {error}"
            );
        }
        assert!(receiver.try_recv().is_err());
        // 空字符串或纯空白的历史 bot_id 绑定按未绑定处理，保持广播给所有在线 Bot 的约定。
        for value in ["", "   "] {
            send_scheduled_message(
                &db,
                &event(Some(value)),
                "private",
                "100",
                &api_call(),
                &senders,
            )
            .await
            .unwrap();
            assert!(receiver.try_recv().is_ok());
        }

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_target_is_rejected_before_transport_admission() {
        let db = crate::test_support::test_db(1).await;
        let (sender, mut receiver) = mpsc::channel(1);
        let senders = DashMap::new();
        senders.insert(11, sender);
        for value in ["", "invalid", "0", "-1", " 100 ", "9223372036854775808"] {
            let error = send_scheduled_message(
                &db,
                &event(Some("11")),
                "private",
                value,
                &api_call(),
                &senders,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, NuonoError::InvalidInput(_)),
                "{value}: {error}"
            );
        }
        assert!(receiver.try_recv().is_err());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_without_bots_is_an_error() {
        let db = crate::test_support::test_db(1).await;
        let error = send_scheduled_message(
            &db,
            &event(None),
            "private",
            "100",
            &api_call(),
            &DashMap::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, NuonoError::MessageSend(_)));
        db.shutdown().await.unwrap();
    }

    async fn paid_group(db: &Database, group_id: &str) {
        db.ensure_group(&nuono_storage::groups::GroupIdentity::onebot(group_id), "")
            .await
            .unwrap();
    }
    async fn disabled_group(db: &Database, group_id: &str) {
        let group = db
            .ensure_group(&nuono_storage::groups::GroupIdentity::onebot(group_id), "")
            .await
            .unwrap();
        nuono_app::groups::GroupStore::toggle_group(db, group.id, false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn group_message_numeric_aliases_cannot_bypass_disabled_group() {
        let db = crate::test_support::test_db(1).await;
        disabled_group(&db, "100").await;
        let (sender, mut receiver) = mpsc::channel(8);
        let senders = DashMap::new();
        senders.insert(11, sender);
        let api_call = serde_json::json!({
            "action": "send_group_msg", "params": {"group_id": 100}
        });

        for target in ["0100", "+100"] {
            for binding in [Some("11"), None, Some("")] {
                let error = send_scheduled_message(
                    &db,
                    &event(binding),
                    "group",
                    target,
                    &api_call,
                    &senders,
                )
                .await
                .unwrap_err();
                assert!(
                    matches!(error, NuonoError::PermissionDenied(_)),
                    "{target}: {error}"
                );
            }
        }
        assert!(receiver.try_recv().is_err());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn subscription_numeric_aliases_cannot_bypass_disabled_group() {
        let db = crate::test_support::test_db(1).await;
        disabled_group(&db, "100").await;
        paid_group(&db, "200").await;
        for target in ["0100", "+100"] {
            crate::service::pubsub::subscriptions::create(&db, target, "news", None)
                .await
                .unwrap();
            crate::service::pubsub::subscriptions::create(&db, "subscriber", "news", Some(target))
                .await
                .unwrap();
        }
        crate::service::pubsub::subscriptions::create(&db, "200", "news", None)
            .await
            .unwrap();
        let (sender, mut receiver) = mpsc::channel(8);
        let senders = DashMap::new();
        senders.insert(11, sender);

        push_scheduled_subscription(&db, &event(Some("11")), "news", "hi", &senders)
            .await
            .unwrap();
        let TransportFrame::Text(payload) = receiver.try_recv().unwrap() else {
            panic!("expected text frame")
        };
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["params"]["group_id"], 200);
        assert!(
            receiver.try_recv().is_err(),
            "no alias of disabled group 100 may be enqueued"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn subscription_push_reports_partial_failure_and_keeps_healthy_targets() {
        let db = crate::test_support::test_db(1).await;
        paid_group(&db, "100").await;
        disabled_group(&db, "200").await;
        crate::service::pubsub::subscriptions::create(&db, "invalid", "news", None)
            .await
            .unwrap();
        crate::service::pubsub::subscriptions::create(&db, "100", "news", None)
            .await
            .unwrap();
        crate::service::pubsub::subscriptions::create(&db, "200", "news", None)
            .await
            .unwrap();
        let (sender, mut receiver) = mpsc::channel(8);
        let senders = DashMap::new();
        senders.insert(11, sender);

        let error = push_scheduled_subscription(&db, &event(Some("11")), "news", "hi", &senders)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("已入队 1，失败 1，跳过 1"));
        let frame = receiver.try_recv().unwrap();
        let TransportFrame::Text(payload) = frame else {
            panic!("expected text frame")
        };
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["params"]["group_id"], 100);
        assert!(receiver.try_recv().is_err());
        db.shutdown().await.unwrap();
    }
}
