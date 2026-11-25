use super::*;

#[tokio::test]
async fn service_patch_preserves_omitted_policy_and_supports_explicit_clears() {
    use nuono_core::config::{ConfigSchema, PluginConfigManager};
    use nuono_core::core::plugin_service::Plugins;

    let root = std::env::temp_dir().join(format!("nuono-service-patch-{}", uuid::Uuid::new_v4()));
    let manager = PluginConfigManager::new(root.clone());
    let mut policy = Plugins::new("PatchProbe")
        .with_prefix(vec!["old".into()])
        .with_auto_listen_enabled(false);
    policy.min_permission = PermissionLevel::BotOwner;
    policy.area = PluginArea::Direct;
    policy.white_list = vec!["trusted".into()];
    policy.black_list = vec!["blocked".into()];
    policy.force_prefix = vec!["extra".into()];
    policy.disable_force_prefix = true;
    manager
        .register_with_service_config("PatchProbe", ConfigSchema::new(), Some(&policy))
        .await
        .unwrap();
    let before = manager.read_latest("PatchProbe").await.unwrap();
    let patch: PluginServicePatch =
        serde_json::from_value(serde_json::json!({"prefix": ["parse"]})).unwrap();
    manager
        .set_values_if_revision("PatchProbe", patch.into_values(), Some(&before.revision))
        .await
        .unwrap();
    let saved = manager.read_latest("PatchProbe").await.unwrap().values;
    assert_eq!(saved["prefix"], serde_json::json!(["parse"]));
    assert_eq!(saved["min_permission"], "BotOwner");
    assert_eq!(saved["area"], "DIRECT");
    assert_eq!(saved["white_list"], serde_json::json!(["trusted"]));
    assert_eq!(saved["black_list"], serde_json::json!(["blocked"]));
    assert_eq!(saved["force_prefix"], serde_json::json!(["extra"]));
    assert_eq!(saved["disable_force_prefix"], true);
    assert_eq!(saved["auto_listen_enabled"], false);

    let patch: PluginServicePatch = serde_json::from_value(serde_json::json!({
        "white_list": [], "disable_force_prefix": false, "allow_empty_prefix": true
    }))
    .unwrap();
    let patch = patch.into_values();
    assert_eq!(patch.len(), 2);
    manager.set_values("PatchProbe", patch).await.unwrap();
    let saved = manager.read_latest("PatchProbe").await.unwrap().values;
    assert_eq!(saved["white_list"], serde_json::json!([]));
    assert_eq!(saved["disable_force_prefix"], false);
    assert_eq!(saved["min_permission"], "BotOwner");
    assert_eq!(saved["allow_empty_prefix"], false);
    assert_eq!(saved["auto_listen_enabled"], false);
    let patch: PluginServicePatch = serde_json::from_value(serde_json::json!({
        "auto_listen_enabled": true,
    }))
    .unwrap();
    manager
        .set_values("PatchProbe", patch.into_values())
        .await
        .unwrap();
    let saved = manager.read_latest("PatchProbe").await.unwrap().values;
    assert_eq!(saved["auto_listen_enabled"], true);
    assert_eq!(saved["prefix"], serde_json::json!(["parse"]));
    assert!(
        serde_json::from_value::<PluginServicePatch>(serde_json::json!({}))
            .unwrap()
            .into_values()
            .is_empty()
    );
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn service_patch_cannot_enable_automatic_listening_without_a_declared_capability() {
    use nuono_core::config::{ConfigSchema, PluginConfigManager};
    use nuono_core::core::plugin_service::Plugins;

    let root = std::env::temp_dir().join(format!("nuono-listener-patch-{}", uuid::Uuid::new_v4()));
    let manager = PluginConfigManager::new(root.clone());
    manager
        .register_with_service_config(
            "Ordinary",
            ConfigSchema::new(),
            Some(&Plugins::new("Ordinary")),
        )
        .await
        .unwrap();
    let before = manager.read_latest("Ordinary").await.unwrap();
    let patch: PluginServicePatch =
        serde_json::from_value(serde_json::json!({"auto_listen_enabled": true})).unwrap();
    assert!(
        manager
            .set_values_if_revision("Ordinary", patch.into_values(), Some(&before.revision))
            .await
            .is_err()
    );
    assert_eq!(
        manager.read_latest("Ordinary").await.unwrap().revision,
        before.revision
    );
    for malformed in [serde_json::json!("true"), serde_json::json!(1)] {
        assert!(
            serde_json::from_value::<PluginServicePatch>(
                serde_json::json!({"auto_listen_enabled": malformed})
            )
            .is_err()
        );
    }
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[test]
fn service_config_ignores_preserved_listener_values_without_capability() {
    use nuono_core::config::ConfigValue;
    let mut service = PluginServiceConfig::default();
    let values = HashMap::from([("auto_listen_enabled".into(), ConfigValue::Bool(true))]);
    apply_saved_service_values(&mut service, &values, false);
    assert!(!service.auto_listen_enabled);
    apply_saved_service_values(&mut service, &values, true);
    assert!(service.auto_listen_enabled);
}

#[test]
fn diagnostics_patch_requires_an_explicit_unsigned_integer_and_rejects_other_fields() {
    for value in [0, 1000, 30_000, 3_600_000] {
        let body: DiagnosticsBody = serde_json::from_value(serde_json::json!({
            "slow_threshold_ms": value
        }))
        .unwrap();
        assert_eq!(body.slow_threshold_ms, value);
    }
    for body in [
        serde_json::json!({}),
        serde_json::json!({"slow_threshold_ms": null}),
        serde_json::json!({"slow_threshold_ms": -1}),
        serde_json::json!({"slow_threshold_ms": 0.5}),
        serde_json::json!({"slow_threshold_ms": "1000"}),
        serde_json::json!({"slow_threshold_ms": 1000, "calls": 0}),
    ] {
        assert!(serde_json::from_value::<DiagnosticsBody>(body).is_err());
    }
}

#[test]
fn command_handlers_share_matching_kind_and_summary() {
    let triggers = [
        TriggerType::EventCommand,
        TriggerType::Command,
        TriggerType::Regex,
    ]
    .into_iter()
    .map(|kind| TriggerView {
        trigger_type: trigger_type_to_string(kind),
        command_name: None,
        pattern: "probe".into(),
        prefix: String::new(),
        block: false,
        to_me: false,
        priority: 0,
    })
    .collect();
    let group = ServiceView {
        name: "probe".into(),
        config: SvConfig {
            min_permission: PermissionLevel::Everyone,
            priority: 0,
            enabled: true,
            area: "ALL".into(),
            black_list: vec![],
            white_list: vec![],
        },
        command_count: 2,
        triggers,
    };
    assert_eq!(group.triggers[0].trigger_type, "command");
    let summary = summarize_triggers(&[group]);
    assert_eq!(summary.total, 3);
    assert_eq!(summary.command, 2);
    assert_eq!(summary.regex, 1);
}

#[test]
fn plugin_list_exposes_description_beside_existing_fields() {
    let item = PluginListItem {
        info: InstalledPluginInfo {
            id: Some("probe".into()),
            name: "probe".into(),
            display_name: "Probe Display".into(),
            version: "0.1.0".into(),
            plugin_role: nuono_core::plugin::PluginRole::Feature,
            required_dependencies: Vec::new(),
            optional_dependencies: Vec::new(),
            enabled: true,
            customer_toggleable: false,
            allow_user_overrides: false,
        },
        description: "Plugin metadata supplied by the host".into(),
        icon_url: None,
        diagnostics: PluginDiagnostics::default(),
    };
    let json = serde_json::to_value(item).unwrap();
    assert_eq!(json["name"], "probe");
    assert_eq!(json["display_name"], "Probe Display");
    assert_eq!(json["description"], "Plugin metadata supplied by the host");
    assert_eq!(json["enabled"], true);
    assert_eq!(json["customer_toggleable"], false);
    assert!(json.get("priority").is_none());
    assert!(json["icon_url"].is_null());
    assert_eq!(json["diagnostics"]["calls"], 0);
    assert_eq!(json["diagnostics"]["failures"], 0);
    assert_eq!(json["diagnostics"]["slow_threshold_ms"], 1000);
    assert!(json["diagnostics"]["last_slow"].is_null());
}

#[test]
fn service_config_has_no_plugin_priority_or_framework_field() {
    use nuono_core::config::ConfigValue;

    let defaults: PluginServiceConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(defaults.prefix.is_empty());
    assert!(defaults.force_prefix.is_empty());
    let mut service: PluginServiceConfig = serde_json::from_value(serde_json::json!({
        "priority": 99, "min_permission": "GroupOwner", "prefix": ["probe"], "allow_empty_prefix": true
    }))
    .unwrap();
    assert!(service.allow_empty_prefix);
    let values = HashMap::from([
        ("priority".into(), ConfigValue::Int(-9999)),
        (
            "min_permission".into(),
            ConfigValue::String("GroupAdmin".into()),
        ),
    ]);
    apply_saved_service_values(&mut service, &values, false);
    let encoded = serde_json::to_value(&service).unwrap();
    assert!(encoded.get("priority").is_none());
    assert_eq!(encoded["min_permission"], "GroupAdmin");
    assert_eq!(encoded["prefix"], serde_json::json!(["probe"]));
}

#[test]
fn rule_metadata_preserves_signed_priorities_and_block() {
    let trigger = TriggerView {
        trigger_type: "command".into(),
        command_name: Some("probe".into()),
        pattern: "probe".into(),
        prefix: "/".into(),
        block: true,
        to_me: false,
        priority: i32::MIN,
    };
    let view = ServiceView {
        name: "probe".into(),
        config: SvConfig {
            min_permission: PermissionLevel::Everyone,
            priority: i32::MIN,
            enabled: true,
            area: "ALL".into(),
            black_list: vec![],
            white_list: vec![],
        },
        command_count: 1,
        triggers: vec![trigger],
    };
    let encoded = serde_json::to_value(view).unwrap();
    assert_eq!(encoded["config"]["priority"], i32::MIN);
    assert_eq!(encoded["triggers"][0]["priority"], i32::MIN);
    assert_eq!(encoded["triggers"][0]["block"], true);
    let item = ServiceListItem {
        plugin: "probe".into(),
        plugin_display_name: "Probe Display".into(),
        name: "commands".into(),
        enabled: true,
        min_permission: PermissionLevel::Everyone,
        priority: i32::MAX,
        area: "ALL".into(),
        black_list: vec![],
        white_list: vec![],
        prefix: vec![],
        trigger_count: 1,
    };
    let encoded = serde_json::to_value(item).unwrap();
    assert_eq!(encoded["priority"], i32::MAX);
    assert_eq!(encoded["plugin"], "probe");
    assert_eq!(encoded["plugin_display_name"], "Probe Display");
    assert_eq!(encoded["name"], "commands");
}
