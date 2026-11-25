use super::super::loading::collect_declaration_sync;
use super::*;
use crate::plugin::PluginMessageListener;
use std::sync::{Mutex, atomic::AtomicU8};

struct ListenerPlugin {
    name: String,
    mode: Arc<AtomicU8>,
    received: Arc<Mutex<Vec<(String, String, Value)>>>,
    declarations: Arc<AtomicUsize>,
    prefix: String,
}

#[async_trait::async_trait]
impl Plugin for ListenerPlugin {
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "listener fixture"
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn service_config(&self) -> Plugins {
        Plugins::new(&self.name)
            .with_prefix(vec![self.prefix.clone()])
            .with_auto_listen_enabled(true)
            .with_area(PluginArea::All)
    }
    fn message_listener(&self) -> Option<PluginMessageListener> {
        self.declarations.fetch_add(1, Ordering::SeqCst);
        Some(
            PluginMessageListener::new("链接及分享卡片")
                .with_text_pattern(r"(?i)https?://")
                .unwrap()
                .with_segment_types(["json"]),
        )
    }
    async fn on_event(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        self.received.lock().unwrap().push((
            "command".into(),
            event.command_text().into(),
            event.raw().clone(),
        ));
        Ok(PluginResponse::Stop)
    }
    async fn on_message_listen(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        assert_eq!(event.command_prefix(), "");
        assert_eq!(event.command_text(), event.text());
        self.received.lock().unwrap().push((
            "listener".into(),
            event.text().into(),
            event.raw().clone(),
        ));
        match self.mode.load(Ordering::SeqCst) {
            0 => Ok(PluginResponse::Continue),
            1 => Ok(PluginResponse::Stop),
            2 => Ok(event.reply_text("fixture reply")),
            3 => Ok(PluginResponse::WaitReply {
                reply_message: serde_json::json!({}),
                session_id: "fixture".into(),
                timeout_secs: 1,
                callback_id: "callback".into(),
                plugin_name: self.name.clone(),
                event_context: serde_json::json!({}),
            }),
            4 => anyhow::bail!("listener fixture failure"),
            _ => panic!("listener fixture panic"),
        }
    }
}

struct Fixture {
    h: Harness,
    mode: Arc<AtomicU8>,
    received: Arc<Mutex<Vec<(String, String, Value)>>>,
    declarations: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new(prefix: &str) -> Self {
        let h = Harness::new("auto-listener").await;
        let mode = Arc::new(AtomicU8::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let declarations = Arc::new(AtomicUsize::new(0));
        let loaded = LoadedPlugin {
            plugin: Box::new(ListenerPlugin {
                name: h.plugin.clone(),
                mode: mode.clone(),
                received: received.clone(),
                declarations: declarations.clone(),
                prefix: prefix.into(),
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
        *h.host.plugins.write().await = vec![loaded].into_iter().map(Into::into).collect();
        // 路由测试用例会连续紧密执行；触发准入限流由专门的测试覆盖，此处放开限流。
        h.host.set_unlimited_trigger_policy_for_test(&h.plugin);
        Self {
            h,
            mode,
            received,
            declarations,
        }
    }

    async fn run(&self, raw: Value) -> PluginProcessResult {
        let ctx = PluginFilterContext {
            user_id: "u1".into(),
            group_id: None,
            msg_type: "private".into(),
            actor_permission: ActorPermission::everyone(),
        };
        self.h.host.process_message("", &raw, &ctx).await
    }
    fn event(text: &str) -> Value {
        serde_json::json!({"post_type":"message","message_type":"private", "user_id":"u1",
            "raw_message":text,"message":[{"type":"text","data":{"text":text}}]})
    }
}

#[tokio::test]
async fn automatic_listener_routes_bare_links_and_cards_but_keeps_command_namespace() {
    let f = Fixture::new("url").await;
    let mut card = Fixture::event("分享卡片");
    card["message"] =
        serde_json::json!([{"type":"json","data":{"data":"{\"url\":\"https://example.com\"}"}}]);
    for raw in [Fixture::event("https://example.com/AbC"), card.clone()] {
        f.run(raw).await;
    }
    f.run(Fixture::event("url帮助")).await;
    f.run(Fixture::event("URL https://example.com/AbC")).await;
    f.run(Fixture::event("普通消息")).await;
    let calls = f.received.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["listener", "listener", "command", "command"]
    );
    assert_eq!(calls[1].2, card, "原始JSON段与平台身份必须保留");
    assert_eq!(calls[2].1, "帮助");
    assert_eq!(calls[3].1, "https://example.com/AbC");
    assert_eq!(f.declarations.load(Ordering::SeqCst), 1, "派发不得重复声明");
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).calls, 4);
    f.h.cleanup().await;
}

#[tokio::test]
async fn automatic_listener_cannot_capture_another_owner_reserved_route_or_conflict() {
    let f = Fixture::new("url").await;
    f.h.host
        .register_plugin_config(Plugins::new("peer").with_prefix(vec!["ww".into()]))
        .await;
    f.run(Fixture::event("wwhttps://example.com")).await;
    f.run(Fixture::event("/ai https://example.com")).await;
    f.h.host
        .register_plugin_config(Plugins::new("duplicate").with_prefix(vec!["url".into()]))
        .await;
    assert!(matches!(
        f.run(Fixture::event("urlhttps://example.com"))
            .await
            .response,
        PluginResponse::Reply(_)
    ));
    assert!(f.received.lock().unwrap().is_empty());
    f.h.cleanup().await;
}

#[tokio::test]
async fn automatic_listener_checks_switch_permission_area_and_access_before_callback() {
    let f = Fixture::new("url").await;
    let original = (*f.h.host.get_plugin_config(&f.h.plugin).await.unwrap()).clone();
    for change in 0..5 {
        let mut cfg = original.clone();
        match change {
            0 => cfg.enabled = false,
            1 => cfg.auto_listen_enabled = Some(false),
            2 => cfg.min_permission = PermissionLevel::BotAdmin,
            3 => cfg.area = PluginArea::Group,
            _ => cfg.black_list = vec!["u1".into()],
        }
        f.h.host.register_plugin_config(cfg).await;
        f.run(Fixture::event("https://example.com")).await;
    }
    let mut cfg = original;
    cfg.white_list = vec!["another-user".into()];
    f.h.host.register_plugin_config(cfg).await;
    f.run(Fixture::event("https://example.com")).await;
    assert!(f.received.lock().unwrap().is_empty());
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).calls, 0);
    f.h.cleanup().await;
}

#[tokio::test]
async fn automatic_listener_preserves_response_failure_and_single_entry_semantics() {
    let f = Fixture::new("").await;
    for mode in 0..6 {
        f.mode.store(mode, Ordering::SeqCst);
        let result = f.run(Fixture::event("https://example.com")).await;
        assert!(match mode {
            0 | 4 | 5 => matches!(result.response, PluginResponse::Continue),
            1 => matches!(result.response, PluginResponse::Stop),
            2 => matches!(result.response, PluginResponse::Reply(_)),
            _ => matches!(result.response, PluginResponse::WaitReply { .. }),
        });
    }
    assert_eq!(
        f.received.lock().unwrap().len(),
        6,
        "空命令前缀也不能双调用"
    );
    assert!(f.received.lock().unwrap().iter().all(|c| c.0 == "listener"));
    assert_eq!(f.h.host.plugin_diagnostics(&f.h.plugin).failures, 2);
    f.run(Fixture::event("帮助")).await;
    assert_eq!(f.received.lock().unwrap().last().unwrap().0, "command");
    f.h.cleanup().await;
}

#[tokio::test]
async fn automatic_listener_group_switch_is_independent_from_commands_and_plugin_enablement() {
    use crate::config::schema::ConfigValue;
    let f = Fixture::new("url").await;
    let group = f.h.group("listener");
    let manager =
        f.h.host
            .config_manager
            .read()
            .await
            .as_ref()
            .unwrap()
            .clone();
    let ctx = PluginFilterContext {
        user_id: "u1".into(),
        group_id: Some(group.clone()),
        msg_type: "group".into(),
        actor_permission: ActorPermission::everyone(),
    };
    let bare = serde_json::json!({"post_type":"message", "message_type":"group", "user_id":"u1",
        "group_id":group, "raw_message":"https://example.com"});
    f.h.host
        .test_group_patch(
            &group,
            &f.h.plugin,
            None,
            serde_json::Map::from_iter([("auto_listen_enabled".into(), serde_json::json!(false))]),
        )
        .await
        .unwrap();
    f.h.host
        .test_group_snapshot(Some(&group), f.h.host.process_message("", &bare, &ctx))
        .await;
    assert!(f.received.lock().unwrap().is_empty());
    let mut command = bare.clone();
    command["raw_message"] = serde_json::json!("url帮助");
    f.h.host
        .test_group_snapshot(Some(&group), f.h.host.process_message("", &command, &ctx))
        .await;
    assert_eq!(
        f.received.lock().unwrap().len(),
        1,
        "监听关闭不影响命令入口"
    );
    manager
        .set_values(
            &f.h.plugin,
            HashMap::from([("auto_listen_enabled".into(), ConfigValue::Bool(false))]),
        )
        .await
        .unwrap();
    f.h.host.refresh_plugin_config(&f.h.plugin).await.unwrap();
    f.h.host
        .test_group_patch(
            &group,
            &f.h.plugin,
            None,
            serde_json::Map::from_iter([("auto_listen_enabled".into(), serde_json::json!(true))]),
        )
        .await
        .unwrap();
    f.h.host
        .test_group_snapshot(Some(&group), f.h.host.process_message("", &bare, &ctx))
        .await;
    assert_eq!(
        f.received.lock().unwrap().len(),
        2,
        "群显式开启覆盖监听默认关闭"
    );
    f.h.set_group_enabled(&group, false).await;
    f.h.host
        .test_group_snapshot(Some(&group), f.h.host.process_message("", &bare, &ctx))
        .await;
    assert_eq!(f.received.lock().unwrap().len(), 2, "群插件关闭仍为总闸");
    f.h.cleanup().await;
}
