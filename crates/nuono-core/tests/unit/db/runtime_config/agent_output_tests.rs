use super::*;
use crate::{config::ConfigManager, test_support};
use nuono_app::agent::output::OutputMode;
use serde_json::json;

#[tokio::test]
async fn agent_output_is_transactional_defaults_to_complete_and_survives_restart() {
    let _env = test_support::env_lock().await;
    let db = test_support::test_db(2).await;
    let path =
        std::env::temp_dir().join(format!("unused-agent-output-{}.toml", uuid::Uuid::new_v4()));
    let manager = ConfigManager::for_test(NuonoConfig::default(), path.clone());
    manager.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        manager.snapshot().await.ai.output.default.mode,
        OutputMode::Complete
    );
    manager
        .update_patch(RuntimeConfigPatch {
            settings: json!({"ai":{"output":{"default":{"mode":"native","interval_ms":500}}}}),
            secrets: vec![],
        })
        .await
        .unwrap();
    let saved = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
    assert_eq!(saved["ai"]["output"]["default"]["mode"], "native");
    for output in [
        json!({"default":{"mode":"typo"}}),
        json!({"default":{"interval_ms":299}}),
        json!({"default":{"segment_chars":0}}),
    ] {
        assert!(
            manager
                .update_patch(RuntimeConfigPatch {
                    settings: json!({"ai":{"output":output}}),
                    secrets: vec![("mail.smtp_password".into(), Some("must-not-commit".into()))]
                })
                .await
                .is_err()
        );
        assert_eq!(
            db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
            saved
        );
        assert!(db.get_secret("mail.smtp_password").await.unwrap().is_none());
        assert_eq!(
            manager.snapshot().await.ai.output.default.mode,
            OutputMode::Native
        );
    }
    let restarted = ConfigManager::for_test(NuonoConfig::default(), path);
    restarted.attach_database(db.clone()).await.unwrap();
    assert_eq!(
        restarted.snapshot().await.ai.output.default.mode,
        OutputMode::Native
    );
    assert_eq!(
        restarted.snapshot().await.ai.output.default.interval_ms,
        500
    );
    assert_eq!(
        restarted.snapshot().await.ai.output.default.segment_chars,
        400
    );
    db.shutdown().await.unwrap();
}
