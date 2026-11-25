use super::*;
use chrono::Utc;
use nuono_app::{auth::CapabilityProvider, entitlement::ServiceEntitlement};

#[derive(Clone)]
struct ServicePolicyPlugin {
    name: String,
    fail_load: bool,
    fail_disable: bool,
}
struct Policy;
#[async_trait::async_trait]
impl ServiceEntitlement for Policy {
    async fn check_group(
        &self,
        _query: &nuono_app::entitlement::GroupEntitlementQuery,
    ) -> nuono_error::Result<nuono_app::entitlement::EntitlementDecision> {
        Ok(nuono_app::entitlement::EntitlementDecision::denied())
    }
    fn requires_assistant_grant(&self) -> bool {
        true
    }
}
impl CapabilityProvider for Policy {
    fn granted(&self, _: crate::auth::Role) -> Vec<&'static str> {
        vec!["view_policy_fixture"]
    }
}
impl ServicePolicyPlugin {
    fn loaded(&self) -> LoadedPlugin {
        LoadedPlugin {
            plugin: Box::new(self.clone()),
        }
    }
}
#[async_trait::async_trait]
impl Plugin for ServicePolicyPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "managed service policy fixture"
    }
    fn extensions(&self) -> nuono_plugin::PluginExtensions {
        nuono_plugin::PluginExtensions::new()
            .with_service_entitlement(Arc::new(Policy))
            .with_capabilities(Arc::new(Policy))
    }
    fn web_pages(&self) -> Vec<nuono_plugin::PluginWebPage> {
        vec![
            nuono_plugin::PluginWebPage::new(
                "account",
                "Service account",
                vec![nuono_plugin::PluginWebAsset::new(
                    "index.html",
                    "text/html",
                    b"<html></html>",
                )],
            )
            .with_access(nuono_plugin::PluginWebAccessPolicy::Authenticated),
        ]
    }
    async fn on_load(&mut self, _: &PluginContext) -> anyhow::Result<()> {
        assert!(
            !nuono_app::entitlement::is_group_active(7).await,
            "activation is closed before callbacks"
        );
        anyhow::ensure!(!self.fail_load, "policy fixture load failed");
        Ok(())
    }
    async fn on_disable(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fail_disable, "policy fixture cleanup failed");
        Ok(())
    }
}

/// 使用独立测试进程验证全局部署级策略钩子，避免影响其他并发运行的宿主测试。
#[tokio::test]
async fn managed_service_policy_host_lifecycle() {
    if std::env::var_os("NUONO_POLICY_TEST_CHILD").is_none() {
        let path = concat!(module_path!(), "::managed_service_policy_host_lifecycle");
        let test = path.split_once("::").unwrap().1;
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("NUONO_POLICY_TEST_CHILD", "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    use nuono_app::auth::{Role, extension_capabilities};
    let (host, db, name) = host_for_activation_test().await;
    let settings = nuono_app::assistant::AssistantSettings {
        enabled: true,
        ..Default::default()
    };
    assert!(db.is_group_enabled("no-private-plugin").await.unwrap());
    assert_eq!(
        settings.effective_limit(None, Utc::now()),
        settings.daily_limit
    );
    assert!(host.plugin_web_pages(Role::User).await.is_empty());
    let plugin = ServicePolicyPlugin {
        name: name.clone(),
        fail_load: false,
        fail_disable: false,
    };
    host.init_with_plugins(vec![plugin.loaded()]).await.unwrap();
    assert!(!db.is_group_enabled("no-private-plugin").await.unwrap());
    assert_eq!(settings.effective_limit(None, Utc::now()), 0);
    assert_eq!(host.plugin_web_pages(Role::User).await.len(), 1);
    assert!(extension_capabilities(Role::User).contains(&"view_policy_fixture"));
    host.toggle(&name, false).await.unwrap();
    assert!(db.is_group_enabled("no-private-plugin").await.unwrap());
    assert_eq!(
        settings.effective_limit(None, Utc::now()),
        settings.daily_limit
    );
    assert!(host.plugin_web_pages(Role::User).await.is_empty());
    assert!(!extension_capabilities(Role::User).contains(&"view_policy_fixture"));
    host.toggle(&name, true).await.unwrap();
    host.reload_with_candidate(&name, plugin.loaded(), true)
        .await
        .unwrap();
    let failed = ServicePolicyPlugin {
        fail_load: true,
        ..plugin.clone()
    };
    assert!(
        host.reload_with_candidate(&name, failed.loaded(), true)
            .await
            .is_err()
    );
    assert!(
        db.is_plugin_enabled(&name).await.unwrap(),
        "failure retains persisted enabled intent"
    );
    let grant = Some((Utc::now() + chrono::Duration::days(1), 20));
    assert_eq!(
        settings.effective_limit(grant, Utc::now()),
        0,
        "even valid grants wait for policy recovery"
    );
    assert!(!db.is_group_enabled("no-private-plugin").await.unwrap());
    assert!(host.plugin_web_pages(Role::User).await.is_empty());
    assert!(!extension_capabilities(Role::User).contains(&"view_policy_fixture"));
    // 即使激活失败已移除插件实例，显式执行停用操作也依然能正常工作。
    host.toggle(&name, false).await.unwrap();
    assert!(!db.is_plugin_enabled(&name).await.unwrap());
    assert!(db.is_group_enabled("no-private-plugin").await.unwrap());
    let startup = ServicePolicyPlugin {
        name: format!("{name}Startup"),
        fail_load: true,
        fail_disable: false,
    };
    host.init_with_plugins(vec![startup.loaded()])
        .await
        .unwrap();
    assert!(db.is_plugin_enabled(&startup.name).await.unwrap());
    assert!(!nuono_app::plugin_extensions::assistant_available());
    host.toggle(&startup.name, false).await.unwrap();
    let cleanup = ServicePolicyPlugin {
        name: format!("{name}Cleanup"),
        fail_disable: true,
        ..plugin
    };
    host.init_with_plugins(vec![cleanup.loaded()])
        .await
        .unwrap();
    assert!(host.toggle(&cleanup.name, false).await.is_err());
    assert!(!db.is_plugin_enabled(&cleanup.name).await.unwrap());
    assert!(
        db.is_group_enabled("no-private-plugin").await.unwrap(),
        "callback failure cannot leave a ghost policy after explicit disable"
    );
    assert!(host.plugin_web_pages(Role::User).await.is_empty());
    assert_eq!(
        settings.effective_limit(None, Utc::now()),
        settings.daily_limit
    );
    let _ = host.shutdown().await;
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_phase_factory_plugin_mounts_entitlement_and_web_router_cleanly() {
    if std::env::var_os("NUONO_FACTORY_POLICY_TEST_CHILD").is_none() {
        let path = concat!(
            module_path!(),
            "::two_phase_factory_plugin_mounts_entitlement_and_web_router_cleanly"
        );
        let test = path.split_once("::").unwrap().1;
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env("NUONO_FACTORY_POLICY_TEST_CHILD", "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }

    use nuono_plugin::{
        FactoryPlugin, MountedPlugin, PluginDescriptor, PluginFactory, PluginWebAccessPolicy,
        PluginWebRouter,
        core_seams::{EntitlementDecision, GroupEntitlementQuery},
        web::{ApiJson, get},
    };

    struct LiveEntitlement;
    #[async_trait::async_trait]
    impl ServiceEntitlement for LiveEntitlement {
        async fn check_group(
            &self,
            query: &GroupEntitlementQuery,
        ) -> nuono_error::Result<EntitlementDecision> {
            if query.group_id() == 42 {
                Ok(EntitlementDecision::Allowed)
            } else {
                Ok(EntitlementDecision::denied_with_reason("not subscribed"))
            }
        }
    }

    struct DemoFactory {
        name: String,
    }
    #[async_trait::async_trait]
    impl PluginFactory for DemoFactory {
        fn describe(&self) -> PluginDescriptor {
            PluginDescriptor::new(&self.name, "0.1.0", "two-phase factory test")
                .protects_services(true)
        }

        async fn mount(&self, ctx: PluginContext) -> anyhow::Result<MountedPlugin> {
            // 确认在 mount(ctx) 尚未执行完毕期间，权限校验默认采取拒绝策略（fail-closed）。
            assert!(!nuono_app::entitlement::is_group_active(42).await);
            let _ = ctx.db().backend();
            let router = PluginWebRouter::new()
                .route(
                    "/ping",
                    get(|| async { ApiJson(serde_json::json!({"pong": true})) })
                        .access(PluginWebAccessPolicy::Public),
                )
                .into_erased();
            Ok(MountedPlugin::new()
                .with_service_entitlement(Arc::new(LiveEntitlement))
                .with_web_router(router))
        }
    }

    // 开源版默认行为：未加载任何策略插件时，所有群组默认放行。
    assert!(nuono_app::entitlement::is_group_active(999).await);
    assert!(!nuono_app::entitlement::requires_assistant_grant());

    let (host, db, name) = host_for_activation_test().await;
    let loaded = LoadedPlugin {
        plugin: Box::new(FactoryPlugin::new(DemoFactory { name: name.clone() })),
    };
    host.init_with_plugins(vec![loaded]).await.unwrap();

    // 完成挂载与激活后，实时权限策略生效：群 42 放行，群 999 拒绝。
    assert!(nuono_app::entitlement::is_group_active(42).await);
    assert!(!nuono_app::entitlement::is_group_active(999).await);

    // 已挂载的 PluginWebRouter 路由权限策略与请求派发端到端生效。
    assert_eq!(
        host.plugin_web_policy(&name, &http::Method::GET, "/ping")
            .await,
        PluginWebAccessPolicy::Public
    );
    let resp = host
        .handle_web_request(
            &name,
            &crate::plugin::PluginWebRequest::new(http::Method::GET, "/ping"),
        )
        .await;
    let crate::plugin::PluginWebResponse::Json(body) = resp else {
        panic!("expected json response from mounted PluginWebRouter");
    };
    assert_eq!(body["data"]["pong"], true);

    let _ = host.shutdown().await;
    drop(host);
    db.shutdown().await.unwrap();
}
