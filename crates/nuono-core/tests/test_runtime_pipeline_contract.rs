use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use nuono_adapter_fixture::FixtureAdapter as OneBotAdapter;
use nuono_core::api::Bot;
use nuono_core::api::waiter::{WaitManager, WaitScope};
use nuono_core::config::PluginConfigManager;
use nuono_core::core::RuntimeBotRoute;
use nuono_core::core::event_bus::{
    Event, EventBus, EventHandler as BusEventHandler, EventResult, EventType,
};
use nuono_core::core::paths::Paths;
use nuono_core::core::{
    ApiClient, CommandDispatchServices, EventHandler, EventKernel, EventKernelPipeline,
    FilterHandler, FrameworkControl, MatchData, MatcherRegistry, PermissionManager,
    RuntimeEventPipeline, RuntimeEventStage, SessionManager, TaskExecutor, Trigger, TriggerHandler,
    TriggerRegistry, TriggerType,
};
use nuono_core::onebot::Message;
use nuono_core::plugin::Event as PluginEvent;
use nuono_core::{ConfigManager, Database};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct RecordingBusHandler {
    count: Arc<AtomicUsize>,
    stop: bool,
}

#[async_trait]
impl BusEventHandler for RecordingBusHandler {
    async fn handle(&self, _event: &Event) -> anyhow::Result<EventResult> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(if self.stop {
            EventResult::Stop
        } else {
            EventResult::Continue
        })
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::MessageReceived]
    }
}

struct RecordingTriggerHandler {
    count: Arc<AtomicUsize>,
    fail: bool,
}

struct RouteCaptureNativeHandler {
    route: Arc<std::sync::Mutex<Option<RuntimeBotRoute>>>,
}

#[derive(Default)]
struct VirtualConversationSink(std::sync::Mutex<Vec<Value>>);

#[async_trait]
impl nuono_core::core::RuntimeActionSink for VirtualConversationSink {
    async fn send_action(&self, action: Value) -> nuono_core::error::Result<()> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }

    async fn call_action(&self, action: Value) -> nuono_core::error::Result<Value> {
        self.send_action(action).await?;
        Ok(json!({"message_id": "virtual-reply"}))
    }
}

struct VirtualConversationCommand {
    wait: bool,
}

#[async_trait]
impl nuono_core::core::CommandHandler for VirtualConversationCommand {
    async fn handle(&self, bot: &Bot, event: &PluginEvent<'_>, _: MatchData) -> anyhow::Result<()> {
        bot.send("legacy reply").await?;
        if self.wait {
            let response = bot
                .wait_for_reply(std::time::Duration::from_secs(5))
                .await?;
            bot.send(format!("received: {}", PluginEvent::new(&response).text()))
                .await?;
        } else {
            let target = event.message_target()?;
            bot.send_routed(&target, "routed reply").await?;
            let identity = nuono_core::domain::IdentityRef::new(
                nuono_core::domain::Platform::OneBot,
                if event.group_id_str().is_some() {
                    nuono_core::domain::IdentityKind::Group
                } else {
                    nuono_core::domain::IdentityKind::User
                },
                nuono_core::domain::IdentityScheme::Native,
                event
                    .group_id_str()
                    .or_else(|| event.user_id_str())
                    .unwrap(),
                None::<String>,
            )?;
            let native_target = if event.group_id_str().is_some() {
                nuono_core::domain::DeliveryTarget::conversation(
                    nuono_core::domain::ConversationRef::group(identity)?,
                )?
            } else {
                nuono_core::domain::DeliveryTarget::actor(identity)?
            };
            bot.send_native_routed(&target.route, &native_target, "native reply")
                .await?;
        }
        Ok(())
    }
}

fn virtual_message(bot_id: i64, group: bool, text: &str) -> Value {
    let mut event = json!({
        "time": 1,
        "post_type": "message",
        "message_type": if group { "group" } else { "private" },
        "sub_type": if group { "normal" } else { "friend" },
        "self_id": bot_id,
        "user_id": -9000000000002_i64,
        "message_id": -1,
        "message": [{"type": "text", "data": {"text": text}}],
        "raw_message": text,
        "sender": {"user_id": -9000000000002_i64, "nickname": "virtual user", "role": "member"}
    });
    if group {
        event["group_id"] = json!(-9000000000003_i64);
    }
    event
}

async fn process_virtual_message(
    fixture: &RuntimeFixture,
    route: &RuntimeBotRoute,
    sink: Arc<VirtualConversationSink>,
    event: Value,
) -> nuono_core::core::RuntimeEventProcessResult {
    use nuono_core::core::platform::PlatformAdapter;
    let unified = OneBotAdapter.parse_incoming(&event.to_string()).unwrap();
    fixture
        .kernel
        .process_unified_event_with_route("onebot", unified, Some(sink), Some(route))
        .await
        .unwrap()
}

#[async_trait]
impl nuono_core::core::CommandHandler for RouteCaptureNativeHandler {
    async fn handle(
        &self,
        _bot: &Bot,
        event: &PluginEvent<'_>,
        _match_data: MatchData,
    ) -> anyhow::Result<()> {
        *self.route.lock().unwrap() = event.route();
        Ok(())
    }
}

#[async_trait]
impl TriggerHandler for RecordingTriggerHandler {
    async fn handle(
        &self,
        _bot: &Bot,
        _event: &Message,
        _match_data: MatchData,
    ) -> anyhow::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            anyhow::bail!("expected handler failure");
        }
        Ok(())
    }
}

struct RuntimeFixture {
    kernel: EventKernel,
    event_bus: Arc<EventBus>,
    wait_manager: Arc<WaitManager>,
    trigger_registry: Arc<TriggerRegistry>,
    plugin_host: Arc<nuono_core::core::plugin_host::PluginHost>,
    matcher_registry: Arc<MatcherRegistry>,
    core_commands: Arc<nuono_core::core_commands::CoreCommands>,
    config: Arc<ConfigManager>,
    db: Database,
    root: std::path::PathBuf,
}

#[derive(Default)]
struct RecordingRestart(AtomicUsize);

#[async_trait]
impl nuono_core::core_commands::RestartService for RecordingRestart {
    async fn prepare_restart(
        &self,
        _: &str,
        _: Option<nuono_core::core_commands::RestartReplyTarget>,
    ) -> Result<nuono_core::core_commands::PreparedRestart, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(nuono_core::core_commands::PreparedRestart::new(|| async {
            Ok(())
        }))
    }
}

impl RuntimeFixture {
    async fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("nuono-runtime-pipeline-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let db = nuono_core::test_support::test_db(1).await;

        let config = Arc::new(
            ConfigManager::load(Some(root.join("runtime.json")))
                .await
                .unwrap(),
        );

        let event_bus = Arc::new(EventBus::new());
        let permission_manager = Arc::new(PermissionManager::new(db.clone()));
        let wait_manager = Arc::new(WaitManager::new());
        let plugin_config_manager = Arc::new(PluginConfigManager::new(root.join("plugin-config")));
        let plugin_host = nuono_core::core::plugin_host::PluginHost::new(
            db.clone(),
            event_bus.clone(),
            permission_manager.clone(),
            Paths::with_root(&root),
            wait_manager.clone(),
            FrameworkControl::new(),
            plugin_config_manager,
        )
        .await
        .unwrap();
        let filter_handler = Arc::new(FilterHandler::new(db.clone()));
        let trigger_registry = plugin_host.trigger_registry();
        let mut event_handler = EventHandler::new(
            db.clone(),
            CommandDispatchServices {
                trigger_registry: trigger_registry.clone(),
                cooldown: plugin_host.cooldown_tracker.clone(),
                stats: plugin_host.command_stats.clone(),
            },
            permission_manager.clone(),
            event_bus.clone(),
            wait_manager.clone(),
            Arc::new(ApiClient::new()),
        );
        event_handler.set_plugin_host(plugin_host.clone());

        let matcher_registry = Arc::new(MatcherRegistry::new(Arc::new(SessionManager::new())));
        let core_commands = Arc::new(
            nuono_core::core_commands::CoreCommands::new(
                config.clone(),
                permission_manager.clone(),
                nuono_core::SystemMonitor::new(),
            )
            .with_status_source(db.clone(), plugin_host.clone(), root.join("data")),
        );
        let kernel = EventKernel::new(
            db.clone(),
            event_bus.clone(),
            permission_manager,
            plugin_host.clone(),
            Arc::new(TaskExecutor::new(1)),
            config.clone(),
            EventKernelPipeline {
                filter_handler,
                wait_manager: wait_manager.clone(),
                event_handler: Arc::new(event_handler),
                matcher_registry: matcher_registry.clone(),
                adapter: Arc::new(OneBotAdapter),
                core_commands: core_commands.clone(),
            },
        );

        Self {
            kernel,
            event_bus,
            wait_manager,
            trigger_registry,
            plugin_host,
            matcher_registry,
            core_commands,
            config,
            db,
            root,
        }
    }

    async fn cleanup(self) {
        let Self {
            kernel, db, root, ..
        } = self;
        drop(kernel);
        let _ = db.shutdown().await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}

fn message_event() -> Value {
    json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": 42,
        "user_id": 7,
        "message_id": "m-1",
        "message": [],
        "raw_message": "hello",
        "sender": {"user_id": 7}
    })
}

fn notice_event() -> Value {
    json!({
        "post_type": "notice",
        "notice_type": "group_increase",
        "self_id": 42
    })
}

fn trigger_event() -> Value {
    json!({
        "time": 1,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "self_id": 42,
        "user_id": 7,
        "message_id": 1,
        "message": [{"type": "text", "data": {"text": "hello"}}],
        "raw_message": "hello",
        "sender": {"user_id": 7, "nickname": "tester"}
    })
}

#[tokio::test]
async fn explicit_plugin_namespace_bypasses_waits_and_fallbacks_without_changing_block() {
    let fixture = RuntimeFixture::new().await;
    let owner = "PrefixOwner";
    fixture
        .plugin_host
        .register_plugin_config(
            nuono_core::core::Plugins::new(owner).with_prefix(vec!["url".into()]),
        )
        .await;
    let called = Arc::new(AtomicUsize::new(0));
    fixture.trigger_registry.register(
        Trigger::new(
            TriggerType::FullMatch,
            "帮助",
            "",
            false,
            false,
            Arc::new(RecordingTriggerHandler {
                count: called.clone(),
                fail: false,
            }),
        )
        .with_plugin_name(owner),
    );
    let fallback = Arc::new(AtomicUsize::new(0));
    let fallback_called = fallback.clone();
    fixture
        .matcher_registry
        .register(
            nuono_core::core::Matcher::new("catch-all").handle_fn(move |_| {
                let fallback_called = fallback_called.clone();
                async move {
                    fallback_called.fetch_add(1, Ordering::SeqCst);
                    nuono_core::core::HandlerResult::Stop
                }
            }),
        )
        .await;
    let published = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: published.clone(),
        stop: false,
    }));
    for (text, expected_calls) in [("URL帮助", 1), ("url不存在的功能", 1)] {
        let mut event = trigger_event();
        event["raw_message"] = json!(text);
        event["message"] = json!([{"type":"text", "data":{"text":text}}]);
        let session = nuono_core::onebot::build_session_id(42, &event);
        let mut wait = fixture
            .wait_manager
            .register_wait(session.clone(), std::time::Duration::from_secs(30));
        let (tx, mut replies) = mpsc::channel(4);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event, tx)
            .await
            .unwrap();
        assert!(!result.blocked, "路由选择不能把 block=false 改为消费");
        assert!(matches!(
            wait.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(called.load(Ordering::SeqCst), expected_calls);
        assert_eq!(fallback.load(Ordering::SeqCst), 0);
        assert!(
            replies.try_recv().is_err(),
            "普通插件命令不能流入 Agent 兜底"
        );
        fixture.wait_manager.cancel(&session);
    }
    assert_eq!(
        published.load(Ordering::SeqCst),
        2,
        "block=false 仍允许最终发布"
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn conflicting_plugin_prefix_replies_before_callbacks_and_preserves_pending_wait() {
    let fixture = RuntimeFixture::new().await;
    for name in ["PrefixA", "PrefixB"] {
        fixture
            .plugin_host
            .register_plugin_config(
                nuono_core::core::Plugins::new(name).with_prefix(vec!["ww".into()]),
            )
            .await;
    }
    let called = Arc::new(AtomicUsize::new(0));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Message,
        "",
        "",
        false,
        false,
        Arc::new(RecordingTriggerHandler {
            count: called.clone(),
            fail: false,
        }),
    ));
    let mut event = trigger_event();
    event["raw_message"] = json!("WW帮助");
    event["message"] = json!([{"type":"text", "data":{"text":"WW帮助"}}]);
    let session = nuono_core::onebot::build_session_id(42, &event);
    let mut wait = fixture
        .wait_manager
        .register_wait(session.clone(), std::time::Duration::from_secs(30));
    let (tx, mut replies) = mpsc::channel(4);
    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();
    assert!(result.blocked);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert!(matches!(
        wait.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let nuono_app::transport::TransportFrame::Text(reply) = replies.try_recv().unwrap() else {
        panic!("expected a conflict reply")
    };
    let reply: Value = serde_json::from_str(&reply).unwrap();
    let message = reply_plain_text(&reply);
    assert!(message.contains("冲突"), "{message}");
    assert!(message.contains("PrefixA") && message.contains("PrefixB"));
    fixture.wait_manager.cancel(&session);
    fixture.cleanup().await;
}

fn reply_plain_text(reply: &Value) -> String {
    reply["params"]["message"]
        .as_array()
        .expect("OneBot reply uses a segment array")
        .iter()
        .filter(|segment| segment["type"] == "text")
        .map(|segment| segment["data"]["text"].as_str().expect("text segment"))
        .collect()
}

#[test]
fn default_runtime_stage_order_is_contractual() {
    assert_eq!(
        RuntimeEventPipeline::default().stages(),
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

#[tokio::test]
async fn core_commands_take_precedence_over_waits_and_plugin_triggers() {
    let fixture = RuntimeFixture::new().await;
    fixture
        .config
        .attach_database(fixture.db.clone())
        .await
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Message,
        "",
        "",
        false,
        true,
        Arc::new(RecordingTriggerHandler {
            count: count.clone(),
            fail: false,
        }),
    ));
    fixture
        .core_commands
        .set_enabled("status", false)
        .await
        .unwrap();
    for (command, expected) in [
        ("nuono帮助", "nuono 状态"),
        ("nuono状态", "已禁用"),
        ("nuono重启", "该指令仅限机器人管理员及以上使用。"),
    ] {
        let mut event = trigger_event();
        event["raw_message"] = json!(command);
        event["message"] = json!([{"type":"text", "data":{"text":command}}]);
        let session = nuono_core::onebot::build_session_id(42, &event);
        let mut wait = fixture
            .wait_manager
            .register_wait(session.clone(), std::time::Duration::from_secs(30));
        let (tx, mut rx) = mpsc::channel(4);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event, tx)
            .await
            .unwrap();
        assert!(result.blocked);
        let frame = rx.try_recv().expect("核心指令应回复");
        let nuono_app::transport::TransportFrame::Text(text) = frame else {
            panic!("expected text action")
        };
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert!(reply_plain_text(&reply).contains(expected), "{reply}");
        assert!(matches!(
            wait.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(fixture.wait_manager.has_wait(&session));
        fixture.wait_manager.cancel(&session);
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn status_card_records_consumed_input_and_successful_output_once() {
    use sea_orm::EntityTrait;
    let fixture = RuntimeFixture::new().await;
    let (tx, mut rx) = mpsc::channel(4);
    let mut event = message_event();
    event["raw_message"] = json!("nuono帮助");
    event["message"] = json!([{"type":"text", "data":{"text":"nuono帮助"}}]);
    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event.clone(), tx.clone())
        .await
        .unwrap();
    assert!(result.blocked);
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("help command must produce a reply")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let stats = nuono_storage::status_stats::get_status_stats(
                &fixture.db,
                "42",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            if stats.current.received == 1 && stats.current.sent == 1 && stats.current.commands == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("core command metadata should flush");
    let rows = nuono_storage::entity::messages::Entity::find()
        .all(fixture.db.orm())
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.text_content.is_empty() && row.raw_payload_json.is_none())
    );

    event["message_id"] = json!(event["message_id"].as_i64().unwrap_or(1) + 1);
    event["raw_message"] = json!("nuono状态");
    event["message"] = json!([{"type":"text", "data":{"text":"nuono状态"}}]);
    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();
    assert!(result.blocked);
    let nuono_app::transport::TransportFrame::Text(text) =
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("status command must produce a reply")
            .unwrap()
    else {
        panic!("expected image action");
    };
    let action: Value = serde_json::from_str(&text).unwrap();
    let segment = action["params"]["message"]
        .as_array()
        .unwrap()
        .iter()
        .find(|segment| segment["type"] == "image")
        .expect("status must send image through the production transport");
    use base64::Engine;
    let source = segment["data"]["file"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.strip_prefix("base64://").unwrap())
        .unwrap();
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 1000);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let stats = nuono_storage::status_stats::get_status_stats(
                &fixture.db,
                "42",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            if stats.current.received == 2 && stats.current.sent == 2 && stats.current.commands == 2
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("image send should count once");
    fixture.cleanup().await;
}

#[tokio::test]
async fn status_stats_include_fast_wait_replies_without_saving_their_body() {
    use nuono_core::core::platform::PlatformAdapter;
    use sea_orm::EntityTrait;
    let fixture = RuntimeFixture::new().await;
    let mut event = message_event();
    event["raw_message"] = json!("private-wait-body");
    event["message"] = json!([{"type":"text", "data":{"text":"private-wait-body"}}]);
    let session = nuono_core::onebot::build_session_id(42, &event);
    let waiting = fixture
        .wait_manager
        .register_wait(session, std::time::Duration::from_secs(10));
    let unified = OneBotAdapter.parse_incoming(&event.to_string()).unwrap();
    assert!(
        fixture
            .kernel
            .try_deliver_waiting_unified_event("onebot", &unified, None, None)
            .await
            .unwrap()
    );
    waiting.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let stats = nuono_storage::status_stats::get_status_stats(
                &fixture.db,
                "42",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            if stats.current.received == 1 {
                assert_eq!(stats.current.active_users, 1);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let rows = nuono_storage::entity::messages::Entity::find()
        .all(fixture.db.orm())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text_content, "");
    assert_eq!(rows[0].raw_payload_json, None);
    fixture.cleanup().await;
}

#[tokio::test]
async fn restart_uses_bound_owner_identity_and_respects_revocation() {
    let fixture = RuntimeFixture::new().await;
    fixture
        .config
        .attach_database(fixture.db.clone())
        .await
        .unwrap();
    let restart = Arc::new(RecordingRestart::default());
    fixture.core_commands.set_restart_service(restart.clone());

    let owner = fixture
        .db
        .create_user(nuono_core::db::NewUser {
            username: "restart-owner".into(),
            email: None,
            password: "restart-owner-password".into(),
            role: "owner".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let identity = nuono_core::domain::IdentityRef::new(
        nuono_core::domain::Platform::OneBot,
        nuono_core::domain::IdentityKind::User,
        nuono_core::domain::IdentityScheme::Native,
        "7",
        None::<String>,
    )
    .unwrap();
    let bound = fixture
        .db
        .admin_bind_platform_identity(owner.id, owner.id, &identity)
        .await
        .unwrap();

    for (revoked, expected) in [
        (false, "已安排"),
        (true, "该指令仅限机器人管理员及以上使用。"),
    ] {
        if revoked {
            fixture
                .db
                .admin_unbind_platform_identity(
                    owner.id,
                    owner.id,
                    bound.id.parse().unwrap(),
                    bound.revision.parse().unwrap(),
                )
                .await
                .unwrap();
        }
        let mut event = trigger_event();
        event["raw_message"] = json!("nuono重启");
        event["message"] = json!([{"type":"text", "data":{"text":"nuono重启"}}]);
        let (tx, mut rx) = mpsc::channel(4);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event, tx)
            .await
            .unwrap();
        assert!(result.blocked);
        let nuono_app::transport::TransportFrame::Text(text) = rx.try_recv().unwrap() else {
            panic!("expected text action")
        };
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert!(reply_plain_text(&reply).contains(expected), "{reply}");
    }
    assert_eq!(restart.0.load(Ordering::SeqCst), 1);
    fixture.cleanup().await;
}

#[tokio::test]
async fn core_command_switches_survive_restart_and_concurrent_updates() {
    let fixture = RuntimeFixture::new().await;
    let path = fixture.root.join("runtime.json");
    let original = std::fs::read(&path).unwrap();
    fixture
        .config
        .attach_database(fixture.db.clone())
        .await
        .unwrap();
    let (help, status) = tokio::join!(
        fixture.core_commands.set_enabled("help", false),
        fixture.core_commands.set_enabled("status", false),
    );
    help.unwrap();
    status.unwrap();
    let disabled_keys = |commands: Vec<nuono_core::core_commands::ResolvedCoreCommand>| {
        commands
            .into_iter()
            .filter(|item| !item.enabled)
            .map(|item| item.definition.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        disabled_keys(fixture.core_commands.resolved().await.1).len(),
        2
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let restarted = ConfigManager::load(Some(path)).await.unwrap();
    restarted.attach_database(fixture.db.clone()).await.unwrap();
    assert_eq!(
        restarted.snapshot().await.core_commands.policies,
        fixture.config.snapshot().await.core_commands.policies
    );
    assert!(
        fixture
            .core_commands
            .set_enabled("nonexistent", false)
            .await
            .is_err()
    );
    fixture
        .core_commands
        .set_enabled("help", true)
        .await
        .unwrap();
    assert_eq!(
        disabled_keys(fixture.core_commands.resolved().await.1),
        vec!["status"]
    );
    let stored = fixture
        .db
        .get_global_config("runtime.settings.v1")
        .await
        .unwrap()
        .unwrap();
    let stored_disabled: Vec<&str> = stored["core_commands"]["policies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|policy| policy["enabled"] == false)
        .filter_map(|policy| policy["id"].as_str())
        .collect();
    assert_eq!(stored_disabled, vec!["status"]);
    drop(restarted);
    fixture.cleanup().await;
}

#[tokio::test]
async fn core_commands_still_pass_through_the_security_filter() {
    let fixture = RuntimeFixture::new().await;
    nuono_core::service::security::add_blacklist(&fixture.db, "user", "7", Some("test"))
        .await
        .unwrap();
    let mut event = trigger_event();
    event["raw_message"] = json!("nuono帮助");
    event["message"] = json!([{"type":"text", "data":{"text":"nuono帮助"}}]);
    let (tx, mut rx) = mpsc::channel(4);
    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();
    assert!(result.blocked);
    assert!(rx.try_recv().is_err());
    fixture.cleanup().await;
}

#[tokio::test]
async fn blocked_message_skips_later_stages_and_event_publish() {
    let fixture = RuntimeFixture::new().await;
    let count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: count.clone(),
        stop: false,
    }));

    let event = json!({
        "post_type": "message",
        "message_type": "group",
        "self_id": 42,
        "group_id": 100,
        "user_id": 7,
        "message_id": "m-wait",
        "message": [],
        "raw_message": "reply",
        "sender": {"user_id": 7}
    });
    let session_id = nuono_core::onebot::build_session_id(42, &event);
    let waiting_reply = fixture.wait_manager.clone();
    let receiver = waiting_reply.register_wait(session_id, std::time::Duration::from_secs(30));
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event.clone(), tx)
        .await
        .unwrap();

    assert!(result.blocked);
    assert!(result.is_message);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(receiver.await.unwrap(), event);
    fixture.cleanup().await;
}

#[tokio::test]
async fn non_message_events_do_not_run_message_stages_or_publish() {
    let fixture = RuntimeFixture::new().await;
    let count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: count.clone(),
        stop: false,
    }));
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &notice_event().to_string(), notice_event(), tx)
        .await
        .unwrap();

    assert!(!result.blocked);
    assert!(!result.is_message);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn event_bus_stop_does_not_write_back_to_runtime_blocked() {
    let fixture = RuntimeFixture::new().await;
    let count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: count.clone(),
        stop: true,
    }));
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &message_event().to_string(), message_event(), tx)
        .await
        .unwrap();

    assert!(!result.blocked);
    assert!(result.is_message);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    fixture.cleanup().await;
}

#[tokio::test]
async fn blocking_trigger_skips_event_publish() {
    let fixture = RuntimeFixture::new().await;
    let trigger_count = Arc::new(AtomicUsize::new(0));
    let publish_count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: publish_count.clone(),
        stop: false,
    }));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Command,
        "hello",
        "",
        true,
        false,
        Arc::new(RecordingTriggerHandler {
            count: trigger_count.clone(),
            fail: false,
        }),
    ));
    let event = trigger_event();
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();

    assert!(result.blocked);
    assert_eq!(trigger_count.load(Ordering::SeqCst), 1);
    assert_eq!(publish_count.load(Ordering::SeqCst), 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn non_blocking_trigger_continues_to_event_publish() {
    let fixture = RuntimeFixture::new().await;
    let trigger_count = Arc::new(AtomicUsize::new(0));
    let publish_count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: publish_count.clone(),
        stop: false,
    }));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Command,
        "hello",
        "",
        false,
        false,
        Arc::new(RecordingTriggerHandler {
            count: trigger_count.clone(),
            fail: false,
        }),
    ));
    let event = trigger_event();
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();

    assert!(!result.blocked);
    assert_eq!(trigger_count.load(Ordering::SeqCst), 1);
    assert_eq!(publish_count.load(Ordering::SeqCst), 1);
    fixture.cleanup().await;
}

#[tokio::test]
async fn native_string_identity_trigger_executes_and_blocks() {
    let fixture = RuntimeFixture::new().await;
    let trigger_count = Arc::new(AtomicUsize::new(0));
    let publish_count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: publish_count.clone(),
        stop: false,
    }));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Command,
        "hello",
        "",
        true,
        false,
        Arc::new(RecordingTriggerHandler {
            count: trigger_count.clone(),
            fail: false,
        }),
    ));
    let event = json!({
        "time": 1,
        "post_type": "message",
        "platform": "qq_official",
        "message_type": "group",
        "sub_type": "normal",
        "self_id": "bot-app-id",
        "user_id": "user-openid",
        "group_id": "group-openid",
        "message_id": "message-native-id",
        "message": [{"type": "text", "data": {"text": "hello"}}],
        "raw_message": "hello",
        "sender": {"user_id": "user-openid", "nickname": "tester"}
    });
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();

    assert!(result.blocked);
    assert_eq!(trigger_count.load(Ordering::SeqCst), 1);
    assert_eq!(publish_count.load(Ordering::SeqCst), 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn native_string_identity_to_me_trigger_matches_projected_mention() {
    let fixture = RuntimeFixture::new().await;
    let trigger_count = Arc::new(AtomicUsize::new(0));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Command,
        "hello",
        "",
        true,
        true,
        Arc::new(RecordingTriggerHandler {
            count: trigger_count.clone(),
            fail: false,
        }),
    ));
    let event = json!({
        "time": 1,
        "post_type": "message",
        "platform": "qq_official",
        "message_type": "group",
        "sub_type": "normal",
        "self_id": "bot-app-id",
        "user_id": "user-openid",
        "group_id": "group-openid",
        "message_id": "message-native-id",
        "message": [
            {"type": "at", "data": {"qq": "bot-app-id"}},
            {"type": "text", "data": {"text": "hello"}}
        ],
        "raw_message": "hello",
        "sender": {"user_id": "user-openid", "nickname": "tester"}
    });
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();

    assert!(result.blocked);
    assert_eq!(trigger_count.load(Ordering::SeqCst), 1);
    fixture.cleanup().await;
}

#[tokio::test]
async fn runtime_route_reaches_command_handler() {
    let fixture = RuntimeFixture::new().await;
    let captured = Arc::new(std::sync::Mutex::new(None));
    fixture.trigger_registry.register(
        nuono_core::core::Command::new(
            "login",
            Arc::new(RouteCaptureNativeHandler {
                route: captured.clone(),
            }),
        )
        .to_triggers()
        .pop()
        .unwrap(),
    );
    let event = json!({
        "time": 1,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "self_id": 42,
        "user_id": 7,
        "message_id": 1,
        "message": [{"type": "text", "data": {"text": "login"}}],
        "raw_message": "login",
        "sender": {"user_id": 7, "nickname": "tester"}
    });
    let route = RuntimeBotRoute::new("onebot_v11", "42", "42");
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event_with_route(42, &event.to_string(), event, tx, Some(&route))
        .await
        .unwrap();

    assert!(result.blocked);
    assert_eq!(*captured.lock().unwrap(), Some(route));
    fixture.cleanup().await;
}

#[tokio::test]
async fn sandbox_commands_capture_legacy_routed_and_native_replies_without_onebot() {
    let fixture = RuntimeFixture::new().await;
    fixture.trigger_registry.register(
        nuono_core::core::Command::new(
            "virtual-echo",
            Arc::new(VirtualConversationCommand { wait: false }),
        )
        .to_triggers()
        .pop()
        .unwrap(),
    );
    fixture.trigger_registry.register(
        nuono_core::core::Command::new(
            "virtual-mention",
            Arc::new(VirtualConversationCommand { wait: false }),
        )
        .with_to_me(true)
        .to_triggers()
        .pop()
        .unwrap(),
    );
    let router = fixture.kernel.action_router();
    assert!(router.is_empty());
    for group in [false, true] {
        let bot_id = if group {
            -9000000000011_i64
        } else {
            -9000000000001_i64
        };
        let route = RuntimeBotRoute::new("onebot", format!("sandbox-{bot_id}"), bot_id.to_string());
        let sink = Arc::new(VirtualConversationSink::default());
        let generation = router.register(route.clone(), sink.clone());
        let mut event = virtual_message(
            bot_id,
            group,
            if group {
                "virtual-mention"
            } else {
                "virtual-echo"
            },
        );
        if group {
            event["message"] = json!([
                {"type": "at", "data": {"qq": bot_id.to_string()}},
                {"type": "text", "data": {"text": "virtual-mention"}}
            ]);
        }
        let result = process_virtual_message(&fixture, &route, sink.clone(), event).await;
        assert!(
            result.blocked,
            "registered command must consume the message"
        );
        {
            let replies = sink.0.lock().unwrap();
            assert_eq!(replies.len(), 3, "{replies:?}");
            for (reply, expected) in
                replies
                    .iter()
                    .zip(["legacy reply", "routed reply", "native reply"])
            {
                assert_eq!(reply_plain_text(reply), expected);
                assert!(matches!(
                    reply["action"].as_str(),
                    Some("send_msg" | "send_private_msg" | "send_group_msg")
                ));
                let target_key = if group { "group_id" } else { "user_id" };
                assert_eq!(
                    reply["params"][target_key]
                        .as_i64()
                        .map(|id| id.to_string())
                        .or_else(|| reply["params"][target_key].as_str().map(str::to_owned))
                        .unwrap(),
                    if group {
                        "-9000000000003"
                    } else {
                        "-9000000000002"
                    }
                );
            }
        }
        assert!(router.unregister(&route, generation));
    }
    assert!(router.is_empty());
    fixture.cleanup().await;
}

#[tokio::test]
async fn sandbox_inline_wait_accepts_followup_and_other_sessions_cannot_satisfy_it() {
    let fixture = RuntimeFixture::new().await;
    fixture
        .plugin_host
        .register_plugin_config(nuono_core::core::Plugins::new("VirtualConversation"))
        .await;
    fixture.trigger_registry.register(
        nuono_core::core::Command::new(
            "virtual-wait",
            Arc::new(VirtualConversationCommand { wait: true }),
        )
        .to_triggers()
        .pop()
        .unwrap()
        .with_plugin_name("VirtualConversation"),
    );
    let bot_id = -9000000000001_i64;
    let route = RuntimeBotRoute::new("onebot", "sandbox-wait", bot_id.to_string());
    let sink = Arc::new(VirtualConversationSink::default());
    let packet = virtual_message(bot_id, false, "virtual-wait");
    let session_id = nuono_core::onebot::build_session_id(bot_id, &packet);
    let initial = process_virtual_message(&fixture, &route, sink.clone(), packet);
    let followup = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fixture
                .wait_manager
                .snapshot_scoped(&route, &session_id)
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("command must register its wait");
        let other_route = RuntimeBotRoute::new("onebot", "sandbox-other", "-9000000000011");
        process_virtual_message(
            &fixture,
            &other_route,
            Arc::new(VirtualConversationSink::default()),
            virtual_message(-9000000000011, false, "wrong session"),
        )
        .await;
        assert!(
            fixture
                .wait_manager
                .snapshot_scoped(&route, &session_id)
                .is_some()
        );
        let result = process_virtual_message(
            &fixture,
            &route,
            sink.clone(),
            virtual_message(bot_id, false, "next reply"),
        )
        .await;
        assert!(result.blocked, "wait stage must consume followup");
    };
    let (result, ()) = tokio::join!(initial, followup);
    assert!(result.blocked);
    assert!(
        fixture
            .wait_manager
            .snapshot_scoped(&route, &session_id)
            .is_none()
    );
    {
        let replies = sink.0.lock().unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(reply_plain_text(&replies[1]), "received: next reply");
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn failed_blocking_trigger_still_skips_event_publish() {
    let fixture = RuntimeFixture::new().await;
    let trigger_count = Arc::new(AtomicUsize::new(0));
    let publish_count = Arc::new(AtomicUsize::new(0));
    fixture.event_bus.subscribe(Arc::new(RecordingBusHandler {
        count: publish_count.clone(),
        stop: false,
    }));
    fixture.trigger_registry.register(Trigger::new(
        TriggerType::Command,
        "hello",
        "",
        true,
        false,
        Arc::new(RecordingTriggerHandler {
            count: trigger_count.clone(),
            fail: true,
        }),
    ));
    let event = trigger_event();
    let (tx, _rx) = mpsc::channel(1);

    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();

    assert!(result.blocked);
    assert_eq!(trigger_count.load(Ordering::SeqCst), 1);
    assert_eq!(publish_count.load(Ordering::SeqCst), 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn notices_and_requests_cannot_consume_a_private_message_wait() {
    let fixture = RuntimeFixture::new().await;
    let event = message_event();
    let session = nuono_core::onebot::build_session_id(42, &event);
    let mut waiting = fixture
        .wait_manager
        .register_wait(session.clone(), std::time::Duration::from_secs(30));
    for post in ["notice", "request"] {
        let notice = json!({"post_type":post, "self_id":42, "user_id":7, "group_id":100, "notice_type":"notify", "request_type":"group"});
        let (tx, _rx) = mpsc::channel(1);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &notice.to_string(), notice, tx)
            .await
            .unwrap();
        assert!(!result.is_message && !result.blocked);
        assert!(matches!(
            waiting.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(fixture.wait_manager.has_wait(&session));
    }
    let (tx, _rx) = mpsc::channel(1);
    assert!(
        fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event.clone(), tx)
            .await
            .unwrap()
            .blocked
    );
    assert_eq!(waiting.await.unwrap(), event);
    fixture.cleanup().await;
}

#[tokio::test]
async fn waiting_replies_recheck_blacklists_permissions_and_plugin_policy() {
    use nuono_core::core::{PluginArea, Plugins};
    for policy in [
        "blacklist",
        "banned",
        "permission",
        "plugin_pm",
        "area",
        "plugin_blacklist",
        "plugin_whitelist",
    ] {
        let fixture = RuntimeFixture::new().await;
        let mut config = Plugins::new("WaitingPolicy");
        fixture
            .plugin_host
            .register_plugin_config(config.clone())
            .await;
        let admin = fixture
            .db
            .create_user(nuono_core::db::NewUser {
                username: format!("wait-admin-{policy}"),
                email: None,
                password: "wait-admin-password".into(),
                role: "admin".into(),
                status: "active".into(),
                email_verified: true,
            })
            .await
            .unwrap();
        let identity = nuono_core::domain::IdentityRef::new(
            nuono_core::domain::Platform::OneBot,
            nuono_core::domain::IdentityKind::User,
            nuono_core::domain::IdentityScheme::Native,
            "7",
            None::<String>,
        )
        .unwrap();
        let bound = fixture
            .db
            .admin_bind_platform_identity(admin.id, admin.id, &identity)
            .await
            .unwrap();
        let mut event = message_event();
        event["platform"] = json!("onebot");
        let session = nuono_core::onebot::build_session_id(42, &event);
        let required = (policy == "permission")
            .then_some(nuono_core::core::command::PermissionLevel::BotAdmin);
        let waiting = fixture.wait_manager.register_scoped_wait(
            session.clone(),
            std::time::Duration::from_secs(30),
            WaitScope {
                plugin_name: Some("WaitingPolicy".into()),
                required_permission: required,
                ..WaitScope::from_event(42, &event)
            },
        );
        match policy {
            "blacklist" => {
                nuono_storage::security::add_blacklist(&fixture.db, "user", "7", None)
                    .await
                    .unwrap();
            }
            "banned" => {
                fixture
                    .db
                    .set_global_config("banned_users", json!(["7"]))
                    .await
                    .unwrap();
            }
            "permission" => {
                fixture
                    .db
                    .admin_unbind_platform_identity(
                        admin.id,
                        admin.id,
                        bound.id.parse().unwrap(),
                        bound.revision.parse().unwrap(),
                    )
                    .await
                    .unwrap();
            }
            "plugin_pm" => {
                config.min_permission = nuono_core::core::permission::PermissionLevel::BotOwner;
            }
            "area" => {
                config.area = PluginArea::Group;
            }
            "plugin_blacklist" => {
                config.black_list = vec!["7".into()];
            }
            "plugin_whitelist" => {
                config.white_list = vec!["8".into()];
            }
            _ => unreachable!(),
        }
        fixture.plugin_host.register_plugin_config(config).await;
        let (tx, _rx) = mpsc::channel(1);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event.clone(), tx)
            .await
            .unwrap();
        assert!(result.blocked, "changed policy: {policy}");
        assert!(
            waiting.await.is_err(),
            "forbidden reply delivered after {policy}"
        );
        assert!(!fixture.wait_manager.has_wait(&session));
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn disabled_plugin_waits_are_cancelled_without_swallowing_next_message() {
    let fixture = RuntimeFixture::new().await;
    let mut config = nuono_core::core::Plugins::new("DisabledWait");
    fixture
        .plugin_host
        .register_plugin_config(config.clone())
        .await;
    let event = message_event();
    let session = nuono_core::onebot::build_session_id(42, &event);
    let waiting = fixture.wait_manager.register_scoped_wait(
        session.clone(),
        std::time::Duration::from_secs(30),
        WaitScope {
            plugin_name: Some("DisabledWait".into()),
            ..WaitScope::from_event(42, &event)
        },
    );
    config.enabled = false;
    fixture.plugin_host.register_plugin_config(config).await;
    assert!(waiting.await.is_err());
    let (tx, _rx) = mpsc::channel(1);
    let result = fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();
    assert!(!result.blocked);
    assert!(!fixture.wait_manager.has_wait(&session));
    fixture.cleanup().await;
}

#[tokio::test]
async fn reserved_ai_entry_preserves_an_unprefixed_wait_even_with_slash_plugin() {
    let fixture = RuntimeFixture::new().await;
    fixture
        .plugin_host
        .register_plugin_config(
            nuono_core::core::Plugins::new("Slash").with_prefix(vec!["/".into()]),
        )
        .await;
    let mut event = message_event();
    event["raw_message"] = json!("/ai hello");
    event["message"] = json!([{"type":"text", "data":{"text":"/ai hello"}}]);
    let session = nuono_core::onebot::build_session_id(42, &event);
    let mut waiting = fixture
        .wait_manager
        .register_wait(session.clone(), std::time::Duration::from_secs(30));
    let (tx, _rx) = mpsc::channel(8);
    fixture
        .kernel
        .process_runtime_post_event(42, &event.to_string(), event, tx)
        .await
        .unwrap();
    assert!(matches!(
        waiting.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(fixture.wait_manager.has_wait(&session));
    drop(waiting);
    fixture.cleanup().await;
}

#[tokio::test]
async fn waiting_replies_respect_group_shutdown_and_fail_closed_on_filter_errors() {
    use sea_orm::{ConnectionTrait, Statement};
    for policy in ["group_disabled", "group_plugin_disabled", "filter_error"] {
        let fixture = RuntimeFixture::new().await;
        fixture
            .db
            .sync_plugin("GroupWait", "1", &json!({}))
            .await
            .unwrap();
        fixture
            .plugin_host
            .register_plugin_config(nuono_core::core::Plugins::new("GroupWait"))
            .await;
        let mut event = message_event();
        event["message_type"] = json!("group");
        event["group_id"] = json!(100);
        let session = nuono_core::onebot::build_session_id(42, &event);
        let waiting = fixture.wait_manager.register_scoped_wait(
            session.clone(),
            std::time::Duration::from_secs(30),
            WaitScope {
                plugin_name: Some("GroupWait".into()),
                ..WaitScope::from_event(42, &event)
            },
        );
        match policy {
            "group_disabled" => {
                let group = fixture
                    .db
                    .ensure_group(&nuono_storage::groups::GroupIdentity::onebot("100"), "")
                    .await
                    .unwrap();
                nuono_app::groups::GroupStore::toggle_group(&fixture.db, group.id, false)
                    .await
                    .unwrap();
            }
            "group_plugin_disabled" => {
                let group = fixture
                    .db
                    .ensure_group(&nuono_storage::groups::GroupIdentity::onebot("100"), "")
                    .await
                    .unwrap();
                fixture.db.orm().execute_raw(Statement::from_sql_and_values(fixture.db.backend(),
                    "INSERT INTO nuono_plugin_overrides (plugin_id,user_id,group_id,enabled,config_patch_json,service_patch_json,revision,created_at_ms,updated_at_ms) VALUES ('GroupWait',NULL,$1,false,'{}','{}',1,0,0)",[group.id.into()])).await.unwrap();
            }
            "filter_error" => {
                fixture
                    .db
                    .orm()
                    .execute_raw(Statement::from_string(
                        fixture.db.backend(),
                        "DROP TABLE nuono_filter_rules",
                    ))
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let (tx, _rx) = mpsc::channel(1);
        let result = fixture
            .kernel
            .process_runtime_post_event(42, &event.to_string(), event, tx)
            .await
            .unwrap();
        assert!(result.blocked, "policy: {policy}");
        assert!(
            waiting.await.is_err(),
            "reply was delivered despite {policy}"
        );
        fixture.cleanup().await;
    }
}
