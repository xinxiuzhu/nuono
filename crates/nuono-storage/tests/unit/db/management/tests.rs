use super::*;
use crate::{db::BrowseRequest, test_support::test_db};
use serde_json::json;

fn request(values: Value, original: Option<Value>, confirmation: Option<&str>) -> MutationRequest {
    MutationRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        values: values.as_object().unwrap().clone(),
        original: original.map(|row| row.as_object().unwrap().clone()),
        confirmation: confirmation.map(str::to_string),
    }
}
async fn rows(db: &Database, table: &str) -> Vec<Value> {
    db.browse_table(table, &BrowseRequest::default())
        .await
        .unwrap()
        .rows
}
async fn count(db: &Database, table: &str) -> i64 {
    db.orm()
        .query_one_raw(Statement::from_string(
            db.backend(),
            format!("SELECT COUNT(*) AS n FROM {table}"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}
async fn mutation_count(db: &Database) -> i64 {
    db.orm()
        .query_one_raw(Statement::from_string(
            db.backend(),
            "SELECT COUNT(*) AS n FROM nuono_global_configs WHERE key LIKE 'db_mutation.%'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}
async fn event_audit_count(db: &Database) -> i64 {
    db.orm()
        .query_one_raw(Statement::from_string(
            db.backend(),
            "SELECT COUNT(*) AS n FROM nuono_audit_events WHERE category = 'event'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}
async fn sql(db: &Database, sql: &str) {
    db.orm().execute_unprepared(sql).await.unwrap();
}
fn rule_values() -> Value {
    json!({"pattern":"initial", "filter_type":"receive", "match_mode":"simple"})
}

#[tokio::test]
async fn crud_checks_snapshots_and_commits_audit_and_idempotency() {
    let db = test_db(3).await;
    let create = request(rule_values(), None, None);
    let created = db
        .mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &create,
            "test-owner",
        )
        .await
        .unwrap();
    assert_eq!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &create,
            "test-owner"
        )
        .await
        .unwrap(),
        created
    );
    assert_eq!(count(&db, "nuono_filter_rules").await, 1);
    let before = rows(&db, "nuono_filter_rules").await.remove(0);
    let update = request(
        json!({"pattern":"' OR 1=1 -- %_ literal"}),
        Some(before.clone()),
        None,
    );
    let updated = db
        .mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &update,
            "test-owner",
        )
        .await
        .unwrap();
    assert_eq!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &update,
            "test-owner"
        )
        .await
        .unwrap(),
        updated
    );
    let stale = request(json!({"pattern":"overwrite"}), Some(before), None);
    assert!(matches!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &stale,
            "test-owner"
        )
        .await,
        Err(NuonoError::Conflict(_))
    ));
    let current = rows(&db, "nuono_filter_rules").await.remove(0);
    assert_eq!(current["pattern"], "' OR 1=1 -- %_ literal");
    let mut changed_payload = update.clone();
    changed_payload
        .values
        .insert("pattern".into(), json!("different"));
    assert!(matches!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &changed_payload,
            "test-owner"
        )
        .await,
        Err(NuonoError::Conflict(_))
    ));
    assert!(matches!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &update,
            "other-actor"
        )
        .await,
        Err(NuonoError::Conflict(_))
    ));
    let missing_confirmation = request(json!({}), Some(current.clone()), None);
    assert!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Delete,
            &missing_confirmation,
            "test-owner"
        )
        .await
        .is_err()
    );
    let delete = request(json!({}), Some(current), Some("nuono_filter_rules"));
    let deleted = db
        .mutate_table(
            "nuono_filter_rules",
            MutationOperation::Delete,
            &delete,
            "test-owner",
        )
        .await
        .unwrap();
    assert_eq!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Delete,
            &delete,
            "test-owner"
        )
        .await
        .unwrap(),
        deleted
    );
    assert!(rows(&db, "nuono_filter_rules").await.is_empty());
    assert_eq!(mutation_count(&db).await, 3);
    assert_eq!(event_audit_count(&db).await, 3);
    let audits = db
        .orm()
        .query_all_raw(Statement::from_string(
            db.backend(),
            "SELECT actor_label, details_json FROM nuono_audit_events WHERE category = 'event'",
        ))
        .await
        .unwrap();
    for audit in audits {
        assert_eq!(
            audit.try_get::<String>("", "actor_label").unwrap(),
            "test-owner"
        );
        let data: String = audit.try_get("", "details_json").unwrap();
        assert!(!data.contains("OR 1=1") && !data.contains("initial"));
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_retry_creates_one_record_and_one_receipt() {
    let db = test_db(3).await;
    let request = request(rule_values(), None, None);
    let (left, right) = tokio::join!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &request,
            "owner"
        ),
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &request,
            "owner"
        )
    );
    assert_eq!(left.unwrap(), right.unwrap());
    assert_eq!(count(&db, "nuono_filter_rules").await, 1);
    assert_eq!(mutation_count(&db).await, 1);
    assert_eq!(event_audit_count(&db).await, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn audit_failure_rolls_back_record_and_retry_receipt() {
    let db = test_db(3).await;
    if db.backend() == DbBackend::Postgres {
        sql(&db, "CREATE FUNCTION reject_fixture_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture audit unavailable'; END; $$").await;
        sql(&db, "CREATE TRIGGER reject_fixture_audit BEFORE INSERT ON nuono_audit_events FOR EACH ROW EXECUTE FUNCTION reject_fixture_audit()").await;
    } else {
        sql(&db, "CREATE TRIGGER reject_fixture_audit BEFORE INSERT ON nuono_audit_events BEGIN SELECT RAISE(ABORT, 'fixture audit unavailable'); END").await;
    }
    let request = request(rule_values(), None, None);
    assert!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &request,
            "owner"
        )
        .await
        .is_err()
    );
    assert_eq!(count(&db, "nuono_filter_rules").await, 0);
    assert_eq!(mutation_count(&db).await, 0);
    let drop = if db.backend() == DbBackend::Postgres {
        "DROP TRIGGER reject_fixture_audit ON nuono_audit_events"
    } else {
        "DROP TRIGGER reject_fixture_audit"
    };
    sql(&db, drop).await;
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Create,
        &request,
        "owner",
    )
    .await
    .unwrap();
    assert_eq!(count(&db, "nuono_filter_rules").await, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutation_validation_preserves_protected_fields_and_schema_changes() {
    let db = test_db(2).await;
    for values in [
        json!({"pattern":"x"}),
        json!({"pattern":"x", "filter_type":null, "match_mode":"simple"}),
        json!({"pattern":"x", "filter_type":"receive", "match_mode":"x".repeat(11)}),
        json!({"id":"77", "pattern":"x", "filter_type":"receive", "match_mode":"simple"}),
        json!({"pattern":"x", "filter_type":{}, "match_mode":"simple"}),
    ] {
        assert!(
            db.mutate_table(
                "nuono_filter_rules",
                MutationOperation::Create,
                &request(values, None, None),
                "owner"
            )
            .await
            .is_err()
        );
    }
    assert!(matches!(
        db.mutate_table(
            "nuono_users",
            MutationOperation::Create,
            &request(json!({"role":"owner"}), None, None),
            "owner"
        )
        .await,
        Err(NuonoError::PermissionDenied(_))
    ));
    assert_eq!(count(&db, "nuono_filter_rules").await, 0);
    sql(
        &db,
        "ALTER TABLE nuono_filter_rules ADD COLUMN required_extra TEXT NOT NULL",
    )
    .await;
    assert!(
        !db.inspect_table("nuono_filter_rules")
            .await
            .unwrap()
            .table
            .management
            .create
    );
    assert!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &request(rule_values(), None, None),
            "owner"
        )
        .await
        .is_err()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn boolean_defaults_nulls_and_large_record_keys_preserve_types() {
    let db = test_db(2).await;
    let create = request(json!({"filter_type":"receive", "pattern":"x"}), None, None);
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Create,
        &create,
        "owner",
    )
    .await
    .unwrap();
    let before = rows(&db, "nuono_filter_rules").await.remove(0);
    assert_eq!(before["match_mode"], "simple");
    let update = request(
        json!({"enabled":false, "target_id":null}),
        Some(before),
        None,
    );
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Update,
        &update,
        "owner",
    )
    .await
    .unwrap();
    let after = rows(&db, "nuono_filter_rules").await.remove(0);
    assert!(after["enabled"] == "false" || after["enabled"] == "0");
    assert!(after["target_id"].is_null());
    let invalid_regex = request(
        json!({"match_mode":"regex", "pattern":"["}),
        Some(after),
        None,
    );
    assert!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &invalid_regex,
            "owner"
        )
        .await
        .is_err()
    );
    if db.backend() == DbBackend::Postgres {
        sql(
            &db,
            "ALTER TABLE nuono_filter_rules ALTER COLUMN id TYPE BIGINT",
        )
        .await;
    }
    sql(&db, "INSERT INTO nuono_filter_rules (id, pattern, filter_type, match_mode) VALUES (9007199254740993, 'large id', 'receive', 'simple')").await;
    let original = rows(&db, "nuono_filter_rules")
        .await
        .into_iter()
        .find(|row| row["id"] == "9007199254740993")
        .unwrap();
    let update = request(json!({"pattern":"updated"}), Some(original), None);
    let result = db
        .mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &update,
            "owner",
        )
        .await
        .unwrap();
    assert_eq!(result.key["id"], "9007199254740993");
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_receipts_survive_schema_changes_and_uuid_spelling() {
    let db = test_db(2).await;
    let create = request(rule_values(), None, None);
    let result = db
        .mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &create,
            "owner",
        )
        .await
        .unwrap();
    sql(&db, "ALTER TABLE nuono_filter_rules ADD COLUMN required_extra TEXT NOT NULL DEFAULT 'private-default-fixture'").await;
    let schema = db.inspect_table("nuono_filter_rules").await.unwrap();
    let extra = schema
        .columns
        .iter()
        .find(|column| column.name == "required_extra")
        .unwrap();
    assert!(extra.has_default && !extra.readable && extra.default_value.is_none());
    let mut retry = create.clone();
    retry.request_id = retry.request_id.to_uppercase();
    assert_eq!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &retry,
            "owner"
        )
        .await
        .unwrap(),
        result
    );
    assert_eq!(count(&db, "nuono_filter_rules").await, 1);
    assert_eq!(event_audit_count(&db).await, 1);
    db.shutdown().await.unwrap();
}

#[test]
fn digest_encoding_is_independent_of_object_key_order() {
    let mut left = Vec::new();
    let mut right = Vec::new();
    canonical_json(
        &serde_json::from_str(r#"{"z": {"b":1,"a":2},"a":3}"#).unwrap(),
        &mut left,
    )
    .unwrap();
    canonical_json(
        &serde_json::from_str(r#"{"a":3,"z":{"a":2,"b":1}}"#).unwrap(),
        &mut right,
    )
    .unwrap();
    assert_eq!(left, right);
}

#[tokio::test]
async fn concurrent_edits_have_one_winner_and_stale_delete_keeps_the_record() {
    let db = test_db(3).await;
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Create,
        &request(rule_values(), None, None),
        "owner",
    )
    .await
    .unwrap();
    let before = rows(&db, "nuono_filter_rules").await.remove(0);
    let left = request(
        json!({"pattern":"first writer"}),
        Some(before.clone()),
        None,
    );
    let right = request(
        json!({"pattern":"second writer"}),
        Some(before.clone()),
        None,
    );
    let (a, b) = tokio::join!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &left,
            "owner"
        ),
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Update,
            &right,
            "owner"
        ),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()).unwrap(),
        NuonoError::Conflict(_)
    ));
    let stale_delete = request(json!({}), Some(before), Some("nuono_filter_rules"));
    assert!(matches!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Delete,
            &stale_delete,
            "owner"
        )
        .await,
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(count(&db, "nuono_filter_rules").await, 1);
    assert_eq!(event_audit_count(&db).await, 2);
    assert_eq!(mutation_count(&db).await, 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn filter_rules_validate_type_and_manage_nullable_fields() {
    let db = test_db(2).await;
    let invalid_type = request(
        json!({"filter_type":"owner","target_id":"example","pattern":"reason"}),
        None,
        None,
    );
    assert!(matches!(
        db.mutate_table(
            "nuono_filter_rules",
            MutationOperation::Create,
            &invalid_type,
            "owner"
        )
        .await,
        Err(NuonoError::InvalidInput(_))
    ));
    let blacklist = request(
        json!({"filter_type":"blacklist_user","target_id":"example","pattern":"spam"}),
        None,
        None,
    );
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Create,
        &blacklist,
        "owner",
    )
    .await
    .unwrap();
    let original = rows(&db, "nuono_filter_rules").await.remove(0);
    assert_eq!(original["pattern"], "spam");
    let update = request(json!({"pattern":"updated reason"}), Some(original), None);
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Update,
        &update,
        "owner",
    )
    .await
    .unwrap();
    let current = rows(&db, "nuono_filter_rules").await.remove(0);
    assert_eq!(current["pattern"], "updated reason");
    db.mutate_table(
        "nuono_filter_rules",
        MutationOperation::Delete,
        &request(json!({}), Some(current), Some("nuono_filter_rules")),
        "owner",
    )
    .await
    .unwrap();
    assert!(rows(&db, "nuono_filter_rules").await.is_empty());
    db.shutdown().await.unwrap();
}
