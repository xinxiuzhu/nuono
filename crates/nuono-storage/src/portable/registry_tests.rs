use super::*;
use nuono_app::{
    plugin_data::PluginMigrationForBackend,
    plugin_manifest::{PersistentAsset, PluginStorageDeclaration, PluginStorageVersion},
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

async fn migration_count(db: &crate::Database) -> i64 {
    db.orm()
        .query_one_raw(Statement::from_string(
            db.backend(),
            "SELECT COUNT(*) AS n FROM nuono_plugin_migrations",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}

fn with_table() -> PluginStorageDeclaration {
    PluginStorageDeclaration::from_migrations(
        "registry_test",
        vec![PluginMigrationForBackend {
            version: 1,
            description: "create rows".into(),
            pg_sql: "CREATE TABLE registry_test_rows(id BIGINT PRIMARY KEY)".into(),
            sqlite_sql: "CREATE TABLE registry_test_rows(id INTEGER PRIMARY KEY) STRICT".into(),
        }],
    )
    .unwrap()
}

#[tokio::test]
async fn a_metadata_only_plugin_can_introduce_storage_and_then_owns_its_prefix() {
    let db = crate::test_support::test_db(2).await;
    let id = "NuonoExample";
    let empty = PluginStorageDeclaration::from_migrations(id, vec![]).unwrap();
    db.register_plugin_storage(id, "1", &empty).await.unwrap();
    let mut declaration = with_table();
    declaration.manifest.assets = empty.manifest.assets;
    db.register_plugin_storage(id, "2", &declaration)
        .await
        .unwrap();
    let registered = registered_plugins(db.orm()).await.unwrap();
    assert_eq!(registered[0].manifest.table_prefix, "registry_test_");
    assert_eq!(registered[0].version, Some(1));
    declaration.manifest.table_prefix = "registry_".into();
    assert!(
        db.register_plugin_storage(id, "3", &declaration)
            .await
            .is_err()
    );
    assert_eq!(registered_plugins(db.orm()).await.unwrap(), registered);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn plugins_without_tables_register_metadata_without_fabricated_history() {
    let db = crate::test_support::test_db(2).await;
    for (id, prefix) in [
        ("SampleNN", "sample_nn_"),
        ("demo-kit", "demo_kit_"),
        ("NuonoBridgeAdapter", "nuono_bridge_adapter_"),
    ] {
        let mut declaration = PluginStorageDeclaration::from_migrations(id, vec![]).unwrap();
        assert!(declaration.versions.is_empty());
        // 将表前缀故意改回未归一化的原始插件 ID，测试存储层入口（而不仅仅是构造函数）也会执行表前缀归一化。
        declaration.manifest.table_prefix = id.into();
        db.register_plugin_storage(id, "1.0", &declaration)
            .await
            .unwrap();
        declaration
            .manifest
            .assets
            .push(PersistentAsset::DataDirectory {
                key: "extra".into(),
                path: format!("{id}/extra"),
            });
        db.register_plugin_storage(id, "1.1", &declaration)
            .await
            .unwrap();
        db.mark_plugin_code_absent(id).await.unwrap();
        let registered = registered_plugins(db.orm()).await.unwrap();
        let record = registered.iter().find(|p| p.plugin_id == id).unwrap();
        assert_eq!(record.version, None);
        assert_eq!(record.code_version.as_deref(), Some("1.1"));
        assert!(!record.installed);
        assert_eq!(record.manifest.table_prefix, prefix);
        assert_eq!(record.manifest.assets.len(), 2);
    }
    assert_eq!(migration_count(&db).await, 0);
    let schema = registered_schema(db.orm()).await.unwrap();
    let image = export(db.orm(), &schema).await.unwrap();
    let target = super::tests::sqlite().await;
    import(&target, &Schema::from_image(&image).unwrap(), &image)
        .await
        .unwrap();
    assert_eq!(
        registered_plugins(&target).await.unwrap(),
        registered_plugins(db.orm()).await.unwrap()
    );
    target.close().await.unwrap();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn migration_table_has_only_execution_fields_and_metadata_updates_do_not_rewrite_history() {
    let db = crate::test_support::test_db(2).await;
    let mut declaration = with_table();
    db.register_plugin_storage("registry_test", "1", &declaration)
        .await
        .unwrap();
    let query = match db.backend() {
        DbBackend::Sqlite => {
            "SELECT name FROM pragma_table_info('nuono_plugin_migrations') ORDER BY cid"
        }
        DbBackend::Postgres => {
            "SELECT column_name AS name FROM information_schema.columns WHERE table_schema='public' AND table_name='nuono_plugin_migrations' ORDER BY ordinal_position"
        }
        _ => unreachable!(),
    };
    let columns: Vec<String> = db
        .orm()
        .query_all_raw(Statement::from_string(db.backend(), query))
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get("", "name").unwrap())
        .collect();
    assert_eq!(
        columns,
        [
            "plugin_id",
            "version",
            "description",
            "checksum",
            "applied_at_ms"
        ]
    );
    let ledger = || {
        Statement::from_string(
            db.backend(),
            "SELECT checksum,applied_at_ms FROM nuono_plugin_migrations",
        )
    };
    let before = db.orm().query_one_raw(ledger()).await.unwrap().unwrap();
    declaration
        .manifest
        .assets
        .push(PersistentAsset::DataDirectory {
            key: "images".into(),
            path: "registry_test/images".into(),
        });
    db.register_plugin_storage("registry_test", "2", &declaration)
        .await
        .unwrap();
    let after = db.orm().query_one_raw(ledger()).await.unwrap().unwrap();
    assert_eq!(
        before.try_get::<String>("", "checksum").unwrap(),
        after.try_get::<String>("", "checksum").unwrap()
    );
    assert_eq!(
        before.try_get::<i64>("", "applied_at_ms").unwrap(),
        after.try_get::<i64>("", "applied_at_ms").unwrap()
    );
    assert_eq!(
        after.try_get::<String>("", "checksum").unwrap(),
        declaration.versions[0].checksum().unwrap()
    );
    assert_eq!(migration_count(&db).await, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_comment_only_data_only_and_noop_ddl_cannot_create_migration_records() {
    let db = crate::test_support::test_db(2).await;
    let original = with_table();
    db.register_plugin_storage("registry_test", "1", &original)
        .await
        .unwrap();
    for sql in [
        "",
        " -- no migration\n",
        "SELECT 1",
        "INSERT INTO registry_test_rows VALUES(1)",
        "CREATE TABLE IF NOT EXISTS registry_test_rows(id INTEGER PRIMARY KEY)",
    ] {
        let mut declaration = original.clone();
        declaration.versions.push(PluginStorageVersion {
            version: 2,
            description: "noop".into(),
            postgres_sql: sql.into(),
            sqlite_sql: sql.into(),
        });
        assert!(
            db.register_plugin_storage("registry_test", "2", &declaration)
                .await
                .is_err(),
            "{sql:?}"
        );
        assert_eq!(migration_count(&db).await, 1);
        let rows = db
            .orm()
            .query_one_raw(Statement::from_string(
                db.backend(),
                "SELECT COUNT(*) AS n FROM registry_test_rows",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rows.try_get::<i64>("", "n").unwrap(),
            0,
            "failed migrations must roll back data as well as history"
        );
    }
    assert_eq!(
        registered_plugins(db.orm()).await.unwrap()[0]
            .code_version
            .as_deref(),
        Some("1")
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn normalized_prefix_collisions_and_invalid_prefixes_fail_atomically() {
    let db = crate::test_support::test_db(2).await;
    let first = PluginStorageDeclaration::from_migrations("SampleNN", vec![]).unwrap();
    db.register_plugin_storage("SampleNN", "1", &first)
        .await
        .unwrap();
    let duplicate = PluginStorageDeclaration::from_migrations("sample_nn", vec![]).unwrap();
    assert!(
        db.register_plugin_storage("sample_nn", "1", &duplicate)
            .await
            .is_err()
    );
    for prefix in [
        "x; DROP TABLE nuono_plugins",
        "1bad_",
        "bad.prefix",
        "中文_",
    ] {
        let mut declaration = PluginStorageDeclaration::from_migrations("bad", vec![]).unwrap();
        declaration.manifest.table_prefix = prefix.into();
        assert!(
            db.register_plugin_storage("bad", "1", &declaration)
                .await
                .is_err(),
            "{prefix}"
        );
    }
    for prefix in ["Bad_", "bad-name_", "bad__name_", "missing"] {
        assert!(
            db.orm()
                .execute_raw(Statement::from_sql_and_values(
                    db.backend(),
                    "UPDATE nuono_plugins SET table_prefix=$1 WHERE plugin_id='SampleNN'",
                    [prefix.into()]
                ))
                .await
                .is_err(),
            "database constraint: {prefix}"
        );
    }
    assert_eq!(registered_plugins(db.orm()).await.unwrap().len(), 1);
    assert_eq!(migration_count(&db).await, 0);
    db.shutdown().await.unwrap();
}
