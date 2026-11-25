//! 数据库连接池初始化（支持 PostgreSQL 与 SQLite 双后端，全局共享单个 SeaORM 连接池）。
//! 框架表结构创建与迁移历史统一由 `nuono_schema_migrations` 迁移台账表记录。
use std::time::Duration;

use sea_orm::{ConnectOptions, DatabaseConnection, DbBackend, DbErr, RuntimeErr};

use crate::config::DatabaseConfig;
use crate::error::{NuonoError, Result};

pub(crate) struct DbHandles {
    pub conn: DatabaseConnection,
    pub backend: DbBackend,
    pub sqlite_lock: Option<super::SqliteDatabaseLock>,
    pub ai_installed: bool,
}

pub(crate) fn is_sqlite_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("sqlite:") || lower == ":memory:"
}

fn url_has_param(url: &str, key: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.split('&').any(|kv| {
        kv.split('=')
            .next()
            .is_some_and(|k| k.eq_ignore_ascii_case(key))
    })
}

fn connection_options(url: &str, max_connections: u32) -> ConnectOptions {
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(max_connections.max(1))
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300));
    options
}

fn sqlite_connection_is_locked(error: &DbErr) -> bool {
    let DbErr::Conn(RuntimeErr::SqlxError(error)) = error else {
        return false;
    };
    error
        .as_database_error()
        .filter(|error| {
            error
                .try_downcast_ref::<sea_orm::SqlxSqliteError>()
                .is_some()
        })
        .and_then(|error| error.code())
        .is_some_and(|code| sqlite_is_lock_code(&code))
}

fn sqlite_is_lock_code(code: &str) -> bool {
    // SQLite 错误码可能带有高位扩展结果码，低 8 位（0xff）才是主错误码（5=SQLITE_BUSY, 6=SQLITE_LOCKED）。
    code.parse::<u32>()
        .is_ok_and(|code| matches!(code & 0xff, 5 | 6))
}

async fn retry_sqlite_connection<C, F>(
    mut connect: C,
    budget: Duration,
    retry_delay: Duration,
) -> std::result::Result<DatabaseConnection, DbErr>
where
    C: FnMut() -> F,
    F: std::future::Future<Output = std::result::Result<DatabaseConnection, DbErr>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut last_lock_error = None;
    loop {
        let error = match tokio::time::timeout_at(deadline, connect()).await {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(error)) => error,
            Err(_) => {
                return Err(last_lock_error
                    .unwrap_or(DbErr::ConnectionAcquire(sea_orm::ConnAcquireErr::Timeout)));
            }
        };
        if !sqlite_connection_is_locked(&error) {
            return Err(error);
        }
        let next_attempt = tokio::time::Instant::now() + retry_delay;
        if next_attempt >= deadline {
            return Err(error);
        }
        last_lock_error = Some(error);
        tokio::time::sleep_until(next_attempt).await;
    }
}

#[tracing::instrument(skip(cfg), err(Debug))]
pub(crate) async fn connect_and_migrate(cfg: &DatabaseConfig) -> Result<DbHandles> {
    let mut handles = if is_sqlite_url(&cfg.url) {
        connect_sqlite(cfg).await?
    } else {
        connect_postgres(cfg).await?
    };
    match super::migrations::run_framework(&handles.conn).await {
        Ok(ai_installed) => handles.ai_installed = ai_installed,
        Err(error) => {
            let _ = handles.conn.close_by_ref().await;
            return Err(error);
        }
    }
    tracing::debug!(backend = ?handles.backend, ai_installed = handles.ai_installed, "数据库迁移完成");
    Ok(handles)
}

async fn connect_postgres(cfg: &DatabaseConfig) -> Result<DbHandles> {
    let mut options = connection_options(&cfg.url, cfg.max_connections);
    // 连接池中的每个物理 PostgreSQL 会话都持有一把共享维护锁（pg_try_advisory_lock_shared），
    // 其生命周期与底层连接绑定（不受 Database 包装层或 ORM 克隆影响）；离线迁移工具则需要获取与之互斥的排他锁。
    options.idle_timeout(None).max_lifetime(None);
    options.map_sqlx_postgres_pool_opts(|pool| {
        pool.after_connect(|connection, _| {
            Box::pin(async move {
                let acquired: bool =
                    sea_orm::sqlx::query_scalar("SELECT pg_try_advisory_lock_shared($1)")
                        .bind(0x4e55_4f4e_4f4d_4149_i64)
                        .fetch_one(&mut *connection)
                        .await?;
                if !acquired {
                    return Err(sea_orm::sqlx::Error::Protocol(
                        "数据库正在离线迁移，拒绝启动运行连接".into(),
                    ));
                }
                Ok(())
            })
        })
    });
    let conn = sea_orm::Database::connect(options)
        .await
        .map_err(|error| NuonoError::DatabaseConnection(format!("PostgreSQL 连接失败: {error}")))?;
    Ok(DbHandles {
        conn,
        backend: DbBackend::Postgres,
        sqlite_lock: None,
        ai_installed: false,
    })
}

async fn connect_sqlite(cfg: &DatabaseConfig) -> Result<DbHandles> {
    let filename = super::sqlite_database_path(&cfg.url)?;
    let in_memory = filename.is_none();
    if let Some(filename) = &filename
        && let Some(parent) = filename.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        create_private_parent(parent).map_err(|error| {
            NuonoError::DatabaseConnection(format!("SQLite 数据库父目录创建失败: {error}"))
        })?;
    }
    let sqlite_lock = filename
        .as_deref()
        .map(super::SqliteDatabaseLock::acquire_shared)
        .transpose()?;
    let url = if cfg.url == ":memory:" {
        "sqlite::memory:"
    } else {
        &cfg.url
    };
    let mut options = connection_options(url, if in_memory { 1 } else { cfg.max_connections });
    let create_if_missing = !url_has_param(&cfg.url, "mode");
    let default_wal = !in_memory && !url_has_param(&cfg.url, "journal");
    options.map_sqlx_sqlite_opts(move |mut options| {
        if !in_memory && create_if_missing {
            options = options.create_if_missing(true);
        }
        if default_wal {
            options = options.pragma("journal_mode", "WAL");
        }
        options
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(15))
            .pragma("synchronous", "NORMAL")
    });
    // 将文件锁句柄交由底层连接池的 after_connect 回调持有：
    // 若仅挂在顶层 Database 结构体上，当调用方保留了 orm().clone() 而丢弃 Database 时会过早释放文件锁。
    let pool_lock = sqlite_lock.clone();
    options.map_sqlx_sqlite_pool_opts(move |pool| {
        let pool_lock = pool_lock.clone();
        pool.after_connect(move |_, _| {
            let guard = pool_lock.clone();
            Box::pin(async move {
                let _guard = guard;
                Ok(())
            })
        })
    });
    // 首次将 SQLite 切换到 journal_mode=WAL 时，需要在迁移开始前立即获取独占锁；
    // SQLite 内置的 busy_timeout 不会在每次日志模式切换冲突时自动等待，
    // 因此并发打开尚未切到 WAL 的数据库时必须重试整个建连过程，并在所有重试间共享同一个总超时截止时间。
    let conn = retry_sqlite_connection(
        || sea_orm::Database::connect(options.clone()),
        Duration::from_secs(15),
        Duration::from_millis(50),
    )
    .await
    .map_err(|error| NuonoError::DatabaseConnection(format!("SQLite 连接失败: {error}")))?;
    // 明文密钥（runtime_secrets、整库）的既定安全边界是文件系统权限：库文件本身
    // 无条件收紧为 0600（幂等，含历史宽松安装）；WAL/SHM 伴随文件由私有父目录覆盖。
    if let Some(filename) = &filename {
        restrict_sqlite_file_permissions(filename);
    }
    Ok(DbHandles {
        conn,
        backend: DbBackend::Sqlite,
        sqlite_lock,
        ai_installed: false,
    })
}

/// 仅对本次新建的父目录用 0700：已存在的目录可能是部署者共享的位置，不静默 chmod。
#[cfg(unix)]
fn create_private_parent(parent: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(parent)
}

#[cfg(not(unix))]
fn create_private_parent(parent: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(parent)
}

#[cfg(unix)]
fn restrict_sqlite_file_permissions(filename: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::set_permissions(filename, std::fs::Permissions::from_mode(0o600)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %filename.display(),
            %error,
            "收紧 SQLite 库文件权限失败"
        ),
    }
}

#[cfg(not(unix))]
fn restrict_sqlite_file_permissions(_filename: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::path::Path;

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_library_file_and_new_parent_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!(
            "nuono-sqlite-perms-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let nested = base.join("fresh").join("dir");
        let db_path = nested.join("nuono.db");
        let cfg = DatabaseConfig {
            url: format!("sqlite://{}?mode=rwc", db_path.to_string_lossy()),
            max_connections: 1,
        };

        let handles = connect_sqlite(&cfg).await.expect("SQLite 连接失败");
        handles.conn.close().await.expect("关闭连接失败");
        let dir_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "新建父目录必须 0700");
        let file_mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "库文件必须 0600");

        // 历史宽松安装：库文件被放宽后重连必须被幂等收回 0600。
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let handles = connect_sqlite(&cfg).await.expect("SQLite 重连失败");
        handles.conn.close().await.expect("关闭连接失败");
        let file_mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "重连必须把库文件收紧回 0600");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sqlite_lock_codes_include_extensions_and_exclude_other_errors() {
        for code in ["5", "6", "261", "262", "517", "773"] {
            assert!(sqlite_is_lock_code(code), "{code}");
        }
        for code in ["", "0", "1", "19", "55", "-5", "5.0", "database is locked"] {
            assert!(!sqlite_is_lock_code(code), "{code}");
        }
        for error in [
            DbErr::Conn(RuntimeErr::Internal("database is locked".into())),
            DbErr::Conn(RuntimeErr::SqlxError(std::sync::Arc::new(
                sea_orm::SqlxError::Protocol("(code: 5) database is locked".into()),
            ))),
            DbErr::Custom("(code: 6) database table is locked".into()),
        ] {
            assert!(!sqlite_connection_is_locked(&error));
        }
    }

    #[tokio::test]
    async fn sqlite_connect_retry_returns_non_lock_error_without_retrying() {
        let mut attempts = 0;
        let expected = DbErr::Conn(RuntimeErr::Internal("database is locked".into()));
        let error = retry_sqlite_connection(
            || {
                attempts += 1;
                std::future::ready(Err(expected.clone()))
            },
            Duration::from_secs(15),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error, expected);
    }

    #[tokio::test]
    async fn sqlite_connect_retry_budget_includes_pending_connection_time() {
        let mut attempts = 0;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            retry_sqlite_connection(
                || {
                    attempts += 1;
                    std::future::pending()
                },
                Duration::from_millis(20),
                Duration::from_millis(5),
            ),
        )
        .await
        .expect("the retry budget must also bound an unfinished connect attempt")
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(matches!(
            error,
            DbErr::ConnectionAcquire(sea_orm::ConnAcquireErr::Timeout)
        ));
    }

    #[test]
    fn sqlite_backend_detection() {
        assert!(!is_sqlite_url("postgres://nuono:nuono@127.0.0.1/nuono"));
        assert!(is_sqlite_url("sqlite://x.db"));
        assert!(is_sqlite_url("sqlite::memory:"));
        assert!(is_sqlite_url("sqlite:///tmp/nuono.db?mode=rwc"));
    }

    #[test]
    fn sqlite_url_param_detection() {
        assert!(url_has_param("sqlite://a.db?mode=rwc", "mode"));
        assert!(url_has_param("sqlite://a.db?MODE=ro", "mode"));
        assert!(!url_has_param("sqlite://a.db", "mode"));
        assert!(!url_has_param("sqlite://a.db?mode=rwc", "journal"));
        assert!(url_has_param("sqlite://a.db?journal=WAL", "journal"));
    }

    #[test]
    fn sqlite_parse_respects_path_and_params() {
        assert_eq!(
            super::super::sqlite_database_path("sqlite:///tmp/nuono_abs.db?mode=rwc").unwrap(),
            Some(Path::new("/tmp/nuono_abs.db").to_path_buf())
        );
        assert_eq!(
            super::super::sqlite_database_path("sqlite://data/nuono.db?mode=rwc").unwrap(),
            Some(Path::new("data/nuono.db").to_path_buf())
        );
        assert_eq!(
            super::super::sqlite_database_path("sqlite:///tmp/a%3Fb%23c.db?mode=ro").unwrap(),
            Some(Path::new("/tmp/a?b#c.db").to_path_buf())
        );
        for url in [
            "sqlite::memory:",
            "sqlite://named?mode=memory",
            "sqlite://?mode=memory&cache=private",
        ] {
            assert_eq!(super::super::sqlite_database_path(url).unwrap(), None);
        }
        assert!(super::super::sqlite_database_path("sqlite://a.db?mode=invalid").is_err());
    }

    /// 两个不同绝对路径的文件库必须互相隔离，且同一文件库的多连接池内
    /// 每个连接都能看到迁移出的 schema（回归：连接串解析错会写到默认库）。
    #[tokio::test]
    async fn sqlite_file_databases_are_isolated_and_shared_within_pool() {
        let dir =
            std::env::temp_dir().join(format!("nuono_conn_test_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.db");
        let path_b = dir.join("b.db");

        let mk_cfg = |url: String| crate::config::DatabaseConfig {
            url,
            max_connections: 2,
        };

        let db_a =
            Database::from_config(&mk_cfg(format!("sqlite://{}?mode=rwc", path_a.display())))
                .await
                .expect("db_a 建库失败");
        let db_b =
            Database::from_config(&mk_cfg(format!("sqlite://{}?mode=rwc", path_b.display())))
                .await
                .expect("db_b 建库失败");

        // 多连接池内每个连接都应有 schema：连续两次查询命中不同连接也应成功。
        assert!(db_a.ping().await.unwrap());
        assert!(db_a.ping().await.unwrap());

        // 连接池中的每个连接都必须生效已配置的 SQLite PRAGMA 参数。
        use sea_orm::ConnectionTrait;
        for (pragma, column, expected) in [
            ("foreign_keys", "foreign_keys", 1_i64),
            ("busy_timeout", "timeout", 15_000),
            ("synchronous", "synchronous", 1),
        ] {
            let row = db_a
                .orm()
                .query_one_raw(sea_orm::Statement::from_string(
                    db_a.backend(),
                    format!("PRAGMA {pragma}"),
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", column).unwrap(), expected);
        }
        let journal = db_a
            .orm()
            .query_one_raw(sea_orm::Statement::from_string(
                db_a.backend(),
                "PRAGMA journal_mode",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            journal.try_get::<String>("", "journal_mode").unwrap(),
            "wal"
        );

        db_a.set_global_config("isolation_probe", serde_json::json!("a"))
            .await
            .unwrap();

        // db_a 内可读
        let in_a = db_a.get_global_config("isolation_probe").await.unwrap();
        assert_eq!(in_a, Some(serde_json::json!("a")));
        // db_b 隔离：看不到 db_a 的数据
        let in_b = db_b.get_global_config("isolation_probe").await.unwrap();
        assert_eq!(in_b, None);

        db_a.shutdown().await.unwrap();
        db_b.shutdown().await.unwrap();

        // 临时目录清理（非测试标记库，shutdown 不会删，需手动）
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn migration_ledgers_are_independent_and_reconnect_does_not_rerun_sql() {
        let mut db = crate::test_support::test_db(2).await;
        let test_name = db.test_db_name.take().unwrap();
        let url = test_name.clone();
        let cfg_url = if db.backend() == sea_orm::DbBackend::Postgres {
            let admin = std::env::var("NUONO_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://nuono:nuono@127.0.0.1:5432/postgres".into());
            let mut parsed = url::Url::parse(&admin).unwrap();
            parsed.set_path(&format!("/{url}"));
            parsed.to_string()
        } else {
            format!("sqlite://{}?mode=rwc", url)
        };
        let cfg = crate::config::DatabaseConfig {
            url: cfg_url,
            max_connections: 2,
        };
        let declaration = nuono_app::plugin_manifest::PluginStorageDeclaration::from_migrations(
            "restart",
            vec![crate::db::PluginMigrationForBackend {
                version: 1,
                description: "restart probe".into(),
                pg_sql: "CREATE TABLE restart_probe(id BIGINT PRIMARY KEY)".into(),
                sqlite_sql: "CREATE TABLE restart_probe(id INTEGER PRIMARY KEY) STRICT".into(),
            }],
        )
        .unwrap();
        db.register_plugin_storage("restart", "1", &declaration)
            .await
            .unwrap();
        db.orm()
            .execute_unprepared("INSERT INTO restart_probe(id) VALUES(1)")
            .await
            .unwrap();
        db.sync_plugin("restart-policy", "1", &serde_json::json!({}))
            .await
            .unwrap();
        db.set_plugin_slow_threshold("restart-policy", 30_000)
            .await
            .unwrap();
        db.shutdown().await.unwrap();
        let reopened = Database::from_config(&cfg).await.unwrap();
        assert_eq!(
            reopened
                .get_plugin_slow_threshold("restart-policy")
                .await
                .unwrap(),
            Some(30_000)
        );
        reopened
            .register_plugin_storage("restart", "1", &declaration)
            .await
            .unwrap();
        use sea_orm::ConnectionTrait;
        for (table, expected) in [
            (
                "nuono_schema_migrations",
                crate::db::framework_migration_sources().len() as i64,
            ),
            ("nuono_plugin_migrations", 1),
            ("restart_probe", 1),
        ] {
            let row = reopened
                .orm()
                .query_one_raw(sea_orm::Statement::from_string(
                    reopened.backend(),
                    format!("SELECT COUNT(*) AS count FROM {table}"),
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), expected);
        }
        // 数据库中若存在当前代码未知的框架迁移记录，启动时必须报错拒绝，绝不能静默放行。
        reopened
            .orm()
            .execute_raw(sea_orm::Statement::from_string(
                reopened.backend(),
                "INSERT INTO nuono_schema_migrations (module,version,description,checksum,applied_at_ms) VALUES ('core',9999,'unknown','x',1)",
            ))
            .await
            .unwrap();
        reopened.shutdown().await.unwrap();
        assert!(Database::from_config(&cfg).await.is_err());
        if cfg.url.starts_with("sqlite:") {
            crate::test_support::remove_sqlite_test_files(&test_name);
        } else {
            crate::test_support::drop_test_db(&test_name).await;
        }
    }
    #[tokio::test]
    async fn sqlite_lock_lives_with_cloned_orm_pool() {
        let dir = std::env::temp_dir().join(format!("nuono-pool-lock-{}", uuid::Uuid::new_v4()));
        let path = dir.join("database.db");
        let db = Database::from_config(&DatabaseConfig {
            url: format!("sqlite://{}?mode=rwc", path.display()),
            max_connections: 2,
        })
        .await
        .unwrap();
        let orm = db.orm().clone();
        drop(db);
        assert!(super::super::SqliteDatabaseLock::acquire_exclusive(&path).is_err());
        orm.ping().await.unwrap();
        orm.close().await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(super::super::SqliteDatabaseLock::acquire_exclusive(&path).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
