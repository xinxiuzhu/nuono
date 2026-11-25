use super::*;
use crate::core::runtime_action_router::RuntimeActionRouter;
use nuono_app::transport::{RuntimeActionSink, RuntimeBotRoute};
use serde_json::{Value, json};

#[derive(Default)]
struct RecordingSink(std::sync::Mutex<Vec<Value>>);
#[async_trait::async_trait]
impl RuntimeActionSink for RecordingSink {
    async fn send_action(&self, _action: Value) -> Result<()> {
        panic!("must await a platform receipt")
    }
    async fn call_action(&self, action: Value) -> Result<Value> {
        self.0.lock().unwrap().push(action);
        Ok(json!({"status":"ok","retcode":0,"data":{"message_id":"native-id"}}))
    }
}
async fn native_task(db: &Database) -> ScheduledTask {
    let request = json!({"route":{"platform":"onebot","instance_id":"second","bot_self_id":"42"},"target":{"type":"actor","actor":{"platform":"one_bot","kind":"user","scheme":"native","value":"123"}},"message":"**hello**","message_type":"markdown"});
    super::super::tasks::create_definition(
        db,
        &nuono_app::scheduled_tasks::TaskDefinition {
            name: "native".into(),
            cron_expr: "0 0 8 * * *".into(),
            timezone: "Asia/Shanghai".into(),
            action_type: "send_native_message".into(),
            action_params: Some(request.to_string()),
            bot_id: None,
        },
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn manual_delivery_is_exact_and_duplicate_requests_do_not_send_twice() {
    let db = crate::test_support::test_db(2).await;
    let task = native_task(&db).await;
    let (scheduler, mut events) = Scheduler::new(db.clone());
    let scheduler = Arc::new(scheduler);
    scheduler.start().await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (first, duplicate) = tokio::join!(
        scheduler.run_once(task.id, &id),
        scheduler.run_once(task.id, &id)
    );
    assert_eq!(first.unwrap().id, duplicate.unwrap().id);
    let delivery = events.recv().await.unwrap();
    assert!(events.try_recv().is_err());
    // 重复调用 start 不得把已准入且正在运行中的任务误标为中断（interrupted）。
    scheduler.start().await.unwrap();
    let router = RuntimeActionRouter::new();
    let selected = Arc::new(RecordingSink::default());
    let other = Arc::new(RecordingSink::default());
    router.register(RuntimeBotRoute::new("onebot", "first", "42"), other.clone());
    router.register(
        RuntimeBotRoute::new("onebot", "second", "42"),
        selected.clone(),
    );
    handle_scheduled_delivery(&delivery, &db, &DashMap::new(), &router)
        .await
        .unwrap();
    handle_scheduled_delivery(&delivery, &db, &DashMap::new(), &router)
        .await
        .unwrap();
    assert_eq!(selected.0.lock().unwrap().len(), 1);
    assert!(other.0.lock().unwrap().is_empty());
    let runs = nuono_storage::task_runs::list(&db, task.id).await.unwrap();
    assert_eq!(runs[0].status, "succeeded");
    // OneBot Markdown 降级为纯文本发送的详情会记录在持久化执行历史中。
    assert!(runs[0].detail.as_ref().unwrap().contains("降级"));
    assert_eq!(
        scheduler.run_once(task.id, &id).await.unwrap().status,
        "succeeded"
    );
    assert!(events.try_recv().is_err());
    scheduler.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn full_manual_queue_has_a_terminal_receipt_and_is_never_replayed() {
    let db = crate::test_support::test_db(2).await;
    let task = native_task(&db).await;
    let (scheduler, mut events) = Scheduler::new(db.clone());
    let id = uuid::Uuid::new_v4().to_string();
    scheduler.run_once(task.id, &id).await.unwrap();
    let queued = events.recv().await.unwrap();
    for _ in 0..EVENT_CAPACITY {
        scheduler.event_tx.try_send(queued.clone()).unwrap();
    }
    let rejected_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        scheduler
            .run_once(task.id, &rejected_id)
            .await
            .unwrap()
            .status,
        "rejected"
    );
    events.recv().await.unwrap();
    assert_eq!(
        scheduler
            .run_once(task.id, &rejected_id)
            .await
            .unwrap()
            .status,
        "rejected"
    );
    assert_eq!(events.len(), EVENT_CAPACITY - 1);
    scheduler.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
#[test]
fn native_policy_keys_include_channels_and_canonicalize_onebot_aliases() {
    let target:crate::domain::DeliveryTarget=serde_json::from_value(json!({"type":"conversation","conversation":{"type":"group","group":{"platform":"one_bot","kind":"group","scheme":"native","value":"00123"}}})).unwrap();
    assert_eq!(scheduled_group_id(&target).unwrap(), Some("123".into()));
    let channel:crate::domain::DeliveryTarget=serde_json::from_value(json!({"type":"channel","channel":{"platform":"discord","kind":"channel","scheme":"snowflake","value":"123"}})).unwrap();
    assert_eq!(scheduled_group_id(&channel).unwrap(), Some("123".into()));
}
