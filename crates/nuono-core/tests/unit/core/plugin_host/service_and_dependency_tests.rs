use super::*;
use crate::plugin::{
    LoadedPlugin, Plugin, PluginRole, PluginServiceError, PluginServiceHandler, PluginServiceResult,
};
use serde_json::{Value, json};
use std::sync::Mutex;

struct UpstreamDemoService(&'static str);

#[async_trait::async_trait]
impl PluginServiceHandler for UpstreamDemoService {
    async fn handle(&self, action: &str, _payload: Value) -> PluginServiceResult<Value> {
        match action {
            "ping" => Ok(json!({ "tag": self.0 })),
            "expired" => Err(PluginServiceError::unauthorized("凭据已失效", Some(220))),
            "rate_limited" => Err(PluginServiceError::rate_limited("请求过于频繁", Some(3000))),
            "upstream" => Err(PluginServiceError::upstream("上游超时", None)),
            other => Err(PluginServiceError::action_not_found("auth_svc", other)),
        }
    }
}

struct TestPlugin {
    name: &'static str,
    role: PluginRole,
    required: Vec<&'static str>,
    optional: Vec<&'static str>,
    provide_tag: Option<&'static str>,
    lookup_upstream: Option<&'static str>,
    load_order: Arc<Mutex<Vec<String>>>,
    observed_service_in_load: Arc<Mutex<Option<String>>>,
}

impl TestPlugin {
    fn new(name: &'static str, load_order: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name,
            role: PluginRole::Feature,
            required: Vec::new(),
            optional: Vec::new(),
            provide_tag: None,
            lookup_upstream: None,
            load_order,
            observed_service_in_load: Arc::new(Mutex::new(None)),
        }
    }

    fn with_role(mut self, role: PluginRole) -> Self {
        self.role = role;
        self
    }

    fn with_required(mut self, deps: Vec<&'static str>) -> Self {
        self.required = deps;
        self
    }

    fn with_optional(mut self, deps: Vec<&'static str>) -> Self {
        self.optional = deps;
        self
    }

    fn providing(mut self, tag: &'static str) -> Self {
        self.provide_tag = Some(tag);
        self
    }

    fn observing(mut self, upstream: &'static str, slot: Arc<Mutex<Option<String>>>) -> Self {
        self.lookup_upstream = Some(upstream);
        self.observed_service_in_load = slot;
        self
    }

    fn loaded(self) -> LoadedPlugin {
        LoadedPlugin {
            plugin: Box::new(self),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for TestPlugin {
    fn name(&self) -> &str {
        self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "插件角色、依赖拓扑与通用服务总线测试桩"
    }

    fn plugin_role(&self) -> PluginRole {
        self.role
    }

    fn required_dependencies(&self) -> Vec<&'static str> {
        self.required.clone()
    }

    fn optional_dependencies(&self) -> Vec<&'static str> {
        self.optional.clone()
    }

    async fn on_load(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        self.load_order
            .lock()
            .expect("锁已中毒")
            .push(self.name.to_owned());
        if let Some(tag) = self.provide_tag {
            ctx.register_service(Arc::new(UpstreamDemoService(tag)));
        }
        if let Some(upstream) = self.lookup_upstream {
            let found = ctx
                .call_service(upstream, "ping", json!({}))
                .await
                .ok()
                .and_then(|val| val["tag"].as_str().map(str::to_owned));
            *self.observed_service_in_load.lock().expect("锁已中毒") = found;
        }
        Ok(())
    }
}

struct TempRoot(std::path::PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn test_host() -> (Arc<PluginHost>, Database, TempRoot) {
    let db = crate::test_support::test_db(2).await;
    let root = std::env::temp_dir().join(format!("nuono-service-deps-{}", uuid::Uuid::new_v4()));
    let paths = crate::core::paths::Paths::with_root(&root);
    std::fs::create_dir_all(&paths.data).expect("创建数据目录失败");
    let event_bus = Arc::new(EventBus::new());
    let permission_manager = Arc::new(PermissionManager::new(db.clone()));
    let wait_manager = Arc::new(WaitManager::new());
    let framework_control = FrameworkControl::new();
    let config_manager = Arc::new(crate::config::PluginConfigManager::with_database(
        paths.data.clone(),
        db.clone(),
    ));
    let host = PluginHost::new(
        db.clone(),
        event_bus,
        permission_manager,
        paths,
        wait_manager,
        framework_control,
        config_manager,
    )
    .await
    .expect("构造测试宿主失败");
    (host, db, TempRoot(root))
}

#[tokio::test]
async fn topological_order_service_eviction_and_dependency_gating() {
    let (host, db, _dir) = test_host().await;
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_opt = Arc::new(Mutex::new(None::<String>));
    let observed_req = Arc::new(Mutex::new(None::<String>));

    // 故意把下游放在上游前面，验证宿主按依赖拓扑自动重排，使上游先完成 on_load 并注册通用服务处理器。
    let loaded = vec![
        TestPlugin::new("feature_opt", order.clone())
            .with_optional(vec!["auth_svc"])
            .observing("auth_svc", observed_opt.clone())
            .loaded(),
        TestPlugin::new("feature_req", order.clone())
            .with_required(vec!["auth_svc"])
            .observing("auth_svc", observed_req.clone())
            .loaded(),
        TestPlugin::new("cycle_a", order.clone())
            .with_required(vec!["cycle_b"])
            .loaded(),
        TestPlugin::new("cycle_b", order.clone())
            .with_required(vec!["cycle_a"])
            .loaded(),
        TestPlugin::new("auth_svc", order.clone())
            .with_role(PluginRole::Service)
            .providing("svc-v1")
            .loaded(),
    ];

    host.init_with_plugins(loaded)
        .await
        .expect("批量加载插件失败");

    // 上游必须最先加载，且两个下游加载时均能立即通过 call_service 调用上游；循环依赖插件不得进入活跃列表。
    let load_order = order.lock().expect("锁已中毒").clone();
    assert_eq!(load_order, vec!["auth_svc", "feature_opt", "feature_req"]);
    assert_eq!(
        observed_opt.lock().expect("锁已中毒").as_deref(),
        Some("svc-v1")
    );
    assert_eq!(
        observed_req.lock().expect("锁已中毒").as_deref(),
        Some("svc-v1")
    );
    assert!(
        !host
            .all_plugins()
            .await
            .iter()
            .any(|name| name.starts_with("cycle_"))
    );

    // 验证通用服务总线能明确区分 Unauthorized、RateLimited 与普通 Upstream 错误。
    let unauth = host
        .service_registry()
        .call("auth_svc", "expired", json!({}))
        .await
        .expect_err("应返回凭据失效错误");
    assert!(unauth.is_unauthorized());
    assert!(!unauth.is_rate_limited());
    let limited = host
        .service_registry()
        .call("auth_svc", "rate_limited", json!({}))
        .await
        .expect_err("应返回限流错误");
    assert!(limited.is_rate_limited());
    assert!(!limited.is_unauthorized());
    let upstream = host
        .service_registry()
        .call("auth_svc", "upstream", json!({}))
        .await
        .expect_err("应返回上游网络错误");
    assert!(!upstream.is_unauthorized());
    assert!(!upstream.is_rate_limited());

    // 确认 service 角色已落盘且群级开关门控直接放行（仅受全局开关控制）。
    assert_eq!(host.plugin_role("auth_svc").await, PluginRole::Service);
    assert_eq!(
        db.plugin_policy("auth_svc")
            .await
            .expect("查询策略失败")
            .plugin_role,
        "service"
    );
    assert!(
        host.group_enablement_allows("auth_svc", Some("123456"))
            .await
    );

    // 存在活跃的 required 下游时，禁止停用上游插件且服务处理器保持可用。
    let blocked = host
        .toggle("auth_svc", false)
        .await
        .expect_err("存在启用中的强依赖下游时应拦截停用上游");
    assert!(matches!(blocked, crate::error::NuonoError::Conflict(_)));
    assert!(
        host.service_registry().has_service("auth_svc"),
        "停用被拦截后服务处理器必须继续保留"
    );

    // 先停用 required 下游，再停用上游：停用应成功、服务处理器立即摘除、optional 下游不受阻断。
    host.toggle("feature_req", false)
        .await
        .expect("停用强依赖下游失败");
    host.toggle("auth_svc", false)
        .await
        .expect("仅剩 optional 下游时停用上游不应受阻断");
    assert!(
        !host.service_registry().has_service("auth_svc"),
        "上游停用后宿主必须立即摘除其服务处理器"
    );
    assert!(
        host.service_registry()
            .call("auth_svc", "ping", json!({}))
            .await
            .expect_err("停用后调用必须返回不可用")
            .is_unavailable()
    );
    assert!(
        host.get_plugin_config("feature_opt")
            .await
            .is_some_and(|cfg| cfg.enabled),
        "optional 下游应保持正常运行"
    );

    // 上游停用期间，尝试启用 required 下游应被拦截；optional 下游重启用不受阻断。
    let req_err = host
        .toggle("feature_req", true)
        .await
        .expect_err("上游未启用时启用强依赖下游必须报错");
    assert!(matches!(req_err, crate::error::NuonoError::Conflict(_)));
    host.toggle("feature_opt", false)
        .await
        .expect("停用可选下游失败");
    host.toggle("feature_opt", true)
        .await
        .expect("上游停用时仍应允许重新启用可选下游");

    // 重新启用上游后服务处理器恢复，随后可重新启用 required 下游。
    host.toggle("auth_svc", true)
        .await
        .expect("重新启用上游失败");
    let ping = host
        .service_registry()
        .call("auth_svc", "ping", json!({}))
        .await
        .expect("上游恢复后应能调用服务");
    assert_eq!(ping["tag"].as_str(), Some("svc-v1"));
    host.toggle("feature_req", true)
        .await
        .expect("上游恢复后应允许启用强依赖下游");

    host.shutdown().await.expect("宿主关闭失败");
    assert!(
        !host.service_registry().has_service("auth_svc"),
        "宿主关闭后必须清空全部服务处理器"
    );
    db.shutdown().await.expect("数据库关闭失败");
}

#[tokio::test]
async fn service_plugin_blocks_non_owner_and_group_or_user_overrides() {
    let (host, db, _dir) = test_host().await;
    let order = Arc::new(Mutex::new(Vec::<String>::new()));

    let owner = db
        .create_user(nuono_storage::db::NewUser {
            username: "svc-owner".into(),
            email: None,
            password: "strong-password-123".into(),
            role: "owner".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .expect("创建 owner 失败");
    let admin = db
        .create_user(nuono_storage::db::NewUser {
            username: "svc-admin".into(),
            email: None,
            password: "strong-password-123".into(),
            role: "admin".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .expect("创建 admin 失败");
    let group = db
        .ensure_group(
            &nuono_storage::groups::GroupIdentity::onebot("999001"),
            "service-filter-group",
        )
        .await
        .expect("创建测试群失败");

    host.init_with_plugins(vec![
        TestPlugin::new("auth_svc", order.clone())
            .with_role(PluginRole::Service)
            .loaded(),
        TestPlugin::new("feature_main", order.clone())
            .with_role(PluginRole::Feature)
            .loaded(),
    ])
    .await
    .expect("加载插件失败");

    let manager = host
        .config_manager
        .read()
        .await
        .as_ref()
        .cloned()
        .expect("配置管理器缺失");
    let owner_actor = nuono_app::configuration::ConfigurationActor {
        user_id: owner.id,
        mode: nuono_app::configuration::ConfigurationMode::Administrator,
    };

    // 群管理插件列表必须彻底隐藏 service 插件，仅返回 feature 插件。
    let group_plugins =
        crate::service::group_plugins::list_for_group(&db, &host, &manager, group.id, &owner_actor)
            .await
            .expect("列出群插件失败");
    let group_names: Vec<&str> = group_plugins
        .iter()
        .map(|item| item.plugin_name.as_str())
        .collect();
    assert_eq!(group_names, vec!["feature_main"]);

    // 群覆盖保存与群授权委派均拒绝 service 插件。
    let override_err = crate::service::group_plugins::save_override(
        &db,
        &host,
        &manager,
        group.id,
        "auth_svc",
        &owner_actor,
        nuono_storage::plugin_overrides::OverridePatch {
            config_version: 1,
            set_enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect_err("不得允许对 service 插件写入群级覆盖");
    assert!(matches!(
        override_err,
        crate::error::NuonoError::PermissionDenied(_)
    ));

    let grant_err = db
        .grant_group_configuration(
            owner.id,
            &nuono_storage::group_authorizations::ManualGroupGrant {
                user_id: admin.id,
                group_id: group.id,
                plugin_id: Some("auth_svc".into()),
                source_key: "svc-grant".into(),
                valid_from_ms: 0,
                expires_at_ms: None,
            },
        )
        .await
        .expect_err("不得允许对 service 插件签发群级配置授权");
    assert!(matches!(
        grant_err,
        crate::error::NuonoError::PermissionDenied(_)
    ));

    // admin 不能操作 service 插件策略；owner 也不能给 service 插件开启个人/群覆盖，但可以启停。
    let policy = db.plugin_policy("auth_svc").await.expect("读取策略失败");
    let admin_err = host
        .save_policy_and_apply(
            admin.id,
            "auth_svc",
            nuono_storage::plugin_global_settings::PluginPolicyPatch {
                expected_revision: policy.revision,
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect_err("非 owner 管理员不得启停 service 插件");
    assert!(matches!(
        admin_err,
        crate::error::NuonoError::PermissionDenied(_)
    ));

    let bad_override_flag = host
        .save_policy_and_apply(
            owner.id,
            "auth_svc",
            nuono_storage::plugin_global_settings::PluginPolicyPatch {
                expected_revision: policy.revision,
                allow_group_overrides: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect_err("不得允许给 service 插件开放群级覆盖开关");
    assert!(matches!(
        bad_override_flag,
        crate::error::NuonoError::Validation(_)
    ));

    host.save_policy_and_apply(
        owner.id,
        "auth_svc",
        nuono_storage::plugin_global_settings::PluginPolicyPatch {
            expected_revision: policy.revision,
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("owner 应能停用 service 插件");
    assert!(
        !db.is_plugin_enabled("auth_svc")
            .await
            .expect("查询启用态失败")
    );

    host.shutdown().await.expect("宿主关闭失败");
    db.shutdown().await.expect("数据库关闭失败");
}
