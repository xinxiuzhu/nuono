use super::*;
use sea_orm::{ActiveModelTrait, DbBackend, Statement};

#[tokio::test]
async fn declared_enable_default_applies_once_and_preserves_operator_choice() {
    let db = crate::test_support::test_db(2).await;
    let schema = serde_json::json!({});
    db.sync_plugins_batch_with_defaults(&[("external-service", "1", &schema, false)])
        .await
        .unwrap();
    assert!(!db.is_plugin_enabled("external-service").await.unwrap());
    db.set_plugin_enabled("external-service", true)
        .await
        .unwrap();
    db.sync_plugins_batch_with_defaults(&[("external-service", "2", &schema, false)])
        .await
        .unwrap();
    assert!(db.is_plugin_enabled("external-service").await.unwrap());
    db.set_plugin_enabled("external-service", false)
        .await
        .unwrap();
    db.sync_plugins_batch_with_defaults(&[("external-service", "3", &schema, true)])
        .await
        .unwrap();
    assert!(!db.is_plugin_enabled("external-service").await.unwrap());
}

#[tokio::test]
async fn slow_threshold_defaults_boundaries_and_sync_preserve_operator_policy() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("network", "1", &serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(db.get_plugin_slow_threshold("network").await.unwrap(), None);
    assert_eq!(db.get_plugin_slow_threshold("missing").await.unwrap(), None);
    assert!(
        !db.set_plugin_slow_threshold("missing", 5_000)
            .await
            .unwrap()
    );
    for threshold in [0, 1, 30_000, MAX_SLOW_PLUGIN_THRESHOLD_MS] {
        assert!(
            db.set_plugin_slow_threshold("network", threshold)
                .await
                .unwrap()
        );
        assert_eq!(
            db.get_plugin_slow_threshold("network").await.unwrap(),
            Some(threshold)
        );
    }
    assert!(
        db.set_plugin_slow_threshold("network", MAX_SLOW_PLUGIN_THRESHOLD_MS + 1)
            .await
            .is_err()
    );
    assert!(
        db.set_plugin_slow_threshold("network", u64::MAX)
            .await
            .is_err()
    );
    assert_eq!(
        db.get_plugin_slow_threshold("network").await.unwrap(),
        Some(MAX_SLOW_PLUGIN_THRESHOLD_MS)
    );
    db.set_plugin_slow_threshold("network", 30_000)
        .await
        .unwrap();
    db.set_plugin_enabled("network", false).await.unwrap();
    db.set_plugin_customer_toggleable("network", false)
        .await
        .unwrap();
    db.sync_plugins_batch(&[("network", "2", &serde_json::json!({"new":true}))])
        .await
        .unwrap();
    let row = plugins::Entity::find()
        .filter(plugins::Column::PluginId.eq("network"))
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.slow_threshold_ms, Some(30_000));
    assert!(!row.enabled);
    assert!(!row.allow_group_overrides);
    let mut legacy: plugins::ActiveModel = row.into();
    legacy.slow_threshold_ms = Set(None);
    legacy.update(db.orm()).await.unwrap();
    assert_eq!(db.get_plugin_slow_threshold("network").await.unwrap(), None);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_threshold_database_constraint_rejects_invalid_values() {
    let db = crate::test_support::test_db(2).await;
    db.sync_plugin("policy", "1", &serde_json::json!({}))
        .await
        .unwrap();
    let sql = if db.backend() == DbBackend::Postgres {
        "UPDATE nuono_plugins SET slow_threshold_ms = $1 WHERE plugin_id = $2"
    } else {
        "UPDATE nuono_plugins SET slow_threshold_ms = ? WHERE plugin_id = ?"
    };
    for threshold in [-1_i64, MAX_SLOW_PLUGIN_THRESHOLD_MS as i64 + 1] {
        assert!(
            db.orm()
                .execute_raw(Statement::from_sql_and_values(
                    db.backend(),
                    sql,
                    [threshold.into(), "policy".into()]
                ))
                .await
                .is_err()
        );
    }
    assert_eq!(db.get_plugin_slow_threshold("policy").await.unwrap(), None);
    db.shutdown().await.unwrap();
}
