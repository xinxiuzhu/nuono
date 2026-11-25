use super::{RuntimeConfigPatch, SETTINGS_KEY, runtime_values};
use crate::config::{ConfigManager, NuonoConfig};
use crate::test_support;
use nuono_app::image_output::{ImageOutputFormat, ImageOutputSettings};
use serde_json::{Value, json};

async fn legacy_config_file() -> (std::path::PathBuf, std::path::PathBuf, String) {
    let root = std::env::temp_dir().join(format!("nuono-image-output-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("nuono.toml");
    let mut legacy = toml::Value::try_from(NuonoConfig::default()).unwrap();
    legacy.as_table_mut().unwrap().remove("image_output");
    let original = toml::to_string(&legacy).unwrap();
    std::fs::write(&path, &original).unwrap();
    (root, path, original)
}

fn patch(settings: Value) -> RuntimeConfigPatch {
    RuntimeConfigPatch {
        settings,
        secrets: vec![],
    }
}

#[tokio::test]
async fn legacy_image_output_settings_default_and_partial_updates_survive_restart() {
    let _env_lock = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let (root, path, original) = legacy_config_file().await;
    let mut legacy = runtime_values(&NuonoConfig::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("image_output");
    legacy["mail"]["smtp_host"] = json!("smtp.keep.example");
    db.set_global_config(SETTINGS_KEY, legacy.clone())
        .await
        .unwrap();

    let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
    manager.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        manager.snapshot().await.image_output,
        ImageOutputSettings::default()
    );
    // 挂载时直接按默认值补齐读取已有 JSON 记录，而不改写数据库中的原记录。
    assert_eq!(
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
        legacy
    );

    manager
        .update_patch(patch(json!({"image_output": {"format": "jpeg"}})))
        .await
        .unwrap();
    let saved = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
    assert_eq!(saved["image_output"], json!({"format": "jpeg"}));
    assert_eq!(
        manager.snapshot().await.image_output,
        ImageOutputSettings {
            format: ImageOutputFormat::Jpeg,
            jpeg_quality: 80,
        }
    );
    let mut unrelated = saved.clone();
    unrelated.as_object_mut().unwrap().remove("image_output");
    assert_eq!(unrelated, legacy);

    manager
        .update_patch(patch(json!({"image_output": {"jpeg_quality": 92}})))
        .await
        .unwrap();
    let saved = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
    assert_eq!(
        saved["image_output"],
        json!({"format": "jpeg", "jpeg_quality": 92})
    );
    assert_eq!(saved["mail"]["smtp_host"], "smtp.keep.example");

    manager
        .update_patch(patch(json!({"log": {"level": "WARN"}})))
        .await
        .unwrap();
    assert_eq!(
        db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap()["image_output"],
        saved["image_output"]
    );
    let restarted_jpeg = ConfigManager::load(Some(path.clone())).await.unwrap();
    restarted_jpeg.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        restarted_jpeg.snapshot().await.image_output,
        ImageOutputSettings {
            format: ImageOutputFormat::Jpeg,
            jpeg_quality: 92,
        }
    );

    manager
        .update_patch(patch(json!({"image_output": {"format": "png"}})))
        .await
        .unwrap();
    let saved = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
    assert_eq!(
        saved["image_output"],
        json!({"format": "png", "jpeg_quality": 92})
    );
    let restarted = ConfigManager::load(Some(path.clone())).await.unwrap();
    restarted.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        restarted.snapshot().await.image_output,
        ImageOutputSettings {
            format: ImageOutputFormat::Png,
            jpeg_quality: 92,
        }
    );
    assert_eq!(restarted.snapshot().await.log.level, "WARN");
    assert_eq!(
        restarted.snapshot().await.mail.smtp_host,
        "smtp.keep.example"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    db.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_image_output_rolls_back_settings_secrets_and_memory() {
    let _env_lock = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let (root, path, original) = legacy_config_file().await;
    let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
    manager.attach_database(db.clone()).await.unwrap();
    let before = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();

    for image_output in [
        json!({"format": "png", "jpeg_quality": 0}),
        json!({"format": "jpeg", "jpeg_quality": 0}),
        json!({"format": "png", "jpeg_quality": 101}),
        json!({"format": "jpeg", "jpeg_quality": 255}),
        json!({"jpeg_quality": -1}),
        json!({"jpeg_quality": 256}),
        json!({"jpeg_quality": "80"}),
        json!({"format": "webp"}),
    ] {
        assert!(
            manager
                .update_patch(RuntimeConfigPatch {
                    settings: json!({"image_output": image_output, "log": {"level": "DEBUG"}}),
                    secrets: vec![("mail.smtp_password".into(), Some("must-not-commit".into()))],
                })
                .await
                .is_err()
        );
        assert_eq!(
            db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
            before
        );
        assert_eq!(db.get_secret("mail.smtp_password").await.unwrap(), None);
        assert_eq!(
            manager.snapshot().await.image_output,
            ImageOutputSettings::default()
        );
        assert_eq!(manager.snapshot().await.log.level, "INFO");
    }

    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    db.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn image_output_is_published_only_after_persistence_succeeds() {
    let _env_lock = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let manager = ConfigManager::for_test(
        NuonoConfig::default(),
        std::env::temp_dir().join("unused-image-output-config.toml"),
    );
    manager.attach_database(db.clone()).await.unwrap();
    db.shutdown().await.unwrap();
    assert!(
        manager
            .update_patch(patch(json!({
                "image_output": {"format": "jpeg", "jpeg_quality": 90}
            })))
            .await
            .is_err()
    );
    assert_eq!(
        manager.snapshot().await.image_output,
        ImageOutputSettings::default()
    );
}
