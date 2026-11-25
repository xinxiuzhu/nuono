use super::*;

struct BrandedPlugin {
    name: String,
    display_name: &'static str,
    fail_on_load: bool,
    invalid_declaration: bool,
}

#[async_trait::async_trait]
impl Plugin for BrandedPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        self.display_name
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn description(&self) -> &str {
        "Display name test plugin"
    }

    fn config_fields(&self) -> Vec<crate::core::PluginConfigField> {
        assert!(!self.invalid_declaration, "invalid declaration");
        vec![]
    }

    async fn on_load(&mut self, _ctx: &crate::plugin::PluginContext) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fail_on_load, "load failed");
        Ok(())
    }
}

#[test]
fn plugin_declaration_display_name_defaults_and_whitespace_fallback_preserve_identity() {
    let legacy = LoadedPlugin {
        plugin: Box::new(DeclarationCandidatePlugin::new("stable-id".into())),
    };
    assert_eq!(legacy.display_name(), "stable-id");
    let legacy = super::super::loading::collect_declaration_sync(&legacy).unwrap();
    assert_eq!(legacy.display_name, legacy.name);

    for (display_name, expected) in [(" \t\n", "stable-id"), (" DemoPlugin ", "DemoPlugin")] {
        let plugin = LoadedPlugin {
            plugin: Box::new(BrandedPlugin {
                name: "stable-id".into(),
                display_name,
                fail_on_load: false,
                invalid_declaration: false,
            }),
        };
        let declaration = super::super::loading::collect_declaration_sync(&plugin).unwrap();
        assert_eq!(declaration.name, "stable-id");
        assert_eq!(declaration.service_config.name, "stable-id");
        assert_eq!(declaration.display_name, expected);
    }
}

#[tokio::test]
async fn display_name_survives_disabled_failed_and_rejected_plugins_without_changing_ids() {
    let (host, db, suffix) = host_for_activation_test().await;
    let disabled = format!("Disabled{suffix}");
    let failed = format!("Failed{suffix}");
    let rejected = format!("Rejected{suffix}");
    db.sync_plugin(&disabled, "old", &serde_json::json!({}))
        .await
        .unwrap();
    db.set_plugin_enabled(&disabled, false).await.unwrap();
    let plugins = [
        (disabled.clone(), false, false),
        (failed.clone(), true, false),
        (rejected.clone(), false, true),
    ]
    .into_iter()
    .map(|(name, fail_on_load, invalid_declaration)| LoadedPlugin {
        plugin: Box::new(BrandedPlugin {
            name,
            // 展示名称相同绝不能导致两个不同插件的身份被合并。
            display_name: " DemoPlugin ",
            fail_on_load,
            invalid_declaration,
        }),
    })
    .collect();
    host.init_with_plugins(plugins).await.unwrap();

    let infos = host.installed_plugin_infos().await;
    assert_eq!(infos.len(), 3);
    for name in [&disabled, &failed, &rejected] {
        assert_eq!(infos[name].display_name, "DemoPlugin");
        assert!(!infos[name].enabled);
        assert!(host.is_plugin_installed(name).await);
    }
    assert!(!host.is_plugin_installed("DemoPlugin").await);
    assert!(host.get_plugin_config("DemoPlugin").await.is_none());
    assert_eq!(host.all_plugins().await, vec![disabled.clone()]);
    let installed = crate::service::plugins::get_installed_plugins(&db, &host)
        .await
        .unwrap();
    assert_eq!(installed.len(), 3);
    assert!(
        installed
            .iter()
            .all(|plugin| plugin.display_name == "DemoPlugin")
    );
    assert!(installed.iter().all(|plugin| plugin.name != "DemoPlugin"));
    assert!(
        installed
            .iter()
            .find(|plugin| plugin.name == rejected)
            .unwrap()
            .id
            .is_none()
    );
    host.toggle(&disabled, true).await.unwrap();
    assert!(host.get_plugin_info(&disabled).await.enabled);
    assert_eq!(
        host.get_plugin_info(&disabled).await.display_name,
        "DemoPlugin"
    );
    assert!(matches!(
        host.toggle("DemoPlugin", true).await,
        Err(crate::error::NuonoError::PluginNotFound(_))
    ));

    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_program_ignores_database_history_and_rejects_old_name_toggle() {
    let (host, db, old_name) = host_for_activation_test().await;
    db.sync_plugin(&old_name, "old-version", &serde_json::json!({}))
        .await
        .unwrap();
    host.init_with_plugins(vec![]).await.unwrap();

    assert!(host.installed_plugin_infos().await.is_empty());
    assert!(!host.is_plugin_installed(&old_name).await);
    assert!(host.get_plugin_info(&old_name).await.version.is_empty());
    for enabled in [false, true] {
        assert!(matches!(
            host.toggle(&old_name, enabled).await,
            Err(crate::error::NuonoError::PluginNotFound(_))
        ));
    }
    assert!(db.is_plugin_enabled(&old_name).await.unwrap());
    assert_eq!(
        crate::service::plugins::get_plugins(&db)
            .await
            .unwrap()
            .len(),
        1
    );
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn installed_directory_keeps_disabled_and_initial_load_failure() {
    let (host, db, suffix) = host_for_activation_test().await;
    let disabled = format!("Disabled{suffix}");
    let failed = format!("Failed{suffix}");
    db.sync_plugin(&disabled, "previous", &serde_json::json!({}))
        .await
        .unwrap();
    db.set_plugin_enabled(&disabled, false).await.unwrap();
    host.init_with_plugins(vec![
        LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin::new(disabled.clone())),
        },
        LoadedPlugin {
            plugin: Box::new(ActivationFailPlugin {
                name: failed.clone(),
                fail_on_load: true,
                panic_on_load: false,
                fail_on_enable: false,
                panic_on_enable: false,
                secret_fields: vec![],
            }),
        },
    ])
    .await
    .unwrap();

    let infos = host.installed_plugin_infos().await;
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[&disabled].version, "2.0.0");
    assert_eq!(infos[&disabled].display_name, disabled);
    assert_eq!(infos[&failed].version, "1.0.0");
    assert_eq!(infos[&failed].description, "激活失败测试插件");
    assert_eq!(infos[&failed].display_name, failed);
    assert!(!infos[&disabled].enabled);
    assert!(!infos[&failed].enabled);
    assert_eq!(host.all_plugins().await, vec![disabled.clone()]);
    assert!(host.is_plugin_installed(&failed).await);
    assert_eq!(host.get_plugin_info(&failed).await.version, "1.0.0");
    assert!(!db.is_plugin_enabled(&failed).await.unwrap());

    host.toggle(&disabled, true).await.unwrap();
    assert!(host.get_plugin_info(&disabled).await.enabled);
    host.toggle(&disabled, false).await.unwrap();
    assert!(host.is_plugin_installed(&disabled).await);
    assert!(!host.get_plugin_info(&disabled).await.enabled);
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_first_install_stays_visible_without_publishing_schema_or_database_row() {
    let (host, db, name) = host_for_activation_test().await;
    let plan = host
        .resolve_load_candidates(vec![LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin {
                panic_config_fields: true,
                ..DeclarationCandidatePlugin::new(name.clone())
            }),
        }])
        .await
        .unwrap();
    assert_eq!(plan.rejected.len(), 1);
    assert!(host.installed_plugin_infos().await.is_empty());
    assert_eq!(host.install_load_plan(plan).await.unwrap(), 0);

    let infos = host.installed_plugin_infos().await;
    assert_eq!(infos.len(), 1);
    assert!(infos[&name].version.is_empty());
    assert!(infos[&name].description.contains("声明无效"));
    assert!(!infos[&name].enabled);
    assert_eq!(infos[&name].config_schema, serde_json::json!({}));
    assert!(host.is_plugin_installed(&name).await);
    assert!(host.all_plugins().await.is_empty());
    assert!(host.get_plugin_config(&name).await.is_none());
    assert!(host.plugin_secret_schemas().await.is_empty());
    assert!(
        crate::service::plugins::get_plugins(&db)
            .await
            .unwrap()
            .is_empty()
    );
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn directly_injected_instance_remains_known_after_enable_failure() {
    let (host, db, name) = host_for_activation_test().await;
    host.plugins.write().await.push(
        LoadedPlugin {
            plugin: Box::new(ActivationFailPlugin {
                name: name.clone(),
                fail_on_load: true,
                panic_on_load: false,
                fail_on_enable: false,
                panic_on_enable: false,
                secret_fields: vec![],
            }),
        }
        .into(),
    );
    db.sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    assert!(host.is_plugin_installed(&name).await);
    assert_eq!(host.installed_plugin_infos().await[&name].version, "1.0.0");

    assert!(host.toggle(&name, true).await.is_err());
    assert!(host.all_plugins().await.is_empty());
    assert!(host.is_plugin_installed(&name).await);
    assert!(!host.get_plugin_info(&name).await.enabled);
    assert_eq!(
        host.get_plugin_info(&name).await.description,
        "激活失败测试插件"
    );
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}
