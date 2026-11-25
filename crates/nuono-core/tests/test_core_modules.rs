use nuono_core::api::MessageContext;
use nuono_core::api::waiter::WaitManager;
use nuono_core::core::bot_session::{BotSessionManager, SendActor, SendPriority};
use nuono_core::core::config_change::{ConfigChangeEvent, ConfigChangeType};
use nuono_core::core::event_kernel::{
    RuntimeEventContext, RuntimeEventPipeline, RuntimeEventStage, RuntimeEventStageHandler,
};
use nuono_core::core::model::{MessageReceive, UserType};
use nuono_core::core::platform::{
    MessageType, SenderInfo, UnifiedEvent, UnifiedMessage, UnifiedSegment,
};
use nuono_core::core::runtime_stages::WaitSessionRuntimeStage;
use nuono_core::core::task_executor::{TaskExecutor, TaskFn, TaskMetadata};
use nuono_core::plugin::PluginResponse;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_bot_session_manager_register_unregister() {
    let manager = BotSessionManager::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(10);
    let actor = std::sync::Arc::new(SendActor::new(123, tx));

    assert!(!manager.is_online(123));
    assert_eq!(manager.active_count(), 0);

    manager.register(123, actor);
    assert!(manager.is_online(123));
    assert_eq!(manager.active_count(), 1);

    manager.unregister(123);
    assert!(!manager.is_online(123));
    assert_eq!(manager.active_count(), 0);
}

#[tokio::test]
async fn test_bot_session_manager_active_bot_ids() {
    let manager = BotSessionManager::new();
    let (tx1, _rx1) = tokio::sync::mpsc::channel(10);
    let (tx2, _rx2) = tokio::sync::mpsc::channel(10);

    manager.register(100, std::sync::Arc::new(SendActor::new(100, tx1)));
    manager.register(200, std::sync::Arc::new(SendActor::new(200, tx2)));

    let mut ids = manager.active_bot_ids();
    ids.sort();
    assert_eq!(ids, vec![100, 200]);
}

#[test]
fn test_send_actor_priority_order() {
    assert!(SendPriority::System < SendPriority::High);
    assert!(SendPriority::High < SendPriority::Normal);
    assert!(SendPriority::Normal < SendPriority::Low);
}

#[tokio::test]
async fn test_task_executor_submit_and_complete() {
    let executor = TaskExecutor::new(2);

    let metadata = TaskMetadata::new("test_task", "test_plugin", 5);
    let task_id = metadata.task_id.clone();

    let func: TaskFn = Box::new(|| Box::pin(async { Ok(json!({"result": "success"})) }));

    let result_id = executor.submit(metadata, func).await.unwrap();
    assert_eq!(result_id, task_id);

    tokio::time::sleep(Duration::from_millis(100)).await;

    let stats = executor.stats().await;
    assert_eq!(stats.completed, 1);
}

#[tokio::test]
async fn test_task_executor_multiple_tasks() {
    let executor = TaskExecutor::new(1);

    for i in 0..3 {
        let metadata = TaskMetadata::new(format!("task_{}", i), "test_plugin", i);
        let func: TaskFn = Box::new(move || {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(json!({"task": i}))
            })
        });
        executor.submit(metadata, func).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    let stats = executor.stats().await;
    assert_eq!(stats.completed, 3);
}

#[test]
fn test_task_metadata_builder() {
    let metadata = TaskMetadata::new("test", "plugin", 5)
        .with_service_name("service")
        .with_timeout(Duration::from_secs(30));

    assert_eq!(metadata.name, "test");
    assert_eq!(metadata.plugin_name, "plugin");
    assert_eq!(metadata.service_name, Some("service".to_string()));
    assert!(metadata.timeout.is_some());
}

#[test]
fn test_config_change_event_builder() {
    let event = ConfigChangeEvent::new(
        ConfigChangeType::PluginEnabled,
        "test_plugin",
        Some(json!(false)),
        json!(true),
    )
    .with_changed_by("admin")
    .with_reason("测试启用");

    assert_eq!(event.change_type, ConfigChangeType::PluginEnabled);
    assert_eq!(event.target, "test_plugin");
    assert_eq!(event.old_value, Some(json!(false)));
    assert_eq!(event.new_value, json!(true));
    assert_eq!(event.changed_by, Some("admin".to_string()));
}

#[test]
fn test_config_change_types_different() {
    assert_ne!(
        ConfigChangeType::PluginEnabled,
        ConfigChangeType::ServiceEnabled
    );
    assert_ne!(
        ConfigChangeType::PluginConfig,
        ConfigChangeType::GlobalConfig
    );
}

#[test]
fn test_runtime_event_pipeline_order() {
    let pipeline = RuntimeEventPipeline::default();
    assert_eq!(
        pipeline.stages(),
        &[
            RuntimeEventStage::WaitSession,
            RuntimeEventStage::Filter,
            RuntimeEventStage::CoreCommands,
            RuntimeEventStage::PluginHost,
            RuntimeEventStage::TriggerDispatch,
            RuntimeEventStage::Matcher,
            RuntimeEventStage::AgentRuntime,
            RuntimeEventStage::EventPublish,
        ]
    );
}

#[test]
fn test_message_context_preserves_channel_id() {
    let event = json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": 42,
        "user_id": 1001,
        "channel_id": "123456789012345678",
        "message_id": "987654321098765432",
        "raw_message": "ping"
    });

    let ctx = MessageContext::from_onebot_json(&event).unwrap();

    assert_eq!(ctx.channel_id.as_deref(), Some("123456789012345678"));
    assert_eq!(ctx.message_id, "987654321098765432");
}

#[test]
fn test_plugin_text_reply_preserves_channel_id() {
    let event = json!({
        "post_type": "message",
        "message_type": "private",
        "user_id": 1001,
        "channel_id": "123456789012345678"
    });

    let PluginResponse::Reply(action) = PluginResponse::text(&event, "pong") else {
        panic!("expected reply action");
    };

    assert_eq!(action["params"]["channel_id"], json!("123456789012345678"));
    assert_eq!(action["params"]["message_type"], json!("private"));
}

#[test]
fn test_feishu_runtime_json_preserves_native_identities() {
    let event = UnifiedEvent::Message(UnifiedMessage {
        message_id: "om_123".to_string(),
        self_id: 0,
        user_id: 0,
        group_id: None,
        content: "ping".to_string(),
        message_type: MessageType::Group,
        sub_type: String::new(),
        sender: SenderInfo {
            user_id: "ou_123".to_string(),
            nickname: None,
            card: None,
            role: None,
        },
        time: 0,
        raw: json!({
            "app_id": "cli_app",
            "open_id": "ou_123",
            "chat_id": "oc_123",
            "channel_id": "oc_123"
        }),
        segments: vec![UnifiedSegment::Text {
            text: "ping".to_string(),
        }],
    });

    let runtime_event =
        nuono_core::core::runtime_projection::unified_event_to_runtime_json("feishu", &event);

    assert_eq!(runtime_event["self_id"], json!("cli_app"));
    assert_eq!(runtime_event["user_id"], json!("ou_123"));
    assert_eq!(runtime_event["group_id"], json!("oc_123"));
    assert_eq!(runtime_event["chat_id"], json!("oc_123"));
    assert_eq!(runtime_event["channel_id"], json!("oc_123"));
}

#[tokio::test]
async fn test_wait_session_runtime_stage_blocks_waiting_session() {
    let wait_manager = Arc::new(WaitManager::new());
    let stage = WaitSessionRuntimeStage::new(wait_manager.clone());
    let event = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 100,
        "user_id": 200,
        "message": "reply",
        "raw_message": "reply"
    });
    let session_id = nuono_core::onebot::build_session_id(12345, &event);
    let rx = wait_manager.register_wait(session_id, Duration::from_secs(30));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);

    let mut ctx = RuntimeEventContext {
        locale: nuono_app::localization::LocaleContext::default(),
        bot_id: 12345,
        raw_text: "{}",
        event: event.clone(),
        bot_sender: Some(tx),
        action_sink: None,
        action_transport: None,
        domain_event: None,
        unified_event: None,
        resolved_user_id: None,
        resolved_identity_id: None,
        blocked: false,
        is_message: true,
        reply_attempts: std::sync::atomic::AtomicUsize::new(0),
        reply_ok: std::sync::atomic::AtomicUsize::new(0),
    };

    stage.handle(&mut ctx).await.unwrap();

    assert!(ctx.blocked);
    assert_eq!(rx.await.unwrap(), event);
    assert_eq!(wait_manager.wait_count(), 0);
}

#[test]
fn test_message_receive_defaults() {
    let msg = MessageReceive::default();
    assert_eq!(msg.bot_id, "Bot");
    assert_eq!(msg.user_type, UserType::Group);
}

#[test]
fn test_user_type_string_conversion() {
    assert_eq!(UserType::Group.as_str(), "group");
    assert_eq!(UserType::Direct.as_str(), "direct");
    assert_eq!(UserType::Channel.as_str(), "channel");
    assert_eq!(UserType::SubChannel.as_str(), "sub_channel");
}

#[test]
fn test_user_type_is_direct_check() {
    assert!(!UserType::Group.is_direct());
    assert!(UserType::Direct.is_direct());
    assert!(!UserType::Channel.is_direct());
}
