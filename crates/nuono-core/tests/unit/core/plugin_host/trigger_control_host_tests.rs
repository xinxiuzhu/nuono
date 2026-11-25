//! 通过宿主回调与生产消息管线端到端验证触发准入与限流控制。
use super::super::loading::collect_declaration_sync;
use super::*;
use crate::core::{
    ApiClient, CommandDispatchServices, EventHandler, EventKernel, EventKernelPipeline,
    FilterHandler, MatcherRegistry, SessionManager, TaskExecutor,
};
use crate::plugin::PluginMessageListener;
use std::sync::Mutex;
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Copy)]
struct RuleSpec {
    pattern: &'static str,
    label: &'static str,
    block: bool,
    priority: i32,
    permission: Option<crate::core::command::PermissionLevel>,
}

impl RuleSpec {
    fn ping(label: &'static str, block: bool, priority: i32) -> Self {
        Self {
            pattern: "ping",
            label,
            block,
            priority,
            permission: None,
        }
    }
}

struct RecordingRule {
    label: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl CommandHandler for RecordingRule {
    async fn handle(&self, _: &crate::api::Bot, _: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(self.label.into());
        Ok(())
    }
}

#[derive(Default)]
struct ChildGate {
    started: Notify,
    release: Notify,
    finished: Notify,
}

struct AdmissionPlugin {
    name: String,
    prefix: &'static str,
    listener: bool,
    rules: Vec<RuleSpec>,
    calls: Arc<Mutex<Vec<String>>>,
    tasks: Arc<PluginTaskScope>,
    child: Arc<ChildGate>,
}

#[async_trait::async_trait]
impl Plugin for AdmissionPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "host admission fixture"
    }
    fn service_config(&self) -> Plugins {
        let config = Plugins::new(&self.name)
            .with_prefix(vec![self.prefix.into()])
            .with_area(PluginArea::All);
        if self.listener {
            config.with_auto_listen_enabled(true)
        } else {
            config
        }
    }
    fn message_listener(&self) -> Option<PluginMessageListener> {
        self.listener.then(|| {
            PluginMessageListener::new("links")
                .with_text_pattern(r"https?://")
                .unwrap()
        })
    }
    fn triggers(&self) -> Vec<Trigger> {
        self.rules
            .iter()
            .map(|rule| {
                let mut trigger = Trigger::new_event(
                    TriggerType::FullMatch,
                    rule.pattern,
                    rule.block,
                    false,
                    Arc::new(RecordingRule {
                        label: rule.label,
                        calls: self.calls.clone(),
                    }),
                )
                .with_priority(rule.priority);
                trigger.required_permission = rule.permission;
                trigger
            })
            .collect()
    }
    async fn on_event(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        if event.post_type() != "message" {
            self.calls.lock().unwrap().push("notice".into());
            return Ok(PluginResponse::Continue);
        }
        self.calls
            .lock()
            .unwrap()
            .push(format!("event:{}", event.command_text()));
        if event.command_text() == "work" {
            let child = self.child.clone();
            assert!(self.tasks.try_spawn("admitted-child", async move {
                child.started.notify_one();
                child.release.notified().await;
                child.finished.notify_one();
            }));
        }
        Ok(PluginResponse::Continue)
    }
    async fn on_message_listen(&self, _: &Event) -> anyhow::Result<PluginResponse> {
        self.calls.lock().unwrap().push("listener".into());
        Ok(PluginResponse::Continue)
    }
    async fn on_message(&self, _: &Event, _: &str, _: &str) -> anyhow::Result<PluginResponse> {
        self.calls.lock().unwrap().push("wait-callback".into());
        Ok(PluginResponse::Stop)
    }
}

struct AdmissionFixture {
    h: Harness,
    kernel: EventKernel,
    handler: Arc<EventHandler>,
    calls: Arc<Mutex<Vec<String>>>,
    tasks: Arc<PluginTaskScope>,
    child: Arc<ChildGate>,
}

fn single_group_request() -> PluginTriggerPolicy {
    PluginTriggerPolicy {
        user_cooldown_ms: 0,
        max_in_flight_per_user: 0,
        group_requests_per_minute: 1,
        group_burst: 1,
    }
}

impl AdmissionFixture {
    async fn new(
        prefix: &'static str,
        listener: bool,
        rules: Vec<RuleSpec>,
        policy: PluginTriggerPolicy,
    ) -> Self {
        let h = Harness::new("trigger-control").await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tasks = Arc::new(PluginTaskScope::new(&h.plugin));
        let child = Arc::new(ChildGate::default());
        let loaded = LoadedPlugin {
            plugin: Box::new(AdmissionPlugin {
                name: h.plugin.clone(),
                prefix,
                listener,
                rules,
                calls: calls.clone(),
                tasks: tasks.clone(),
                child: child.clone(),
            }),
        };
        let declaration = collect_declaration_sync(&loaded).unwrap();
        h.host.remember_plugin_declaration(&declaration).await;
        let manager = h.host.config_manager.read().await.as_ref().unwrap().clone();
        manager
            .register_with_service_config(
                &h.plugin,
                Default::default(),
                Some(&declaration.service_config),
            )
            .await
            .unwrap();
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
        assert_eq!(
            h.host.plugin_trigger_policy(&h.plugin),
            PluginTriggerPolicy::default()
        );
        assert_eq!(
            h.host
                .set_plugin_trigger_policy(&h.plugin, policy)
                .await
                .unwrap(),
            policy
        );

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
                event_handler: handler.clone(),
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
            handler,
            calls,
            tasks,
            child,
        }
    }

    fn message(text: &str, user: i64) -> Value {
        serde_json::json!({"time":1,"post_type":"message","message_type":"group",
            "sub_type":"normal","self_id":42,"group_id":100,"user_id":user,
            "message_id":1,"raw_message":text,"message":[{"type":"text","data":{"text":text}}],
            "sender":{"user_id":user,"nickname":"fixture","role":"member"}})
    }

    async fn process(&self, event: Value) -> bool {
        let (sender, _receiver) = mpsc::channel(32);
        self.kernel
            .process_runtime_post_event(42, &event.to_string(), event, sender)
            .await
            .unwrap()
            .blocked
    }

    async fn dispatch_rules(&self, text: &str, user: i64) -> crate::core::handler::HandleResult {
        let (sender, _receiver) = mpsc::channel(32);
        self.h
            .host
            .test_group_snapshot(
                Some("100"),
                self.handler.handle_event(
                    42,
                    &Self::message(text, user).to_string(),
                    Some(sender),
                    None,
                ),
            )
            .await
            .unwrap()
    }

    fn recorded(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    async fn cleanup(self) {
        let Self {
            h,
            kernel,
            handler,
            tasks,
            ..
        } = self;
        tasks.shutdown().await;
        drop(kernel);
        drop(handler);
        h.cleanup().await;
    }
}

#[tokio::test]
async fn unprefixed_continue_does_not_consume_group_budget_before_a_real_match() {
    let f = AdmissionFixture::new(
        "",
        false,
        vec![RuleSpec::ping("rule", false, 0)],
        single_group_request(),
    )
    .await;
    assert!(
        !f.process(AdmissionFixture::message("ordinary conversation", 7))
            .await
    );
    assert_eq!(f.recorded(), ["event:ordinary conversation"]);
    assert!(!f.process(AdmissionFixture::message("ping", 7)).await);
    assert!(
        f.recorded().iter().any(|call| call == "rule"),
        "a Continue-only callback consumed the only token"
    );
    f.process(AdmissionFixture::message("ping", 8)).await;
    assert_eq!(
        f.recorded()
            .iter()
            .filter(|call| call.as_str() == "rule")
            .count(),
        1
    );
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).failures, 0);
    f.cleanup().await;
}

#[tokio::test]
async fn one_message_shares_admission_across_on_event_and_ordered_rules() {
    let f = AdmissionFixture::new(
        "ctl",
        false,
        vec![
            RuleSpec::ping("second", true, 2),
            RuleSpec::ping("first", false, 1),
            RuleSpec::ping("never", false, 3),
        ],
        single_group_request(),
    )
    .await;
    assert!(f.process(AdmissionFixture::message("ctlping", 7)).await);
    assert_eq!(f.recorded(), ["event:ping", "first", "second"]);
    let before = f.h.host.plugin_diagnostics(&f.h.plugin);
    assert_eq!(before.calls, 3);
    assert_eq!(before.failures, 0);
    assert!(f.process(AdmissionFixture::message("ctlping", 8)).await);
    assert_eq!(f.recorded(), ["event:ping", "first", "second"]);
    assert_eq!(
        f.h.host.plugin_diagnostics(&f.h.plugin),
        before,
        "a rejected callback is not an executed or failed call"
    );
    f.cleanup().await;
}

#[tokio::test]
async fn rejected_rule_retains_its_block_value_and_does_not_count_as_failure() {
    for block in [false, true] {
        let f = AdmissionFixture::new(
            "",
            false,
            vec![RuleSpec::ping("limited", block, 0)],
            single_group_request(),
        )
        .await;
        let tail = Arc::new(Mutex::new(Vec::new()));
        f.h.host.trigger_registry().register(
            Trigger::new_event(
                TriggerType::FullMatch,
                "ping",
                false,
                false,
                Arc::new(RecordingRule {
                    label: "tail",
                    calls: tail.clone(),
                }),
            )
            .with_priority(10),
        );
        let first = f.dispatch_rules("ping", 7).await;
        assert!(first.matched);
        assert_eq!(first.blocked, block);
        let before = f.h.host.plugin_diagnostics(&f.h.plugin);
        let rejected = f.dispatch_rules("ping", 8).await;
        assert!(rejected.matched);
        assert_eq!(rejected.blocked, block);
        assert_eq!(f.recorded(), ["limited"]);
        assert_eq!(tail.lock().unwrap().len(), if block { 0 } else { 2 });
        assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin), before);
        assert_eq!(before.failures, 0);
        f.cleanup().await;
    }
}

#[tokio::test]
async fn automatic_listener_is_limited_but_non_message_callbacks_are_exempt() {
    let f = AdmissionFixture::new("ctl", true, vec![], single_group_request()).await;
    f.process(AdmissionFixture::message("https://example.com/first", 7))
        .await;
    let before = f.h.host.plugin_diagnostics(&f.h.plugin);
    assert_eq!(f.recorded(), ["listener"]);
    f.process(AdmissionFixture::message("https://example.com/second", 8))
        .await;
    assert_eq!(f.recorded(), ["listener"]);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin), before);
    let context = PluginFilterContext {
        user_id: "7".into(),
        group_id: Some("100".into()),
        msg_type: "group".into(),
        actor_permission: ActorPermission::everyone(),
    };
    let notice = serde_json::json!({"post_type":"notice","notice_type":"group_increase",
        "self_id":42,"group_id":100,"user_id":7});
    for _ in 0..2 {
        f.h.host
            .test_group_snapshot(Some("100"), f.h.host.process_message("", &notice, &context))
            .await;
    }
    assert_eq!(f.recorded(), ["listener", "notice", "notice"]);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).failures, 0);
    f.cleanup().await;
}

#[tokio::test]
async fn plugin_and_command_permission_rejections_do_not_spend_admission() {
    let f = AdmissionFixture::new(
        "ctl",
        false,
        vec![
            RuleSpec {
                pattern: "admin",
                label: "admin",
                block: true,
                priority: 0,
                permission: Some(crate::core::command::PermissionLevel::BotAdmin),
            },
            RuleSpec::ping("allowed", false, 1),
        ],
        single_group_request(),
    )
    .await;
    let original = (*f.h.host.get_plugin_config(&f.h.plugin).await.unwrap()).clone();
    let mut restricted = original.clone();
    restricted.min_permission = PermissionLevel::BotAdmin;
    let manager =
        f.h.host
            .config_manager
            .read()
            .await
            .as_ref()
            .unwrap()
            .clone();
    manager
        .set_values(
            &f.h.plugin,
            HashMap::from([(
                "min_permission".into(),
                ConfigValue::String(PermissionLevel::BotAdmin.as_str().into()),
            )]),
        )
        .await
        .unwrap();
    f.h.host.register_plugin_config(restricted).await;
    f.process(AdmissionFixture::message("ctlping", 7)).await;
    assert!(f.recorded().is_empty());
    manager
        .set_values(
            &f.h.plugin,
            HashMap::from([(
                "min_permission".into(),
                ConfigValue::String(original.min_permission.as_str().into()),
            )]),
        )
        .await
        .unwrap();
    f.h.host.register_plugin_config(original).await;
    assert!(!f.dispatch_rules("ctladmin", 7).await.matched);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).calls, 0);
    assert!(f.dispatch_rules("ctlping", 7).await.matched);
    assert_eq!(f.recorded(), ["allowed"]);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).failures, 0);
    f.cleanup().await;
}

#[tokio::test]
async fn managed_child_retains_user_in_flight_guard_after_on_event_returns() {
    let f = AdmissionFixture::new(
        "ctl",
        false,
        vec![RuleSpec::ping("rule", false, 0)],
        PluginTriggerPolicy {
            user_cooldown_ms: 0,
            max_in_flight_per_user: 1,
            group_requests_per_minute: 0,
            group_burst: 1,
        },
    )
    .await;
    assert!(!f.process(AdmissionFixture::message("ctlwork", 7)).await);
    tokio::time::timeout(Duration::from_secs(5), f.child.started.notified())
        .await
        .unwrap();
    let before = f.h.host.plugin_diagnostics(&f.h.plugin);
    assert_eq!(before.calls, 1);
    assert_eq!(
        before.in_flight, 0,
        "the callback already returned; its managed child keeps admission alive"
    );
    f.process(AdmissionFixture::message("ctlping", 7)).await;
    assert_eq!(f.recorded(), ["event:work"]);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin), before);
    f.child.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), f.child.finished.notified())
        .await
        .unwrap();
    f.tasks.shutdown().await;
    assert!(!f.process(AdmissionFixture::message("ctlping", 7)).await);
    assert_eq!(f.recorded(), ["event:work", "event:ping", "rule"]);
    f.cleanup().await;
}

#[tokio::test]
async fn exhausted_admission_does_not_prevent_a_registered_reply_from_being_delivered() {
    let f = AdmissionFixture::new(
        "ctl",
        false,
        vec![RuleSpec::ping("rule", false, 0)],
        single_group_request(),
    )
    .await;
    f.process(AdmissionFixture::message("ctlping", 7)).await;
    assert_eq!(f.recorded(), ["event:ping", "rule"]);
    let before = f.h.host.plugin_diagnostics(&f.h.plugin);
    let reply = AdmissionFixture::message("this is the requested reply", 7);
    let session = crate::onebot::build_session_id(42, &reply);
    let mut scope = crate::api::waiter::WaitScope::from_event(42, &reply);
    scope.plugin_name = Some(f.h.plugin.clone());
    let receiver =
        f.h.host
            .wait_manager
            .register_scoped_wait(session, Duration::from_secs(5), scope);
    assert!(
        f.process(reply.clone()).await,
        "the waiting stage must consume the reply before ordinary admission"
    );
    let delivered = tokio::time::timeout(Duration::from_secs(5), receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered["raw_message"], reply["raw_message"]);
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin), before);
    // 旧版回调同样豁免触发限流，但仍保留完整的安全与开关校验。
    assert!(matches!(
        f.h.host
            .test_group_snapshot(
                Some("100"),
                f.h.host
                    .dispatch_to_plugin(&f.h.plugin, &reply, "session", "callback")
            )
            .await,
        PluginResponse::Stop
    ));
    assert_eq!(f.recorded().last().unwrap(), "wait-callback");
    f.cleanup().await;
}
