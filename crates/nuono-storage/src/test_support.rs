//! 测试基建：每个测试一个独立的后端实例。
//!
//! 默认仍是 PostgreSQL：每个测试建一个 `nuono_test_<uuid>` 数据库，跑完由
//! [`Database::shutdown`] 顺带 DROP（PostgreSQL 没有 SQLite 临时文件那样的天然隔离，
//! 管理连接走 `postgres` 库，`nuono` 角色带 `CREATEDB`）。
//!
//! 设置环境变量 `NUONO_TEST_DATABASE_BACKEND=sqlite` 可把全部 `test_db` 切到 SQLite：
//! 每个测试一个临时数据库文件（`$TMPDIR/nuono_test_<uuid>.db`），同样由
//! [`Database::shutdown`] 删除（含 `-wal` / `-shm`）。PG 侧环境变量与清理逻辑不变。

use crate::config::DatabaseConfig;
use crate::db::Database;
use sea_orm::{ConnectOptions, ConnectionTrait, DbBackend, Statement};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 进程级环境变量互斥锁。
///
/// `cargo test` 在同一进程内并行跑多个测试线程，而测试控制环境变量（如 `NUONO_TEST_*`）是进程全局的：
/// 任何会设置或清除环境变量的测试都必须先 `let _guard = env_lock().await`，避免测试间互相污染。
pub async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// 测试后端选择：`sqlite` 用临时文件 SQLite，其余（含未设置）用 PostgreSQL。
const BACKEND_ENV: &str = "NUONO_TEST_DATABASE_BACKEND";

/// SQLite 测试库文件名前缀，便于手工排查时一眼认出残留文件。
const SQLITE_TEST_FILE_PREFIX: &str = "nuono_test_";
/// 测试库（PG）前缀，便于手工排查时一眼认出残留库。
const TEST_DB_PREFIX: &str = "nuono_test_";

/// 测试数据库的管理连接串。CI 用 `NUONO_TEST_DATABASE_URL` 覆盖，开发机保留便捷默认值。
const DEFAULT_TEST_ADMIN_URL: &str = "postgres://nuono:nuono@127.0.0.1:5432/postgres";

fn test_admin_url() -> String {
    std::env::var("NUONO_TEST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_TEST_ADMIN_URL.to_string())
}

fn sqlite_test_requested() -> bool {
    std::env::var(BACKEND_ENV)
        .map(|v| v.eq_ignore_ascii_case("sqlite"))
        .unwrap_or(false)
}

fn database_url_for_name(admin_url: &str, name: &str) -> String {
    let mut url =
        url::Url::parse(admin_url).expect("NUONO_TEST_DATABASE_URL 必须是合法 PostgreSQL URL");
    url.set_path(&format!("/{name}"));
    url.to_string()
}

/// 建一个独立测试库（或 SQLite 临时文件）并返回已迁移的 [`Database`]。
/// `max_connections` 通常给 2~4。
///
/// 调用方照旧 `db.shutdown().await`，关闭时会把测试库/临时文件一并清理，不会累积泄漏。
pub async fn test_db(max_connections: u32) -> Database {
    if sqlite_test_requested() {
        test_db_sqlite(max_connections).await
    } else {
        test_db_postgres(max_connections).await
    }
}

async fn test_db_postgres(max_connections: u32) -> Database {
    let name = format!("{TEST_DB_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let admin_url = test_admin_url();
    let admin = sea_orm::Database::connect(admin_options(admin_url.clone()))
        .await
        .expect("连接测试 PostgreSQL 管理库失败");
    admin
        .execute_unprepared(&format!("CREATE DATABASE \"{name}\""))
        .await
        .expect("创建隔离 PostgreSQL 测试库失败");
    admin.close().await.expect("关闭测试管理连接失败");

    let url = database_url_for_name(&admin_url, &name);
    let cfg = DatabaseConfig {
        url,
        max_connections,
    };
    let mut db = Database::from_config(&cfg)
        .await
        .expect("测试数据库迁移失败");
    db.mark_test_db(name);
    db
}

async fn test_db_sqlite(max_connections: u32) -> Database {
    let path = std::env::temp_dir().join(format!(
        "{SQLITE_TEST_FILE_PREFIX}{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    // sqlite:///abs/path.db → 绝对路径文件，父目录由连接 helper 兜底创建
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
    let cfg = DatabaseConfig {
        url,
        max_connections,
    };
    let mut db = Database::from_config(&cfg)
        .await
        .expect("SQLite 测试数据库迁移失败");
    db.mark_test_db(path.to_string_lossy().into_owned());
    db
}

/// 删除 SQLite 测试文件（含 WAL / SHM 伴随文件）。失败静默：测试已结束，清理失败
/// 不应再报错。
pub fn remove_sqlite_test_files(path: &str) {
    for suffix in ["", "-wal", "-shm", ".nuono-lock"] {
        let file = format!("{path}{suffix}");
        let _ = std::fs::remove_file(&file);
    }
}

/// 终止连接并 DROP 测试库。失败静默：测试已经结束，清理失败不应再报错。
pub async fn drop_test_db(name: &str) {
    // 安全兜底：严格校验 `nuono_test_<32位十六进制UUID>` 命名格式，绝不允许删除测试命名空间之外的任何数据库。
    if !name.strip_prefix(TEST_DB_PREFIX).is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return;
    }
    let Ok(admin) = sea_orm::Database::connect(admin_options(test_admin_url())).await else {
        return;
    };
    let _ = admin
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            [name.into()],
        ))
        .await;
    let _ = admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS \"{name}\""))
        .await;
    let _ = admin.close().await;
}

fn admin_options(url: String) -> ConnectOptions {
    let mut options = ConnectOptions::new(url);
    options.max_connections(1).sqlx_logging(false);
    options
}
