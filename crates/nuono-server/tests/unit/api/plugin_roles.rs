use super::*;
use nuono_core::plugin::{
    LoadedPlugin, Plugin, PluginRole, PluginServiceError, PluginServiceHandler, PluginServiceResult,
};
use serde_json::{Value, json};

struct DummyUpstreamHandler;

#[async_trait::async_trait]
impl PluginServiceHandler for DummyUpstreamHandler {
    async fn handle(&self, action: &str, _payload: Value) -> PluginServiceResult<Value> {
        match action {
            "ping" => Ok(json!({ "id": "service-probe" })),
            other => Err(PluginServiceError::action_not_found("service_probe", other)),
        }
    }
}

struct RoleFixturePlugin {
    name: &'static str,
    role: PluginRole,
    optional: Vec<&'static str>,
}

#[async_trait::async_trait]
impl Plugin for RoleFixturePlugin {
    fn name(&self) -> &str {
        self.name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "插件角色 HTTP 接口验收测试桩"
    }

    fn plugin_role(&self) -> PluginRole {
        self.role
    }

    fn optional_dependencies(&self) -> Vec<&'static str> {
        self.optional.clone()
    }

    async fn on_load(&mut self, ctx: &nuono_core::plugin::PluginContext) -> anyhow::Result<()> {
        if self.role.is_service() {
            ctx.register_service(Arc::new(DummyUpstreamHandler));
        }
        Ok(())
    }
}

#[tokio::test]
async fn service_plugins_are_hidden_from_group_personal_and_non_owner_apis() {
    let f = Fixture::new(true).await;
    let owner = f.session("role-owner", "owner").await;
    let admin = f.session("role-admin", "admin").await;
    let user = f.session("role-user", "user").await;

    f.state
        .manager
        .plugin_host
        .init_with_plugins(vec![
            LoadedPlugin {
                plugin: Box::new(RoleFixturePlugin {
                    name: "feature_probe",
                    role: PluginRole::Feature,
                    optional: vec!["service_probe"],
                }),
            },
            LoadedPlugin {
                plugin: Box::new(RoleFixturePlugin {
                    name: "service_probe",
                    role: PluginRole::Service,
                    optional: vec![],
                }),
            },
        ])
        .await
        .expect("注入测试插件失败");

    let ping = f
        .state
        .manager
        .plugin_host
        .service_registry()
        .call("service_probe", "ping", json!({}))
        .await
        .expect("已加载 service 插件应能响应通用服务调用");
    assert_eq!(ping["id"].as_str(), Some("service-probe"));

    let gid = f
        .state
        .db
        .ensure_group(
            &nuono_core::db::groups::GroupIdentity::onebot("556677"),
            "role-test-group",
        )
        .await
        .expect("创建测试群失败")
        .id;

    // 1. 群管理插件列表不得暴露 service 插件，直接写群覆盖和签发群授权也必须返回 403。
    let (status, group_list) = f
        .authenticated(&format!("/api/groups/{gid}/plugins"), &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{group_list}");
    let group_names: Vec<&str> = group_list["data"]
        .as_array()
        .expect("群插件列表应为数组")
        .iter()
        .filter_map(|item| item["plugin_name"].as_str())
        .collect();
    assert!(group_names.contains(&"feature_probe"));
    assert!(!group_names.contains(&"service_probe"));

    let (status, _) = f
        .authenticated_json(
            "POST",
            &format!("/api/groups/{gid}/plugins/service_probe/config"),
            &owner,
            json!({"expected_revision":"0","config_version":"1","set_enabled":false}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let user_id = f
        .state
        .db
        .find_user_by_username("role-user")
        .await
        .unwrap()
        .unwrap()
        .id;
    let (status, _) = f
        .authenticated_json(
            "POST",
            &format!("/api/groups/{gid}/authorizations"),
            &owner,
            json!({
                "user_id": user_id.to_string(),
                "plugin_id": "service_probe",
                "source_key": "forbid-service-grant",
                "valid_from_ms": "0",
                "expires_at_ms": null
            }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 2. 个人插件配置列表与保存均不得暴露或允许修改 service 插件。
    let (status, personal_list) = f.authenticated("/api/plugins/personal", &user).await;
    assert_eq!(status, StatusCode::OK, "{personal_list}");
    let personal_names: Vec<&str> = personal_list["data"]
        .as_array()
        .expect("个人插件列表应为数组")
        .iter()
        .filter_map(|item| item["plugin_id"].as_str())
        .collect();
    assert!(personal_names.contains(&"feature_probe"));
    assert!(!personal_names.contains(&"service_probe"));

    let (status, _) = f
        .authenticated_json(
            "POST",
            "/api/plugins/personal/service_probe",
            &user,
            json!({"expected_revision":"0","config_version":"1","set_enabled":false}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 3. 全局插件管理接口：admin 看不到且无权查看/启停 service 插件，仅 owner 可见并启停。
    let (status, admin_list) = f.authenticated("/api/plugins", &admin).await;
    assert_eq!(status, StatusCode::OK, "{admin_list}");
    let admin_names: Vec<&str> = admin_list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(admin_names.contains(&"feature_probe"));
    assert!(!admin_names.contains(&"service_probe"));

    let (status, _) = f.authenticated("/api/plugins/service_probe", &admin).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f
        .authenticated("/api/plugins/service_probe/policy", &admin)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, owner_list) = f.authenticated("/api/plugins", &owner).await;
    assert_eq!(status, StatusCode::OK, "{owner_list}");
    let service_entry = owner_list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"].as_str() == Some("service_probe"))
        .expect("owner 应能看到 service 插件");
    assert_eq!(service_entry["plugin_role"], "service");

    let (status, policy_resp) = f
        .authenticated("/api/plugins/service_probe/policy", &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{policy_resp}");
    assert_eq!(policy_resp["data"]["plugin_role"], "service");
    let revision = policy_resp["data"]["revision"].as_str().unwrap().to_owned();

    // admin 尝试切换 service 插件应被拦截。
    let (status, _) = f
        .authenticated_json(
            "POST",
            "/api/plugins/service_probe/toggle",
            &admin,
            json!({"enabled": false, "expected_revision": revision}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // owner 尝试停用 service 插件应成功、立即摘除服务处理器，且不阻断 optional 下游 feature_probe。
    let (status, toggle_resp) = f
        .authenticated_json(
            "POST",
            "/api/plugins/service_probe/toggle",
            &owner,
            json!({"enabled": false, "expected_revision": revision}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{toggle_resp}");
    assert!(
        !f.state
            .manager
            .plugin_host
            .service_registry()
            .has_service("service_probe")
    );
    assert!(
        f.state
            .manager
            .plugin_host
            .service_registry()
            .call("service_probe", "ping", json!({}))
            .await
            .expect_err("停用后调用必须返回服务不可用")
            .is_unavailable()
    );
    assert!(
        f.state
            .manager
            .plugin_host
            .get_plugin_config("feature_probe")
            .await
            .is_some_and(|cfg| cfg.enabled)
    );

    f.close().await;
}
