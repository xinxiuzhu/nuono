use super::*;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

async fn registration(db: &Database) -> plugins::Model {
    plugins::Entity::find()
        .filter(plugins::Column::PluginId.eq("policy"))
        .one(db.orm())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn trigger_policy_is_explicit_persisted_and_survives_registry_sync_and_rebuild() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("policy", "1", &serde_json::json!({"retained":true}))
        .await
        .unwrap();
    assert_eq!(db.get_plugin_trigger_policy("policy").await.unwrap(), None);
    assert!(registration(&db).await.trigger_policy_json.is_none());
    assert!(matches!(
        db.get_plugin_trigger_policy("missing").await,
        Err(NuonoError::PluginNotFound(_))
    ));
    assert!(matches!(
        db.set_plugin_trigger_policy("missing", &PluginTriggerPolicy::default())
            .await,
        Err(NuonoError::PluginNotFound(_))
    ));
    assert_eq!(plugins::Entity::find().count(db.orm()).await.unwrap(), 1);

    let policy = PluginTriggerPolicy {
        user_cooldown_ms: 30_000,
        max_in_flight_per_user: 2,
        group_requests_per_minute: 120,
        group_burst: 7,
    };
    db.set_plugin_trigger_policy("policy", &policy)
        .await
        .unwrap();
    let saved = registration(&db).await;
    assert_eq!(
        serde_json::from_str::<PluginTriggerPolicy>(saved.trigger_policy_json.as_deref().unwrap())
            .unwrap(),
        policy
    );
    db.set_plugin_enabled("policy", false).await.unwrap();
    db.set_plugin_customer_toggleable("policy", false)
        .await
        .unwrap();
    db.set_plugin_slow_threshold("policy", 45_000)
        .await
        .unwrap();
    db.sync_plugin("policy", "2", &serde_json::json!({"updated":true}))
        .await
        .unwrap();
    db.sync_plugins_batch(&[("policy", "3", &serde_json::json!({"rebuilt":true}))])
        .await
        .unwrap();
    db.prune_plugin_registrations(&["policy".into()])
        .await
        .unwrap();
    assert_eq!(
        db.get_plugin_trigger_policy("policy").await.unwrap(),
        Some(policy)
    );
    let rebuilt = registration(&db).await;
    assert_eq!(rebuilt.plugin_id, saved.plugin_id);
    assert!(!rebuilt.enabled);
    assert!(!rebuilt.allow_group_overrides);
    assert_eq!(rebuilt.slow_threshold_ms, Some(45_000));
    assert_eq!(rebuilt.config_json, "{}");

    for valid in [
        PluginTriggerPolicy {
            user_cooldown_ms: 0,
            max_in_flight_per_user: 0,
            group_requests_per_minute: 0,
            group_burst: 1,
        },
        PluginTriggerPolicy {
            user_cooldown_ms: 3_600_000,
            max_in_flight_per_user: 32,
            group_requests_per_minute: 60_000,
            group_burst: 1000,
        },
    ] {
        db.set_plugin_trigger_policy("policy", &valid)
            .await
            .unwrap();
        assert_eq!(
            db.get_plugin_trigger_policy("policy").await.unwrap(),
            Some(valid)
        );
    }
    let before_invalid = registration(&db).await;
    for invalid in [
        PluginTriggerPolicy {
            user_cooldown_ms: u64::MAX,
            ..policy
        },
        PluginTriggerPolicy {
            max_in_flight_per_user: 33,
            ..policy
        },
        PluginTriggerPolicy {
            group_requests_per_minute: 60_001,
            ..policy
        },
        PluginTriggerPolicy {
            group_burst: 0,
            ..policy
        },
        PluginTriggerPolicy {
            group_burst: 1001,
            ..policy
        },
    ] {
        assert!(
            db.set_plugin_trigger_policy("policy", &invalid)
                .await
                .is_err()
        );
        assert_eq!(registration(&db).await, before_invalid);
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_or_invalid_stored_trigger_policy_never_silently_becomes_defaults() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("policy", "1", &serde_json::json!({}))
        .await
        .unwrap();
    let before = registration(&db).await;
    assert!(
        plugins::Entity::update_many()
            .col_expr(plugins::Column::TriggerPolicyJson, Expr::value("not json"))
            .filter(plugins::Column::PluginId.eq("policy"))
            .exec(db.orm())
            .await
            .is_err(),
        "database must reject malformed JSON before it can become authoritative"
    );
    assert_eq!(registration(&db).await, before);
    for raw in [
        "null",
        "{}",
        r#"{"user_cooldown_ms":0,"max_in_flight_per_user":1,"group_requests_per_minute":30,"group_burst":0}"#,
        r#"{"user_cooldown_ms":1000,"max_in_flight_per_user":1,"group_requests_per_minute":30,"group_burst":3,"unknown":true}"#,
    ] {
        plugins::Entity::update_many()
            .col_expr(plugins::Column::TriggerPolicyJson, Expr::value(raw))
            .filter(plugins::Column::PluginId.eq("policy"))
            .exec(db.orm())
            .await
            .unwrap();
        assert!(matches!(
            db.get_plugin_trigger_policy("policy").await,
            Err(NuonoError::ConfigValidation(_))
        ));
        db.sync_plugin("policy", "2", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            registration(&db).await.trigger_policy_json.as_deref(),
            Some(raw)
        );
    }
    db.set_plugin_trigger_policy("policy", &PluginTriggerPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        db.get_plugin_trigger_policy("policy").await.unwrap(),
        Some(PluginTriggerPolicy::default())
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn framework_baseline_preserves_trigger_policy_and_group_overrides() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("policy", "old", &serde_json::json!({"keep":true}))
        .await
        .unwrap();
    db.set_plugin_enabled("policy", false).await.unwrap();
    db.set_plugin_customer_toggleable("policy", false)
        .await
        .unwrap();
    db.set_plugin_slow_threshold("policy", 12_345)
        .await
        .unwrap();
    let original = registration(&db).await;
    crate::db::migrations::run_framework(db.orm())
        .await
        .unwrap();
    assert_eq!(registration(&db).await, original);
    db.shutdown().await.unwrap();
}
