use super::{RuntimeConfigPatch, SETTINGS_KEY, runtime_values};
use crate::config::{ConfigManager, NuonoConfig, message_interactions_revision};
use crate::error::NuonoError;
use crate::test_support;
use nuono_app::message_interactions::{ButtonMode, MarkdownMode, MessageInteractionSettings};
use serde_json::{Value, json};

fn patch(settings: Value) -> RuntimeConfigPatch {
    RuntimeConfigPatch {
        settings,
        secrets: vec![],
    }
}

async fn config_file() -> (std::path::PathBuf, std::path::PathBuf, String) {
    let root =
        std::env::temp_dir().join(format!("nuono-message-settings-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("nuono.toml");
    let mut legacy = toml::Value::try_from(NuonoConfig::default()).unwrap();
    legacy
        .as_table_mut()
        .unwrap()
        .remove("message_interactions");
    let original = toml::to_string(&legacy).unwrap();
    std::fs::write(&path, &original).unwrap();
    (root, path, original)
}

#[tokio::test]
async fn old_settings_default_then_exact_routes_and_partial_policy_persist_without_toml_rewrite() {
    let _env = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let (root, path, original) = config_file().await;
    let mut legacy = runtime_values(&NuonoConfig::default()).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("message_interactions");
    db.set_global_config(SETTINGS_KEY, legacy.clone())
        .await
        .unwrap();
    let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
    manager.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        manager.snapshot().await.message_interactions,
        MessageInteractionSettings::default()
    );
    assert_eq!(
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
        legacy
    );
    let revision =
        message_interactions_revision(&manager.snapshot().await.message_interactions).unwrap();
    manager.update_message_interactions_patch(patch(json!({"message_interactions":{
        "default":{"button_columns":4},
        "routes":[{"platform":"onebot_v11","instance_id":"adapter/甲","bot_self_id":"00042","markdown":"plain"}]
    }})), &revision).await.unwrap();
    let settings = manager.snapshot().await.message_interactions.clone();
    assert_eq!(settings.default.button_columns, 4);
    assert_eq!(
        settings
            .resolve("onebot_v11", "adapter/甲", "00042")
            .markdown,
        MarkdownMode::Plain
    );
    assert_eq!(
        settings.resolve("onebot_v11", "adapter/甲", "42").markdown,
        MarkdownMode::Auto
    );
    let rich_saved =
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap()["message_interactions"].clone();
    manager
        .update_patch(patch(json!({"log":{"level":"WARN"}})))
        .await
        .unwrap();
    assert_eq!(
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap()["message_interactions"],
        rich_saved
    );
    let restarted = ConfigManager::load(Some(path.clone())).await.unwrap();
    restarted.attach_database(db.clone()).await.unwrap();
    assert_eq!(restarted.snapshot().await.message_interactions, settings);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    db.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn separate_managers_compare_the_locked_database_subtree_and_only_one_wins() {
    let _env = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let (root, path, _) = config_file().await;
    let first = ConfigManager::load(Some(path.clone())).await.unwrap();
    let second = ConfigManager::load(Some(path.clone())).await.unwrap();
    first.attach_database(db.clone()).await.unwrap();
    second.attach_database(db.clone()).await.unwrap();
    let revision =
        message_interactions_revision(&first.snapshot().await.message_interactions).unwrap();
    let (left, right) = tokio::join!(
        first.update_message_interactions_patch(
            patch(json!({"message_interactions":{"default":{"markdown":"plain"}}})),
            &revision
        ),
        second.update_message_interactions_patch(
            patch(json!({"message_interactions":{"default":{"buttons":"text"}}})),
            &revision
        ),
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(matches!(
        left.as_ref().err().or(right.as_ref().err()),
        Some(NuonoError::Conflict(_))
    ));
    let restart = ConfigManager::load(Some(path.clone())).await.unwrap();
    restart.attach_database(db.clone()).await.unwrap();
    let saved = restart.snapshot().await;
    assert!(matches!(
        (
            saved.message_interactions.default.markdown,
            saved.message_interactions.default.buttons
        ),
        (MarkdownMode::Plain, ButtonMode::Auto) | (MarkdownMode::Auto, ButtonMode::Text)
    ));
    // 并发更新落败的一方不会将已被拒绝的候选值写入本地内存缓存。
    if left.is_err() {
        assert_eq!(
            first.snapshot().await.message_interactions,
            MessageInteractionSettings::default()
        );
    }
    if right.is_err() {
        assert_eq!(
            second.snapshot().await.message_interactions,
            MessageInteractionSettings::default()
        );
    }
    db.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_or_stale_rich_write_rolls_back_ordinary_settings_secrets_and_memory() {
    let _env = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let (root, path, original) = config_file().await;
    let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
    manager.attach_database(db.clone()).await.unwrap();
    let before = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
    let revision =
        message_interactions_revision(&manager.snapshot().await.message_interactions).unwrap();
    for rich in [
        json!({"default":{"button_columns":0}}),
        json!({"default":{"markdown":"template"}}),
        json!({"routes":[{"platform":"qq_official","instance_id":"x","bot_self_id":"1"},{"platform":"qq_official","instance_id":"x","bot_self_id":"1"}]}),
    ] {
        let write = RuntimeConfigPatch {
            settings: json!({"message_interactions":rich,"log":{"level":"DEBUG"}}),
            secrets: vec![("mail.smtp_password".into(), Some("must-not-commit".into()))],
        };
        assert!(
            manager
                .update_message_interactions_patch(write, &revision)
                .await
                .is_err()
        );
        assert_eq!(
            db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
            before
        );
        assert_eq!(db.get_secret("mail.smtp_password").await.unwrap(), None);
        assert_eq!(manager.snapshot().await.log.level, "INFO");
    }
    assert!(matches!(
        manager
            .update_message_interactions_patch(
                patch(json!({"message_interactions":{"default":{"button_columns":3}}})),
                &"0".repeat(64)
            )
            .await,
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
        before
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    db.shutdown().await.unwrap();
    assert!(
        manager
            .update_message_interactions_patch(
                patch(json!({"message_interactions":{"default":{"button_columns":3}}})),
                &revision
            )
            .await
            .is_err()
    );
    assert_eq!(
        manager.snapshot().await.message_interactions,
        MessageInteractionSettings::default()
    );
    std::fs::remove_dir_all(root).unwrap();
}
