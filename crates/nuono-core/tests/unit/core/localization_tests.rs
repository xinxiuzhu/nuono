use super::*;
use crate::plugin::LoadedPlugin;
use nuono_app::localization::{Locale, LocaleScope};
use nuono_plugin::{
    Command, CommandHandler, Event as PluginEvent, Plugin, PluginResponse, Plugins,
};
use serde_json::{Value, json};
use std::sync::Mutex;

type Captures = Arc<Mutex<Vec<(&'static str, String, Locale, bool, Value)>>>;

struct LocaleProbe {
    name: String,
    captures: Captures,
    locales: nuono_app::localization::LocaleService,
}

fn capture(records: &Captures, stage: &'static str, event: &PluginEvent<'_>) {
    records.lock().unwrap().push((
        stage,
        event.user_id_str().unwrap(),
        event.locale(),
        event.domain_event().is_some(),
        event.raw().clone(),
    ));
}

#[async_trait::async_trait]
impl Plugin for LocaleProbe {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "locale pipeline fixture"
    }
    fn service_config(&self) -> Plugins {
        Plugins::new(&self.name).with_prefix(vec!["loc".into()])
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::new(
            "ping",
            Arc::new(LocaleCommand(self.captures.clone())),
        )]
    }
    async fn on_event(&self, event: &PluginEvent) -> anyhow::Result<PluginResponse> {
        capture(&self.captures, "event", event);
        if event.command_text() == "wait" {
            return Ok(PluginResponse::WaitReply {
                reply_message: json!({"action":"send_private_msg", "params":{"user_id":7,"message":"waiting"}}),
                session_id: crate::onebot::build_session_id(42, event.raw()),
                timeout_secs: 5,
                callback_id: "locale".into(),
                plugin_name: self.name.clone(),
                event_context: event.raw().clone(),
            });
        }
        // 在当前阶段修改语言偏好只应影响后续消息，不应改变当前消息的语言。
        if event.user_id_str().as_deref() == Some("7") {
            self.locales
                .set_preference(&event.user_locale_scope()?.unwrap(), Some(Locale::Ko))
                .await?;
        }
        Ok(PluginResponse::Continue)
    }
    async fn on_message(
        &self,
        event: &PluginEvent,
        _: &str,
        _: &str,
    ) -> anyhow::Result<PluginResponse> {
        capture(&self.captures, "wait", event);
        Ok(PluginResponse::Continue)
    }
}

struct LocaleCommand(Captures);
#[async_trait::async_trait]
impl CommandHandler for LocaleCommand {
    async fn handle(
        &self,
        _: &crate::api::Bot,
        event: &PluginEvent<'_>,
        _: nuono_plugin::MatchData,
    ) -> anyhow::Result<()> {
        capture(&self.0, "command", event);
        Ok(())
    }
}

fn message(user: i64, text: &str) -> Value {
    json!({"time":1,"post_type":"message","message_type":"private","sub_type":"friend", "self_id":42,
        "user_id":user,"message_id":1,"raw_message":text,"message":[{"type":"text","data":{"text":text}}],
        "sender":{"user_id":user,"nickname":"locale tester"}})
}

#[tokio::test]
async fn locale_runtime_snapshot_is_shared_by_event_command_and_wait_and_isolates_concurrent_users()
{
    let root = std::env::temp_dir().join(format!("nuono-locale-runtime-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = crate::test_support::test_db(4).await;
    let config = Arc::new(crate::config::ConfigManager::for_test(
        crate::config::NuonoConfig::default(),
        root.join("nuono.toml"),
    ));
    let locales = crate::localization::service(db.clone(), config.clone());
    let bus = Arc::new(EventBus::new());
    let permissions = Arc::new(PermissionManager::new(db.clone()));
    let waits = Arc::new(WaitManager::new());
    let host = PluginHost::new_with_locales(
        db.clone(),
        bus.clone(),
        permissions.clone(),
        crate::core::Paths::with_root(&root),
        waits.clone(),
        crate::core::FrameworkControl::new(),
        Arc::new(crate::config::PluginConfigManager::with_database(
            root.join("data"),
            db.clone(),
        )),
        locales.clone(),
    )
    .await
    .unwrap();
    let captures = Arc::new(Mutex::new(Vec::new()));
    let plugin_name = format!("LocaleProbe{}", uuid::Uuid::new_v4().simple());
    host.init_with_plugins(vec![LoadedPlugin {
        plugin: Box::new(LocaleProbe {
            name: plugin_name.clone(),
            captures: captures.clone(),
            locales: locales.clone(),
        }),
    }])
    .await
    .unwrap();
    // 该测试桩重点验证连续快速发送不同指令时的队列与语言偏好行为。
    // 触发准入限流逻辑由 trigger_control_host_tests 单独覆盖。
    host.set_unlimited_trigger_policy_for_test(&plugin_name);
    let mut handler = EventHandler::new(
        db.clone(),
        crate::core::CommandDispatchServices {
            trigger_registry: host.trigger_registry(),
            cooldown: host.cooldown_tracker.clone(),
            stats: host.command_stats.clone(),
        },
        permissions.clone(),
        bus.clone(),
        waits.clone(),
        Arc::new(crate::core::ApiClient::new()),
    );
    handler.set_plugin_host(host.clone());
    let kernel = EventKernel::new(
        db.clone(),
        bus,
        permissions.clone(),
        host.clone(),
        Arc::new(TaskExecutor::new(1)),
        config.clone(),
        EventKernelPipeline {
            filter_handler: Arc::new(FilterHandler::new(db.clone())),
            wait_manager: waits.clone(),
            event_handler: Arc::new(handler),
            matcher_registry: Arc::new(MatcherRegistry::new(Arc::new(
                crate::core::SessionManager::new(),
            ))),
            adapter: Arc::new(nuono_adapter_fixture::FixtureAdapter),
            core_commands: Arc::new(crate::core_commands::CoreCommands::new(
                config,
                permissions,
                crate::SystemMonitor::new(),
            )),
        },
    );
    let route = crate::core::RuntimeBotRoute::new("onebot", "locale-test", "42");
    let bot = crate::core::runtime_transport::route_identity(&route).unwrap();
    let user_scope = |user: &str| {
        LocaleScope::user(
            bot.clone(),
            crate::domain::IdentityRef::new(
                crate::domain::Platform::OneBot,
                crate::domain::IdentityKind::User,
                crate::domain::IdentityScheme::Native,
                user,
                None::<String>,
            )
            .unwrap(),
        )
        .unwrap()
    };
    locales
        .set_preference(&user_scope("7"), Some(Locale::En))
        .await
        .unwrap();
    locales
        .set_preference(&user_scope("8"), Some(Locale::Ja))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let first = message(7, "loc ping");
    let second = message(8, "loc ping");
    let first_json = first.to_string();
    let second_json = second.to_string();
    let (a, b) = tokio::join!(
        kernel.process_runtime_post_event_with_route(
            42,
            &first_json,
            first.clone(),
            tx.clone(),
            Some(&route)
        ),
        kernel.process_runtime_post_event_with_route(
            42,
            &second_json,
            second.clone(),
            tx.clone(),
            Some(&route)
        ),
    );
    a.unwrap();
    b.unwrap();
    {
        let records = captures.lock().unwrap();
        for (user, locale) in [("7", Locale::En), ("8", Locale::Ja)] {
            for stage in ["event", "command"] {
                assert!(
                    records.iter().any(|record| record.0 == stage
                        && record.1 == user
                        && record.2 == locale
                        && record.3),
                    "missing {stage} {user}: {records:?}"
                );
            }
        }
        assert!(records.iter().all(|record| record.4.get("locale").is_none()
            && record.4["route"] == serde_json::to_value(&route).unwrap()));
    }
    assert_eq!(
        locales.get_preference(&user_scope("7")).await.unwrap(),
        Some(Locale::Ko)
    );
    let start = message(7, "loc wait");
    kernel
        .process_runtime_post_event_with_route(
            42,
            &start.to_string(),
            start.clone(),
            tx.clone(),
            Some(&route),
        )
        .await
        .unwrap();
    let mut projected = start.clone();
    crate::core::runtime_projection::inject_runtime_route(&mut projected, Some(&route));
    let session = nuono_plugin::api::waiter::WaitManager::event_session_id(42, &projected).unwrap();
    assert!(waits.has_wait(&session));
    locales
        .set_preference(&user_scope("7"), Some(Locale::En))
        .await
        .unwrap();
    let reply = message(7, "reply");
    kernel
        .process_runtime_post_event_with_route(42, &reply.to_string(), reply, tx, Some(&route))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if captures.lock().unwrap().iter().any(|r| r.0 == "wait") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        captures
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == "wait" && r.2 == Locale::En && r.3 && r.4["raw_message"] == "reply")
    );
    assert!(!waits.has_wait(&session));
    host.shutdown().await.unwrap();
    drop(kernel);
    drop(host);
    db.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(root).await;
}
