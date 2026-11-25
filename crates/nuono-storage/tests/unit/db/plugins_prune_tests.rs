use crate::entity::{groups, plugin_overrides};
use sea_orm::{EntityTrait, Set};

#[tokio::test]
async fn uninstall_keeps_settings_overrides_and_business_data() {
    let db = crate::test_support::test_db(2).await;
    for name in ["installed", "removed"] {
        db.sync_plugin(name, "1", &serde_json::json!({}))
            .await
            .unwrap();
        db.register_plugin_storage(
            name,
            "1",
            &nuono_app::plugin_manifest::PluginStorageDeclaration::from_migrations(name, vec![])
                .unwrap(),
        )
        .await
        .unwrap();
    }
    let group = db
        .ensure_group(
            &crate::groups::GroupIdentity {
                platform: "qq".into(),
                namespace: "".into(),
                conversation_kind: "group:numeric".into(),
                native_parent_id: "".into(),
                native_group_id: "123".into(),
            },
            "",
        )
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    plugin_overrides::Entity::insert(plugin_overrides::ActiveModel {
        plugin_id: Set("removed".into()),
        group_id: Set(Some(group.id)),
        user_id: Set(None),
        enabled: Set(Some(false)),
        config_patch_json: Set("{\"answer\":42}".into()),
        service_patch_json: Set("{}".into()),
        revision: Set(1),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
        ..Default::default()
    })
    .exec(db.orm())
    .await
    .unwrap();
    let mut settings = db.plugin_configuration("removed").await.unwrap();
    settings.values.insert("retained".into(), true.into());
    db.save_plugin_configuration("removed", &settings)
        .await
        .unwrap();
    let before = db.plugin_policy("removed").await.unwrap();
    let overrides = plugin_overrides::Entity::find()
        .all(db.orm())
        .await
        .unwrap();
    db.prune_plugin_registrations(&["installed".into()])
        .await
        .unwrap();
    let after_prune = db.plugin_policy("removed").await.unwrap();
    assert!(!after_prune.installed);
    assert_eq!(after_prune.enabled, before.enabled);
    assert_eq!(
        after_prune.allow_user_overrides,
        before.allow_user_overrides
    );
    assert_eq!(
        after_prune.allow_group_overrides,
        before.allow_group_overrides
    );
    assert_eq!(after_prune.config_json, before.config_json);
    assert_eq!(after_prune.service_config_json, before.service_config_json);
    assert_eq!(after_prune.config_version, before.config_version);
    assert_eq!(after_prune.revision, before.revision);
    assert_eq!(after_prune.updated_at_ms, before.updated_at_ms);
    assert_eq!(
        plugin_overrides::Entity::find()
            .all(db.orm())
            .await
            .unwrap(),
        overrides
    );
    assert!(
        groups::Entity::find_by_id(group.id)
            .one(db.orm())
            .await
            .unwrap()
            .is_some()
    );
    db.register_plugin_storage(
        "removed",
        "1",
        &nuono_app::plugin_manifest::PluginStorageDeclaration::from_migrations("removed", vec![])
            .unwrap(),
    )
    .await
    .unwrap();
    let after_reinstall = db.plugin_policy("removed").await.unwrap();
    assert!(after_reinstall.installed);
    assert_eq!(after_reinstall.enabled, before.enabled);
    assert_eq!(after_reinstall.config_json, before.config_json);
    assert_eq!(
        after_reinstall.service_config_json,
        before.service_config_json
    );
    assert_eq!(after_reinstall.revision, before.revision);
    assert_eq!(after_reinstall.updated_at_ms, before.updated_at_ms);
    db.shutdown().await.unwrap();
}
