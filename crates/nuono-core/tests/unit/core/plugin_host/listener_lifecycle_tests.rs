use super::super::*;
use crate::config::PluginConfigManager;
use crate::plugin::{LoadedPlugin, Plugin, PluginMessageListener};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ChangingListener {
    name: String,
    mode: Arc<AtomicUsize>,
    declarations: Arc<AtomicUsize>,
    listens: Arc<AtomicUsize>,
    commands: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Plugin for ChangingListener {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "监听声明生命周期测试"
    }

    fn service_config(&self) -> Plugins {
        self.declarations.fetch_add(1, Ordering::SeqCst);
        Plugins::new(&self.name)
            .with_prefix(vec!["go".into()])
            .with_auto_listen_enabled(true)
    }

    fn message_listener(&self) -> Option<PluginMessageListener> {
        self.declarations.fetch_add(1, Ordering::SeqCst);
        let pattern = match self.mode.load(Ordering::SeqCst) {
            1 => "^alpha$",
            2 => "^beta$",
            _ => return None,
        };
        Some(
            PluginMessageListener::new("当前激活周期的监听范围")
                .with_text_pattern(pattern)
                .unwrap(),
        )
    }

    async fn on_event(&self, _: &Event) -> anyhow::Result<PluginResponse> {
        self.commands.fetch_add(1, Ordering::SeqCst);
        Ok(PluginResponse::Continue)
    }

    async fn on_message_listen(&self, _: &Event) -> anyhow::Result<PluginResponse> {
        self.listens.fetch_add(1, Ordering::SeqCst);
        Ok(PluginResponse::Continue)
    }
}

async fn deliver(host: &PluginHost, text: &str) {
    let raw = serde_json::json!({
        "post_type": "message",
        "message_type": "private",
        "self_id": 1,
        "user_id": 42,
        "raw_message": text,
        "message": [{"type": "text", "data": {"text": text}}],
    });
    let filter = PluginFilterContext {
        user_id: "42".into(),
        group_id: None,
        msg_type: "private".into(),
        actor_permission: ActorPermission::everyone(),
    };
    assert!(matches!(
        host.process_message("", &raw, &filter).await.response,
        PluginResponse::Continue
    ));
}

#[tokio::test]
async fn enable_publishes_the_current_listener_declaration_and_preserves_disabled_metadata() {
    let db = crate::test_support::test_db(2).await;
    let name = format!("ChangingListener{}", uuid::Uuid::new_v4().simple());
    let root = std::env::temp_dir().join(format!("nuono-{name}"));
    let manager = Arc::new(PluginConfigManager::new(root.clone()));
    let host = PluginHost::new(
        db.clone(),
        Arc::new(EventBus::new()),
        Arc::new(PermissionManager::new(db.clone())),
        crate::core::Paths::with_root(&root),
        Arc::new(WaitManager::new()),
        FrameworkControl::new(),
        manager.clone(),
    )
    .await
    .unwrap();
    let mode = Arc::new(AtomicUsize::new(0));
    let declarations = Arc::new(AtomicUsize::new(0));
    let listens = Arc::new(AtomicUsize::new(0));
    let commands = Arc::new(AtomicUsize::new(0));
    host.init_with_plugins(vec![LoadedPlugin {
        plugin: Box::new(ChangingListener {
            name: name.clone(),
            mode: mode.clone(),
            declarations: declarations.clone(),
            listens: listens.clone(),
            commands: commands.clone(),
        }),
    }])
    .await
    .unwrap();
    // 验证连续消息之间的监听器声明变更，关闭准入限流以免干扰测试。
    host.set_unlimited_trigger_policy_for_test(&name);
    assert!(host.get_plugin_info(&name).await.message_listener.is_none());
    assert_eq!(
        host.get_plugin_config(&name)
            .await
            .unwrap()
            .auto_listen_enabled,
        None
    );
    deliver(&host, "alpha").await;
    assert_eq!(listens.load(Ordering::SeqCst), 0);

    // None -> Some 时，服务 schema、运行开关和用于派发/页面的声明同步发布。
    host.toggle(&name, false).await.unwrap();
    mode.store(1, Ordering::SeqCst);
    host.toggle(&name, true).await.unwrap();
    assert_eq!(
        host.get_plugin_info(&name)
            .await
            .message_listener
            .unwrap()
            .text_pattern(),
        Some("^alpha$")
    );
    assert_eq!(
        host.get_plugin_config(&name)
            .await
            .unwrap()
            .auto_listen_enabled,
        Some(true)
    );
    deliver(&host, "alpha").await;
    assert_eq!(listens.load(Ordering::SeqCst), 1);
    deliver(&host, "go alpha").await;
    assert_eq!(commands.load(Ordering::SeqCst), 1);
    assert_eq!(listens.load(Ordering::SeqCst), 1);

    host.toggle(&name, false).await.unwrap();
    let disabled = host.get_plugin_info(&name).await;
    assert!(!disabled.enabled);
    assert_eq!(
        disabled.message_listener.unwrap().text_pattern(),
        Some("^alpha$")
    );
    deliver(&host, "alpha").await;
    assert_eq!(listens.load(Ordering::SeqCst), 1);
    mode.store(2, Ordering::SeqCst);
    host.toggle(&name, true).await.unwrap();
    assert_eq!(
        host.get_plugin_info(&name)
            .await
            .message_listener
            .unwrap()
            .text_pattern(),
        Some("^beta$")
    );
    let declared = declarations.load(Ordering::SeqCst);
    for _ in 0..2 {
        assert!(host.installed_plugin_infos().await.contains_key(&name));
        deliver(&host, "alpha").await;
    }
    deliver(&host, "beta").await;
    assert_eq!(listens.load(Ordering::SeqCst), 2);
    assert_eq!(
        declarations.load(Ordering::SeqCst),
        declared,
        "管理读取和消息匹配不能重复声明"
    );

    // Some -> None 后，文件中的旧 true 保留，但不再授予监听能力或管理字段。
    host.toggle(&name, false).await.unwrap();
    mode.store(0, Ordering::SeqCst);
    host.toggle(&name, true).await.unwrap();
    assert!(host.get_plugin_info(&name).await.message_listener.is_none());
    assert_eq!(
        host.get_plugin_config(&name)
            .await
            .unwrap()
            .auto_listen_enabled,
        None
    );
    assert!(
        !manager
            .get_form_schema(&name)
            .await
            .unwrap()
            .contains_key("auto_listen_enabled")
    );
    assert_eq!(
        manager.get_runtime_config(&name).await.unwrap()["auto_listen_enabled"],
        true
    );
    deliver(&host, "beta").await;
    assert_eq!(listens.load(Ordering::SeqCst), 2);

    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
    tokio::fs::remove_dir_all(root).await.unwrap();
}
