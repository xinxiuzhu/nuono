use super::*;
use crate::{
    db::{
        BrowseRequest, DatabaseColumnPolicy as Column, DatabaseFieldType as Kind,
        DatabaseTablePolicy as Table,
    },
    test_support::test_db,
};
use serde_json::json;

fn request(values: Value, original: Option<Value>, confirmation: Option<&str>) -> MutationRequest {
    MutationRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        values: values.as_object().unwrap().clone(),
        original: original.map(|row| row.as_object().unwrap().clone()),
        confirmation: confirmation.map(str::to_owned),
    }
}

async fn fixture(db: &Database) -> Table {
    let (boolean, integer, json_type) = if db.backend() == DbBackend::Postgres {
        ("BOOLEAN", "BIGINT", "JSONB")
    } else {
        ("INTEGER", "integer", "TEXT")
    };
    db.orm().execute_unprepared(&format!("CREATE TABLE plugin_notes (note_key TEXT PRIMARY KEY, title TEXT NOT NULL, enabled {boolean} NOT NULL DEFAULT TRUE, count {integer} NOT NULL DEFAULT 0, document {json_type}, secret TEXT NOT NULL DEFAULT 'private-fixture', created_at TEXT DEFAULT 'fixture-time')")).await.unwrap();
    Table::read(
        "plugin_notes",
        vec![
            Column::edit("note_key", Kind::Text),
            Column::edit("title", Kind::Text).max_length(40),
            Column::edit("enabled", Kind::Boolean),
            Column::edit("count", Kind::Integer),
            Column::edit("document", Kind::Json),
            Column::read("created_at"),
        ],
    )
    .manage()
}

async fn rows(db: &Database, policy: &Table) -> Vec<Value> {
    db.browse_table_with_policy(&policy.table, &BrowseRequest::default(), Some(policy))
        .await
        .unwrap()
        .rows
}

#[tokio::test]
async fn plugin_crud_preserves_manual_keys_large_integers_json_and_private_columns() {
    let db = test_db(3).await;
    let policy = fixture(&db).await;
    let schema = db
        .inspect_table_with_policy("plugin_notes", Some(&policy))
        .await
        .unwrap();
    let key = schema
        .columns
        .iter()
        .find(|column| column.name == "note_key")
        .unwrap();
    assert!(key.insertable && !key.writable);
    assert!(
        schema.table.management.create
            && schema.table.management.update
            && schema.table.management.delete
    );
    let secret = schema
        .columns
        .iter()
        .find(|column| column.name == "secret")
        .unwrap();
    assert!(
        !secret.readable
            && !secret.writable
            && secret.has_default
            && secret.default_value.is_none()
    );
    let create = request(
        json!({"note_key":"new-key","title":"test","count":"9007199254740993","document":r#"{"id":9007199254740993}"#}),
        None,
        None,
    );
    let first = db
        .mutate_table_with_policy(
            "plugin_notes",
            MutationOperation::Create,
            &create,
            "owner",
            Some(&policy),
        )
        .await
        .unwrap();
    assert_eq!(first.key["note_key"], "new-key");
    assert_eq!(
        first,
        db.mutate_table_with_policy(
            "plugin_notes",
            MutationOperation::Create,
            &create,
            "owner",
            Some(&policy)
        )
        .await
        .unwrap()
    );
    assert!(matches!(
        db.mutate_table("plugin_notes", MutationOperation::Create, &create, "owner")
            .await,
        Err(NuonoError::PermissionDenied(_))
    ));
    let mut revoked_key = policy.clone();
    revoked_key
        .columns
        .retain(|column| column.name != "note_key");
    assert!(matches!(
        db.mutate_table_with_policy(
            "plugin_notes",
            MutationOperation::Create,
            &create,
            "owner",
            Some(&revoked_key)
        )
        .await,
        Err(NuonoError::PermissionDenied(_))
    ));
    let original = rows(&db, &policy).await.remove(0);
    assert_eq!(original["count"], "9007199254740993");
    assert!(!original.as_object().unwrap().contains_key("secret"));
    assert_eq!(
        serde_json::from_str::<Value>(original["document"].as_str().unwrap()).unwrap()["id"]
            .as_u64(),
        Some(9007199254740993)
    );
    for values in [
        json!({"note_key":"changed"}),
        json!({"secret":"leak"}),
        json!({"count":"9223372036854775808"}),
        json!({"count":"1.5"}),
        json!({"count":9007199254740993u64}),
        json!({"document":"{"}),
        json!({"title":"x".repeat(41)}),
    ] {
        assert!(matches!(
            db.mutate_table_with_policy(
                "plugin_notes",
                MutationOperation::Update,
                &request(values, Some(original.clone()), None),
                "owner",
                Some(&policy)
            )
            .await,
            Err(NuonoError::InvalidInput(_))
        ));
    }
    let update = request(
        json!({"enabled":false,"document":"null"}),
        Some(original.clone()),
        None,
    );
    db.mutate_table_with_policy(
        "plugin_notes",
        MutationOperation::Update,
        &update,
        "owner",
        Some(&policy),
    )
    .await
    .unwrap();
    let current = rows(&db, &policy).await.remove(0);
    assert_eq!(current["document"], "null");
    assert!(current["enabled"] == "false" || current["enabled"] == "0");
    assert!(matches!(
        db.mutate_table_with_policy(
            "plugin_notes",
            MutationOperation::Delete,
            &request(json!({}), Some(original), Some("plugin_notes")),
            "owner",
            Some(&policy)
        )
        .await,
        Err(NuonoError::Conflict(_))
    ));
    db.mutate_table_with_policy(
        "plugin_notes",
        MutationOperation::Update,
        &request(json!({"document":null}), Some(current), None),
        "owner",
        Some(&policy),
    )
    .await
    .unwrap();
    let current = rows(&db, &policy).await.remove(0);
    assert!(current["document"].is_null());
    let exported = db
        .export_table_with_policy("plugin_notes", &BrowseRequest::default(), Some(&policy))
        .await
        .unwrap();
    assert!(!exported.columns.contains(&"secret".to_owned()));
    db.mutate_table_with_policy(
        "plugin_notes",
        MutationOperation::Delete,
        &request(json!({}), Some(current), Some("plugin_notes")),
        "owner",
        Some(&policy),
    )
    .await
    .unwrap();
    assert!(rows(&db, &policy).await.is_empty());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn undeclared_fields_incompatible_types_and_core_overrides_fail_closed() {
    let db = test_db(2).await;
    let mut policy = fixture(&db).await;
    assert!(
        !db.inspect_table("plugin_notes")
            .await
            .unwrap()
            .table
            .browsable
    );
    assert!(
        db.browse_table("plugin_notes", &BrowseRequest::default())
            .await
            .is_err()
    );
    assert!(
        db.mutate_table(
            "plugin_notes",
            MutationOperation::Create,
            &request(json!({"title":"test"}), None, None),
            "owner"
        )
        .await
        .is_err()
    );
    for key in [json!(null), json!("")] {
        assert!(matches!(
            db.mutate_table_with_policy(
                "plugin_notes",
                MutationOperation::Create,
                &request(json!({"note_key":key,"title":"test"}), None, None),
                "owner",
                Some(&policy)
            )
            .await,
            Err(NuonoError::InvalidInput(_))
        ));
    }
    policy
        .columns
        .iter_mut()
        .find(|column| column.name == "title")
        .unwrap()
        .edit_type = Some(Kind::Integer);
    assert!(
        !db.inspect_table_with_policy("plugin_notes", Some(&policy))
            .await
            .unwrap()
            .table
            .management
            .create
    );
    db.orm()
        .execute_unprepared("ALTER TABLE plugin_notes ADD COLUMN required_extra TEXT NOT NULL")
        .await
        .unwrap();
    assert!(
        !db.inspect_table_with_policy("plugin_notes", Some(&policy))
            .await
            .unwrap()
            .table
            .management
            .create
    );
    policy.table = "nuono_users".into();
    assert!(matches!(
        db.inspect_table_with_policy("nuono_users", Some(&policy))
            .await,
        Err(NuonoError::PermissionDenied(_))
    ));
    assert!(
        db.inspect_table_with_policy("plugin_notes", Some(&policy))
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn generated_defaults_and_composite_keys_support_creation_without_key_updates() {
    let db = test_db(2).await;
    let key_type = if db.backend() == DbBackend::Postgres {
        "BIGSERIAL PRIMARY KEY"
    } else {
        "INTEGER PRIMARY KEY AUTOINCREMENT"
    };
    db.orm().execute_unprepared(&format!("CREATE TABLE plugin_defaults (id {key_type}, label TEXT NOT NULL DEFAULT 'database default')")).await.unwrap();
    let mut policy = Table::read(
        "plugin_defaults",
        vec![Column::read("id"), Column::read("label")],
    );
    policy.create = true;
    policy.delete = true;
    let create = request(json!({}), None, None);
    assert!(
        db.inspect_table_with_policy("plugin_defaults", Some(&policy))
            .await
            .unwrap()
            .table
            .management
            .create
    );
    db.mutate_table_with_policy(
        "plugin_defaults",
        MutationOperation::Create,
        &create,
        "owner",
        Some(&policy),
    )
    .await
    .unwrap();
    assert_eq!(rows(&db, &policy).await[0]["label"], "database default");
    db.orm().execute_unprepared("CREATE TABLE plugin_tags (account TEXT, tag TEXT, note TEXT NOT NULL, PRIMARY KEY (account, tag))").await.unwrap();
    let policy = Table::read(
        "plugin_tags",
        ["account", "tag", "note"]
            .into_iter()
            .map(|name| Column::edit(name, Kind::Text))
            .collect(),
    )
    .manage();
    let create = request(
        json!({"account":"account","tag":"tag","note":"content"}),
        None,
        None,
    );
    let result = db
        .mutate_table_with_policy(
            "plugin_tags",
            MutationOperation::Create,
            &create,
            "owner",
            Some(&policy),
        )
        .await
        .unwrap();
    assert_eq!(result.key.len(), 2);
    let original = rows(&db, &policy).await.remove(0);
    assert!(
        db.mutate_table_with_policy(
            "plugin_tags",
            MutationOperation::Update,
            &request(json!({"account":"overwrite"}), Some(original.clone()), None),
            "owner",
            Some(&policy)
        )
        .await
        .is_err()
    );
    db.mutate_table_with_policy(
        "plugin_tags",
        MutationOperation::Delete,
        &request(json!({}), Some(original), Some("plugin_tags")),
        "owner",
        Some(&policy),
    )
    .await
    .unwrap();
    assert!(rows(&db, &policy).await.is_empty());
    db.shutdown().await.unwrap();
}
