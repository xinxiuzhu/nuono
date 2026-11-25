use super::*;
use crate::{config::NuonoConfig, db::Database, test_support};
use nuono_app::{
    model_catalog::{CreateAiModel, UpdateAiModel},
    settings::{RuntimeConfigSnapshot, RuntimeSettingsStore, RuntimeSettingsValidator},
};
use sea_orm::{ConnectionTrait, DbBackend};
use serde_json::{Value, json};
use std::sync::Arc;

fn patch(settings: Value) -> RuntimeConfigPatch {
    RuntimeConfigPatch {
        settings,
        secrets: vec![],
    }
}
fn input(name: &str, default: bool) -> CreateAiModel<'_> {
    CreateAiModel {
        name,
        provider: "fixture",
        provider_type: "openai_compatible",
        model_id: name,
        api_key: Some("fixture-secret"),
        base_url: "https://example.invalid/v1",
        is_default: default,
    }
}
async fn manager(db: &Database) -> ConfigManager {
    let manager = ConfigManager::for_test(
        NuonoConfig::default(),
        "/tmp/unused-model-config.toml".into(),
    );
    manager.attach_database(db.clone()).await.unwrap();
    manager
}
async fn settings(db: &Database) -> Value {
    db.get_global_config("runtime.settings.v1")
        .await
        .unwrap()
        .unwrap()
}
async fn reject(db: &Database, table: &str, event: &str, condition: &str) {
    let sql = if db.backend() == DbBackend::Postgres {
        format!(
            "CREATE FUNCTION fail_model_atomic() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF {condition} THEN RAISE EXCEPTION 'injected write failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_model_atomic BEFORE {event} ON {table} FOR EACH ROW EXECUTE FUNCTION fail_model_atomic()"
        )
    } else {
        format!(
            "CREATE TRIGGER reject_model_atomic BEFORE {event} ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT, 'injected write failure'); END"
        )
    };
    db.orm().execute_unprepared(&sql).await.unwrap();
}
async fn remove_rejection(db: &Database, table: &str) {
    let sql = if db.backend() == DbBackend::Postgres {
        format!("DROP TRIGGER reject_model_atomic ON {table}; DROP FUNCTION fail_model_atomic()")
    } else {
        "DROP TRIGGER reject_model_atomic".into()
    };
    db.orm().execute_unprepared(&sql).await.unwrap();
}

#[tokio::test]
async fn failed_default_insert_and_update_preserve_the_previous_default() {
    let db = test_support::test_db(4).await;
    let old = db.create_ai_model(input("old", true)).await.unwrap();
    reject(&db, "nuono_ai_models", "INSERT", "NEW.name = 'reject'").await;
    assert!(db.create_ai_model(input("reject", true)).await.is_err());
    assert_eq!(db.get_default_ai_model().await.unwrap().unwrap().id, old.id);
    assert_eq!(db.list_ai_models().await.unwrap().len(), 1);
    remove_rejection(&db, "nuono_ai_models").await;
    let target = db.create_ai_model(input("target", false)).await.unwrap();
    reject(
        &db,
        "nuono_ai_models",
        "UPDATE",
        &format!("NEW.id = {} AND NEW.is_default = true", target.id),
    )
    .await;
    assert!(
        db.update_ai_model(
            target.id,
            UpdateAiModel {
                is_default: Some(true),
                name: Some("new name"),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(db.get_default_ai_model().await.unwrap().unwrap().id, old.id);
    assert_eq!(db.get_ai_model(target.id).await.unwrap().unwrap(), target);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn create_disabled_is_a_single_write_and_settings_failure_leaves_no_model() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let manager = manager(&db).await;
    let row = manager
        .change_ai_model(
            ModelChange::Create {
                input: input("disabled", false),
                enabled: false,
            },
            Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!row.enabled);
    let before = manager.snapshot().await;
    reject(
        &db,
        "nuono_global_configs",
        "UPDATE",
        "NEW.key = 'runtime.settings.v1'",
    )
    .await;
    assert!(
        manager
            .change_ai_model(
                ModelChange::Create {
                    input: input("reject", true),
                    enabled: false
                },
                Default::default()
            )
            .await
            .is_err()
    );
    assert_eq!(db.list_ai_models().await.unwrap().len(), 1);
    assert!(Arc::ptr_eq(&before, &manager.snapshot().await));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn delete_and_rebind_commit_together_and_failed_settings_write_rolls_back_everything() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let old = db.create_ai_model(input("old", true)).await.unwrap();
    let new = db.create_ai_model(input("new", false)).await.unwrap();
    let manager = manager(&db).await;
    manager
        .update_patch(patch(
            json!({"ai":{"chat_model_alias":format!("id:{}",old.id)},"log":{"level":"WARN"}}),
        ))
        .await
        .unwrap();
    let before = settings(&db).await;
    let memory = manager.snapshot().await;
    reject(
        &db,
        "nuono_global_configs",
        "UPDATE",
        "NEW.key = 'runtime.settings.v1'",
    )
    .await;
    let rebind = || patch(json!({"ai":{"chat_model_alias":format!("id:{}",new.id)}}));
    assert!(
        manager
            .change_ai_model(ModelChange::Delete { id: old.id }, rebind())
            .await
            .is_err()
    );
    assert_eq!(db.get_ai_model(old.id).await.unwrap().unwrap(), old);
    assert_eq!(settings(&db).await, before);
    assert!(Arc::ptr_eq(&memory, &manager.snapshot().await));
    remove_rejection(&db, "nuono_global_configs").await;
    manager
        .change_ai_model(ModelChange::Delete { id: old.id }, rebind())
        .await
        .unwrap();
    assert!(db.get_ai_model(old.id).await.unwrap().is_none());
    assert_eq!(
        manager.snapshot().await.ai.chat_model_alias,
        format!("id:{}", new.id)
    );
    assert_eq!(manager.snapshot().await.log.level, "WARN");
    let restarted = self::manager(&db).await;
    assert_eq!(
        restarted.snapshot().await.ai.chat_model_alias,
        manager.snapshot().await.ai.chat_model_alias
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_delete_preserves_settings_secrets_and_memory() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let old = db.create_ai_model(input("old", true)).await.unwrap();
    let manager = manager(&db).await;
    let before = settings(&db).await;
    let memory = manager.snapshot().await;
    reject(
        &db,
        "nuono_ai_models",
        "DELETE",
        &format!("OLD.id = {}", old.id),
    )
    .await;
    let mut update = patch(json!({"log":{"level":"WARN"}}));
    update
        .secrets
        .push(("ai.api_key".into(), Some("new-secret".into())));
    assert!(
        manager
            .change_ai_model(ModelChange::Delete { id: old.id }, update)
            .await
            .is_err()
    );
    assert_eq!(settings(&db).await, before);
    assert!(
        !db.all_secret_values()
            .await
            .unwrap()
            .contains_key("ai.api_key")
    );
    assert!(Arc::ptr_eq(&memory, &manager.snapshot().await));
    assert!(db.get_ai_model(old.id).await.unwrap().is_some());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_stable_binding_protects_deletion_disabling_and_key_removal_even_when_ai_is_off() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let row = db.create_ai_model(input("bound", false)).await.unwrap();
    let manager = manager(&db).await;
    for slot in [
        "chat_model_alias",
        "tool_model_alias",
        "model_alias",
        "provider_fallback_chain",
        "agent_memory_policy",
    ] {
        let ai = match slot {
            "provider_fallback_chain" => json!({slot:[format!("id:{}",row.id)]}),
            "agent_memory_policy" => json!({slot:{"embedding_model_id":row.id}}),
            _ => json!({slot:format!("id:{}",row.id)}),
        };
        manager.update_patch(patch(json!({"ai":ai}))).await.unwrap();
        assert!(matches!(
            manager
                .change_ai_model(ModelChange::Delete { id: row.id }, Default::default())
                .await,
            Err(NuonoError::Conflict(_))
        ));
        assert!(matches!(
            manager
                .change_ai_model(
                    ModelChange::Update {
                        id: row.id,
                        patch: UpdateAiModel {
                            enabled: Some(false),
                            ..Default::default()
                        }
                    },
                    Default::default()
                )
                .await,
            Err(NuonoError::Conflict(_))
        ));
        assert!(db.delete_ai_model(row.id).await.is_err());
        assert!(
            db.update_ai_model(
                row.id,
                UpdateAiModel {
                    api_key: Some(""),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        let reset = match slot {
            "provider_fallback_chain" => json!({slot:[]}),
            "agent_memory_policy" => json!({slot:{"embedding_model_id":null}}),
            _ => json!({slot:""}),
        };
        manager
            .update_patch(patch(json!({"ai":reset})))
            .await
            .unwrap();
    }
    assert!(db.get_ai_model(row.id).await.unwrap().unwrap().enabled);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn competing_selection_and_deletion_have_one_winner_and_no_dangling_reference() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let first = manager(&db).await;
    let second = manager(&db).await;
    for _ in 0..8 {
        let model = db.create_ai_model(input("race", false)).await.unwrap();
        let (selected, deleted) = tokio::join!(
            first.update_patch(patch(
                json!({"ai":{"chat_model_alias":format!("id:{}",model.id)}})
            )),
            second.change_ai_model(ModelChange::Delete { id: model.id }, Default::default()),
        );
        assert_ne!(selected.is_ok(), deleted.is_ok());
        let bound = settings(&db).await["ai"]["chat_model_alias"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            bound == format!("id:{}", model.id),
            db.get_ai_model(model.id).await.unwrap().is_some()
        );
        first
            .update_patch(patch(json!({"ai":{"chat_model_alias":""}})))
            .await
            .unwrap();
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_default_changes_and_disjoint_patches_do_not_lose_writes() {
    let db = test_support::test_db(4).await;
    let (left, right) = tokio::join!(
        db.create_ai_model(input("left", true)),
        db.create_ai_model(input("right", true))
    );
    let (left, right) = (left.unwrap(), right.unwrap());
    assert_eq!(
        db.list_ai_models()
            .await
            .unwrap()
            .iter()
            .filter(|row| row.is_default)
            .count(),
        1
    );
    let (a, b) = tokio::join!(
        db.update_ai_model(
            left.id,
            UpdateAiModel {
                name: Some("renamed"),
                is_default: Some(true),
                ..Default::default()
            }
        ),
        db.update_ai_model(
            left.id,
            UpdateAiModel {
                base_url: Some("https://changed.invalid/v1"),
                ..Default::default()
            }
        )
    );
    a.unwrap();
    b.unwrap();
    let row = db.get_ai_model(left.id).await.unwrap().unwrap();
    assert_eq!(row.name, "renamed");
    assert_eq!(row.base_url, "https://changed.invalid/v1");
    assert!(!db.get_ai_model(right.id).await.unwrap().unwrap().is_default);
    db.shutdown().await.unwrap();
}

struct BlockedValidator(tokio::sync::Notify);
#[async_trait::async_trait]
impl RuntimeSettingsValidator for BlockedValidator {
    async fn validate(&self, _: RuntimeConfigSnapshot) -> Result<Value> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelled_validation_rolls_back_the_model_and_releases_transaction_locks() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(4).await;
    let manager = manager(&db).await;
    let model = db.create_ai_model(input("kept", true)).await.unwrap();
    let before = settings(&db).await;
    let validator = Arc::new(BlockedValidator(tokio::sync::Notify::new()));
    let worker_db = db.clone();
    let worker_validator = validator.clone();
    let task = tokio::spawn(async move {
        RuntimeSettingsStore::change_model(
            &worker_db,
            ModelChange::Delete { id: model.id },
            &patch(json!({"log":{"level":"WARN"}})),
            worker_validator.as_ref(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), validator.0.notified())
        .await
        .unwrap();
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        manager.update_patch(patch(json!({"log":{"level":"ERROR"}}))),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(db.get_ai_model(model.id).await.unwrap().is_some());
    let mut after = settings(&db).await;
    after["log"]["level"] = before["log"]["level"].clone();
    assert_eq!(after, before);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn secret_failure_after_settings_write_rolls_back_model_settings_and_snapshot() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(3).await;
    let model = db.create_ai_model(input("keep", true)).await.unwrap();
    let manager = manager(&db).await;
    let before = settings(&db).await;
    let memory = manager.snapshot().await;
    reject(
        &db,
        "nuono_runtime_secrets",
        "INSERT",
        "NEW.key = 'ai.api_key'",
    )
    .await;
    let mut change = patch(json!({"log":{"level":"WARN"}}));
    change
        .secrets
        .push(("ai.api_key".into(), Some("new-key".into())));
    assert!(
        manager
            .change_ai_model(ModelChange::Delete { id: model.id }, change)
            .await
            .is_err()
    );
    assert_eq!(db.get_ai_model(model.id).await.unwrap().unwrap(), model);
    assert_eq!(settings(&db).await, before);
    assert!(Arc::ptr_eq(&memory, &manager.snapshot().await));
    assert!(
        !db.all_secret_values()
            .await
            .unwrap()
            .contains_key("ai.api_key")
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn effective_fallback_binding_is_checked_even_when_the_stored_tree_omits_it() {
    let _environment_lock = test_support::env_lock().await;
    let db = test_support::test_db(3).await;
    let row = db.create_ai_model(input("fallback", false)).await.unwrap();
    // 缺失字段的旧版配置会从启动基线中继承该模型绑定。
    db.set_global_config("runtime.settings.v1", json!({"ai":{}}))
        .await
        .unwrap();
    let mut startup = NuonoConfig::default();
    startup.ai.chat_model_alias = format!("id:{}", row.id);
    let manager = ConfigManager::for_test(startup, "/tmp/unused-model-config.toml".into());
    manager.attach_database(db.clone()).await.unwrap();
    assert!(matches!(
        manager
            .change_ai_model(ModelChange::Delete { id: row.id }, Default::default())
            .await,
        Err(NuonoError::Conflict(_))
    ));
    assert!(db.get_ai_model(row.id).await.unwrap().is_some());
    // 提交指向不存在模型的悬空绑定时必须直接报错，不能被其他有效值掩盖而写入存储。
    assert!(
        manager
            .update_patch(patch(json!({"ai":{"chat_model_alias":"id:2147483647"}})))
            .await
            .is_err()
    );
    assert_eq!(settings(&db).await, json!({"ai":{}}));
    db.shutdown().await.unwrap();
}
