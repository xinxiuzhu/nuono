use super::*;
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    block_load: std::sync::atomic::AtomicBool,
    load_release: Notify,
    bus_calls: AtomicUsize,
    block_command: std::sync::atomic::AtomicBool,
    tool_entered: Notify,
    disable_observed: AtomicUsize,
    entered: Notify,
    background_started: Notify,
    events: AtomicUsize,
    cancelled: AtomicUsize,
    background_cancelled: AtomicUsize,
    config_calls: AtomicUsize,
    config_value: AtomicUsize,
    command_calls: AtomicUsize,
}
struct DropCount(Arc<Probe>, bool);
impl Drop for DropCount {
    fn drop(&mut self) {
        let counter = if self.1 {
            &self.0.background_cancelled
        } else {
            &self.0.cancelled
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }
}
struct IsolatedPlugin {
    name: String,
    prefix: String,
    probe: Arc<Probe>,
    hang_event: bool,
    hang_config: bool,
    hang_disable: bool,
}
struct CommandProbe(Arc<Probe>);
#[async_trait::async_trait]
impl CommandHandler for CommandProbe {
    async fn handle(&self, _: &crate::api::Bot, _: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        self.0.command_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.block_command.load(Ordering::SeqCst) {
            let _dropped = DropCount(self.0.clone(), false);
            self.0.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
struct BusProbe(Arc<Probe>);
#[async_trait::async_trait]
impl crate::core::event_bus::EventHandler for BusProbe {
    fn event_types(&self) -> Vec<crate::core::event_bus::EventType> {
        vec![crate::core::event_bus::EventType::BotConnected]
    }
    async fn handle(
        &self,
        _: &crate::core::event_bus::Event,
    ) -> anyhow::Result<crate::core::event_bus::EventResult> {
        self.0.bus_calls.fetch_add(1, Ordering::SeqCst);
        Ok(crate::core::event_bus::EventResult::Continue)
    }
}
struct ToolProbe(Arc<Probe>);
#[async_trait::async_trait]
impl AgentTool for ToolProbe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "pending lifecycle probe"
    }
    async fn call(&self, _: AgentToolContext, _: Value) -> Result<Value> {
        let _dropped = DropCount(self.0.clone(), false);
        self.0.tool_entered.notify_one();
        std::future::pending().await
    }
}
#[async_trait::async_trait]
impl Plugin for IsolatedPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "plugin isolation fault fixture"
    }
    fn service_config(&self) -> Plugins {
        Plugins::new(&self.name).with_prefix(vec![self.prefix.clone()])
    }
    fn config_fields(&self) -> Vec<crate::core::config_schema::PluginConfigField> {
        vec![crate::core::config_schema::PluginConfigField::number(
            "count", "Count", "", 3,
        )]
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::new(
            "ping",
            Arc::new(CommandProbe(self.probe.clone())),
        )]
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(ToolProbe(self.probe.clone()))]
    }
    async fn on_load(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        ctx.event_bus()
            .subscribe(Arc::new(BusProbe(self.probe.clone())));
        let probe = self.probe.clone();
        let dropped = DropCount(probe.clone(), true);
        ctx.spawn("pending-fixture", async move {
            let _dropped = dropped;
            probe.background_started.notify_one();
            std::future::pending::<()>().await;
        });
        if self.probe.block_load.load(Ordering::SeqCst) {
            self.probe.entered.notify_one();
            self.probe.load_release.notified().await;
        }
        Ok(())
    }
    async fn on_event(&self, _: &Event) -> anyhow::Result<PluginResponse> {
        self.probe.events.fetch_add(1, Ordering::SeqCst);
        if self.hang_event {
            let _dropped = DropCount(self.probe.clone(), false);
            self.probe.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(PluginResponse::Stop)
    }
    async fn on_config_changed(&mut self, config: &Value) -> anyhow::Result<()> {
        self.probe.config_calls.fetch_add(1, Ordering::SeqCst);
        if self.hang_config {
            self.probe.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.probe
            .config_value
            .store(config["count"].as_u64().unwrap() as usize, Ordering::SeqCst);
        Ok(())
    }
    async fn on_disable(&mut self) -> anyhow::Result<()> {
        self.probe.disable_observed.store(
            self.probe.cancelled.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        if self.hang_disable {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
fn fixture(
    name: &str,
    prefix: &str,
    probe: Arc<Probe>,
    event: bool,
    config: bool,
    disable: bool,
) -> LoadedPlugin {
    LoadedPlugin {
        plugin: Box::new(IsolatedPlugin {
            name: name.into(),
            prefix: prefix.into(),
            probe,
            hang_event: event,
            hang_config: config,
            hang_disable: disable,
        }),
    }
}
async fn send(host: &PluginHost, text: &str) -> PluginProcessResult {
    host.process_message("", &serde_json::json!({"post_type":"message","message_type":"private","user_id":"7","raw_message":text}),
        &PluginFilterContext { user_id: "7".into(), group_id: None, msg_type: "private".into(), actor_permission: ActorPermission::everyone() }).await
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("unrelated plugin operation must make progress")
}

/// 存储校验在 on_load 之前执行，在 PostgreSQL 下耗时可能更长。
/// 因此需等待回调真正进入人为阻塞点后，再开始执行隔离性断言。
async fn activation_reaches_block<T: std::fmt::Debug>(
    probe: &Probe,
    activation: &mut tokio::task::JoinHandle<T>,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            biased;
            result = &mut *activation => panic!("activation ended before reaching its block: {result:?}"),
            _ = probe.entered.notified() => {},
        }
    })
    .await
    .expect("database preparation must reach on_load");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            probe.background_started.notified()
        )
        .await
        .is_err(),
        "business task must remain queued until activation commits",
    );
    assert!(
        !activation.is_finished(),
        "the lifecycle timeout must not substitute for the deliberately blocked callback"
    );
}

#[tokio::test]
async fn hanging_callback_does_not_block_other_plugin_and_disable_revokes_old_snapshots() {
    let h = Harness::new("isolated-event").await;
    h.host.toggle(&h.plugin, false).await.unwrap();
    let a = format!("{}A", h.plugin);
    let b = format!("{}B", h.plugin);
    let probe_a = Arc::new(Probe::default());
    let probe_b = Arc::new(Probe::default());
    h.host
        .init_with_plugins(vec![
            fixture(&a, "a:", probe_a.clone(), true, false, false),
            fixture(&b, "b:", probe_b.clone(), false, false, false),
        ])
        .await
        .unwrap();
    bounded(probe_a.background_started.notified()).await;
    let old = h.host.hosted_plugin(&a).await.unwrap().invocation();
    let old_command = h
        .host
        .trigger_registry
        .get_all_commands()
        .into_iter()
        .find(|c| c.plugin_name.as_deref() == Some(&a))
        .unwrap();
    let worker_host = h.host.clone();
    let worker = tokio::spawn(async move { send(&worker_host, "a:wait").await });
    bounded(probe_a.entered.notified()).await;
    let manager = h.host.config_manager.read().await.as_ref().unwrap().clone();
    let monitor = h.host.start_config_monitor();
    // 在插件 A 挂起期间运行一次无变更的配置扫描，验证不会阻塞。
    bounded(h.host.refresh_plugin_config(&a)).await.unwrap();
    assert_eq!(
        bounded(send(&h.host, "b:ping"))
            .await
            .plugin_name
            .as_deref(),
        Some(b.as_str())
    );
    manager
        .set_values(&b, HashMap::from([("count".into(), ConfigValue::Int(9))]))
        .await
        .unwrap();
    bounded(h.host.refresh_plugin_config(&b)).await.unwrap();
    assert_eq!(probe_b.config_value.load(Ordering::SeqCst), 9);
    bounded(h.host.toggle(&a, false)).await.unwrap();
    bounded(worker).await.unwrap();
    assert_eq!(probe_a.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(probe_a.background_cancelled.load(Ordering::SeqCst), 1);
    h.host.toggle(&a, true).await.unwrap();
    assert!(
        old.read().await.is_none(),
        "re-enable must not revive a captured instance generation"
    );
    let bot = crate::api::Bot::new(
        1,
        h.host.event_bus.clone(),
        h.host.wait_manager.clone(),
        Arc::new(crate::core::ApiClient::new()),
        None,
        None,
    );
    let raw = serde_json::json!({"post_type":"message","message_type":"private","user_id":7,"raw_message":"ping"});
    assert!(
        old_command
            .handler
            .as_ref()
            .unwrap()
            .handle(&bot, &Event::new(&raw), MatchData::new("ping".into()))
            .await
            .is_err()
    );
    assert_eq!(
        probe_a.command_calls.load(Ordering::SeqCst),
        0,
        "a previously matched command must remain revoked"
    );
    monitor.abort();
    let _ = monitor.await;
    h.host.shutdown().await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn config_timeout_is_reported_once_and_other_plugin_remains_available() {
    let h = Harness::new("isolated-config").await;
    h.host.toggle(&h.plugin, false).await.unwrap();
    let a = format!("{}A", h.plugin);
    let b = format!("{}B", h.plugin);
    let probe = Arc::new(Probe::default());
    h.host
        .init_with_plugins(vec![
            fixture(&a, "a:", probe.clone(), false, true, false),
            fixture(&b, "b:", Arc::new(Probe::default()), false, false, false),
        ])
        .await
        .unwrap();
    let manager = h.host.config_manager.read().await.as_ref().unwrap().clone();
    manager
        .set_values(&a, HashMap::from([("count".into(), ConfigValue::Int(9))]))
        .await
        .unwrap();
    let host = h.host.clone();
    let name = a.clone();
    let refresh = tokio::spawn(async move { host.refresh_plugin_config(&name).await });
    bounded(probe.entered.notified()).await;
    assert_eq!(
        bounded(send(&h.host, "b:ping"))
            .await
            .plugin_name
            .as_deref(),
        Some(b.as_str())
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(7), refresh)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(manager.status(&a).await.runtime_error.is_some());
    assert!(bounded(h.host.refresh_plugin_config(&a)).await.is_err());
    assert_eq!(probe.config_calls.load(Ordering::SeqCst), 1);
    bounded(h.host.toggle(&a, false)).await.unwrap();
    h.host.shutdown().await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn pending_disable_callback_expires_and_keeps_plugin_disabled() {
    let h = Harness::new("isolated-disable").await;
    let a = format!("{}A", h.plugin);
    let probe = Arc::new(Probe::default());
    h.host
        .init_with_plugins(vec![fixture(&a, "a:", probe.clone(), false, false, true)])
        .await
        .unwrap();
    bounded(probe.background_started.notified()).await;
    let result = tokio::time::timeout(Duration::from_secs(7), h.host.toggle(&a, false))
        .await
        .expect("disable deadline");
    assert!(result.is_err());
    assert!(!h.host.get_plugin_config(&a).await.unwrap().enabled);
    assert!(!h.db.is_plugin_enabled(&a).await.unwrap());
    assert_eq!(probe.background_cancelled.load(Ordering::SeqCst), 1);
    assert!(
        h.host
            .hosted_plugin(&a)
            .await
            .unwrap()
            .invocation()
            .read()
            .await
            .is_none()
    );
    h.cleanup().await;
}

#[tokio::test]
async fn aborted_enable_revokes_prepared_generation_and_aborts_new_scope() {
    let h = Harness::new("aborted-enable").await;
    let a = format!("{}A", h.plugin);
    let probe = Arc::new(Probe::default());
    h.host
        .init_with_plugins(vec![fixture(&a, "a:", probe.clone(), false, false, false)])
        .await
        .unwrap();
    bounded(probe.background_started.notified()).await;
    let old = h.host.hosted_plugin(&a).await.unwrap().invocation();
    h.host.toggle(&a, false).await.unwrap();
    probe.block_load.store(true, Ordering::SeqCst);
    let host = h.host.clone();
    let name = a.clone();
    let mut enabling = tokio::spawn(async move { host.toggle(&name, true).await });
    activation_reaches_block(&probe, &mut enabling).await;
    let prepared = h.host.hosted_plugin(&a).await.unwrap().invocation();
    enabling.abort();
    assert!(enabling.await.unwrap_err().is_cancelled());
    assert!(old.read().await.is_none());
    assert!(prepared.read().await.is_none());
    assert!(
        h.host
            .hosted_plugin(&a)
            .await
            .unwrap()
            .invocation()
            .read()
            .await
            .is_none()
    );
    assert!(!h.host.get_plugin_config(&a).await.unwrap().enabled);
    h.host
        .event_bus
        .publish(crate::core::event_bus::Event::new(
            crate::core::event_bus::EventType::BotConnected,
            1,
            serde_json::json!({}),
        ))
        .await;
    assert_eq!(
        probe.bus_calls.load(Ordering::SeqCst),
        0,
        "aborted activation removes event-bus subscriptions immediately"
    );

    bounded(async {
        while probe.background_cancelled.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    h.host.shutdown().await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn disable_cancels_and_joins_registered_command_and_tool_before_lifecycle_callback() {
    let h = Harness::new("registered-quiescence").await;
    let a = format!("{}A", h.plugin);
    let probe = Arc::new(Probe::default());
    h.host
        .init_with_plugins(vec![fixture(&a, "a:", probe.clone(), false, false, false)])
        .await
        .unwrap();
    probe.block_command.store(true, Ordering::SeqCst);
    let command = h
        .host
        .trigger_registry
        .get_all_commands()
        .into_iter()
        .find(|c| c.plugin_name.as_deref() == Some(&a))
        .unwrap();
    let host = h.host.clone();
    let command_task = tokio::spawn(async move {
        let bot = crate::api::Bot::new(
            1,
            host.event_bus.clone(),
            host.wait_manager.clone(),
            Arc::new(crate::core::ApiClient::new()),
            None,
            None,
        );
        let raw = serde_json::json!({"post_type":"message","message_type":"private","user_id":7,"raw_message":"ping"});
        command
            .handler
            .as_ref()
            .unwrap()
            .handle(&bot, &Event::new(&raw), MatchData::new("ping".into()))
            .await
    });
    let tool = h
        .host
        .tool_registry
        .get(&format!("plugin.{a}.probe"))
        .unwrap();
    let tool_task = tokio::spawn(async move {
        let raw = serde_json::json!({"post_type":"message","message_type":"private","user_id":7,"raw_message":"ping"});
        tool.call(
            AgentToolContext::new(AgentContext::from_runtime_event(1, &raw).unwrap()),
            serde_json::json!({}),
        )
        .await
    });
    bounded(probe.entered.notified()).await;
    bounded(probe.tool_entered.notified()).await;
    bounded(h.host.toggle(&a, false)).await.unwrap();
    assert!(bounded(command_task).await.unwrap().is_err());
    assert!(bounded(tool_task).await.unwrap().is_err());
    assert_eq!(
        probe.disable_observed.load(Ordering::SeqCst),
        2,
        "on_disable starts after both callback futures have been dropped"
    );
    h.cleanup().await;
}

async fn shutdown_during_activation(reenable: bool) {
    let h = Harness::new("shutdown-activation").await;
    let a = format!("{}A", h.plugin);
    let probe = Arc::new(Probe::default());
    if reenable {
        h.host
            .init_with_plugins(vec![fixture(&a, "a:", probe.clone(), false, false, false)])
            .await
            .unwrap();
        bounded(probe.background_started.notified()).await;
        h.host.toggle(&a, false).await.unwrap();
    }
    probe.block_load.store(true, Ordering::SeqCst);
    let host = h.host.clone();
    let name = a.clone();
    let activation_probe = probe.clone();
    let mut activation = tokio::spawn(async move {
        if reenable {
            host.toggle(&name, true).await
        } else {
            host.init_with_plugins(vec![fixture(
                &name,
                "a:",
                activation_probe,
                false,
                false,
                false,
            )])
            .await
        }
    });
    activation_reaches_block(&probe, &mut activation).await;
    let host = h.host.clone();
    let shutdown = tokio::spawn(async move { host.shutdown().await });
    bounded(async {
        while !h.host.closing.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        bounded(h.host.toggle(&a, true)).await.is_err(),
        "closing rejects new lifecycle work immediately"
    );
    assert!(
        !shutdown.is_finished(),
        "shutdown waits for already admitted activation"
    );
    probe.load_release.notify_one();
    bounded(activation).await.unwrap().unwrap();
    bounded(shutdown).await.unwrap().unwrap();
    assert!(h.host.all_plugins().await.is_empty());
    assert!(
        h.host
            .tool_registry
            .get(&format!("plugin.{a}.probe"))
            .is_none()
    );
    assert_eq!(
        probe.background_cancelled.load(Ordering::SeqCst),
        if reenable { 2 } else { 1 }
    );
    assert_eq!(
        h.host
            .event_bus
            .subscriber_count(crate::core::event_bus::EventType::BotConnected),
        0
    );
    h.cleanup().await;
}
#[tokio::test]
async fn shutdown_fences_an_in_progress_enable_generation() {
    shutdown_during_activation(true).await;
}
#[tokio::test]
async fn shutdown_fences_an_in_progress_install_publication() {
    shutdown_during_activation(false).await;
}
