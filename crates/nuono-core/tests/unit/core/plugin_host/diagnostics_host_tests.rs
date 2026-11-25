use super::*;

struct FailingMessageHandler;

#[async_trait::async_trait]
impl TriggerHandler for FailingMessageHandler {
    async fn handle(&self, _: &crate::api::Bot, _: &Message, _: MatchData) -> anyhow::Result<()> {
        anyhow::bail!("handler failure")
    }
}

struct FailingNativeHandler;

#[async_trait::async_trait]
impl CommandHandler for FailingNativeHandler {
    async fn handle(&self, _: &crate::api::Bot, _: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        anyhow::bail!("native handler failure")
    }
}

fn bot(host: &PluginHost) -> crate::api::Bot {
    crate::api::Bot::new(
        1,
        host.event_bus.clone(),
        host.wait_manager.clone(),
        Arc::new(crate::core::ApiClient::new()),
        None,
        None,
    )
}

fn private_message(text: &str) -> (Value, Message) {
    let value = serde_json::json!({
        "time":1,"self_id":1,"post_type":"message","message_type":"private",
        "sub_type":"friend","message_id":2,"user_id":42,"raw_message":text,
        "message":[{"type":"text","data":{"text":text}}],"font":0,
        "sender":{"user_id":42,"nickname":"test","sex":"unknown","age":0}
    });
    let message = nuono_plugin::onebot::parse_message(&value.to_string()).unwrap();
    (value, message)
}

#[tokio::test]
async fn registered_command_alias_counts_once_and_error_still_blocks_later_rules() {
    let h = Harness::new("diagnostic-command").await;
    let commands = [
        Command::from_message("ping", Arc::new(FailingMessageHandler)).with_alias("p"),
        Command::from_message("never", Arc::new(NoopHandler)),
    ];
    {
        let plugins = h.host.plugins.read().await;
        let loaded = plugins[0].plugin.read().await;
        h.host
            .register_scoped_plugin_triggers(&loaded, &commands)
            .await
            .unwrap();
    }
    let registry = h.host.trigger_registry();
    let (_, message) = private_message("not matched");
    assert!(!registry.handle(&message, &bot(&h.host)).await.unwrap());
    assert_eq!(h.host.plugin_diagnostics(&h.plugin).calls, 0);
    registry.register(diagnostics::observe_trigger(
        Trigger::new(
            TriggerType::Message,
            "later",
            "",
            false,
            false,
            Arc::new(NoopHandler),
        )
        .with_priority(10),
        h.host.diagnostic_metrics(&h.plugin),
        None,
    ));
    let (_, message) = private_message("p");
    assert!(registry.handle(&message, &bot(&h.host)).await.unwrap());
    let snapshot = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(snapshot.calls, 1);
    assert_eq!(snapshot.completed_calls, 1);
    assert_eq!(snapshot.failures, 1);
    assert_eq!(snapshot.in_flight, 0);
    // 元数据与类型转换保持原有的阻塞标记与优先级语义不变。
    assert!(
        registry
            .get_all_commands()
            .iter()
            .find(|c| c.name == "ping")
            .unwrap()
            .block
    );
    assert_eq!(snapshot.slow_threshold_ms, 1_000);
    h.cleanup().await;
}

#[tokio::test]
async fn native_command_real_event_handler_is_observed_without_placeholder_double_count() {
    let h = Harness::new("diagnostic-native").await;
    {
        let plugins = h.host.plugins.read().await;
        let loaded = plugins[0].plugin.read().await;
        h.host
            .register_scoped_plugin_triggers(
                &loaded,
                &[Command::new("native", Arc::new(FailingNativeHandler)).with_alias("n")],
            )
            .await
            .unwrap();
    }
    let registry = h.host.trigger_registry();
    let trigger = registry
        .get_all_triggers()
        .into_iter()
        .find(|t| t.pattern == "n")
        .unwrap();
    let (raw, _) = private_message("n");
    let error = trigger
        .event_handler
        .as_ref()
        .unwrap()
        .handle(&bot(&h.host), &Event::new(&raw), MatchData::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "native handler failure");
    let snapshot = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(snapshot.calls, 1);
    assert_eq!(snapshot.completed_calls, 1);
    assert_eq!(snapshot.failures, 1);
    h.cleanup().await;
}

#[tokio::test]
async fn event_and_wait_callbacks_only_count_after_enablement_and_keep_stop_semantics() {
    let h = Harness::new("diagnostic-gates").await;
    assert!(h.ran_for(None).await);
    assert_eq!(h.host.plugin_diagnostics(&h.plugin).calls, 1);
    let (raw, _) = private_message("reply");
    assert!(matches!(
        h.host
            .dispatch_to_plugin(&h.plugin, &raw, "session", "callback")
            .await,
        PluginResponse::Stop
    ));
    let before = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(before.calls, 2);
    let mut cfg = (*h.host.get_plugin_config(&h.plugin).await.unwrap()).clone();
    cfg.enabled = false;
    h.host.register_plugin_config(cfg).await;
    assert!(!h.ran_for(None).await);
    assert!(matches!(
        h.host
            .dispatch_to_plugin(&h.plugin, &raw, "session", "callback")
            .await,
        PluginResponse::Continue
    ));
    assert_eq!(h.host.plugin_diagnostics(&h.plugin), before);
    h.cleanup().await;
}

struct MediaTool;

#[async_trait::async_trait]
impl AgentTool for MediaTool {
    fn name(&self) -> &str {
        "media"
    }
    fn description(&self) -> &str {
        "Media test tool"
    }
    async fn call(&self, _: AgentToolContext, _: Value) -> Result<Value> {
        panic!("call_with_media must preserve its own delegate")
    }
    async fn call_with_media(
        &self,
        _: AgentToolContext,
        _: Value,
    ) -> Result<nuono_app::agent::tool::AgentToolOutput> {
        Ok(nuono_app::agent::tool::AgentToolOutput::new(
            serde_json::json!({"media":true}),
        ))
    }
}

#[tokio::test]
async fn scoped_media_tool_calls_the_media_delegate_and_records_once() {
    let h = Harness::new("diagnostic-tool").await;
    let tools = agent_skills::scoped_tools(&h.plugin, vec![Arc::new(MediaTool)], vec![]).unwrap();
    let tool =
        diagnostics::observe_tool(tools[0].clone(), h.host.diagnostic_metrics(&h.plugin), None);
    let context = AgentToolContext::new(
        AgentContext::from_runtime_event(
            1,
            &serde_json::json!({"post_type":"message","user_id":42}),
        )
        .unwrap(),
    );
    tool.call_with_media(context, Value::Null).await.unwrap();
    let snapshot = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(snapshot.calls, 1);
    assert_eq!(snapshot.completed_calls, 1);
    assert_eq!(snapshot.failures, 0);
    h.cleanup().await;
}

struct RulesPlugin(String);

#[async_trait::async_trait]
impl Plugin for RulesPlugin {
    fn name(&self) -> &str {
        &self.0
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "Rules diagnostics test"
    }
    fn triggers(&self) -> Vec<Trigger> {
        vec![
            Trigger::new(
                TriggerType::Prefix,
                "rule",
                "",
                true,
                false,
                Arc::new(FailingMessageHandler),
            ),
            Trigger::new_event(
                TriggerType::Prefix,
                "native-rule",
                false,
                false,
                Arc::new(FailingNativeHandler),
            ),
        ]
    }
}

#[tokio::test]
async fn host_registration_observes_message_and_event_rules_after_matching_only() {
    let h = Harness::new("diagnostic-rules").await;
    let plugin = LoadedPlugin {
        plugin: Box::new(RulesPlugin(h.plugin.clone())),
    };
    h.host
        .register_scoped_plugin_triggers(&plugin, &[])
        .await
        .unwrap();
    let registry = h.host.trigger_registry();
    let (_, unmatched) = private_message("unmatched");
    assert!(!registry.handle(&unmatched, &bot(&h.host)).await.unwrap());
    assert_eq!(h.host.plugin_diagnostics(&h.plugin).calls, 0);
    let (_, message) = private_message("rule args");
    assert!(registry.handle(&message, &bot(&h.host)).await.unwrap());
    let trigger = registry
        .get_all_triggers()
        .into_iter()
        .find(|t| t.pattern == "native-rule")
        .unwrap();
    let (raw, _) = private_message("native-rule args");
    assert!(
        trigger
            .event_handler
            .as_ref()
            .unwrap()
            .handle(&bot(&h.host), &Event::new(&raw), MatchData::default())
            .await
            .is_err()
    );
    let snapshot = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(snapshot.calls, 2);
    assert_eq!(snapshot.completed_calls, 2);
    assert_eq!(snapshot.failures, 2);
    h.cleanup().await;
}

struct StatusWebPlugin {
    name: String,
    status: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Plugin for StatusWebPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "Web diagnostics test"
    }
    async fn handle_web_request(
        &self,
        _: &crate::plugin::PluginWebRequest,
    ) -> crate::plugin::PluginWebResponse {
        let status = self.status.load(Ordering::SeqCst) as u16;
        if status == 503 {
            crate::plugin::PluginWebResponse::Http {
                status,
                headers: vec![],
                body: vec![],
            }
        } else {
            crate::plugin::PluginWebResponse::Status(status, "response".into())
        }
    }
}

#[tokio::test]
async fn web_5xx_is_failure_4xx_is_completed_and_unmatched_routes_are_not_counted() {
    let h = Harness::new("diagnostic-web").await;
    let status = Arc::new(AtomicUsize::new(500));
    h.host.plugins.write().await[0] = LoadedPlugin {
        plugin: Box::new(StatusWebPlugin {
            name: h.plugin.clone(),
            status: status.clone(),
        }),
    }
    .into();
    h.host
        .replace_plugin_web_declarations(
            &h.plugin,
            &[crate::plugin::PluginWebRoute::new(
                http::Method::GET,
                "/status",
                crate::plugin::PluginWebAccessPolicy::Public,
            )],
            &[],
        )
        .await;
    let mut request = crate::plugin::PluginWebRequest {
        method: http::Method::GET,
        path: "/missing".into(),
        query: String::new(),
        headers: http::HeaderMap::new(),
        body: vec![],
        user: None,
    };
    assert!(matches!(
        h.host.handle_web_request(&h.plugin, &request).await,
        crate::plugin::PluginWebResponse::NotFound
    ));
    assert_eq!(h.host.plugin_diagnostics(&h.plugin).calls, 0);
    request.path = "/status".into();
    for code in [500, 503, 404, 403] {
        status.store(code, Ordering::SeqCst);
        h.host.handle_web_request(&h.plugin, &request).await;
    }
    let snapshot = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(snapshot.calls, 4);
    assert_eq!(snapshot.completed_calls, 4);
    assert_eq!(snapshot.failures, 2);
    assert_eq!(snapshot.cancelled_calls, 0);
    h.cleanup().await;
}

#[tokio::test]
async fn enable_disable_and_instance_recreation_keep_process_totals() {
    let h = Harness::new("diagnostic-lifecycle").await;
    h.host
        .set_plugin_slow_threshold(&h.plugin, 45_000)
        .await
        .unwrap();
    h.host.toggle(&h.plugin, false).await.unwrap();
    let disabled = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(disabled.calls, 1);
    assert_eq!(disabled.completed_calls, 1);
    h.host.toggle(&h.plugin, true).await.unwrap();
    let enabled = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(enabled.calls, 3, "on_disable + on_load + on_enable");
    assert_eq!(enabled.failures, 0);
    let candidate = LoadedPlugin {
        plugin: Box::new(RulesPlugin(h.plugin.clone())),
    };
    h.host
        .reload_with_candidate(&h.plugin, candidate, true)
        .await
        .unwrap();
    let reloaded = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(
        reloaded.calls, 7,
        "prior totals + on_disable + on_unload + on_load + on_enable"
    );
    assert_eq!(reloaded.completed_calls, 7);
    assert_eq!(reloaded.failures, 0);
    assert_eq!(reloaded.slow_threshold_ms, 45_000);
    assert_eq!(
        h.db.get_plugin_slow_threshold(&h.plugin).await.unwrap(),
        Some(45_000)
    );
    h.cleanup().await;
}

#[tokio::test]
async fn diagnostic_threshold_updates_validate_installed_identity_and_preserve_totals() {
    let h = Harness::new("diagnostic-policy").await;
    assert!(h.ran_for(None).await);
    let before = h.host.plugin_diagnostics(&h.plugin);
    let after = h
        .host
        .set_plugin_slow_threshold(&h.plugin, 30_000)
        .await
        .unwrap();
    assert_eq!(after.slow_threshold_ms, 30_000);
    assert_eq!(after.calls, before.calls);
    assert_eq!(after.total_duration_ms, before.total_duration_ms);
    assert_eq!(
        h.db.get_plugin_slow_threshold(&h.plugin).await.unwrap(),
        Some(30_000)
    );
    assert_eq!(
        h.host
            .set_plugin_slow_threshold(&h.plugin, 30_000)
            .await
            .unwrap(),
        after
    );
    for invalid in [MAX_SLOW_PLUGIN_THRESHOLD_MS + 1, u64::MAX] {
        assert!(matches!(
            h.host.set_plugin_slow_threshold(&h.plugin, invalid).await,
            Err(crate::error::NuonoError::Validation(_))
        ));
    }
    h.db.sync_plugin("historical-policy", "1", &serde_json::json!({}))
        .await
        .unwrap();
    for unknown in ["missing-policy", "historical-policy"] {
        assert!(matches!(
            h.host.set_plugin_slow_threshold(unknown, 30_000).await,
            Err(crate::error::NuonoError::PluginNotFound(_))
        ));
        assert_eq!(h.db.get_plugin_slow_threshold(unknown).await.unwrap(), None);
    }
    h.host
        .set_plugin_slow_threshold(&h.plugin, 0)
        .await
        .unwrap();
    assert!(h.ran_for(None).await);
    let disabled = h.host.plugin_diagnostics(&h.plugin);
    assert_eq!(disabled.calls, 2);
    assert_eq!(disabled.slow_threshold_ms, 0);
    assert_eq!(disabled.slow_calls, 0);
    assert_eq!(disabled.last_slow, None);
    h.host
        .set_plugin_slow_threshold(&h.plugin, MAX_SLOW_PLUGIN_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(
        h.db.get_plugin_slow_threshold(&h.plugin).await.unwrap(),
        Some(MAX_SLOW_PLUGIN_THRESHOLD_MS)
    );
    h.cleanup().await;
}

#[tokio::test]
async fn concurrent_threshold_updates_finish_with_matching_database_and_live_policy() {
    let h = Harness::new("diagnostic-policy-concurrent").await;
    let futures = (0..24).map(|index| {
        let host = h.host.clone();
        let name = h.plugin.clone();
        async move {
            let threshold = index * 1_000;
            let saved = host
                .set_plugin_slow_threshold(&name, threshold)
                .await
                .unwrap();
            assert_eq!(saved.slow_threshold_ms, threshold);
        }
    });
    futures::future::join_all(futures).await;
    assert_eq!(
        h.db.get_plugin_slow_threshold(&h.plugin).await.unwrap(),
        Some(h.host.plugin_diagnostics(&h.plugin).slow_threshold_ms)
    );
    h.cleanup().await;
}

#[tokio::test]
async fn failed_threshold_write_does_not_publish_live_policy_or_reset_statistics() {
    use sea_orm::{ConnectionTrait, DbBackend};
    let h = Harness::new("diagnostic-policy-failure").await;
    assert!(h.ran_for(None).await);
    let before = h.host.plugin_diagnostics(&h.plugin);
    if h.db.backend() == DbBackend::Postgres {
        h.db.orm().execute_unprepared("CREATE FUNCTION reject_diagnostic_policy() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture policy failure'; END; $$").await.unwrap();
        h.db.orm().execute_unprepared("CREATE TRIGGER reject_diagnostic_policy BEFORE UPDATE OF slow_threshold_ms ON nuono_plugins FOR EACH ROW EXECUTE FUNCTION reject_diagnostic_policy()").await.unwrap();
    } else {
        h.db.orm().execute_unprepared("CREATE TRIGGER reject_diagnostic_policy BEFORE UPDATE OF slow_threshold_ms ON nuono_plugins BEGIN SELECT RAISE(ABORT, 'fixture policy failure'); END").await.unwrap();
    }
    assert!(
        h.host
            .set_plugin_slow_threshold(&h.plugin, 30_000)
            .await
            .is_err()
    );
    assert_eq!(h.host.plugin_diagnostics(&h.plugin), before);
    assert_eq!(
        h.db.get_plugin_slow_threshold(&h.plugin).await.unwrap(),
        None
    );
    h.cleanup().await;
}

struct PolicyReadingPlugin {
    name: String,
    host: std::sync::Weak<PluginHost>,
    observed: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait::async_trait]
impl Plugin for PolicyReadingPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "Persisted diagnostics policy fixture"
    }
    async fn on_load(&mut self, _: &crate::plugin::PluginContext) -> anyhow::Result<()> {
        self.observed.store(
            self.host
                .upgrade()
                .unwrap()
                .plugin_diagnostics(&self.name)
                .slow_threshold_ms,
            Ordering::SeqCst,
        );
        Ok(())
    }
}

#[tokio::test]
async fn persisted_threshold_is_read_before_first_on_load_and_for_disabled_plugins() {
    let h = Harness::new("diagnostic-policy-load").await;
    let loaded = format!("{}Loaded", h.plugin);
    let disabled = format!("{}Disabled", h.plugin);
    let observed = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    let disabled_observed = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    for name in [&loaded, &disabled] {
        h.db.sync_plugin(name, "1", &serde_json::json!({}))
            .await
            .unwrap();
    }
    h.db.set_plugin_slow_threshold(&loaded, 45_000)
        .await
        .unwrap();
    h.db.set_plugin_slow_threshold(&disabled, 0).await.unwrap();
    h.db.set_plugin_enabled(&disabled, false).await.unwrap();
    assert_eq!(h.host.plugin_diagnostics(&loaded).slow_threshold_ms, 1_000);
    let candidates = [
        (loaded.clone(), observed.clone()),
        (disabled.clone(), disabled_observed.clone()),
    ]
    .into_iter()
    .map(|(name, observed)| LoadedPlugin {
        plugin: Box::new(PolicyReadingPlugin {
            name,
            host: Arc::downgrade(&h.host),
            observed,
        }),
    })
    .collect();
    let plan = h.host.resolve_load_candidates(candidates).await.unwrap();
    assert_eq!(h.host.install_load_plan(plan).await.unwrap(), 2);
    assert_eq!(
        observed.load(Ordering::SeqCst),
        45_000,
        "on_load sees persisted policy"
    );
    assert_eq!(
        disabled_observed.load(Ordering::SeqCst),
        u64::MAX,
        "disabled callback stays gated"
    );
    assert_eq!(h.host.plugin_diagnostics(&disabled).slow_threshold_ms, 0);
    assert_eq!(h.host.plugin_diagnostics(&disabled).calls, 0);
    h.cleanup().await;
}
