use nuono_plugin::{Database, orm as sea_orm};
use sea_orm::{ActiveValue::Set, ConnectionTrait, QueryFilter, Schema, entity::prelude::*};
mod record {
    use nuono_plugin::orm as sea_orm;
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sdk_orm_record")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub name: String,
        pub optional: Option<String>,
        pub enabled: bool,
        pub payload: Json,
        pub date: Date,
        pub created_at: DateTimeUtc,
        pub bytes: Vec<u8>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
async fn setup() -> (nuono_core::Database, Database) {
    let host = nuono_core::test_support::test_db(2).await;
    let db = Database::new(host.clone());
    let backend = db.get_database_backend();
    db.execute_raw(backend.build(&Schema::new(backend).create_table_from_entity(record::Entity)))
        .await
        .unwrap();
    (host, db)
}
fn model(id: i64) -> record::ActiveModel {
    record::ActiveModel {
        id: Set(id),
        name: Set("中文 ' bound".into()),
        optional: Set(None),
        enabled: Set(true),
        payload: Set(serde_json::json!({"large":9_007_199_254_740_993i64,"nested":[1,"two"]})),
        date: Set(chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap()),
        created_at: Set(chrono::Utc::now()),
        bytes: Set(vec![0, 255, 1]),
    }
}

#[tokio::test]
async fn sdk_write_transactions_reserve_sqlite_writer_before_reading() {
    use std::time::Duration;
    let (host, db) = setup().await;
    let sqlite = db.get_database_backend() == sea_orm::DbBackend::Sqlite;
    let first = db.begin().await.unwrap();
    assert!(
        record::Entity::find_by_id(1)
            .one(&first)
            .await
            .unwrap()
            .is_none()
    );

    let (starting, started) = tokio::sync::oneshot::channel();
    let (entered, mut ready) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let second_db = db.clone();
    let second = tokio::spawn(async move {
        starting.send(()).unwrap();
        let tx = second_db.begin().await.unwrap();
        entered.send(()).unwrap();
        resume.await.unwrap();
        // 下一个写入事务中的读取能看到前一个已提交的写入结果，随后可安全执行写入。
        assert!(
            record::Entity::find_by_id(1)
                .one(&tx)
                .await
                .unwrap()
                .is_some()
        );
        model(2).insert(&tx).await.unwrap();
        tx.commit().await.unwrap();
    });
    started.await.unwrap();
    if sqlite {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut ready)
                .await
                .is_err(),
            "SQLite SDK transaction started a second writer before the first committed"
        );
    } else {
        // PostgreSQL 支持并发开启彼此独立的事务。
        tokio::time::timeout(Duration::from_secs(5), &mut ready)
            .await
            .unwrap()
            .unwrap();
    }
    model(1).insert(&first).await.unwrap();
    first.commit().await.unwrap();
    if sqlite {
        tokio::time::timeout(Duration::from_secs(5), &mut ready)
            .await
            .unwrap()
            .unwrap();
    }
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    assert!(
        record::Entity::find_by_id(2)
            .one(&db)
            .await
            .unwrap()
            .is_some()
    );
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn sdk_orm_entities_preserve_native_types_on_both_backends() {
    let (host, db) = setup().await;
    let id = 9_007_199_254_740_993i64;
    let inserted = model(id).insert(&db).await.unwrap();
    let row = record::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row, inserted);
    assert_eq!(row.id, id);
    assert_eq!(row.payload["large"].as_i64(), Some(id));
    assert_eq!(row.optional, None);
    assert_eq!(row.bytes, vec![0, 255, 1]);
    assert_eq!(
        record::Entity::find()
            .filter(record::Column::Name.eq("中文 ' bound"))
            .count(&db)
            .await
            .unwrap(),
        1
    );
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn sdk_orm_transactions_roll_back_and_keep_constraint_errors() {
    let (host, db) = setup().await;
    let tx = db.begin().await.unwrap();
    model(1).insert(&tx).await.unwrap();
    tx.commit().await.unwrap();
    let tx = db.begin().await.unwrap();
    model(2).insert(&tx).await.unwrap();
    let error = model(1).insert(&tx).await.unwrap_err();
    assert!(
        matches!(
            error.sql_err(),
            Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
        ),
        "{error:?}"
    );
    tx.rollback().await.unwrap();
    assert!(
        record::Entity::find_by_id(2)
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    let tx = db.begin().await.unwrap();
    model(3).insert(&tx).await.unwrap();
    drop(tx);
    assert!(
        record::Entity::find_by_id(3)
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn private_secrets_and_orm_rows_commit_or_roll_back_together() {
    let (host, db) = setup().await;
    for plugin in ["atomic-plugin", "other-plugin"] {
        host.sync_plugin(plugin, "1", &serde_json::json!({}))
            .await
            .unwrap();
    }
    let secrets = nuono_plugin::PluginSecretStore::new(host.clone(), "atomic-plugin", vec![]);
    let other = nuono_plugin::PluginSecretStore::new(host.clone(), "other-plugin", vec![]);
    let tx = db.begin().await.unwrap();
    model(1).insert(&tx).await.unwrap();
    secrets
        .set_private_in_transaction(&tx, "account_token", "secret-value")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        secrets
            .get_private("account_token")
            .await
            .unwrap()
            .as_deref(),
        Some("secret-value")
    );
    assert!(other.get_private("account_token").await.unwrap().is_none());

    let tx = db.begin().await.unwrap();
    model(2).insert(&tx).await.unwrap();
    secrets
        .set_private_in_transaction(&tx, "account_token", "replacement")
        .await
        .unwrap();
    secrets
        .set_private_in_transaction(&tx, "temporary", "temporary-secret")
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        record::Entity::find_by_id(2)
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        secrets
            .get_private("account_token")
            .await
            .unwrap()
            .as_deref(),
        Some("secret-value")
    );
    assert!(secrets.get_private("temporary").await.unwrap().is_none());

    let tx = db.begin().await.unwrap();
    assert!(
        secrets
            .set_private_in_transaction(&tx, "../invalid", "secret-value")
            .await
            .is_err()
    );
    secrets
        .set_private_in_transaction(&tx, "account_token", " ")
        .await
        .unwrap();
    drop(tx);
    assert_eq!(
        secrets
            .get_private("account_token")
            .await
            .unwrap()
            .as_deref(),
        Some("secret-value")
    );
    let tx = db.begin().await.unwrap();
    secrets
        .set_private_in_transaction(&tx, "account_token", "")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        secrets
            .get_private("account_token")
            .await
            .unwrap()
            .is_none()
    );
    host.shutdown().await.unwrap();
}
