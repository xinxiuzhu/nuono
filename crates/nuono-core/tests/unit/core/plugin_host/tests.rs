use super::*;
use crate::config::PluginConfigManager;
use crate::core::command::{Command, CommandHandler};
use crate::core::trigger::{MatchData, Trigger, TriggerHandler, TriggerType};
use crate::onebot::Message;
use crate::plugin::{LoadedPlugin, Plugin};
use crate::service::group_plugins;
use sea_orm::ConnectionTrait;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "service_policy_tests.rs"]
mod service_policy_tests;

#[path = "activation_contract_tests.rs"]
mod activation_contract_tests;

#[path = "isolation_tests.rs"]
mod isolation_tests;

#[path = "inventory_tests.rs"]
mod inventory_tests;

#[path = "prune_tests.rs"]
mod prune_tests;

#[path = "diagnostics_host_tests.rs"]
mod diagnostics_host_tests;

#[path = "trigger_control_host_tests.rs"]
mod trigger_control_host_tests;

#[path = "auto_listener_tests.rs"]
mod auto_listener_tests;

#[path = "interaction_host_tests.rs"]
mod interaction_host_tests;

#[path = "runtime_tests.rs"]
mod runtime_tests;

#[path = "scope_lifecycle_fault_tests.rs"]
mod scope_lifecycle_fault_tests;

#[path = "wait_reply_fault_tests.rs"]
mod wait_reply_fault_tests;

#[test]
fn plugin_scoped_tools_forward_contextual_resource_declarations() {
    use crate::agent::{ToolResource, ToolScheduling};
    struct DeclaredTool;
    #[async_trait::async_trait]
    impl AgentTool for DeclaredTool {
        fn name(&self) -> &str {
            "notes"
        }
        fn description(&self) -> &str {
            "plugin resource declaration"
        }
        fn scheduling(&self, ctx: &AgentContext, input: &Value) -> ToolScheduling {
            ToolScheduling::concurrent([ToolResource::write(format!(
                "notes:{}:{}",
                ctx.user_identity(),
                input["id"].as_str().unwrap()
            ))])
        }
        async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
            Ok(input)
        }
    }
    let scoped = PluginScopedTool {
        scoped_name: "plugin.example.notes".into(),
        inner: Arc::new(DeclaredTool),
        diagnostics: None,
        epoch: None,
    };
    let ctx = AgentContext::from_runtime_event(
        1,
        &serde_json::json!({"post_type":"message", "user_id":42}),
    )
    .unwrap();
    assert_eq!(scoped.name(), "plugin.example.notes");
    assert_eq!(
        scoped.scheduling(&ctx, &serde_json::json!({"id":"entry"})),
        ToolScheduling::concurrent([ToolResource::write("notes:42:entry")])
    );
}

#[test]
fn plugin_web_routes_require_unambiguous_absolute_paths() {
    use crate::plugin::{PluginWebAccessPolicy, PluginWebRoute};
    use http::Method;

    for path in [
        "",
        "callback",
        "//callback",
        "/callback/",
        "/a//b",
        "/a/../b",
        "/a?x=1",
        "/a#b",
    ] {
        let routes = vec![PluginWebRoute::new(
            Method::POST,
            path,
            PluginWebAccessPolicy::Public,
        )];
        assert!(
            validated_plugin_web_routes("web", routes).is_err(),
            "ambiguous route path {path:?} must be rejected"
        );
    }

    let routes = vec![
        PluginWebRoute::new(Method::POST, "/callback", PluginWebAccessPolicy::Public),
        PluginWebRoute::new(
            Method::GET,
            "/dashboard",
            PluginWebAccessPolicy::Authenticated,
        ),
    ];
    assert!(validated_plugin_web_routes("web", routes).is_ok());
}

#[test]
fn plugin_web_routes_reject_duplicate_method_and_path() {
    use crate::plugin::{PluginWebAccessPolicy, PluginWebRoute};
    use http::Method;

    let routes = vec![
        PluginWebRoute::new(Method::POST, "/callback", PluginWebAccessPolicy::Public),
        PluginWebRoute::new(
            Method::POST,
            "/callback",
            PluginWebAccessPolicy::ManageRuntime,
        ),
    ];
    assert!(validated_plugin_web_routes("web", routes).is_err());
}

#[tokio::test]
async fn plugin_web_policy_matches_exact_route_and_defaults_closed() {
    use crate::plugin::{PluginWebAccessPolicy, PluginWebRoute};
    use http::Method;

    let h = Harness::new("web-policy").await;
    h.host.web_routes.write().await.insert(
        h.plugin.clone(),
        vec![
            PluginWebRoute::new(Method::POST, "/callback", PluginWebAccessPolicy::Public),
            PluginWebRoute::new(
                Method::GET,
                "/dashboard",
                PluginWebAccessPolicy::Authenticated,
            ),
        ],
    );

    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::POST, "/callback")
            .await,
        PluginWebAccessPolicy::Public
    );
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/callback")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/missing")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    assert_eq!(
        h.host
            .plugin_web_policy("missing-plugin", &Method::POST, "/callback")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );

    h.cleanup().await;
}

/// 参数段匹配：`/groups/:id` 必须命中具体群号，字面路径优先于同长度参数路径，
/// 方法不符与段数不符都落回默认关闭。
///
/// 这条测试盯的是一类会静默失效的声明：路由写了 `:group_id` 却按整串比较，
/// 宿主永远找不到它，插件返回的每个请求都是 404，而日志里只有「未找到」。
#[tokio::test]
async fn plugin_web_policy_matches_parameterized_routes() {
    use crate::plugin::{PluginWebAccessPolicy, PluginWebRoute};
    use http::Method;

    let h = Harness::new("web-params").await;
    h.host.web_routes.write().await.insert(
        h.plugin.clone(),
        vec![
            PluginWebRoute::new(
                Method::GET,
                "/groups/:group_id",
                PluginWebAccessPolicy::Public,
            ),
            PluginWebRoute::new(
                Method::GET,
                "/groups/:group_id/payments",
                PluginWebAccessPolicy::Authenticated,
            ),
            PluginWebRoute::new(
                Method::GET,
                "/groups/plans",
                PluginWebAccessPolicy::ManageRuntime,
            ),
            PluginWebRoute::new(
                Method::POST,
                "/groups/:group_id/payments",
                PluginWebAccessPolicy::Authenticated,
            ),
        ],
    );

    // 参数段命中具体值。
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/groups/123456")
            .await,
        PluginWebAccessPolicy::Public
    );
    // 字面路径优先：`/groups/plans` 不会被 `:group_id` 抢走。
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/groups/plans")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    // 更深一层归更深的那条声明，方法也要对上。
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/groups/123456/payments")
            .await,
        PluginWebAccessPolicy::Authenticated
    );
    // 段数不符、方法不符、参数段为空都落回默认关闭。
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/groups/123456/payments/x")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::DELETE, "/groups/123456")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    assert_eq!(
        h.host
            .plugin_web_policy(&h.plugin, &Method::GET, "/groups/")
            .await,
        PluginWebAccessPolicy::ManageRuntime
    );
    // 同一模式换个参数名是重复声明：加载阶段必须拒绝，否则匹配结果看声明顺序。
    assert!(
        validated_plugin_web_routes(
            "web",
            vec![
                PluginWebRoute::new(Method::GET, "/groups/:a", PluginWebAccessPolicy::Public),
                PluginWebRoute::new(
                    Method::GET,
                    "/groups/:b",
                    PluginWebAccessPolicy::Authenticated
                ),
            ],
        )
        .is_err()
    );

    h.cleanup().await;
}

#[tokio::test]
async fn disabled_plugin_web_handler_is_not_dispatched() {
    use crate::plugin::{PluginWebRequest, PluginWebResponse};
    use http::Method;

    let h = Harness::new("web-disabled").await;
    h.host.web_routes.write().await.insert(
        h.plugin.clone(),
        vec![crate::plugin::PluginWebRoute::new(
            Method::GET,
            "/dashboard",
            crate::plugin::PluginWebAccessPolicy::Authenticated,
        )],
    );
    h.host.set_plugin_registry_enabled(&h.plugin, false).await;
    let request = PluginWebRequest {
        method: Method::GET,
        path: "/dashboard".to_string(),
        query: String::new(),
        headers: http::HeaderMap::new(),
        body: vec![],
        user: None,
    };

    assert!(matches!(
        h.host.handle_web_request(&h.plugin, &request).await,
        PluginWebResponse::NotFound
    ));

    h.cleanup().await;
}

/// 仅用于测试的空触发器处理器（用于构造 Trigger 并断言注册归属）。
struct NoopHandler;

#[async_trait::async_trait]
impl TriggerHandler for NoopHandler {
    async fn handle(
        &self,
        _bot: &crate::api::Bot,
        _event: &Message,
        _data: MatchData,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct NoopNativeHandler;

#[async_trait::async_trait]
impl CommandHandler for NoopNativeHandler {
    async fn handle(
        &self,
        _bot: &crate::api::Bot,
        _event: &Event<'_>,
        _data: MatchData,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 记录自己被调用了几次的最小插件。
///
/// `on_event` 与 `on_message` 各自计数：两条路径的闸门是分开接的，共用一个计数器
/// 会让其中一条漏接也照样通过。
struct CountingPlugin {
    name: String,
    events: Arc<AtomicUsize>,
    messages: Arc<AtomicUsize>,
    /// 派发过滤断言用：默认 ALL 保持既有用例行为。
    filter: crate::plugin::PluginEventFilter,
    /// 声明的 secret 字段；默认空，schema 用例注入。
    secret_fields: Vec<crate::plugin::PluginSecretField>,
}

struct CommandTextPlugin {
    name: String,
    received: Arc<std::sync::Mutex<Vec<Value>>>,
}

#[async_trait::async_trait]
impl Plugin for CommandTextPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "记录宿主路由的原消息及命令正文"
    }
    async fn on_event(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        self.received.lock().unwrap().push(serde_json::json!({
            "text": event.text(),
            "command_text": event.command_text(),
            "prefix": event.command_prefix(),
            "raw": event.raw(),
        }));
        Ok(PluginResponse::Continue)
    }
}

#[tokio::test]
async fn on_event_obeys_plugin_namespace_and_preserves_raw_event() {
    let h = Harness::new("prefix-on-event").await;
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    h.host.plugins.write().await[0] = LoadedPlugin {
        plugin: Box::new(CommandTextPlugin {
            name: h.plugin.clone(),
            received: received.clone(),
        }),
    }
    .into();
    let mut config = Plugins::new(&h.plugin).with_prefix(vec!["url".into()]);
    config.allow_empty_prefix = true;
    h.host.register_plugin_config(config).await;
    let filter = PluginFilterContext {
        user_id: "u1".into(),
        group_id: None,
        msg_type: "private".into(),
        actor_permission: ActorPermission::everyone(),
    };
    let raw = serde_json::json!({
        "post_type":"message", "message_type":"private", "user_id":"u1",
        "raw_message":"  URL帮助  ", "message":[{"type":"image", "data":{"file":"opaque"}}],
        "raw":{"native":"preserved"}
    });
    h.host.process_message("", &raw, &filter).await;
    assert_eq!(
        received.lock().unwrap().as_slice(),
        &[serde_json::json!({
            "text": "  URL帮助  ",
            "command_text": "帮助",
            "prefix": "URL",
            "raw": raw.clone(),
        })]
    );
    let mut unprefixed = raw.clone();
    unprefixed["raw_message"] = serde_json::json!("帮助");
    h.host.process_message("", &unprefixed, &filter).await;
    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "非空前缀不能被 allow_empty_prefix 绕过"
    );

    let peer = Plugins::new("LongerNamespace").with_prefix(vec!["urlmusic".into()]);
    h.host.register_plugin_config(peer).await;
    let mut longer = raw.clone();
    longer["raw_message"] = serde_json::json!("urlmusic登录");
    h.host.process_message("", &longer, &filter).await;
    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "短前缀插件不能抢占长前缀"
    );

    let conflicting = Plugins::new("ConflictingNamespace").with_prefix(vec!["URL".into()]);
    h.host.register_plugin_config(conflicting).await;
    let outcome = h.host.process_message("", &raw, &filter).await;
    assert!(matches!(outcome.response, PluginResponse::Reply(_)));
    assert_eq!(received.lock().unwrap().len(), 1, "冲突时不执行任意插件");

    let mut notice = raw;
    notice["post_type"] = serde_json::json!("notice");
    h.host.process_message("", &notice, &filter).await;
    assert_eq!(received.lock().unwrap().len(), 2, "非消息事件不受前缀门控");
    h.cleanup().await;
}

#[tokio::test]
async fn explicit_peer_namespace_cannot_enter_unprefixed_callbacks_or_waits() {
    let h = Harness::new("prefix-peer").await;
    let mut peer = Plugins::new("ReservedNamespace").with_prefix(vec!["ww".into()]);
    peer.enabled = false;
    h.host.register_plugin_config(peer).await;
    let event = serde_json::json!({
        "post_type":"message", "message_type":"private", "user_id":"u1",
        "raw_message":"ww帮助"
    });
    let filter = PluginFilterContext {
        user_id: "u1".into(),
        group_id: None,
        msg_type: "private".into(),
        actor_permission: ActorPermission::everyone(),
    };
    h.host.process_message("", &event, &filter).await;
    h.host
        .dispatch_to_plugin(&h.plugin, &event, "session", "callback")
        .await;
    assert_eq!(h.events.load(Ordering::SeqCst), 0);
    assert_eq!(h.messages.load(Ordering::SeqCst), 0);
    let mut reply = event;
    reply["raw_message"] = serde_json::json!("普通回复");
    h.host
        .dispatch_to_plugin(&h.plugin, &reply, "session", "callback")
        .await;
    assert_eq!(
        h.messages.load(Ordering::SeqCst),
        1,
        "普通后续回复继续既有会话"
    );
    h.cleanup().await;
}

#[async_trait::async_trait]
impl Plugin for CountingPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "用于开关断言的测试插件"
    }
    fn event_filter(&self) -> crate::plugin::PluginEventFilter {
        self.filter
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        self.secret_fields.clone()
    }
    async fn on_event(&self, _event: &Event) -> anyhow::Result<PluginResponse> {
        self.events.fetch_add(1, Ordering::SeqCst);
        Ok(PluginResponse::Stop)
    }
    async fn handle_web_request(
        &self,
        _request: &crate::plugin::PluginWebRequest,
    ) -> crate::plugin::PluginWebResponse {
        crate::plugin::PluginWebResponse::Json(serde_json::json!({"ok": true}))
    }
    async fn on_message(
        &self,
        _event: &Event,
        _session_id: &str,
        _callback_id: &str,
    ) -> anyhow::Result<PluginResponse> {
        self.messages.fetch_add(1, Ordering::SeqCst);
        Ok(PluginResponse::Stop)
    }
}

struct Harness {
    host: Arc<PluginHost>,
    db: Database,
    root: std::path::PathBuf,
    events: Arc<AtomicUsize>,
    messages: Arc<AtomicUsize>,
    /// 逐用例唯一的群号前缀。
    tag: String,
    /// 逐用例唯一的插件名。
    plugin: String,
}

impl Harness {
    async fn new(name: &str) -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("nuono-host-gate-{}-{}", name, id));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let db = crate::test_support::test_db(2).await;

        let host = PluginHost::new(
            db.clone(),
            Arc::new(EventBus::new()),
            Arc::new(PermissionManager::new(db.clone())),
            crate::core::paths::Paths::with_root(&root),
            Arc::new(WaitManager::new()),
            FrameworkControl::new(),
            Arc::new(PluginConfigManager::with_database(
                root.join("plugin-config"),
                db.clone(),
            )),
        )
        .await
        .unwrap();

        let plugin = format!("GroupGated{}", &id[..8]);
        let events = Arc::new(AtomicUsize::new(0));
        let messages = Arc::new(AtomicUsize::new(0));
        host.plugins.write().await.push(
            LoadedPlugin {
                plugin: Box::new(CountingPlugin {
                    name: plugin.clone(),
                    events: events.clone(),
                    messages: messages.clone(),
                    filter: crate::plugin::PluginEventFilter::ALL,
                    secret_fields: vec![],
                }),
            }
            .into(),
        );
        db.sync_plugin(&plugin, "1.0.0", &serde_json::json!({}))
            .await
            .unwrap();
        // 注册表快照就是全局开关的来源，和真实启动路径一致。
        let mut cfg = Plugins::new(&plugin);
        cfg.enabled = true;
        cfg.area = PluginArea::All;
        host.config_manager
            .read()
            .await
            .as_ref()
            .unwrap()
            .register_with_service_config(&plugin, Default::default(), Some(&cfg))
            .await
            .unwrap();
        host.register_plugin_config(cfg).await;

        Self {
            host,
            db,
            root,
            events,
            messages,
            tag: format!("g{}", &id[..8]),
            plugin,
        }
    }

    fn group(&self, suffix: &str) -> String {
        format!("{}-{}", self.tag, suffix)
    }

    /// 跑一遍 path A，返回插件是否被调用。
    async fn ran_for(&self, group_id: Option<&str>) -> bool {
        let before = self.events.load(Ordering::SeqCst);
        let event = serde_json::json!({
            "post_type": "message",
            "message_type": if group_id.is_some() { "group" } else { "private" },
            "user_id": "u1",
            "group_id": group_id,
            "raw_message": "hi",
        });
        let ctx = PluginFilterContext {
            user_id: "u1".to_string(),
            group_id: group_id.map(str::to_string),
            msg_type: if group_id.is_some() {
                "group"
            } else {
                "private"
            }
            .to_string(),
            actor_permission: ActorPermission::everyone(),
        };
        self.host
            .test_group_snapshot(group_id, self.host.process_message("hi", &event, &ctx))
            .await;
        self.events.load(Ordering::SeqCst) > before
    }

    /// 客户在某个群里关掉（或打开）这个插件。`Scope::All` 跳过归属与可用性限制，
    /// 用例因此不必先建群。
    async fn set_group_enabled(&self, group_id: &str, enabled: bool) {
        self.host
            .test_group_patch(
                group_id,
                &self.plugin,
                Some(enabled),
                serde_json::Map::new(),
            )
            .await
            .unwrap();
    }

    async fn cleanup(self) {
        let Self { host, db, root, .. } = self;
        drop(host);
        let _ = db.shutdown().await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 注册一个带指定 secret 字段的插件及其 schema（模拟实例重建后的新实例）。
    async fn install_secrets(&self, fields: Vec<crate::plugin::PluginSecretField>) {
        let events = self.events.clone();
        let messages = self.messages.clone();
        let name = self.plugin.clone();
        let loaded = LoadedPlugin {
            plugin: Box::new(CountingPlugin {
                name,
                events,
                messages,
                filter: crate::plugin::PluginEventFilter::ALL,
                secret_fields: fields,
            }),
        };
        self.host
            .register_plugin_secret_schema(&loaded)
            .await
            .unwrap();
        self.host.plugins.write().await.push(loaded.into());
    }
}

/// 1. 群 X 关掉 → 不跑；群 Y 未设置 → 照跑。
#[tokio::test]
async fn group_override_blocks_only_that_group() {
    let h = Harness::new("scoped").await;
    let blocked = h.group("blocked");
    let other = h.group("other");
    h.set_group_enabled(&blocked, false).await;

    assert!(!h.ran_for(Some(&blocked)).await, "本群已关，插件不该运行");
    assert!(h.ran_for(Some(&other)).await, "别的群没关，必须照常运行");

    h.cleanup().await;
}

/// 3. 私聊没有群，跟随全局。
#[tokio::test]
async fn private_message_follows_global_switch() {
    let h = Harness::new("private").await;
    // 即使某个群关掉了，私聊也不受影响 —— 群级设置只作用于那个群。
    h.set_group_enabled(&h.group("x"), false).await;

    assert!(h.ran_for(None).await, "私聊无群号，应当跟随全局启用");

    h.cleanup().await;
}

/// 4. 非数字群号（飞书 `oc_`）按原字符串解析，不被折成数字或哈希。
#[tokio::test]
async fn non_numeric_group_id_is_resolved_verbatim() {
    let h = Harness::new("textid").await;
    let feishu = format!("oc_chat_{}", h.tag);
    let numeric = format!("100{}", 0);
    h.set_group_enabled(&feishu, false).await;

    assert!(!h.ran_for(Some(&feishu)).await, "飞书群号必须能命中覆盖");
    assert!(
        h.ran_for(Some(&numeric)).await,
        "数字群不该被非数字群的设置污染（键被哈希折叠时会一起挂）"
    );

    h.cleanup().await;
}

/// 5. 全局停用 + 群级打开 → 仍然不跑。
#[tokio::test]
async fn global_disable_beats_group_enable() {
    let h = Harness::new("global").await;
    let group = h.group("eager");
    h.set_group_enabled(&group, true).await;

    crate::service::plugins::set_plugin_enabled(&h.db, &h.plugin, false)
        .await
        .unwrap();
    // 注册表快照也要跟着改，真实路径由 `toggle` 负责。
    let mut cfg = Plugins::new(&h.plugin);
    cfg.enabled = false;
    cfg.area = PluginArea::All;
    h.host.register_plugin_config(cfg).await;

    assert!(
        !h.ran_for(Some(&group)).await,
        "员工的全局停用必须压住群级开启"
    );

    h.cleanup().await;
}

/// 6. 解析出错 → 失败即拒绝，且不打断派发（`process_message` 正常返回 Continue）。
#[tokio::test]
async fn resolution_error_fails_closed_without_breaking_dispatch() {
    let h = Harness::new("dberr").await;
    let group = h.group("broken");
    // 测试桩：通过显式 DDL 删除 `plugins` 表，模拟后续查询该表失败的场景。
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "ALTER TABLE nuono_plugins RENAME COLUMN enabled TO unavailable_enabled",
        ))
        .await
        .unwrap();

    let event = serde_json::json!({
        "post_type": "message",
        "message_type": "group",
        "user_id": "u1",
        "group_id": group,
    });
    let ctx = PluginFilterContext {
        user_id: "u1".to_string(),
        group_id: Some(group.clone()),
        msg_type: "group".to_string(),
        actor_permission: ActorPermission::everyone(),
    };
    let result = h.host.process_message("hi", &event, &ctx).await;

    assert_eq!(h.events.load(Ordering::SeqCst), 0, "解析失败时插件不该运行");
    assert!(
        matches!(result.response, PluginResponse::Continue),
        "失败只影响这个插件，派发本身必须继续"
    );

    h.cleanup().await;
}

/// 7. `dispatch_to_plugin`（WaitReply 回调）同样受开关约束。
///
/// 修复前这条路径完全没有检查：等待窗口里插件被关掉，回调照样进 `on_message`。
#[tokio::test]
async fn wait_reply_callback_respects_enablement() {
    let h = Harness::new("waitreply").await;
    let blocked = h.group("blocked");
    let other = h.group("other");
    h.set_group_enabled(&blocked, false).await;

    let callback = |group: &str| {
        serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "user_id": "u1",
            "group_id": group,
        })
    };

    h.host
        .test_group_snapshot(
            Some(&blocked),
            h.host
                .dispatch_to_plugin(&h.plugin, &callback(&blocked), "s1", "c1"),
        )
        .await;
    assert_eq!(
        h.messages.load(Ordering::SeqCst),
        0,
        "本群已关，待回复回调不该触发"
    );

    h.host
        .test_group_snapshot(
            Some(&other),
            h.host
                .dispatch_to_plugin(&h.plugin, &callback(&other), "s2", "c2"),
        )
        .await;
    assert_eq!(
        h.messages.load(Ordering::SeqCst),
        1,
        "未关闭的群回调必须照常触发"
    );

    // 全局停用同样拦住回调。
    let mut cfg = Plugins::new(&h.plugin);
    cfg.enabled = false;
    h.host.register_plugin_config(cfg).await;
    h.host
        .test_group_snapshot(
            Some(&other),
            h.host
                .dispatch_to_plugin(&h.plugin, &callback(&other), "s3", "c3"),
        )
        .await;
    assert_eq!(
        h.messages.load(Ordering::SeqCst),
        1,
        "全局停用后回调也必须丢弃"
    );

    h.cleanup().await;
}

/// 插件声明的事件类别在宿主侧过滤：声明 MESSAGES 的插件收不到 notice 事件。
#[tokio::test]
async fn event_filter_blocks_undeclared_post_types() {
    let h = Harness::new("filter").await;
    let group = h.group("any");
    // 把已注册插件换成 MESSAGES 过滤（Harness 只装 ALL，这里直接替换注册表）。
    let mut cfg = Plugins::new(&h.plugin);
    cfg.enabled = true;
    cfg.area = PluginArea::All;
    h.host.register_plugin_config(cfg).await;
    {
        let mut plugins = h.host.plugins.write().await;
        if let Some(loaded) = plugins.iter_mut().find(|p| p.name() == h.plugin) {
            // 直接改不了 Box<dyn Plugin> 的字段，重装一个过滤版实例。
            let events = h.events.clone();
            let messages = h.messages.clone();
            let name = h.plugin.clone();
            loaded.plugin.write().await.plugin = Box::new(CountingPlugin {
                name,
                events,
                messages,
                filter: crate::plugin::PluginEventFilter::MESSAGES,
                secret_fields: vec![],
            });
        }
    }

    let notice = serde_json::json!({
        "post_type": "notice",
        "notice_type": "notify",
        "sub_type": "poke",
        "message_type": "group",
        "user_id": "u1",
        "group_id": group.clone(),
        "raw_message": "",
    });
    let ctx = PluginFilterContext {
        user_id: "u1".to_string(),
        group_id: Some(group.clone()),
        msg_type: "group".to_string(),
        actor_permission: ActorPermission::everyone(),
    };
    h.host
        .test_group_snapshot(Some(&group), h.host.process_message("hi", &notice, &ctx))
        .await;
    assert_eq!(
        h.events.load(Ordering::SeqCst),
        0,
        "声明 MESSAGES 的插件不应收到 notice 事件"
    );

    assert!(h.ran_for(Some(&group)).await, "message 事件不受过滤影响");

    h.cleanup().await;
}

/// 权威配置读取失败时必须拒绝派发，绝不能复用此前其他群组的放行结果。
#[tokio::test]
async fn unavailable_configuration_authority_denies_dispatch() {
    let h = Harness::new("unavailable-config").await;
    let group = h.group("warm");
    assert!(h.ran_for(Some(&group)).await);
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "DROP TABLE nuono_plugin_overrides",
        ))
        .await
        .unwrap();
    assert!(
        !h.ran_for(Some(&group)).await,
        "authority failure must not reuse a previous allow decision"
    );
    h.cleanup().await;
}

/// 插件 secret schema：加载即注册、禁用不清空、reload 随当前插件更新。
#[tokio::test]
async fn plugin_secret_schema_survives_disable_and_reload_updates_it() {
    let h = Harness::new("secrets").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "captcha_api_key",
        "验证码 API Key",
        "用于验证码服务",
    )])
    .await;

    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 1);
    assert_eq!(
        schemas[0].key,
        format!("plugin.{}.captcha_api_key", h.plugin)
    );
    assert_eq!(schemas[0].title, "验证码 API Key");
    assert!(!schemas[0].required, "默认非必填");

    // 禁用插件不该让已保存 secret 消失，schema 也仍应可列出。
    h.host.toggle(&h.plugin, false).await.unwrap();
    assert_eq!(h.host.plugin_secret_schemas().await.len(), 1);

    // 实例重建语义：schema 随当前插件更新，新增必填字段即刻可见。
    h.install_secrets(vec![
        crate::plugin::PluginSecretField::new(
            "captcha_api_key",
            "验证码 API Key",
            "用于验证码服务",
        )
        .required(),
        crate::plugin::PluginSecretField::new("game_ck", "游戏 CK", "玩家 ck"),
    ])
    .await;
    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 2);
    // 实例重建后：原字段从非必填更新为必填。
    let captcha = schemas
        .iter()
        .find(|s| s.key.ends_with("captcha_api_key"))
        .expect("reload 后仍应有原字段");
    assert!(captcha.required, "重载后原字段必填标记应更新");
    // 新增的字段也即刻可见。
    assert!(
        schemas.iter().any(|s| s.key.ends_with("game_ck")),
        "reload 后应出现新字段"
    );

    h.cleanup().await;
}

/// 空字段、非法字符或重复字段必须在加载阶段拒绝，绝不静默改名造成碰撞。
#[tokio::test]
async fn plugin_secret_schema_rejects_duplicate_and_invalid_fields() {
    let h = Harness::new("dupsecrets").await;
    let events = h.events.clone();
    let messages = h.messages.clone();
    let name = h.plugin.clone();
    let make = |fields: Vec<crate::plugin::PluginSecretField>| LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: name.clone(),
            events: events.clone(),
            messages: messages.clone(),
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields: fields,
        }),
    };

    // 空字段 key
    let empty = make(vec![crate::plugin::PluginSecretField::new("", "t", "d")]);
    assert!(h.host.register_plugin_secret_schema(&empty).await.is_err());
    // 非法字符（含 `.`，会破坏 canonical key 的无歧义性）
    let unsafe_char = make(vec![crate::plugin::PluginSecretField::new(
        "bad:key", "t", "d",
    )]);
    assert!(
        h.host
            .register_plugin_secret_schema(&unsafe_char)
            .await
            .is_err()
    );
    // 重复字段
    let dup = make(vec![
        crate::plugin::PluginSecretField::new("dup", "t", "d"),
        crate::plugin::PluginSecretField::new("dup", "t2", "d2"),
    ]);
    assert!(h.host.register_plugin_secret_schema(&dup).await.is_err());

    h.cleanup().await;
}

// ── 回归：secret_fields 只取一次，context/store 与 registry 同一 snapshot ──

struct StatefulSecretPlugin {
    name: String,
    fetch_count: Arc<AtomicUsize>,
    fields: Vec<crate::plugin::PluginSecretField>,
}

#[async_trait::async_trait]
impl Plugin for StatefulSecretPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "有状态 secret_fields 的测试插件"
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        self.fetch_count.fetch_add(1, Ordering::SeqCst);
        self.fields.clone()
    }
}

#[tokio::test]
async fn stateful_secret_fields_fetched_once_and_shared_with_store() {
    let h = Harness::new("statefulsecret").await;
    let fetch_count = Arc::new(AtomicUsize::new(0));
    let fields = vec![crate::plugin::PluginSecretField::new("ck", "CK", "desc")];
    let loaded = LoadedPlugin {
        plugin: Box::new(StatefulSecretPlugin {
            name: h.plugin.clone(),
            fetch_count: fetch_count.clone(),
            fields,
        }),
    };
    h.host.register_plugin_secret_schema(&loaded).await.unwrap();
    // 构造 context 必须复用已验证 snapshot，不得再一次调用 secret_fields。
    let ctx = h
        .host
        .plugin_context(
            &h.plugin,
            serde_json::json!({}),
            PluginActivationReason::Enable,
        )
        .await;
    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, format!("plugin.{}.ck", h.plugin));
    let keys: Vec<&str> = ctx.secrets().field_keys().collect();
    assert_eq!(keys, vec!["ck"]);
    assert_eq!(
        fetch_count.load(Ordering::SeqCst),
        1,
        "secret_fields 应只被取一次（registry/store/context 共享同一 snapshot）"
    );
    h.cleanup().await;
}

/// 无 secret 字段的旧式名称（含空格 / 点 / Unicode）仍允许加载——不生成 canonical key；
/// 同名插件一旦声明 secret，插件名即成为 canonical key 组件，必须安全，否则拒绝。
#[tokio::test]
async fn plugins_without_secret_fields_accept_legacy_names() {
    let h = Harness::new("legacyname").await;
    let events = h.events.clone();
    let messages = h.messages.clone();
    let make = |plugin_name: String, fields: Vec<crate::plugin::PluginSecretField>| LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: plugin_name,
            events: events.clone(),
            messages: messages.clone(),
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields: fields,
        }),
    };
    // 无 secret 的旧式名称：允许加载。
    let legacy = make("My Plugin.你好".to_string(), vec![]);
    h.host.register_plugin_secret_schema(&legacy).await.unwrap();
    // 同名插件一旦声明 secret → 拒绝（插件名必须满足安全字符规则）。
    let with_secret = make(
        "My Plugin.你好".to_string(),
        vec![crate::plugin::PluginSecretField::new("ck", "CK", "desc")],
    );
    assert!(
        h.host
            .register_plugin_secret_schema(&with_secret)
            .await
            .is_err(),
        "声明 secret 的插件名必须安全"
    );
    h.cleanup().await;
}

// ── 回归：原子替换清除旧 schema，卸载/重载不残留 stale schema ──

#[tokio::test]
async fn reload_swap_clears_old_schema_and_registers_new() {
    let h = Harness::new("swap").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;
    let old_key = format!("plugin.{}.ck", h.plugin);

    let events = h.events.clone();
    let messages = h.messages.clone();
    let new_loaded = LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: h.plugin.clone(),
            events,
            messages,
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields: vec![crate::plugin::PluginSecretField::new(
                "token", "Token", "desc",
            )],
        }),
    };
    let new_schema = vec![crate::plugin::PluginSecretField::new(
        "token", "Token", "desc",
    )];
    let old = h
        .host
        .swap_plugin_instances(&h.plugin, new_loaded, new_schema)
        .await;
    assert!(!old.is_empty(), "旧实例应被移除");
    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, format!("plugin.{}.token", h.plugin));
    // 旧 key 不再授权（stale schema 已清除）；DB value 本身在卸载后保留。
    assert!(
        h.host.set_plugin_secret(&old_key, "x").await.is_err(),
        "旧 key 不应再被授权"
    );
    h.cleanup().await;
}

/// reload 无可用新实例（插件目录为空 / 校验失败）时必须返回错误，且旧插件与旧 schema
/// 保持原样、旧 key 仍授权 —— 无效新实例不能污染集合 / registry。
#[tokio::test]
async fn reload_with_no_candidate_returns_error_and_keeps_old_state() {
    let h = Harness::new("reloadfail").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;
    let old_key = format!("plugin.{}.ck", h.plugin);
    assert!(
        h.host.reload_plugin(&h.plugin).await.is_err(),
        "无候选实例时 reload 应报错"
    );
    assert_eq!(
        h.host.plugin_secret_schemas().await.len(),
        1,
        "旧 schema 应保留"
    );
    h.host.set_plugin_secret(&old_key, "v").await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn shutdown_clears_secret_schemas() {
    let h = Harness::new("shutsecrets").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;
    assert_eq!(h.host.plugin_secret_schemas().await.len(), 1);
    let _ = h.host.shutdown().await;
    assert!(
        h.host.plugin_secret_schemas().await.is_empty(),
        "shutdown 后 schema 应清空"
    );
    h.cleanup().await;
}

// ── 回归：插件 secret 授权 + DB 收敛到 host 原子方法；卸载后拒绝 ──

#[tokio::test]
async fn host_plugin_secret_set_delete_authorized_and_rejected_after_unload() {
    let h = Harness::new("authz").await;
    let key = format!("plugin.{}.ck", h.plugin);
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;

    // 授权读写删
    h.host.set_plugin_secret(&key, "v1").await.unwrap();
    assert_eq!(h.db.get_secret(&key).await.unwrap().as_deref(), Some("v1"));
    h.host.delete_plugin_secret(&key).await.unwrap();
    assert_eq!(h.db.get_secret(&key).await.unwrap(), None);

    // 凭空 / 未声明 plugin key 必须被授权层拒绝
    assert!(
        h.host
            .set_plugin_secret("plugin.evil.backdoor", "x")
            .await
            .is_err()
    );
    assert!(
        h.host
            .delete_plugin_secret("plugin.evil.backdoor")
            .await
            .is_err()
    );

    // 卸载（shutdown 移除实例并清 schema）后原 key 不再授权，不能借机写入。
    let _ = h.host.shutdown().await;
    assert!(
        h.host
            .set_plugin_secret(&key, "after-unload")
            .await
            .is_err(),
        "卸载后不得创建未授权 key"
    );
    assert!(h.host.delete_plugin_secret(&key).await.is_err());
    h.cleanup().await;
}

// ── 回归：on_unload 失败也必须先清 schema（cleanup 不因回调错误跳过）──

struct FailingUnloadPlugin {
    name: String,
    fields: Vec<crate::plugin::PluginSecretField>,
}

#[async_trait::async_trait]
impl Plugin for FailingUnloadPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "on_unload 失败的测试插件"
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        self.fields.clone()
    }
    async fn on_unload(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("unload boom");
    }
}

#[tokio::test]
async fn unload_failure_still_clears_schema() {
    let h = Harness::new("unloadfail").await;
    let loaded = LoadedPlugin {
        plugin: Box::new(FailingUnloadPlugin {
            name: h.plugin.clone(),
            fields: vec![crate::plugin::PluginSecretField::new("ck", "CK", "desc")],
        }),
    };
    h.host.register_plugin_secret_schema(&loaded).await.unwrap();
    h.host.plugins.write().await.push(loaded.into());
    assert_eq!(h.host.plugin_secret_schemas().await.len(), 1);
    let _ = h.host.shutdown().await;
    assert!(
        h.host.plugin_secret_schemas().await.is_empty(),
        "on_unload 失败也必须清 schema"
    );
    h.cleanup().await;
}

// ── 回归：初始加载强制 Plugin::name 全局唯一 ──

#[tokio::test]
async fn init_dedup_rejects_duplicate_initial_instances() {
    let h = Harness::new("initdup").await;
    let events = h.events.clone();
    let messages = h.messages.clone();
    let name = "DupPlugin".to_string();
    h.db.sync_plugin(&name, "previous", &serde_json::json!({}))
        .await
        .unwrap();
    let make = |secret_fields| LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: name.clone(),
            events: events.clone(),
            messages: messages.clone(),
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields,
        }),
    };
    let plan = h
        .host
        .resolve_load_candidates(vec![
            make(vec![
                crate::plugin::PluginSecretField::new("ck", "CK", "invalid"),
                crate::plugin::PluginSecretField::new("ck", "CK2", "duplicate"),
            ]),
            make(vec![crate::plugin::PluginSecretField::new(
                "ck", "CK", "desc",
            )]),
            make(vec![crate::plugin::PluginSecretField::new(
                "ck", "CK", "desc",
            )]),
        ])
        .await
        .unwrap();
    assert_eq!(plan.candidates.len(), 1, "同名重复初始实例应只保留一个");
    assert_eq!(plan.rejected.len(), 1, "记录首个非法候选，忽略重复有效候选");
    assert_eq!(plan.candidates[0].secret_fields.len(), 1);
    assert_eq!(
        h.host.plugin_secret_schemas().await.len(),
        0,
        "筛选候选不应提前发布 schema"
    );
    assert!(h.db.is_plugin_enabled(&name).await.unwrap());
    assert_eq!(h.host.install_load_plan(plan).await.unwrap(), 1);
    assert!(
        h.db.is_plugin_enabled(&name).await.unwrap(),
        "非法同名候选不能禁用本批最终选中的有效候选"
    );
    assert!(h.host.get_plugin_config(&name).await.unwrap().enabled);
    assert_eq!(
        h.host
            .all_plugins()
            .await
            .iter()
            .filter(|plugin| *plugin == &name)
            .count(),
        1
    );
    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, format!("plugin.{name}.ck"));
    h.cleanup().await;
}

struct DeclarationCandidatePlugin {
    name: String,
    panic_name: bool,
    panic_config_fields: bool,
    panic_commands: bool,
}

impl DeclarationCandidatePlugin {
    fn new(name: String) -> Self {
        Self {
            name,
            panic_name: false,
            panic_config_fields: false,
            panic_commands: false,
        }
    }
}

#[async_trait::async_trait]
impl Plugin for DeclarationCandidatePlugin {
    fn name(&self) -> &str {
        assert!(!self.panic_name, "name panic");
        &self.name
    }

    fn version(&self) -> &str {
        "2.0.0"
    }

    fn description(&self) -> &str {
        "候选声明预检测试插件"
    }

    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        vec![crate::plugin::PluginSecretField::new(
            "token",
            "Token",
            "new candidate secret",
        )]
    }

    fn config_fields(&self) -> Vec<crate::core::PluginConfigField> {
        assert!(!self.panic_config_fields, "config_fields panic");
        vec![crate::core::PluginConfigField::boolean(
            "feature_enabled",
            "Feature",
            "candidate declaration",
            true,
        )]
    }

    fn commands(&self) -> Vec<Command> {
        assert!(!self.panic_commands, "commands panic");
        vec![]
    }
}

#[tokio::test]
async fn resolve_load_candidates_has_no_host_or_database_side_effects() {
    let h = Harness::new("resolve-pure").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck",
        "CK",
        "old secret",
    )])
    .await;
    let valid_name = format!("Valid{}", h.plugin);
    let invalid_name = format!("Invalid{}", h.plugin);
    h.db.sync_plugin(&invalid_name, "previous", &serde_json::json!({}))
        .await
        .unwrap();
    let before_plugins = h.host.all_plugins().await;
    let before_config_count = h.host.config_declarations.read().await.len();
    let before_revision_count = h.host.config_revisions.read().await.len();
    let before_registry_count = h.host.plugin_registry.read().await.len();
    let old_secret_key = format!("plugin.{}.ck", h.plugin);

    let plan = h
        .host
        .resolve_load_candidates(vec![
            LoadedPlugin {
                plugin: Box::new(DeclarationCandidatePlugin::new(valid_name.clone())),
            },
            LoadedPlugin {
                plugin: Box::new(CountingPlugin {
                    name: invalid_name.clone(),
                    events: h.events.clone(),
                    messages: h.messages.clone(),
                    filter: crate::plugin::PluginEventFilter::ALL,
                    secret_fields: vec![
                        crate::plugin::PluginSecretField::new("ck", "CK", "d"),
                        crate::plugin::PluginSecretField::new("ck", "CK2", "d2"),
                    ],
                }),
            },
        ])
        .await
        .unwrap();
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].declaration.name, valid_name);
    assert_eq!(plan.rejected.len(), 1);
    assert!(
        h.db.is_plugin_enabled(&invalid_name).await.unwrap(),
        "筛选拒绝候选不能提前写 DB disabled"
    );
    let db_plugins = crate::service::plugins::get_plugins(&h.db).await.unwrap();
    assert!(db_plugins.iter().all(|p| p.name != valid_name));
    assert!(db_plugins.iter().any(|plugin| plugin.name == invalid_name));
    assert_eq!(
        h.db.plugin_policy(&invalid_name).await.unwrap().revision,
        1,
        "筛选不能改变已保存设置"
    );

    let result = h
        .host
        .resolve_load_candidates(vec![
            LoadedPlugin {
                plugin: Box::new(DeclarationCandidatePlugin::new(valid_name.clone())),
            },
            LoadedPlugin {
                plugin: Box::new(DeclarationCandidatePlugin {
                    panic_name: true,
                    ..DeclarationCandidatePlugin::new("unknown".into())
                }),
            },
        ])
        .await;
    assert!(result.is_err(), "name panic 应转成 Err，不能传播 panic");
    assert_eq!(h.host.all_plugins().await, before_plugins);
    assert_eq!(
        h.host.config_declarations.read().await.len(),
        before_config_count
    );
    assert_eq!(
        h.host.config_revisions.read().await.len(),
        before_revision_count
    );
    assert_eq!(
        h.host.plugin_registry.read().await.len(),
        before_registry_count
    );
    let schemas = h.host.plugin_secret_schemas().await;
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].key, old_secret_key);
    assert!(h.db.is_plugin_enabled(&invalid_name).await.unwrap());
    assert!(
        crate::service::plugins::get_plugins(&h.db)
            .await
            .unwrap()
            .iter()
            .all(|plugin| plugin.name != valid_name)
    );
    h.cleanup().await;
}

#[tokio::test]
async fn init_installs_only_new_batch_and_does_not_publish_on_database_failure() {
    let h = Harness::new("install-batch").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck",
        "CK",
        "old secret",
    )])
    .await;
    let old_count = h
        .host
        .all_plugins()
        .await
        .iter()
        .filter(|name| *name == &h.plugin)
        .count();
    let new_name = format!("New{}", h.plugin);
    h.host
        .init_with_plugins(vec![LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin::new(new_name.clone())),
        }])
        .await
        .unwrap();
    let installed = h.host.all_plugins().await;
    assert_eq!(
        installed.iter().filter(|name| *name == &h.plugin).count(),
        old_count,
        "本批安装不能移除既有实例"
    );
    assert_eq!(installed.last(), Some(&new_name), "保留加载顺序");
    assert!(h.host.get_plugin_config(&h.plugin).await.unwrap().enabled);
    let mut installed_secret_keys: Vec<String> = h
        .host
        .plugin_secret_schemas()
        .await
        .into_iter()
        .map(|field| field.key)
        .collect();
    installed_secret_keys.sort();
    let mut expected_keys = vec![
        format!("plugin.{}.ck", h.plugin),
        format!("plugin.{new_name}.token"),
    ];
    expected_keys.sort();
    assert_eq!(installed_secret_keys, expected_keys);

    // 只破坏本用例的数据库，以便在候选已经合法解析后使批量同步失败。
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "ALTER TABLE nuono_plugins RENAME COLUMN enabled TO unavailable_enabled",
        ))
        .await
        .unwrap();
    let failed_name = format!("Failed{}", h.plugin);
    assert!(
        h.host
            .init_with_plugins(vec![LoadedPlugin {
                plugin: Box::new(DeclarationCandidatePlugin::new(failed_name.clone())),
            }])
            .await
            .is_err()
    );
    assert_eq!(h.host.all_plugins().await, installed);
    let mut remaining_secret_keys: Vec<String> = h
        .host
        .plugin_secret_schemas()
        .await
        .into_iter()
        .map(|field| field.key)
        .collect();
    remaining_secret_keys.sort();
    assert_eq!(remaining_secret_keys, installed_secret_keys);
    assert!(h.host.get_plugin_config(&failed_name).await.is_none());
    assert!(
        !h.host
            .config_declarations
            .read()
            .await
            .contains_key(&failed_name),
        "DB 同步失败不能提前发布配置声明"
    );
    h.cleanup().await;
}

// ── 回归：reload 候选必须恰好一个 ──

#[test]
fn reload_requires_exactly_one_candidate() {
    let make = |n: &str| LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: n.to_string(),
            events: Arc::new(AtomicUsize::new(0)),
            messages: Arc::new(AtomicUsize::new(0)),
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields: vec![],
        }),
    };
    assert!(
        single_reload_candidate(vec![], "x").is_err(),
        "0 个候选应拒绝"
    );
    assert!(
        single_reload_candidate(vec![make("x")], "x").is_ok(),
        "1 个候选应允许"
    );
    assert!(
        single_reload_candidate(vec![make("x"), make("x")], "x").is_err(),
        ">1 个同名候选应拒绝"
    );
}

// ── 回归：toggle(enabled=true) 激活失败必须 fail-closed ──

struct ActivationFailPlugin {
    name: String,
    fail_on_load: bool,
    panic_on_load: bool,
    fail_on_enable: bool,
    panic_on_enable: bool,
    secret_fields: Vec<crate::plugin::PluginSecretField>,
}

#[async_trait::async_trait]
impl Plugin for ActivationFailPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "激活失败测试插件"
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        self.secret_fields.clone()
    }
    async fn on_load(&mut self, _ctx: &PluginContext) -> anyhow::Result<()> {
        if self.fail_on_load {
            anyhow::bail!("on_load 失败");
        }
        if self.panic_on_load {
            panic!("on_load panic");
        }
        Ok(())
    }
    async fn on_enable(&mut self) -> anyhow::Result<()> {
        if self.fail_on_enable {
            anyhow::bail!("on_enable 失败");
        }
        if self.panic_on_enable {
            panic!("on_enable panic");
        }
        Ok(())
    }
}

async fn host_for_activation_test() -> (Arc<PluginHost>, Database, String) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let root = std::env::temp_dir().join(format!("nuono-host-activ-{id}"));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = crate::test_support::test_db(2).await;
    let host = PluginHost::new(
        db.clone(),
        Arc::new(EventBus::new()),
        Arc::new(PermissionManager::new(db.clone())),
        crate::core::paths::Paths::with_root(&root),
        Arc::new(WaitManager::new()),
        FrameworkControl::new(),
        Arc::new(PluginConfigManager::with_database(
            root.join("plugin-config"),
            db.clone(),
        )),
    )
    .await
    .unwrap();
    let name = format!("ActivationFail{}", &id[..8]);
    (host, db, name)
}

struct CountingCommandsPlugin {
    name: String,
    declarations: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Plugin for CountingCommandsPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "command 声明快照测试插件"
    }

    fn commands(&self) -> Vec<Command> {
        let call = self.declarations.fetch_add(1, Ordering::SeqCst) + 1;
        vec![Command::new(
            format!("native-{call}"),
            Arc::new(NoopNativeHandler),
        )]
    }
}

#[tokio::test]
async fn enable_registers_commands_from_single_declaration_snapshot_and_cleans_them() {
    let (host, db, name) = host_for_activation_test().await;
    let declarations = Arc::new(AtomicUsize::new(0));
    host.plugins.write().await.push(
        LoadedPlugin {
            plugin: Box::new(CountingCommandsPlugin {
                name: name.clone(),
                declarations: declarations.clone(),
            }),
        }
        .into(),
    );
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();

    host.toggle(&name, true).await.unwrap();

    assert_eq!(
        declarations.load(Ordering::SeqCst),
        1,
        "一次激活只能收集一次 commands 声明"
    );
    let native_triggers = host
        .trigger_registry()
        .get_triggers(TriggerType::EventCommand);
    assert_eq!(native_triggers.len(), 1);
    assert_eq!(native_triggers[0].pattern, "native-1");
    assert_eq!(
        native_triggers[0].plugin_name.as_deref(),
        Some(name.as_str())
    );
    let commands = host.trigger_registry().get_all_commands();
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.plugin_name.as_deref() == Some(name.as_str()))
            .count(),
        1
    );
    assert!(commands.iter().any(|command| {
        command.name == "native-1"
            && command.plugin_name.as_deref() == Some(name.as_str())
            && command.trigger_type == TriggerType::EventCommand
    }));

    host.toggle(&name, false).await.unwrap();

    assert!(
        host.trigger_registry()
            .get_triggers(TriggerType::EventCommand)
            .iter()
            .all(|trigger| trigger.plugin_name.as_deref() != Some(name.as_str()))
    );
    assert!(
        host.trigger_registry()
            .get_all_commands()
            .iter()
            .all(|command| command.plugin_name.as_deref() != Some(name.as_str()))
    );

    host.toggle(&name, true).await.unwrap();
    assert_eq!(
        declarations.load(Ordering::SeqCst),
        2,
        "重新激活应再收集一次，但注册阶段不能额外调用"
    );
    let native_triggers = host
        .trigger_registry()
        .get_triggers(TriggerType::EventCommand);
    assert_eq!(native_triggers.len(), 1);
    assert_eq!(native_triggers[0].pattern, "native-2");
    assert_eq!(
        native_triggers[0].plugin_name.as_deref(),
        Some(name.as_str())
    );

    host.toggle(&name, false).await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn toggle_enable_activation_failure_is_fail_closed() {
    let cases = [
        (true, false, false, false, "on_load error"),
        (false, true, false, false, "on_load panic"),
        (false, false, true, false, "on_enable error"),
        (false, false, false, true, "on_enable panic"),
    ];
    for (fail_on_load, panic_on_load, fail_on_enable, panic_on_enable, label) in cases {
        let (host, db, name) = host_for_activation_test().await;
        let key = format!("plugin.{name}.ck");
        let plugin = ActivationFailPlugin {
            name: name.clone(),
            fail_on_load,
            panic_on_load,
            fail_on_enable,
            panic_on_enable,
            secret_fields: vec![crate::plugin::PluginSecretField::new("ck", "CK", "desc")],
        };
        let loaded = LoadedPlugin {
            plugin: Box::new(plugin),
        };
        host.register_plugin_secret_schema(&loaded).await.unwrap();
        host.plugins.write().await.push(loaded.into());
        host.db
            .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
            .await
            .unwrap();

        let result = host.toggle(&name, true).await;
        assert!(result.is_err(), "[{label}] 激活失败必须报错");
        assert!(
            host.all_plugins().await.iter().all(|n| n != &name),
            "[{label}] 失败实例不应留在集合"
        );
        assert!(
            host.plugin_secret_schemas().await.is_empty(),
            "[{label}] secret schema 应清空"
        );
        assert!(
            host.get_plugin_config(&name).await.is_none(),
            "[{label}] registry 不应再有该插件"
        );
        assert!(
            host.set_plugin_secret(&key, "x").await.is_err(),
            "[{label}] secret key 不再授权"
        );
        assert!(
            !db.is_plugin_enabled(&name).await.unwrap(),
            "[{label}] DB enabled 应为 false"
        );

        drop(host);
        db.shutdown().await.unwrap();
    }
}

/// 带可配置 panic 的插件：`triggers()` 是否 panic，用于注册阶段 panic 测试。
struct TriggerPlugin {
    name: String,
    panic_on_triggers: bool,
    secret_fields: Vec<crate::plugin::PluginSecretField>,
    trigger_pattern: String,
}

#[async_trait::async_trait]
impl Plugin for TriggerPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "trigger 注册测试插件"
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        self.secret_fields.clone()
    }
    fn triggers(&self) -> Vec<Trigger> {
        if self.panic_on_triggers {
            panic!("triggers panic");
        }
        vec![Trigger::new(
            TriggerType::Keyword,
            &self.trigger_pattern,
            "/",
            false,
            false,
            Arc::new(NoopHandler),
        )]
    }
}

/// 注册阶段（`triggers()`）panic 也必须走 fail-closed 清理，且 registration scope 在
/// panic 后由 RAII 清空，后续插件的 trigger 不污染。
#[tokio::test]
async fn activation_panic_in_registration_is_fail_closed_and_scope_clean() {
    let (host, db, name_a) = host_for_activation_test().await;
    let key_a = format!("plugin.{name_a}.ck");
    // A：triggers() 里 panic。
    let loaded_a = LoadedPlugin {
        plugin: Box::new(TriggerPlugin {
            name: name_a.clone(),
            panic_on_triggers: true,
            secret_fields: vec![crate::plugin::PluginSecretField::new("ck", "CK", "desc")],
            trigger_pattern: "a".into(),
        }),
    };
    host.register_plugin_secret_schema(&loaded_a).await.unwrap();
    host.plugins.write().await.push(loaded_a.into());
    host.db
        .sync_plugin(&name_a, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();

    assert!(
        host.toggle(&name_a, true).await.is_err(),
        "triggers 阶段 panic 激活必须失败"
    );
    assert!(
        host.all_plugins().await.iter().all(|n| n != &name_a),
        "失败实例不应留在集合"
    );
    assert!(
        host.set_plugin_secret(&key_a, "x").await.is_err(),
        "secret 不再授权"
    );
    assert!(
        !db.is_plugin_enabled(&name_a).await.unwrap(),
        "DB enabled 应为 false"
    );

    // B：正常注册一个 trigger，归属必须是 B 自己（A 的 registration scope 未泄漏）。
    let bid = uuid::Uuid::new_v4().simple().to_string();
    let name_b = format!("TriggerB{}", &bid[..8]);
    let loaded_b = LoadedPlugin {
        plugin: Box::new(TriggerPlugin {
            name: name_b.clone(),
            panic_on_triggers: false,
            secret_fields: vec![],
            trigger_pattern: "hello".into(),
        }),
    };
    host.plugins.write().await.push(loaded_b.into());
    host.db
        .sync_plugin(&name_b, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    assert!(host.toggle(&name_b, true).await.is_ok(), "B 应成功启用");

    let triggers = host.trigger_registry().get_all_triggers();
    assert!(
        triggers
            .iter()
            .any(|t| t.plugin_name.as_deref() == Some(name_b.as_str())),
        "B 的 trigger 应归属 B（A 的 scope 未泄漏）"
    );
    assert!(
        triggers
            .iter()
            .all(|t| t.plugin_name.as_deref() != Some(name_a.as_str())),
        "A 不应留下任何 trigger 归属"
    );

    drop(host);
    db.shutdown().await.unwrap();
}

/// 插件在注册 command 之后、`tools()` 阶段 panic：fail-closed 清理必须连同已注册的
/// command 元数据一起清掉（unregister_by_plugin 也要清 commands DashMap）。
struct CommandThenToolsPanicPlugin {
    name: String,
}

#[async_trait::async_trait]
impl Plugin for CommandThenToolsPanicPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "command 注册后 tools panic 插件"
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::from_message("cmd", Arc::new(NoopHandler))]
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        panic!("tools panic");
    }
}

#[tokio::test]
async fn activation_tools_panic_cleans_up_registered_commands() {
    let (host, db, name) = host_for_activation_test().await;
    let loaded = LoadedPlugin {
        plugin: Box::new(CommandThenToolsPanicPlugin { name: name.clone() }),
    };
    host.plugins.write().await.push(loaded.into());
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();

    assert!(
        host.toggle(&name, true).await.is_err(),
        "tools panic 激活必须失败"
    );
    assert!(
        host.trigger_registry()
            .get_all_commands()
            .iter()
            .all(|c| c.plugin_name.as_deref() != Some(name.as_str())),
        "fail-closed 清理后不应残留该插件的 command"
    );
    assert!(
        host.all_plugins().await.iter().all(|n| n != &name),
        "失败实例不应留在集合"
    );
    assert!(
        host.trigger_registry()
            .get_all_triggers()
            .iter()
            .all(|t| t.plugin_name.as_deref() != Some(name.as_str())),
        "清理后不应残留该插件的触发器"
    );

    drop(host);
    db.shutdown().await.unwrap();
}

// ── 统一生命周期事务：config 声明 panic → fail-closed ──

struct ConfigDeclPanicPlugin {
    name: String,
    panic_config_fields: bool,
    panic_service_config: bool,
}

#[async_trait::async_trait]
impl Plugin for ConfigDeclPanicPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "config 声明 panic 测试插件"
    }
    fn config_fields(&self) -> Vec<crate::core::PluginConfigField> {
        if self.panic_config_fields {
            panic!("config_fields panic");
        }
        vec![]
    }
    fn service_config(&self) -> Plugins {
        if self.panic_service_config {
            panic!("service_config panic");
        }
        Plugins::new(self.name())
    }
}

#[tokio::test]
async fn enable_config_declaration_panic_is_fail_closed() {
    for (panic_schema, panic_service, label) in [
        (true, false, "config_fields panic"),
        (false, true, "service_config panic"),
    ] {
        let (host, db, name) = host_for_activation_test().await;
        let loaded = LoadedPlugin {
            plugin: Box::new(ConfigDeclPanicPlugin {
                name: name.clone(),
                panic_config_fields: panic_schema,
                panic_service_config: panic_service,
            }),
        };
        host.db
            .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
            .await
            .unwrap();
        host.plugins.write().await.push(loaded.into());
        assert!(
            host.toggle(&name, true).await.is_err(),
            "[{label}] 激活必须失败"
        );
        assert!(
            host.all_plugins().await.iter().all(|n| n != &name),
            "[{label}] 失败实例不应留集合"
        );
        assert!(
            !db.is_plugin_enabled(&name).await.unwrap(),
            "[{label}] DB 应 disabled"
        );
        drop(host);
        db.shutdown().await.unwrap();
    }
}

// ── 统一生命周期事务：disable 的 on_disable error/panic 也完成清理并聚合返回 ──

struct FailDisableCommandPlugin {
    name: String,
    fail_on_disable: bool,
}

#[async_trait::async_trait]
impl Plugin for FailDisableCommandPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "disable 失败测试插件"
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::from_message("cmd", Arc::new(NoopHandler))]
    }
    async fn on_disable(&mut self) -> anyhow::Result<()> {
        if self.fail_on_disable {
            anyhow::bail!("disable boom");
        }
        Ok(())
    }
}

#[tokio::test]
async fn disable_on_disable_error_still_disables_and_cleans() {
    let (host, db, name) = host_for_activation_test().await;
    let loaded = LoadedPlugin {
        plugin: Box::new(FailDisableCommandPlugin {
            name: name.clone(),
            fail_on_disable: true,
        }),
    };
    host.plugins.write().await.push(loaded.into());
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    host.register_plugin_config(crate::core::Plugins::new(&name))
        .await;

    assert!(host.toggle(&name, true).await.is_ok(), "启用应成功");
    assert!(
        host.trigger_registry()
            .get_all_commands()
            .iter()
            .any(|c| c.plugin_name.as_deref() == Some(name.as_str())),
        "命令应已注册"
    );

    let result = host.toggle(&name, false).await;
    assert!(result.is_err(), "on_disable 失败应返回聚合错误");
    assert!(
        !db.is_plugin_enabled(&name).await.unwrap(),
        "DB 应 disabled"
    );
    assert!(
        host.trigger_registry()
            .get_all_commands()
            .iter()
            .all(|c| c.plugin_name.as_deref() != Some(name.as_str())),
        "命令应被清除"
    );
    if let Some(cfg) = host.get_plugin_config(&name).await {
        assert!(!cfg.enabled, "registry 应 disabled");
    }
    drop(host);
    db.shutdown().await.unwrap();
}

// ── 统一生命周期事务：reload 的 DB 读取错误保持旧状态 ──

#[tokio::test]
async fn reload_db_read_error_preserves_old_state() {
    let h = Harness::new("reloaddberr").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;
    let key = format!("plugin.{}.ck", h.plugin);

    // 让 is_plugin_enabled 读取失败：DROP plugins 表。
    // 测试桩：通过显式 DDL 删除 `plugins` 表，模拟后续查询该表失败的场景。
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "ALTER TABLE nuono_plugins RENAME COLUMN enabled TO unavailable_enabled",
        ))
        .await
        .unwrap();

    assert!(
        h.host.reload_plugin(&h.plugin).await.is_err(),
        "DB 读取失败时 reload 必须报错"
    );
    // 旧实例 / schema / 授权保持原样。
    assert_eq!(
        h.host.plugin_secret_schemas().await.len(),
        1,
        "schema 应保留"
    );
    h.db.orm()
        .execute_unprepared(
            "ALTER TABLE nuono_plugins RENAME COLUMN unavailable_enabled TO enabled",
        )
        .await
        .unwrap();
    h.host.set_plugin_secret(&key, "v").await.unwrap();
    assert!(
        h.host.all_plugins().await.iter().any(|n| n == &h.plugin),
        "旧实例应保留"
    );
    h.cleanup().await;
}

// ── 统一生命周期事务：shutdown 拒绝 on_unload 的新任务并完成清理，聚合错误 ──

struct UnloadSpawnPlugin {
    name: String,
    fail_on_unload: bool,
    tasks: std::sync::Mutex<Option<Arc<PluginTaskScope>>>,
}

#[async_trait::async_trait]
impl Plugin for UnloadSpawnPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "on_unload 里再 spawn 的测试插件"
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::from_message("cmd", Arc::new(NoopHandler))]
    }
    async fn on_load(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        *self.tasks.lock().unwrap() = Some(ctx.tasks().clone());
        Ok(())
    }
    async fn on_unload(&mut self) -> anyhow::Result<()> {
        if let Some(tasks) = self.tasks.lock().unwrap().clone() {
            tasks.spawn("late", async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
        }
        if self.fail_on_unload {
            anyhow::bail!("unload boom");
        }
        Ok(())
    }
}

#[tokio::test]
async fn shutdown_reaps_unload_spawn_and_clears_state() {
    let (host, db, name) = host_for_activation_test().await;
    let loaded = LoadedPlugin {
        plugin: Box::new(UnloadSpawnPlugin {
            name: name.clone(),
            fail_on_unload: true,
            tasks: std::sync::Mutex::new(None),
        }),
    };
    host.plugins.write().await.push(loaded.into());
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    host.register_plugin_config(crate::core::Plugins::new(&name))
        .await;
    // 登记一个待回复回调，验证 shutdown 会清空。
    host.register_pending_wait("sess".to_string(), name.clone())
        .await;

    assert!(host.toggle(&name, true).await.is_ok(), "启用应成功");

    // shutdown 返回聚合错误（on_unload 失败），但仍拒绝新后台任务并完成清理。
    let result = host.shutdown().await;
    assert!(result.is_err(), "shutdown 应返回 on_unload 聚合错误");
    assert!(
        host.take_pending_wait("sess").await.is_none(),
        "pending_wait_callbacks 应被清空"
    );
    assert!(
        host.get_plugin_config(&name).await.is_none(),
        "plugin_registry 应被清空"
    );
    assert!(
        host.trigger_registry()
            .get_all_commands()
            .iter()
            .all(|c| c.plugin_name.as_deref() != Some(name.as_str())),
        "命令应被清空"
    );

    // 未完成所有清理前不要 drop host：先验证 task scope 已移除。
    assert!(
        host.all_plugins().await.iter().all(|n| n != &name),
        "实例应已离开集合"
    );
    drop(host);
    db.shutdown().await.unwrap();
}

// ── 统一生命周期事务：reload 的 secret_fields() panic 必须被吞并保持旧状态 ──

struct PanickingSecretPlugin {
    name: String,
    _events: Arc<AtomicUsize>,
    _messages: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Plugin for PanickingSecretPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "secret_fields panic 测试插件"
    }
    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        panic!("secret_fields boom");
    }
}

#[tokio::test]
async fn reload_secret_field_panic_is_caught_and_preserves_old_state() {
    let h = Harness::new("reloadpanic").await;
    h.install_secrets(vec![crate::plugin::PluginSecretField::new(
        "ck", "CK", "desc",
    )])
    .await;
    let key = format!("plugin.{}.ck", h.plugin);

    // 构造一个 secret_fields() 会 panic 的新候选（未加入集合，模拟磁盘加载结果）。
    let events = h.events.clone();
    let messages = h.messages.clone();
    let panicking = LoadedPlugin {
        plugin: Box::new(PanickingSecretPlugin {
            name: h.plugin.clone(),
            _events: events,
            _messages: messages,
        }),
    };

    // was_enabled=false：panic 发生在 swap 之前，旧实例 / schema / 授权必须保持原样。
    let r = h
        .host
        .reload_with_candidate(&h.plugin, panicking, false)
        .await;
    assert!(
        r.is_err(),
        "secret_fields panic 必须转成可读 Err，而不是向外抛出"
    );
    assert!(
        r.err()
            .map(|e| e.to_string())
            .unwrap_or_default()
            .contains("panic"),
        "错误信息应标明来自插件 panic"
    );
    assert_eq!(
        h.host.plugin_secret_schemas().await.len(),
        1,
        "旧 schema 应保留"
    );
    assert!(
        h.host.all_plugins().await.iter().any(|n| n == &h.plugin),
        "旧实例应保留"
    );
    h.host.set_plugin_secret(&key, "v").await.unwrap();
    h.cleanup().await;
}

#[tokio::test]
async fn reload_declaration_panic_preserves_enabled_old_instance() {
    for (panic_config_fields, panic_commands, label) in
        [(true, false, "config_fields"), (false, true, "commands")]
    {
        let h = Harness::new("reload-declaration-panic").await;
        h.install_secrets(vec![crate::plugin::PluginSecretField::new(
            "ck",
            "CK",
            "old secret",
        )])
        .await;
        let before_plugins = h.host.all_plugins().await;
        let before_description = h.host.get_plugin_info(&h.plugin).await.description;
        let old_secret_key = format!("plugin.{}.ck", h.plugin);
        let candidate = LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin {
                panic_config_fields,
                panic_commands,
                ..DeclarationCandidatePlugin::new(h.plugin.clone())
            }),
        };
        let result = h
            .host
            .reload_with_candidate(&h.plugin, candidate, true)
            .await;
        assert!(result.is_err(), "{label} panic 应拒绝新候选");
        assert_eq!(h.host.all_plugins().await, before_plugins);
        assert_eq!(
            h.host.get_plugin_info(&h.plugin).await.description,
            before_description,
            "{label} panic 不应替换旧实例"
        );
        assert!(h.db.is_plugin_enabled(&h.plugin).await.unwrap());
        assert!(h.host.get_plugin_config(&h.plugin).await.unwrap().enabled);
        let schemas = h.host.plugin_secret_schemas().await;
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].key, old_secret_key);
        h.host
            .set_plugin_secret(&old_secret_key, "v")
            .await
            .unwrap();
        assert!(h.ran_for(None).await, "{label} panic 后旧实例仍能处理事件");
        h.cleanup().await;
    }
}

#[tokio::test]
async fn reload_restores_enabled_plugin_from_single_command_snapshot() {
    let h = Harness::new("reload-command-snapshot").await;
    let declarations = Arc::new(AtomicUsize::new(0));
    h.host
        .reload_with_candidate(
            &h.plugin,
            LoadedPlugin {
                plugin: Box::new(CountingCommandsPlugin {
                    name: h.plugin.clone(),
                    declarations: declarations.clone(),
                }),
            },
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        declarations.load(Ordering::SeqCst),
        1,
        "重建预检与恢复启用必须复用同一次 commands 声明"
    );
    let commands: Vec<_> = h
        .host
        .trigger_registry()
        .get_all_commands()
        .into_iter()
        .filter(|command| command.plugin_name.as_deref() == Some(h.plugin.as_str()))
        .collect();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "native-1");
    let triggers: Vec<_> = h
        .host
        .trigger_registry()
        .get_triggers(TriggerType::EventCommand)
        .into_iter()
        .filter(|trigger| trigger.plugin_name.as_deref() == Some(h.plugin.as_str()))
        .collect();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].pattern, "native-1");
    assert!(h.db.is_plugin_enabled(&h.plugin).await.unwrap());
    assert!(h.host.get_plugin_config(&h.plugin).await.unwrap().enabled);
    h.cleanup().await;
}

struct DisabledConfigurationPlugin {
    name: String,
    field: &'static str,
    loads: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Plugin for DisabledConfigurationPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "禁用实例的配置声明测试插件"
    }

    fn config_fields(&self) -> Vec<crate::core::PluginConfigField> {
        vec![crate::core::PluginConfigField::boolean(
            self.field,
            "Feature",
            "disabled plugin configuration",
            true,
        )]
    }

    fn secret_fields(&self) -> Vec<crate::plugin::PluginSecretField> {
        vec![crate::plugin::PluginSecretField::new(
            self.field,
            "Secret",
            "disabled plugin secret",
        )]
    }

    async fn on_load(&mut self, _ctx: &PluginContext) -> anyhow::Result<()> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn reload_disabled_plugin_updates_forms_without_activation_or_task_scope() {
    let (host, db, name) = host_for_activation_test().await;
    let loads = Arc::new(AtomicUsize::new(0));
    db.sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    db.set_plugin_enabled(&name, false).await.unwrap();
    host.init_with_plugins(vec![LoadedPlugin {
        plugin: Box::new(DisabledConfigurationPlugin {
            name: name.clone(),
            field: "old_feature",
            loads: loads.clone(),
        }),
    }])
    .await
    .unwrap();
    let manager = host.config_manager.read().await.as_ref().cloned().unwrap();
    assert_eq!(
        host.get_plugin_config_fields(&name).await[0].key,
        "old_feature"
    );
    assert!(
        manager
            .get_form_schema(&name)
            .await
            .unwrap()
            .contains_key("old_feature")
    );
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert!(!host.task_scopes.read().await.contains_key(&name));

    host.reload_with_candidate(
        &name,
        LoadedPlugin {
            plugin: Box::new(DisabledConfigurationPlugin {
                name: name.clone(),
                field: "new_feature",
                loads: loads.clone(),
            }),
        },
        false,
    )
    .await
    .unwrap();

    let fields = host.get_plugin_config_fields(&name).await;
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].key, "new_feature");
    let form = manager.get_form_schema(&name).await.unwrap();
    assert!(form.contains_key("new_feature"));
    assert!(!form.contains_key("old_feature"), "旧字段不能留在活表单中");
    let secrets = host.plugin_secret_schemas().await;
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].key, format!("plugin.{name}.new_feature"));
    assert_eq!(
        loads.load(Ordering::SeqCst),
        0,
        "禁用实例重建不调用 on_load"
    );
    assert!(
        !host.task_scopes.read().await.contains_key(&name),
        "禁用实例重建不创建任务作用域"
    );
    assert!(!db.is_plugin_enabled(&name).await.unwrap());
    assert!(!host.get_plugin_config(&name).await.unwrap().enabled);
    drop(host);
    db.shutdown().await.unwrap();
}

// ── 安装阶段：无效候选的 DB 收敛失败必须致命 ──

#[tokio::test]
async fn init_invalid_candidate_database_converge_failure_is_fatal() {
    let h = Harness::new("convergefatal").await;
    // 构造 secret 声明非法的候选（重复字段），安装阶段必须将其 DB 状态收敛为 disabled。
    let events = h.events.clone();
    let messages = h.messages.clone();
    let bad = LoadedPlugin {
        plugin: Box::new(CountingPlugin {
            name: "BadCand".to_string(),
            events,
            messages,
            filter: crate::plugin::PluginEventFilter::ALL,
            secret_fields: vec![
                crate::plugin::PluginSecretField::new("ck", "CK", "d"),
                crate::plugin::PluginSecretField::new("ck", "CK2", "d2"),
            ],
        }),
    };
    // 让 DB 读/写都失败：DROP plugins 表，使收敛 set_plugin_enabled(false) 报错。
    // 测试桩：通过显式 DDL 删除 `plugins` 表，模拟后续查询该表失败的场景。
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "ALTER TABLE nuono_plugins RENAME COLUMN enabled TO unavailable_enabled",
        ))
        .await
        .unwrap();

    let result = h.host.init_with_plugins(vec![bad]).await;
    assert!(
        result.is_err(),
        "无效候选 DB 收敛 disabled 失败时安装必须返回 Err"
    );
    assert!(
        result.err().is_some_and(|e| !e.to_string().is_empty()),
        "错误应有可观察信息"
    );
    h.cleanup().await;
}

// ── 统一生命周期事务：disable / fail-cleanup 只清除自己的 pending_wait_callbacks ──

#[tokio::test]
async fn disable_purges_only_own_pending_waits() {
    let (host, db, name) = host_for_activation_test().await;
    let other = format!("Other{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let loaded = LoadedPlugin {
        plugin: Box::new(FailDisableCommandPlugin {
            name: name.clone(),
            fail_on_disable: true,
        }),
    };
    host.plugins.write().await.push(loaded.into());
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    host.register_plugin_config(crate::core::Plugins::new(&name))
        .await;
    host.toggle(&name, true).await.unwrap();

    host.register_pending_wait("sess-own".to_string(), name.clone())
        .await;
    host.register_pending_wait("sess-other".to_string(), other.clone())
        .await;

    let r = host.toggle(&name, false).await;
    assert!(r.is_err(), "on_disable 失败应返回聚合错误");
    assert!(
        host.take_pending_wait("sess-own").await.is_none(),
        "自己的 wait 应被清除"
    );
    assert_eq!(
        host.take_pending_wait("sess-other").await.as_deref(),
        Some(other.as_str()),
        "别的插件 wait 应保留"
    );
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn activation_failure_cleanup_purges_only_own_pending_waits() {
    let (host, db, name) = host_for_activation_test().await;
    let other = format!("Other{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let loaded = LoadedPlugin {
        plugin: Box::new(ActivationFailPlugin {
            name: name.clone(),
            fail_on_load: true,
            panic_on_load: false,
            fail_on_enable: false,
            panic_on_enable: false,
            secret_fields: vec![],
        }),
    };
    host.plugins.write().await.push(loaded.into());
    host.db
        .sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();

    host.register_pending_wait("sess-own".to_string(), name.clone())
        .await;
    host.register_pending_wait("sess-other".to_string(), other.clone())
        .await;

    let r = host.toggle(&name, true).await;
    assert!(r.is_err(), "激活失败应报错");
    assert!(
        host.take_pending_wait("sess-own").await.is_none(),
        "fail-cleanup 后自己的 wait 应被清除"
    );
    assert_eq!(
        host.take_pending_wait("sess-other").await.as_deref(),
        Some(other.as_str()),
        "别的插件 wait 应保留"
    );
    drop(host);
    db.shutdown().await.unwrap();
}

// ── 统一生命周期事务：toggle_plugin 的 DB 读取错误必须 fail-closed，不触发副作用 ──

#[tokio::test]
async fn toggle_db_read_error_fails_closed_and_preserves_state() {
    let h = Harness::new("toggledberr").await;
    // 显式把注册表快照置为 enabled=true（Harness 默认即如此，这里强调断言基准）。
    let mut cfg = Plugins::new(&h.plugin);
    cfg.enabled = true;
    cfg.area = PluginArea::All;
    h.host.register_plugin_config(cfg).await;

    // 让 is_plugin_enabled 读取失败。
    // 测试桩：通过显式 DDL 删除 `plugins` 表，模拟后续查询该表失败的场景。
    h.db.orm()
        .execute_raw(sea_orm::Statement::from_string(
            h.db.backend(),
            "ALTER TABLE nuono_plugins RENAME COLUMN enabled TO unavailable_enabled",
        ))
        .await
        .unwrap();

    let r = h.host.toggle_plugin(&h.plugin).await;
    assert!(r.is_err(), "toggle DB 读取失败必须报错");
    // 失败即关闭：不得触发 on_load/on_enable 副作用，实例与注册表保持原样。
    assert!(
        h.host.all_plugins().await.iter().any(|n| n == &h.plugin),
        "实例应保留（未发生切换）"
    );
    assert_eq!(
        h.host.get_plugin_config(&h.plugin).await.map(|c| c.enabled),
        Some(true),
        "registry enabled 应保持初始 true"
    );
    h.cleanup().await;
}
struct LiveConfigPlugin {
    name: String,
    loads: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Plugin for LiveConfigPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "live file config test"
    }
    fn config_fields(&self) -> Vec<crate::core::config_schema::PluginConfigField> {
        vec![crate::core::config_schema::PluginConfigField::number(
            "count", "Count", "", 3,
        )]
    }
    async fn on_load(&mut self, _ctx: &PluginContext) -> anyhow::Result<()> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn on_config_changed(&mut self, config: &Value) -> anyhow::Result<()> {
        self.changes.fetch_add(1, Ordering::SeqCst);
        let value = config["count"].as_u64().unwrap();
        if value == 13 {
            anyhow::bail!("business rejected count");
        }
        self.accepted.store(value as usize, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
async fn monitor_refreshes_plugin_and_services_without_reloading_or_repeating_errors() {
    let h = Harness::new("live-files").await;
    h.host
        .set_config_manager(Arc::new(PluginConfigManager::new(
            h.root.join("embedded-config"),
        )))
        .await;
    let loads = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    *h.host.plugins.write().await = vec![LoadedPlugin {
        plugin: Box::new(LiveConfigPlugin {
            name: h.plugin.clone(),
            loads: loads.clone(),
            changes: changes.clone(),
            accepted: accepted.clone(),
        }),
    }]
    .into_iter()
    .map(Into::into)
    .collect();
    h.host.enable_plugin(&h.plugin).await.unwrap();
    let mgr = h.host.config_manager.read().await.as_ref().unwrap().clone();
    let handle = mgr.config_handle(&h.plugin).await.unwrap();
    let path = mgr.plugin_dir(&h.plugin).join("config.toml");
    let monitor = h.host.start_config_monitor();
    // 编辑器常用的替换保存方式；不依赖文件 inode 的监听。
    let temporary = path.with_extension("new");
    tokio::fs::write(
        &temporary,
        "count = 7\nprefix = [\"new\"]\nmin_permission = \"GroupAdmin\"\n",
    )
    .await
    .unwrap();
    tokio::fs::rename(&temporary, &path).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while accepted.load(Ordering::SeqCst) != 7 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.host.refresh_plugin_config(&h.plugin).await.unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(handle.snapshot()["count"], 7);
    assert_eq!(
        h.host.get_plugin_config(&h.plugin).await.unwrap().prefix,
        vec!["new".to_string()]
    );
    assert_eq!(
        h.host
            .get_plugin_config(&h.plugin)
            .await
            .unwrap()
            .min_permission,
        PermissionLevel::GroupAdmin
    );
    tokio::fs::write(&path, "count = \"invalid\"\n")
        .await
        .unwrap();
    assert!(h.host.refresh_plugin_config(&h.plugin).await.is_err());
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(handle.snapshot()["count"], 7);
    tokio::fs::write(&path, "count = 13\n").await.unwrap();
    assert!(h.host.refresh_plugin_config(&h.plugin).await.is_err());
    assert!(mgr.status(&h.plugin).await.runtime_error.is_some());
    assert_eq!(accepted.load(Ordering::SeqCst), 7);
    assert!(h.host.refresh_plugin_config(&h.plugin).await.is_err());
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    monitor.abort();
    let _ = monitor.await;
    h.cleanup().await;
}

#[tokio::test]
async fn legacy_plugin_priority_is_preserved_without_a_service_setting() {
    let h = Harness::new("legacy-priority").await;
    h.host
        .set_config_manager(Arc::new(PluginConfigManager::new(
            h.root.join("embedded-config"),
        )))
        .await;
    let manager = h.host.config_manager.read().await.as_ref().unwrap().clone();
    let defaults = Plugins::new(&h.plugin);
    manager
        .register_with_service_config(&h.plugin, Default::default(), Some(&defaults))
        .await
        .unwrap();
    let path = manager.plugin_dir(&h.plugin).join("config.toml");
    tokio::fs::write(
        &path,
        "priority = -9999\nprefix = [\"new\"]\nmin_permission = \"GroupAdmin\"\n",
    )
    .await
    .unwrap();
    manager.refresh(&h.plugin).await.unwrap();

    let service = h.host.service_config_from_file(&h.plugin, defaults).await;
    assert_eq!(service.prefix, vec!["new".to_string()]);
    assert_eq!(service.min_permission, PermissionLevel::GroupAdmin);
    assert!(
        serde_json::to_value(service)
            .unwrap()
            .get("priority")
            .is_none()
    );
    assert!(
        !manager
            .get_form_schema(&h.plugin)
            .await
            .unwrap()
            .contains_key("priority")
    );
    assert_eq!(
        manager.get_runtime_config(&h.plugin).await.unwrap()["priority"],
        -9999
    );

    let before = tokio::fs::read(&path).await.unwrap();
    assert!(
        manager
            .set_values(
                &h.plugin,
                HashMap::from([("priority".into(), ConfigValue::Int(0))]),
            )
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
    manager
        .set_values(
            &h.plugin,
            HashMap::from([(
                "min_permission".into(),
                ConfigValue::String("BotAdmin".into()),
            )]),
        )
        .await
        .unwrap();
    assert_eq!(
        manager.get_runtime_config(&h.plugin).await.unwrap()["priority"],
        -9999
    );
    h.cleanup().await;
}

#[tokio::test]
async fn scoped_config_save_releases_command_lock_before_monitor_callback_and_updates_group_config()
{
    let h = Harness::new("config-port").await;
    let loads = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    *h.host.plugins.write().await = vec![LoadedPlugin {
        plugin: Box::new(LiveConfigPlugin {
            name: h.plugin.clone(),
            loads: loads.clone(),
            changes: changes.clone(),
            accepted: accepted.clone(),
        }),
    }]
    .into_iter()
    .map(Into::into)
    .collect();
    h.host.enable_plugin(&h.plugin).await.unwrap();
    let manager = h.host.config_manager.read().await.as_ref().unwrap().clone();
    let ctx = h
        .host
        .plugin_context(
            &h.plugin,
            serde_json::json!({"count":3}),
            PluginActivationReason::Enable,
        )
        .await;
    let initial = ctx.config_updates().read_latest().await.unwrap();
    let group =
        h.db.ensure_group(
            &nuono_storage::groups::GroupIdentity::onebot(&h.group("updated")),
            "",
        )
        .await
        .unwrap()
        .id
        .to_string();
    assert_eq!(
        group_plugins::resolve_for_group(&h.db, &manager, &h.plugin, &group)
            .await
            .unwrap()
            .values["count"],
        3
    );

    // 插件指令或事件回调在调用保存时可能已持有该锁，因此保存流程绝不能再去等待一个需要重新获取该锁的回调，否则会死锁。
    let plugin_lock = h.host.plugins.write().await;
    let monitor = h.host.start_config_monitor();
    let saved = tokio::time::timeout(
        Duration::from_secs(2),
        ctx.config_updates()
            .update_patch(serde_json::json!({"count":9}), &initial.revision),
    )
    .await
    .expect("config save must not re-enter the plugin lock")
    .unwrap();
    assert_eq!(saved.values["count"], 9);
    assert_eq!(ctx.current_config()["count"], 9);
    assert_eq!(changes.load(Ordering::SeqCst), 0);
    assert_eq!(
        group_plugins::resolve_for_group(&h.db, &manager, &h.plugin, &group)
            .await
            .unwrap()
            .values["count"],
        9
    );
    drop(plugin_lock);
    tokio::time::timeout(Duration::from_secs(4), async {
        while accepted.load(Ordering::SeqCst) != 9 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.host.refresh_plugin_config(&h.plugin).await.unwrap();
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    monitor.abort();
    let _ = monitor.await;
    drop(ctx);

    // 未挂载文件配置管理器的嵌入式宿主在保存配置时应直接报错，不能伪造持久化成功。
    *h.host.config_manager.write().await = None;
    let unsupported = h
        .host
        .plugin_context(&h.plugin, saved.values, PluginActivationReason::Enable)
        .await;
    assert!(unsupported.config_updates().read_latest().await.is_err());
    assert!(
        unsupported
            .config_updates()
            .update_patch(serde_json::json!({"count":10}), &saved.revision)
            .await
            .is_err()
    );
    drop(unsupported);
    h.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_sdk_uses_real_host_services_and_reaps_subscriptions() {
    use crate::core::event_bus::EventType;
    use nuono_app::plugin_data::PluginDataStore;
    let h = Harness::new("unified-sdk").await;
    // 本批安装会保留已有实例；本用例只让 SDK probe 消费事件，显式禁用默认 Stop 插件。
    h.host.toggle(&h.plugin, false).await.unwrap();
    h.host
        .init_with_plugins(vec![LoadedPlugin {
            plugin: super::sdk_fixture::nuono_create_plugin(),
        }])
        .await
        .unwrap();
    let wait_key = |key: &'static str| async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.db.kv_get("sdk-probe", key).await.unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    };
    wait_key("task-started").await;
    assert_eq!(
        h.db.kv_get("sdk-probe", "loaded").await.unwrap().as_deref(),
        Some("yes")
    );
    assert_eq!(
        h.db.kv_get("sdk-probe", "bus").await.unwrap().as_deref(),
        Some("yes")
    );
    let event_type = EventType::PluginEvent("probe".into());
    assert_eq!(h.host.event_bus.subscriber_count(event_type.clone()), 1);
    let raw = serde_json::json!({"post_type":"message","message_type":"private","self_id":"bot:001","user_id":"user:001","raw_message":"hello"});
    let filter = PluginFilterContext {
        user_id: "user:001".into(),
        group_id: None,
        msg_type: "private".into(),
        actor_permission: ActorPermission::everyone(),
    };
    let response = h.host.process_message("", &raw, &filter).await.response;
    assert!(
        matches!(response, PluginResponse::Reply(ref action) if action["params"]["user_id"]=="user:001")
    );
    h.host
        .reload_with_candidate(
            "sdk-probe",
            LoadedPlugin {
                plugin: super::sdk_fixture::nuono_create_plugin(),
            },
            true,
        )
        .await
        .unwrap();
    assert_eq!(h.host.event_bus.subscriber_count(event_type.clone()), 1);
    h.host.toggle("sdk-probe", false).await.unwrap();
    assert_eq!(h.host.event_bus.subscriber_count(event_type.clone()), 0);
    h.host.toggle("sdk-probe", true).await.unwrap();
    assert_eq!(h.host.event_bus.subscriber_count(event_type.clone()), 1);
    h.host.shutdown().await.unwrap();
    assert_eq!(h.host.event_bus.subscriber_count(event_type), 0);
    h.cleanup().await;
}

#[tokio::test]
async fn sdk_runtime_context_and_subscriptions_follow_host_recreation() {
    use nuono_plugin::PluginStateScope;

    let h = Harness::new("sdk-runtime").await;
    let runtime = nuono_plugin::PluginRuntime::default();
    assert!(runtime.context().is_err());
    let ctx = h
        .host
        .plugin_context(
            &h.plugin,
            serde_json::json!({}),
            PluginActivationReason::Enable,
        )
        .await;
    runtime.bind(&ctx).unwrap();
    assert_eq!(runtime.context().unwrap().plugin_name(), h.plugin);
    let target = nuono_plugin::RuntimeMessageTarget::group(
        nuono_plugin::RuntimeBotRoute::new("qq_official", "east", "bot"),
        "group-openid",
    );
    ctx.subscriptions()
        .set("weather", target, serde_json::json!({"city":"杭州"}))
        .await
        .unwrap();
    ctx.state("preferences", PluginStateScope::global())
        .unwrap()
        .set("city", 1, "杭州")
        .await
        .unwrap();
    let next = h
        .host
        .plugin_context(
            &h.plugin,
            serde_json::json!({}),
            PluginActivationReason::Enable,
        )
        .await;
    assert!(ctx.tasks().is_closed());
    assert!(runtime.context().is_err());
    assert!(runtime.bind(&ctx).is_err());
    runtime.bind(&next).unwrap();
    assert_eq!(
        runtime
            .context()
            .unwrap()
            .subscriptions()
            .list("weather")
            .await
            .unwrap()
            .len(),
        1
    );
    let record = next
        .state("preferences", PluginStateScope::global())
        .unwrap()
        .get("city")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.value_version, 1);
    assert_eq!(record.value, "杭州");
    next.tasks().shutdown().await;
    assert!(runtime.context().is_err());
    h.cleanup().await;
}

#[tokio::test]
async fn managed_plugin_state_validates_input_and_keeps_owner_collection_and_scope_separate() {
    use nuono_domain::{DomainEvent, NativeId};
    use nuono_plugin::PluginStateScope;
    use serde_json::json;

    let h = Harness::new("managed-state").await;
    let ctx = h
        .host
        .plugin_context(&h.plugin, json!({}), PluginActivationReason::Enable)
        .await;
    let other = h
        .host
        .plugin_context(
            "other-state-owner",
            json!({}),
            PluginActivationReason::Enable,
        )
        .await;
    let mut event: DomainEvent = serde_json::from_value(json!({
        "event_id":"state-event",
        "occurred_at":"2026-01-01T00:00:00Z",
        "received_at":"2026-01-01T00:00:00Z",
        "bot":{"instance_id":"east","account_id":{
            "platform":"qq_official","kind":"bot","scheme":"app_id","value":"test-app"
        }},
        "actor":{"platform":"qq_official","kind":"user","scheme":"open_id","value":"user","namespace":"test-app"},
        "conversation":{"type":"group","group":{
            "platform":"qq_official","kind":"group","scheme":"open_id","value":"group","namespace":"test-app"
        }},
        "kind":{"type":"notice","data":{"notice_type":"state-test"}}
    })).unwrap();
    let east = PluginStateScope::conversation(&event).unwrap();
    let user = PluginStateScope::user(&event).unwrap();
    assert_ne!(east, user);
    event.bot.instance_id = NativeId::new("west").unwrap();
    let west = PluginStateScope::conversation(&event).unwrap();
    assert_ne!(east, west);
    event.actor = None;
    assert!(PluginStateScope::user(&event).is_err());

    assert!(ctx.state("_kv", PluginStateScope::global()).is_err());
    let prefs = ctx.state("preferences", east.clone()).unwrap();
    prefs.set("city", 1, "上海").await.unwrap();
    prefs.set("city", 2, &json!({"name":"上海"})).await.unwrap();
    prefs.set("units", 1, "metric").await.unwrap();
    prefs
        .set("空值", 1, &serde_json::Value::Null)
        .await
        .unwrap();
    for separate in [
        other.state("preferences", east.clone()).unwrap(),
        ctx.state("another_collection", east).unwrap(),
        ctx.state("preferences", west).unwrap(),
        ctx.state("preferences", user).unwrap(),
        ctx.state("preferences", PluginStateScope::global())
            .unwrap(),
    ] {
        assert!(separate.get("city").await.unwrap().is_none());
    }
    let first = prefs.list(None, 1).await.unwrap();
    assert_eq!(first[0].0, "city");
    assert_eq!(first[0].1.value_version, 2);
    assert_eq!(first[0].1.value, json!({"name":"上海"}));
    let next = prefs.list(Some(&first[0].0), 100).await.unwrap();
    assert_eq!(
        next.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        ["units", "空值"]
    );
    assert!(next[1].1.value.is_null());
    assert!(prefs.list(None, 101).await.is_err());
    assert!(prefs.set("city", 0, "invalid version").await.is_err());
    assert!(prefs.set("city", 2, &"x".repeat(64 * 1024)).await.is_err());
    let mut nested = serde_json::Value::Null;
    for _ in 0..128 {
        nested = serde_json::Value::Array(vec![nested]);
    }
    assert!(prefs.set("city", 2, &nested).await.is_err());
    assert!(prefs.get("  city").await.is_err());

    ctx.kv().set("city", "null").await.unwrap();
    assert_eq!(ctx.kv().get("city").await.unwrap().as_deref(), Some("null"));
    assert_eq!(ctx.kv().clear().await.unwrap(), 1);
    assert_eq!(prefs.get("city").await.unwrap().unwrap(), first[0].1);
    assert!(prefs.delete("city").await.unwrap());
    assert!(!prefs.delete("city").await.unwrap());
    other.tasks().shutdown().await;
    h.cleanup().await;
}

struct DatabasePolicyFixture {
    name: String,
    panic_policy: bool,
}
#[async_trait::async_trait]
impl Plugin for DatabasePolicyFixture {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "database policy fixture"
    }
    fn database_tables(&self) -> Vec<String> {
        vec![format!("{}_notes", self.name)]
    }
    fn migrations(&self) -> Vec<nuono_plugin::PluginMigrationForBackend> {
        let sql = format!("CREATE TABLE {}_notes (id TEXT PRIMARY KEY)", self.name);
        vec![nuono_plugin::PluginMigrationForBackend {
            version: 1,
            description: "notes storage".into(),
            pg_sql: sql.clone(),
            sqlite_sql: format!("{sql} STRICT"),
        }]
    }
    fn database_management(&self) -> Vec<nuono_plugin::db::DatabaseTablePolicy> {
        if self.panic_policy {
            panic!("fixture declaration failure");
        }
        vec![nuono_plugin::db::DatabaseTablePolicy::read(
            format!("{}_notes", self.name),
            vec![nuono_plugin::db::DatabaseColumnPolicy::read("id")],
        )]
    }
}

#[tokio::test]
async fn database_declarations_include_disabled_plugins_and_isolate_policy_panics() {
    let h = Harness::new("DatabasePolicies").await;
    let healthy = format!("{}_healthy", h.plugin.to_ascii_lowercase());
    let broken = format!("{}_broken", h.plugin.to_ascii_lowercase());
    h.host
        .init_with_plugins(vec![
            LoadedPlugin {
                plugin: Box::new(DatabasePolicyFixture {
                    name: healthy.clone(),
                    panic_policy: false,
                }),
            },
            LoadedPlugin {
                plugin: Box::new(DatabasePolicyFixture {
                    name: broken.clone(),
                    panic_policy: true,
                }),
            },
        ])
        .await
        .unwrap();
    let mut config = Plugins::new(&healthy);
    config.enabled = false;
    h.host.register_plugin_config(config).await;
    let declarations = h.host.database_management_declarations().await.unwrap();
    let good = declarations
        .iter()
        .find(|(name, _, _)| name == &healthy)
        .unwrap();
    assert_eq!(good.1, vec![format!("{healthy}_notes")]);
    assert_eq!(good.2.len(), 1);
    let failed = declarations
        .iter()
        .find(|(name, _, _)| name == &broken)
        .unwrap();
    assert_eq!(failed.1, vec![format!("{broken}_notes")]);
    assert!(failed.2.is_empty());
    h.cleanup().await;
}

#[test]
fn plugin_web_route_body_limits_are_bounded_and_default_compatible() {
    use crate::plugin::{PluginWebAccessPolicy, PluginWebRoute};
    let route = PluginWebRoute::new(http::Method::POST, "/save", PluginWebAccessPolicy::Public);
    assert_eq!(route.max_body_bytes, 2 * 1024 * 1024);
    for limit in [0, 32 * 1024 * 1024 + 1, usize::MAX] {
        assert!(
            validated_plugin_web_routes("limits", vec![route.clone().with_max_body_bytes(limit)])
                .is_err()
        );
    }
    assert!(
        validated_plugin_web_routes("limits", vec![route.with_max_body_bytes(17 * 1024 * 1024)])
            .is_ok()
    );
}

#[tokio::test]
async fn plugin_web_body_limit_uses_exact_enabled_route_and_guards_direct_dispatch() {
    use crate::plugin::{
        PluginWebAccessPolicy, PluginWebRequest, PluginWebResponse, PluginWebRoute,
    };
    let h = Harness::new("web-body-limit").await;
    h.host.web_routes.write().await.insert(
        h.plugin.clone(),
        vec![
            PluginWebRoute::new(http::Method::POST, "/save", PluginWebAccessPolicy::Public)
                .with_max_body_bytes(3),
        ],
    );
    assert_eq!(
        h.host
            .plugin_web_route(&h.plugin, &http::Method::POST, "/save")
            .await
            .unwrap()
            .max_body_bytes,
        3
    );
    for (plugin, method, path) in [
        (h.plugin.as_str(), http::Method::GET, "/save"),
        (h.plugin.as_str(), http::Method::POST, "/save/extra"),
        ("missing", http::Method::POST, "/save"),
    ] {
        assert!(
            h.host
                .plugin_web_route(plugin, &method, path)
                .await
                .is_none()
        );
    }
    let mut request = PluginWebRequest {
        method: http::Method::POST,
        path: "/save".into(),
        query: String::new(),
        headers: http::HeaderMap::new(),
        body: vec![0; 3],
        user: None,
    };
    assert!(matches!(
        h.host.handle_web_request(&h.plugin, &request).await,
        PluginWebResponse::Json(_)
    ));
    request.body.push(0);
    assert!(matches!(
        h.host.handle_web_request(&h.plugin, &request).await,
        PluginWebResponse::Status(413, _)
    ));
    h.host.set_plugin_registry_enabled(&h.plugin, false).await;
    assert!(
        h.host
            .plugin_web_route(&h.plugin, &http::Method::POST, "/save")
            .await
            .is_none()
    );
    h.cleanup().await;
}
