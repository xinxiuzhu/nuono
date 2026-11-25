//! 使用独立的临时数据库文件验证 SQLite 初始建表迁移。

use super::run_framework;
use crate::{config::DatabaseConfig, db::Database, plugin_kv::PluginKVStore};
use sea_orm::{
    ConnectOptions, ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement,
    TransactionTrait,
};
use std::{path::PathBuf, time::Duration};

struct SqliteFixture {
    path: PathBuf,
    config: DatabaseConfig,
}

impl SqliteFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nuono_test_sqlite_baseline_{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        Self {
            config: DatabaseConfig {
                url: format!("sqlite://{}?mode=rwc", path.display()),
                max_connections: 2,
            },
            path,
        }
    }

    async fn connect_raw(&self) -> DatabaseConnection {
        let mut options = ConnectOptions::new(self.config.url.clone());
        options.max_connections(1).min_connections(1);
        options.map_sqlx_sqlite_opts(|options| options.foreign_keys(true));
        sea_orm::Database::connect(options).await.unwrap()
    }
}

impl Drop for SqliteFixture {
    fn drop(&mut self) {
        crate::test_support::remove_sqlite_test_files(&self.path.to_string_lossy());
    }
}

async fn applied_at(conn: &DatabaseConnection) -> i64 {
    let rows = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT version, applied_at_ms FROM nuono_schema_migrations",
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get::<i64>("", "version").unwrap(), 1);
    rows[0].try_get("", "applied_at_ms").unwrap()
}

async fn assert_healthy(conn: &DatabaseConnection) {
    assert!(
        conn.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA foreign_key_check"
        ))
        .await
        .unwrap()
        .is_empty()
    );
    let row = conn
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "PRAGMA integrity_check",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "integrity_check").unwrap(), "ok");
}

#[tokio::test]
async fn sqlite_baseline_bootstraps_once_and_reopens_with_data_intact() {
    let fixture = SqliteFixture::new();
    let db = Database::from_config(&fixture.config).await.unwrap();
    let original_ledger = applied_at(db.orm()).await;
    db.register_plugin_storage(
        "fixture",
        "1",
        &nuono_app::plugin_manifest::PluginStorageDeclaration::from_migrations("fixture", vec![])
            .unwrap(),
    )
    .await
    .unwrap();
    let kv = PluginKVStore::new(db.clone(), "fixture");
    kv.set("state", "{\"count\":42}").await.unwrap();
    db.set_global_config("fixture-setting", serde_json::json!({"enabled":true}))
        .await
        .unwrap();
    db.shutdown().await.unwrap();

    let reopened = Database::from_config(&fixture.config).await.unwrap();
    assert_eq!(applied_at(reopened.orm()).await, original_ledger);
    assert_eq!(
        PluginKVStore::new(reopened.clone(), "fixture")
            .get("state")
            .await
            .unwrap()
            .as_deref(),
        Some("{\"count\":42}")
    );
    assert_eq!(
        reopened.get_global_config("fixture-setting").await.unwrap(),
        Some(serde_json::json!({"enabled":true}))
    );
    assert_healthy(reopened.orm()).await;
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn sqlite_baseline_creation_rolls_back_on_storage_failure() {
    let fixture = SqliteFixture::new();
    let conn = fixture.connect_raw().await;
    conn.execute_unprepared("PRAGMA max_page_count=4")
        .await
        .unwrap();
    assert!(run_framework(&conn).await.is_err());
    assert!(
        conn.query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*'"
        ))
        .await
        .unwrap()
        .is_empty()
    );
    conn.execute_unprepared("PRAGMA max_page_count=100000")
        .await
        .unwrap();
    run_framework(&conn).await.unwrap();
    applied_at(&conn).await;
    assert_healthy(&conn).await;
    conn.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_baseline_rejects_an_unrecognized_ledger_without_replacing_its_data() {
    let fixture = SqliteFixture::new();
    let conn = fixture.connect_raw().await;
    conn.execute_unprepared(
        "CREATE TABLE nuono_schema_migrations(version TEXT PRIMARY KEY,applied_at BIGINT); INSERT INTO nuono_schema_migrations (version, applied_at) VALUES ('fixture_other_schema', 1);
         CREATE TABLE retained_fixture (value TEXT NOT NULL);
         INSERT INTO retained_fixture VALUES ('original');",
    )
    .await
    .unwrap();
    conn.close().await.unwrap();
    let error = Database::from_config(&fixture.config).await.err().unwrap();
    assert!(error.to_string().contains("旧开发数据库"));

    let conn = fixture.connect_raw().await;
    assert!(
        conn.query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE name='nuono_plugin_kv'"
        ))
        .await
        .unwrap()
        .is_none()
    );
    let row = conn
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT value FROM retained_fixture",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "value").unwrap(), "original");
    conn.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_concurrent_startups_apply_the_baseline_once() {
    let fixture = SqliteFixture::new();
    let (first, second) = tokio::join!(
        Database::from_config(&fixture.config),
        Database::from_config(&fixture.config)
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(
        applied_at(first.orm()).await,
        applied_at(second.orm()).await
    );
    assert_healthy(second.orm()).await;
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn sqlite_startup_waits_for_a_reader_before_switching_to_wal() {
    let fixture = SqliteFixture::new();
    let conn = fixture.connect_raw().await;
    run_framework(&conn).await.unwrap();
    let reader = conn.begin().await.unwrap();
    reader
        .query_one_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT COUNT(*) FROM nuono_schema_migrations",
        ))
        .await
        .unwrap();

    // 确认在触发 Database::from_config 启动前，测试连接确实正持有传统回滚日志模式（rollback-journal）的读锁并阻断立即切换 WAL。
    let mut probe = ConnectOptions::new(fixture.config.url.clone());
    probe.max_connections(1).min_connections(1);
    probe.map_sqlx_sqlite_opts(|options| {
        options
            .busy_timeout(Duration::ZERO)
            .pragma("journal_mode", "WAL")
    });
    let error = sea_orm::Database::connect(probe).await.unwrap_err();
    let DbErr::Conn(sea_orm::RuntimeErr::SqlxError(error)) = error else {
        panic!("expected a typed SQLite connection error, got {error:?}");
    };
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("5")
    );

    let opening = Database::from_config(&fixture.config);
    tokio::pin!(opening);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut opening)
            .await
            .is_err()
    );
    assert!(crate::db::SqliteDatabaseLock::acquire_exclusive(&fixture.path).is_err());
    reader.commit().await.unwrap();
    conn.close().await.unwrap();

    let db = tokio::time::timeout(Duration::from_secs(20), opening)
        .await
        .expect("startup should recover after the reader releases its lock")
        .unwrap();
    applied_at(db.orm()).await;
    assert_healthy(db.orm()).await;
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn core_baseline_has_28_tables_and_optional_ai_module_installs_14_tables_on_demand() {
    let fixture = SqliteFixture::new();
    let db = Database::from_config(&fixture.config).await.unwrap();

    let core_tables: Vec<String> = db
        .orm()
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("", "name").unwrap())
        .collect();
    assert_eq!(
        core_tables.len(),
        28,
        "expected 28 core tables: {core_tables:?}"
    );
    assert!(!db.is_ai_module_installed().await.unwrap());
    assert!(db.list_ai_models().await.unwrap().is_empty());
    assert!(db.list_ai_personas().await.unwrap().is_empty());
    assert!(db.list_agent_runs(None, 10).await.unwrap().is_empty());
    assert_eq!(
        db.reconcile_agent_runs_before_start("startup")
            .await
            .unwrap()
            .interrupted_runs,
        0
    );
    assert_eq!(
        db.reconcile_assistant_turns_before_start().await.unwrap(),
        0
    );

    // 验证仅安装核心表（共 28 张表）时，跨数据库可移植导出、导入与校验全流程正常工作。
    let core_schema = crate::portable::registered_schema(db.orm()).await.unwrap();
    assert_eq!(core_schema.tables.len(), 28);
    let core_image = crate::portable::export(db.orm(), &core_schema)
        .await
        .unwrap();
    assert!(core_image.deployment.as_ref().unwrap().modules.is_empty());
    let core_target = SqliteFixture::new();
    let core_target_conn = core_target.connect_raw().await;
    let core_target_schema = crate::portable::Schema::from_image(&core_image).unwrap();
    crate::portable::import(&core_target_conn, &core_target_schema, &core_image)
        .await
        .unwrap();
    crate::portable::verify(&core_target_conn, &core_target_schema, &core_image)
        .await
        .unwrap();
    assert_healthy(&core_target_conn).await;
    core_target_conn.close().await.unwrap();

    // 验证并发按需安装 AI 模块具备幂等性，安装后新增 14 张 AI 模块表（全库共 42 张表）。
    let (a, b) = tokio::join!(db.ensure_ai_module(), db.ensure_ai_module());
    a.unwrap();
    b.unwrap();
    assert!(db.is_ai_module_installed().await.unwrap());
    let all_tables: Vec<String> = db
        .orm()
        .query_all_raw(Statement::from_string(
            DbBackend::Sqlite,
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("", "name").unwrap())
        .collect();
    assert_eq!(
        all_tables.len(),
        42,
        "expected 42 tables with AI module: {all_tables:?}"
    );
    assert_healthy(db.orm()).await;

    // 验证启用 AI 模块后（共 42 张表），跨数据库可移植导出、导入与校验同样能完整往返。
    let ai_schema = crate::portable::registered_schema(db.orm()).await.unwrap();
    assert_eq!(ai_schema.tables.len(), 42);
    let ai_image = crate::portable::export(db.orm(), &ai_schema).await.unwrap();
    assert_eq!(ai_image.deployment.as_ref().unwrap().modules, vec!["ai"]);
    let ai_target = SqliteFixture::new();
    let ai_target_conn = ai_target.connect_raw().await;
    let ai_target_schema = crate::portable::Schema::from_image(&ai_image).unwrap();
    crate::portable::import(&ai_target_conn, &ai_target_schema, &ai_image)
        .await
        .unwrap();
    crate::portable::verify(&ai_target_conn, &ai_target_schema, &ai_image)
        .await
        .unwrap();
    assert_healthy(&ai_target_conn).await;
    ai_target_conn.close().await.unwrap();

    // 重新打开已安装过 AI 模块的数据库，应保持 AI 模块就绪状态。
    db.shutdown().await.unwrap();
    let reopened = Database::from_config(&fixture.config).await.unwrap();
    assert!(reopened.is_ai_module_installed().await.unwrap());
    assert_healthy(reopened.orm()).await;
    reopened.shutdown().await.unwrap();
}
