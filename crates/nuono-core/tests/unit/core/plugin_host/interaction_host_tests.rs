use super::*;
use crate::core::{
    ApiClient, CommandDispatchServices, EventHandler, EventKernel, EventKernelPipeline,
    FilterHandler, MatcherRegistry, SessionManager, TaskExecutor,
};
use nuono_plugin::api::{
    button::ButtonPermission,
    interaction::{InteractionAction, InteractionRegistration, PluginInteraction},
    routed::RuntimeMessageTarget,
    waiter::WaitScope,
};
use std::sync::Mutex;
use tokio::sync::mpsc;

struct InteractionPlugin {
    name: String,
    calls: Arc<Mutex<Vec<String>>>,
    prefix: &'static str,
    block: bool,
    tasks: Arc<PluginTaskScope>,
}
struct InteractionRule(Arc<Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl CommandHandler for InteractionRule {
    async fn handle(
        &self,
        _: &crate::api::Bot,
        event: &Event<'_>,
        _: MatchData,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("rule:{}", event.user_id_str().unwrap()));
        Ok(())
    }
}
#[async_trait::async_trait]
impl Plugin for InteractionPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "interaction fixture"
    }
    fn service_config(&self) -> Plugins {
        Plugins::new(&self.name)
            .with_prefix(vec![self.prefix.into()])
            .with_area(PluginArea::All)
    }
    fn triggers(&self) -> Vec<Trigger> {
        vec![Trigger::new_event(
            TriggerType::FullMatch,
            "ping",
            self.block,
            false,
            Arc::new(InteractionRule(self.calls.clone())),
        )]
    }
    async fn on_event(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("event:{}", event.command_text()));
        if event.command_text() == "rich" {
            return Ok(event.reply(crate::api::MessageSegment::buttons(vec![vec![
                nuono_plugin::api::button::ButtonOne::callback_json(
                    "确认",
                    "stored",
                    serde_json::json!({"stored":true}),
                ),
            ]])));
        }
        if event.command_text() == "ask" {
            return Ok(self.question(event, "first"));
        }
        Ok(PluginResponse::Continue)
    }
    async fn on_message(
        &self,
        event: &Event,
        _: &str,
        callback_id: &str,
    ) -> anyhow::Result<PluginResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("answer:{callback_id}:{}", event.text()));
        if callback_id == "first" {
            Ok(self.question(event, "second"))
        } else {
            Ok(event.reply(crate::api::MessageSegment::buttons(vec![vec![
                nuono_plugin::api::button::ButtonOne::callback_json(
                    "完成",
                    "stored",
                    serde_json::json!({"stored":true}),
                ),
            ]])))
        }
    }
    async fn on_interaction(
        &self,
        event: &Event<'_>,
        interaction: &PluginInteraction,
    ) -> anyhow::Result<PluginResponse> {
        assert_eq!(event.post_type(), "interaction");
        assert_eq!(interaction.callback_id, "stored");
        assert_eq!(interaction.data["stored"], true);
        if interaction.data["hold_child"] == true {
            assert!(
                self.tasks
                    .try_spawn("interaction-child", std::future::pending())
            );
        }
        self.calls
            .lock()
            .unwrap()
            .push(format!("callback:{}", event.user_id_str().unwrap()));
        Ok(PluginResponse::Continue)
    }
}
impl InteractionPlugin {
    fn question(&self, event: &Event<'_>, callback_id: &str) -> PluginResponse {
        let PluginResponse::Reply(reply_message) =
            event.reply(crate::api::MessageSegment::buttons(vec![vec![
                nuono_plugin::api::button::ButtonOne::new("选择", "choice"),
            ]]))
        else {
            unreachable!();
        };
        PluginResponse::WaitReply {
            reply_message,
            session_id: crate::onebot::try_build_session_id(42, event.raw()).unwrap(),
            timeout_secs: 10,
            callback_id: callback_id.into(),
            plugin_name: self.name.clone(),
            event_context: event.raw().clone(),
        }
    }
}
struct Fixture {
    h: Harness,
    kernel: EventKernel,
    calls: Arc<Mutex<Vec<String>>>,
    tasks: Arc<PluginTaskScope>,
}
impl Fixture {
    async fn new(prefix: &'static str, block: bool) -> Self {
        let h = Harness::new("interaction").await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tasks = Arc::new(PluginTaskScope::new(&h.plugin));
        let loaded = LoadedPlugin {
            plugin: Box::new(InteractionPlugin {
                name: h.plugin.clone(),
                calls: calls.clone(),
                prefix,
                block,
                tasks: tasks.clone(),
            }),
        };
        let declaration = super::super::loading::collect_declaration_sync(&loaded).unwrap();
        h.host.remember_plugin_declaration(&declaration).await;
        h.host.prepare_runtime_config(&declaration).await.unwrap();
        h.host
            .register_plugin_config(declaration.service_config)
            .await;
        h.host
            .register_scoped_plugin_triggers(&loaded, &declaration.commands)
            .await
            .unwrap();
        *h.host.plugins.write().await = vec![loaded].into_iter().map(Into::into).collect();
        h.host
            .task_scopes
            .write()
            .await
            .insert(h.plugin.clone(), tasks.clone());
        let mut handler = EventHandler::new(
            h.db.clone(),
            CommandDispatchServices {
                trigger_registry: h.host.trigger_registry(),
                cooldown: h.host.cooldown_tracker.clone(),
                stats: h.host.command_stats.clone(),
            },
            h.host.permission_manager.clone(),
            h.host.event_bus.clone(),
            h.host.wait_manager.clone(),
            Arc::new(ApiClient::new()),
        );
        handler.set_plugin_host(h.host.clone());
        let handler = Arc::new(handler);
        let config = Arc::new(
            crate::config::ConfigManager::load(Some(h.root.join("nuono.toml")))
                .await
                .unwrap(),
        );
        let kernel = EventKernel::new(
            h.db.clone(),
            h.host.event_bus.clone(),
            h.host.permission_manager.clone(),
            h.host.clone(),
            Arc::new(TaskExecutor::new(1)),
            config.clone(),
            EventKernelPipeline {
                filter_handler: Arc::new(FilterHandler::new(h.db.clone())),
                wait_manager: h.host.wait_manager.clone(),
                event_handler: handler,
                matcher_registry: Arc::new(MatcherRegistry::new(Arc::new(SessionManager::new()))),
                adapter: Arc::new(nuono_adapter_fixture::FixtureAdapter),
                core_commands: Arc::new(crate::core_commands::CoreCommands::new(
                    config,
                    h.host.permission_manager.clone(),
                    crate::SystemMonitor::new(),
                )),
            },
        );
        Self {
            h,
            kernel,
            calls,
            tasks,
        }
    }
    fn route() -> crate::core::RuntimeBotRoute {
        crate::core::RuntimeBotRoute::new("onebot", "fixture", "42")
    }
    fn message(text: &str) -> Value {
        serde_json::json!({"post_type":"message","self_id":42,"route":Self::route(),"message_type":"private","sub_type":"friend","time":1,"user_id":"7","message_id":"1","raw_message":text,"message":[{"type":"text","data":{"text":text}}],"sender":{"user_id":"7","nickname":"fixture","role":"member"}})
    }
    async fn token(&self, action: InteractionAction) -> String {
        self.h
            .host
            .action_router
            .interaction_port()
            .unwrap()
            .register(InteractionRegistration {
                plugin: self.h.plugin.clone(),
                target: RuntimeMessageTarget::private(Self::route(), "7"),
                actor_id: "7".into(),
                origin_chat_id: None,
                permission: ButtonPermission::All,
                allowed_user_ids: vec![],
                action,
                ttl_secs: 300,
            })
            .await
            .unwrap()
    }
    async fn click(&self, token: &str) -> crate::core::RuntimeEventProcessResult {
        let event = serde_json::json!({"post_type":"interaction","interaction_id":uuid::Uuid::new_v4().simple().to_string(),"interaction_token":token,
            "user_id":"7","message_type":"private","self_id":42,"sender":{"role":"member"}});
        let (tx, _rx) = mpsc::channel(32);
        self.kernel
            .process_runtime_post_event_with_route(
                42,
                &event.to_string(),
                event,
                tx,
                Some(&Self::route()),
            )
            .await
            .unwrap()
    }
    async fn cleanup(self) {
        let Self {
            h, kernel, tasks, ..
        } = self;
        tasks.shutdown().await;
        drop(kernel);
        h.cleanup().await;
    }
}

#[tokio::test]
async fn command_buttons_use_current_prefix_frequency_and_original_block() {
    for block in [false, true] {
        let f = Fixture::new("ww", block).await;
        let token = f
            .token(InteractionAction::Command {
                command: "ping".into(),
            })
            .await;
        assert_eq!(f.click(&token).await.blocked, block);
        assert_eq!(*f.calls.lock().unwrap(), ["event:ping", "rule:7"]);
        let token = f
            .token(InteractionAction::Command {
                command: "wwping".into(),
            })
            .await;
        f.click(&token).await;
        assert_eq!(
            *f.calls.lock().unwrap(),
            ["event:ping", "rule:7"],
            "same plugin cooldown applies to clicks"
        );
        f.cleanup().await;
    }
}

#[tokio::test]
async fn chat_and_button_callbacks_share_the_same_domain_admission_scope() {
    let f = Fixture::new("ww", true).await;
    let mut message = Fixture::message("wwping");
    message["user_id"] = serde_json::json!(7);
    message["message_id"] = serde_json::json!(1);
    message["sender"]["user_id"] = serde_json::json!(7);
    let (tx, _rx) = mpsc::channel(32);
    f.kernel
        .process_runtime_post_event_with_route(
            42,
            &message.to_string(),
            message,
            tx,
            Some(&Fixture::route()),
        )
        .await
        .unwrap();
    assert_eq!(*f.calls.lock().unwrap(), ["event:ping", "rule:7"]);
    let token = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    f.click(&token).await;
    assert_eq!(
        *f.calls.lock().unwrap(),
        ["event:ping", "rule:7"],
        "a button must not create a second cooldown scope"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn empty_prefix_button_commands_keep_the_registered_plugin_owner() {
    let f = Fixture::new("", false).await;
    let other_calls = Arc::new(Mutex::new(Vec::new()));
    let other = LoadedPlugin {
        plugin: Box::new(InteractionPlugin {
            name: format!("{}Other", f.h.plugin),
            calls: other_calls.clone(),
            prefix: "",
            block: true,
            tasks: Arc::new(PluginTaskScope::new("other-fixture")),
        }),
    };
    let declaration = super::super::loading::collect_declaration_sync(&other).unwrap();
    f.h.host.remember_plugin_declaration(&declaration).await;
    f.h.host
        .register_plugin_config(declaration.service_config)
        .await;
    f.h.host
        .register_scoped_plugin_triggers(&other, &declaration.commands)
        .await
        .unwrap();
    f.h.host.plugins.write().await.push(other.into());
    let token = f
        .token(InteractionAction::Command {
            command: "ping".into(),
        })
        .await;
    assert!(
        !f.click(&token).await.blocked,
        "the owner's non-blocking rule remains non-blocking"
    );
    assert_eq!(*f.calls.lock().unwrap(), ["event:ping", "rule:7"]);
    assert!(
        other_calls.lock().unwrap().is_empty(),
        "a callback token cannot invoke another empty-prefix plugin"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn discord_channel_chat_and_command_buttons_match_rules_without_changing_native_context() {
    use crate::core::platform::{
        MessageType, SenderInfo, UnifiedEvent, UnifiedMessage, UnifiedNotice, UnifiedSegment,
    };
    struct ChannelRule(Arc<Mutex<Vec<Value>>>);
    #[async_trait::async_trait]
    impl CommandHandler for ChannelRule {
        async fn handle(
            &self,
            _: &crate::api::Bot,
            event: &Event<'_>,
            _: MatchData,
        ) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(event.raw().clone());
            Ok(())
        }
    }
    const CHANNEL: &str = "18446744073709551000";
    const GUILD: &str = "18446744073709551001";
    let f = Fixture::new("ww", true).await;
    f.h.host.set_unlimited_trigger_policy_for_test(&f.h.plugin);
    let captured = Arc::new(Mutex::new(Vec::new()));
    f.h.host.trigger_registry().register(
        Trigger::new_event(
            TriggerType::FullMatch,
            "inspect",
            true,
            false,
            Arc::new(ChannelRule(captured.clone())),
        )
        .with_plugin_name(&f.h.plugin),
    );
    let route = crate::core::RuntimeBotRoute::new("discord", "fixture-channel", "42");
    let raw =
        serde_json::json!({"id":"1","author":{"id":"7"},"channel_id":CHANNEL,"guild_id":GUILD});
    let message = UnifiedEvent::Message(UnifiedMessage {
        message_id: "1".into(),
        self_id: 42,
        user_id: 7,
        group_id: None,
        content: "wwinspect".into(),
        message_type: MessageType::Channel,
        sub_type: "normal".into(),
        sender: SenderInfo {
            user_id: "7".into(),
            nickname: None,
            card: None,
            role: None,
        },
        time: 1,
        raw,
        segments: vec![UnifiedSegment::Text {
            text: "wwinspect".into(),
        }],
    });
    assert!(
        f.kernel
            .process_unified_event_with_route("discord", message, None, Some(&route))
            .await
            .unwrap()
            .blocked
    );
    let token =
        f.h.host
            .action_router
            .interaction_port()
            .unwrap()
            .register(InteractionRegistration {
                plugin: f.h.plugin.clone(),
                target: RuntimeMessageTarget::channel(route.clone(), CHANNEL, Some(GUILD)),
                actor_id: "7".into(),
                origin_chat_id: None,
                permission: ButtonPermission::All,
                allowed_user_ids: vec![],
                action: InteractionAction::Command {
                    command: "inspect".into(),
                },
                ttl_secs: 300,
            })
            .await
            .unwrap();
    let click = UnifiedEvent::Notice(UnifiedNotice {
        self_id: 42,
        notice_type: "interaction".into(),
        sub_type: "button".into(),
        raw: serde_json::json!({"post_type":"interaction","interaction_id":"channel-click","interaction_token":token,
            "user_id":"7","message_type":"channel","channel_id":CHANNEL,"guild_id":GUILD}),
    });
    assert!(
        f.kernel
            .process_unified_event_with_route("discord", click, None, Some(&route))
            .await
            .unwrap()
            .blocked
    );
    {
        let events = captured.lock().unwrap();
        assert_eq!(
            events.len(),
            2,
            "both chat and the registered button must reach the declared rule"
        );
        for event in events.iter() {
            assert_eq!(event["message_type"], "channel");
            assert_eq!(event["channel_id"], CHANNEL);
            assert_eq!(event["guild_id"], GUILD);
            assert_eq!(event["route"], serde_json::json!(route));
            assert_eq!(event["raw_message"], "wwinspect");
            let response = Event::new(event).reply("reply");
            let PluginResponse::Reply(action) = response else {
                panic!("reply action")
            };
            assert_eq!(action["params"]["message_type"], "channel");
            assert_eq!(action["params"]["channel_id"], CHANNEL);
            assert_eq!(action["params"]["guild_id"], GUILD);
        }
    }
    f.cleanup().await;
}

#[tokio::test]
async fn callback_is_not_an_event_broadcast_and_obeys_disabled_plugin() {
    let f = Fixture::new("ww", true).await;
    let token = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    f.click(&token).await;
    f.click(&token).await;
    assert_eq!(*f.calls.lock().unwrap(), ["callback:7"]);
    let token = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    let mut config =
        f.h.host
            .get_plugin_config(&f.h.plugin)
            .await
            .unwrap()
            .as_ref()
            .clone();
    config.enabled = false;
    f.h.host.register_plugin_config(config).await;
    f.click(&token).await;
    assert_eq!(*f.calls.lock().unwrap(), ["callback:7"]);
    f.cleanup().await;
}

#[tokio::test]
async fn callback_managed_descendants_keep_the_request_slot_until_cancelled() {
    let f = Fixture::new("ww", true).await;
    f.h.host
        .set_plugin_trigger_policy(
            &f.h.plugin,
            PluginTriggerPolicy {
                user_cooldown_ms: 0,
                max_in_flight_per_user: 1,
                group_requests_per_minute: 0,
                group_burst: 3,
            },
        )
        .await
        .unwrap();
    let first = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true,"hold_child":true}),
        })
        .await;
    f.click(&first).await;
    let second = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    f.click(&second).await;
    assert_eq!(
        *f.calls.lock().unwrap(),
        ["callback:7"],
        "detached managed work still owns the slot"
    );
    f.tasks.shutdown().await;
    f.click(&second).await;
    assert_eq!(
        *f.calls.lock().unwrap(),
        ["callback:7", "callback:7"],
        "cancelling the last managed descendant releases the slot"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn claimed_callbacks_cannot_execute_after_disable_and_reenable() {
    let f = Fixture::new("ww", true).await;
    let token = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    let event = serde_json::json!({"post_type":"interaction","interaction_id":"claimed-before-disable","interaction_token":token,
        "route":Fixture::route(),"user_id":"7","message_type":"private"});
    let claimed = f.h.host.claim_interaction(&event).await.unwrap();
    f.h.host.toggle(&f.h.plugin, false).await.unwrap();
    f.h.host.toggle(&f.h.plugin, true).await.unwrap();
    let interaction = PluginInteraction {
        id: "claimed-before-disable".into(),
        callback_id: "stored".into(),
        data: serde_json::json!({"stored":true}),
    };
    let domain = crate::core::interaction::message_domain(&claimed.message);
    f.h.host
        .dispatch_interaction(
            &f.h.plugin,
            &claimed.message,
            &interaction,
            claimed.reservation,
            domain.as_ref(),
            Default::default(),
        )
        .await;
    assert!(
        f.calls.lock().unwrap().is_empty(),
        "a callback claimed before disable must not enter the re-enabled instance"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn claimed_command_ownership_cannot_enter_callbacks_or_rules_after_reenable() {
    let f = Fixture::new("ww", true).await;
    let token = f
        .token(InteractionAction::Command {
            command: "ping".into(),
        })
        .await;
    let event = serde_json::json!({"post_type":"interaction","interaction_id":"old-command","interaction_token":token,
        "route":Fixture::route(),"user_id":"7","message_type":"private"});
    let claimed = f.h.host.claim_interaction(&event).await.unwrap();
    f.h.host.toggle(&f.h.plugin, false).await.unwrap();
    f.h.host.toggle(&f.h.plugin, true).await.unwrap();
    crate::core::interaction::with_command_owner(
        Some((f.h.plugin.clone(), claimed.epoch)),
        async {
            assert!(!crate::core::interaction::is_current());
            assert!(
                f.h.host
                    .lock_interaction_command(&f.h.plugin)
                    .await
                    .is_none(),
                "snapshotted rule execution must be rejected"
            );
            let context = PluginFilterContext {
                user_id: "7".into(),
                group_id: None,
                msg_type: "private".into(),
                actor_permission: ActorPermission::everyone(),
            };
            f.h.host
                .process_message("", &claimed.message, &context)
                .await;
        },
    )
    .await;
    assert!(
        f.calls.lock().unwrap().is_empty(),
        "old command cannot enter on_event either"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn reply_buttons_cannot_answer_a_replaced_wait_and_do_not_consume_cooldown() {
    let f = Fixture::new("ww", true).await;
    let message = Fixture::message("choice");
    let mut scope = WaitScope::from_event(42, &message);
    scope.plugin_name = Some(f.h.plugin.clone());
    let session = crate::onebot::try_build_session_id(42, &message).unwrap();
    let first = f.h.host.wait_manager.register_scoped_wait(
        session.clone(),
        Duration::from_secs(30),
        scope.clone(),
    );
    let stale = f
        .token(InteractionAction::Reply {
            value: "choice".into(),
        })
        .await;
    let mut replacement =
        f.h.host
            .wait_manager
            .register_scoped_wait(session.clone(), Duration::from_secs(30), scope);
    f.click(&stale).await;
    assert!(replacement.try_recv().is_err());
    assert!(replacement.is_active());
    drop(first);
    let current = f
        .token(InteractionAction::Reply {
            value: "choice".into(),
        })
        .await;
    f.click(&current).await;
    assert_eq!(replacement.try_recv().unwrap()["raw_message"], "choice");
    let command = f
        .token(InteractionAction::Command {
            command: "ping".into(),
        })
        .await;
    assert!(f.click(&command).await.blocked);
    assert_eq!(*f.calls.lock().unwrap(), ["event:ping", "rule:7"]);
    drop(replacement);
    f.cleanup().await;
}

#[tokio::test]
async fn reply_buttons_snapshot_and_consume_only_their_exact_bot_route() {
    let f = Fixture::new("ww", true).await;
    let message_a = Fixture::message("choice");
    let route_b = crate::core::RuntimeBotRoute::new("onebot", "other-instance", "42");
    let mut message_b = message_a.clone();
    message_b["route"] = serde_json::json!(route_b);
    let scope = |message: &Value| {
        let mut scope = WaitScope::from_event(42, message);
        scope.plugin_name = Some(f.h.plugin.clone());
        scope
    };
    let session = crate::onebot::try_build_session_id(42, &message_a).unwrap();
    let mut first = f.h.host.wait_manager.register_scoped_wait(
        session.clone(),
        Duration::from_secs(30),
        scope(&message_a),
    );
    let mut second = f.h.host.wait_manager.register_scoped_wait(
        session.clone(),
        Duration::from_secs(30),
        scope(&message_b),
    );
    assert!(
        first.is_active() && second.is_active(),
        "equal legacy sessions must not replace another route"
    );
    let token_a = f
        .token(InteractionAction::Reply {
            value: "choice".into(),
        })
        .await;
    f.click(&token_a).await;
    assert_eq!(
        first.try_recv().unwrap()["route"],
        serde_json::json!(Fixture::route())
    );
    assert!(second.is_active());

    let registration = || InteractionRegistration {
        plugin: f.h.plugin.clone(),
        target: RuntimeMessageTarget::private(route_b.clone(), "7"),
        actor_id: "7".into(),
        origin_chat_id: None,
        permission: ButtonPermission::All,
        allowed_user_ids: vec![],
        action: InteractionAction::Reply {
            value: "other-choice".into(),
        },
        ttl_secs: 300,
    };
    let token_b =
        f.h.host
            .action_router
            .interaction_port()
            .unwrap()
            .register(registration())
            .await
            .unwrap();
    f.click(&token_b).await;
    assert!(
        second.is_active(),
        "the first route cannot consume the second route's token"
    );
    let event = serde_json::json!({"post_type":"interaction","interaction_id":"second-route-choice","interaction_token":token_b,"user_id":"7","message_type":"private"});
    let (tx, _rx) = mpsc::channel(16);
    f.kernel
        .process_runtime_post_event_with_route(42, &event.to_string(), event, tx, Some(&route_b))
        .await
        .unwrap();
    let reply = second.try_recv().unwrap();
    assert_eq!(reply["route"], serde_json::json!(route_b));
    assert_eq!(reply["raw_message"], "other-choice");
    drop(first);
    drop(second);

    let mut legacy_scope = scope(&message_b);
    legacy_scope.route = None;
    let legacy =
        f.h.host
            .wait_manager
            .register_scoped_wait(session, Duration::from_secs(30), legacy_scope);
    assert!(
        f.h.host
            .action_router
            .interaction_port()
            .unwrap()
            .register(registration())
            .await
            .is_err(),
        "a routed button cannot guess a legacy unscoped wait"
    );
    drop(legacy);
    f.cleanup().await;
}

#[tokio::test]
async fn unified_wait_lane_delivers_registered_choices_without_running_business_callbacks() {
    use crate::core::platform::{UnifiedEvent, UnifiedNotice};
    let f = Fixture::new("ww", true).await;
    let message = Fixture::message("choice");
    let mut scope = WaitScope::from_event(42, &message);
    scope.plugin_name = Some(f.h.plugin.clone());
    let session = crate::onebot::try_build_session_id(42, &message).unwrap();
    let mut receiver =
        f.h.host
            .wait_manager
            .register_scoped_wait(session, Duration::from_secs(30), scope);
    let reply_token = f
        .token(InteractionAction::Reply {
            value: "choice".into(),
        })
        .await;
    let event = |token: &str| {
        UnifiedEvent::Notice(UnifiedNotice {
            self_id: 42,
            notice_type: "interaction".into(),
            sub_type: "button".into(),
            raw: serde_json::json!({"post_type":"interaction","interaction_id":uuid::Uuid::new_v4().simple().to_string(),"interaction_token":token,"user_id":"7","message_type":"private"}),
        })
    };
    let callback = f
        .token(InteractionAction::Callback {
            callback_id: "stored".into(),
            data: serde_json::json!({"stored":true}),
        })
        .await;
    assert!(
        !f.kernel
            .try_deliver_waiting_unified_event(
                "onebot",
                &event(&callback),
                None,
                Some(&Fixture::route())
            )
            .await
            .unwrap()
    );
    assert!(receiver.is_active());
    assert!(
        f.kernel
            .try_deliver_waiting_unified_event(
                "onebot",
                &event(&reply_token),
                None,
                Some(&Fixture::route())
            )
            .await
            .unwrap()
    );
    assert_eq!(receiver.try_recv().unwrap()["raw_message"], "choice");
    assert!(f.calls.lock().unwrap().is_empty());
    // 该回调未被快速通道消费，后续仍会走常规派发流程。
    f.click(&callback).await;
    assert_eq!(*f.calls.lock().unwrap(), ["callback:7"]);
    drop(receiver);
    f.cleanup().await;
}

#[derive(Default)]
struct ReplyCapture {
    actions: Mutex<Vec<Value>>,
    changed: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl crate::core::RuntimeActionSink for ReplyCapture {
    async fn send_action(&self, action: Value) -> crate::error::Result<()> {
        self.actions.lock().unwrap().push(action);
        self.changed.notify_one();
        Ok(())
    }
}
impl ReplyCapture {
    async fn action(&self, index: usize) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let changed = self.changed.notified();
                if let Some(action) = self.actions.lock().unwrap().get(index).cloned() {
                    return action;
                }
                changed.await;
            }
        })
        .await
        .expect("plugin response was not sent")
    }
}
fn response_token(action: &Value) -> String {
    action["params"]["message"]
        .as_array()
        .unwrap()
        .iter()
        .find(|segment| segment["type"] == "buttons")
        .expect("native buttons expected")["data"]["buttons"][0][0]["token"]
        .as_str()
        .expect("host token expected")
        .to_owned()
}
async fn run_telegram_message(f: &Fixture, sink: Arc<ReplyCapture>, text: &str) {
    use crate::core::platform::PlatformAdapter;
    let mut message = Fixture::message(text);
    message["user_id"] = serde_json::json!(7);
    message["message_id"] = serde_json::json!(1);
    message["sender"]["user_id"] = serde_json::json!(7);
    let unified = nuono_adapter_fixture::FixtureAdapter
        .parse_incoming(&message.to_string())
        .unwrap();
    f.kernel
        .process_unified_event_with_route(
            "telegram",
            unified,
            Some(sink),
            Some(&crate::core::RuntimeBotRoute::new(
                "telegram", "fixture", "42",
            )),
        )
        .await
        .unwrap();
}
async fn reply_telegram_choice(f: &Fixture, sink: Arc<ReplyCapture>, token: &str) {
    use crate::core::platform::{UnifiedEvent, UnifiedNotice};
    let event = UnifiedEvent::Notice(UnifiedNotice {
        self_id: 42,
        notice_type: "interaction".into(),
        sub_type: "button".into(),
        raw: serde_json::json!({
            "post_type":"interaction","interaction_id":uuid::Uuid::new_v4().simple().to_string(),"interaction_token":token,"user_id":"7","message_type":"private","chat_id":"7"
        }),
    });
    assert!(
        f.kernel
            .try_deliver_waiting_unified_event(
                "telegram",
                &event,
                Some(sink),
                Some(&crate::core::RuntimeBotRoute::new(
                    "telegram", "fixture", "42"
                ))
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn event_reply_buttons_receive_host_tokens_before_native_send() {
    let f = Fixture::new("ww", true).await;
    let sink = Arc::new(ReplyCapture::default());
    run_telegram_message(&f, sink.clone(), "wwrich").await;
    let sent = sink.action(0).await;
    let token = response_token(&sent);
    let event = serde_json::json!({"post_type":"interaction","interaction_id":"click-native-reply","interaction_token":token,
        "route":crate::core::RuntimeBotRoute::new("telegram","fixture","42"),"user_id":"7","message_type":"private"});
    let claimed =
        f.h.host
            .claim_interaction(&event)
            .await
            .expect("reply button must have a host registration");
    assert_eq!(claimed.plugin, f.h.plugin);
    assert!(matches!(claimed.action, InteractionAction::Callback { .. }));
    assert_eq!(sent["params"]["user_id"], 7);
    drop(claimed);
    f.cleanup().await;
}

#[tokio::test]
async fn legacy_wait_responses_prepare_each_question_after_registering_its_generation() {
    let f = Fixture::new("ww", true).await;
    let sink = Arc::new(ReplyCapture::default());
    run_telegram_message(&f, sink.clone(), "wwask").await;
    let first = response_token(&sink.action(0).await);
    reply_telegram_choice(&f, sink.clone(), &first).await;
    let second = response_token(&sink.action(1).await);
    assert_ne!(
        first, second,
        "replacement wait must issue a token bound to its new generation"
    );
    reply_telegram_choice(&f, sink.clone(), &second).await;
    let final_token = response_token(&sink.action(2).await);
    let final_event = serde_json::json!({"post_type":"interaction","interaction_id":"final-callback","interaction_token":final_token,
        "route":crate::core::RuntimeBotRoute::new("telegram","fixture","42"),"user_id":"7","message_type":"private"});
    let claimed = f.h.host.claim_interaction(&final_event).await.unwrap();
    assert!(matches!(claimed.action, InteractionAction::Callback { .. }));
    assert_eq!(
        *f.calls.lock().unwrap(),
        ["event:ask", "answer:first:choice", "answer:second:choice"]
    );
    drop(claimed);
    f.cleanup().await;
}
