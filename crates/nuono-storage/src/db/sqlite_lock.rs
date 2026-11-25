use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{NuonoError, Result};

fn file_error(error: std::io::Error) -> NuonoError {
    NuonoError::DatabaseConnection(format!("SQLite 文件锁失败: {error}"))
}

/// SQLite 跨进程协同文件锁：常规运行期连接池打开时持有共享锁（shared lock），
/// 离线备份恢复或迁移工具在校验、替换数据库文件前则必须获取排他锁（exclusive lock）。
#[derive(Clone, Debug)]
pub struct SqliteDatabaseLock {
    file: Arc<Mutex<Option<File>>>,
}

impl SqliteDatabaseLock {
    pub fn acquire_shared(database_path: &Path) -> Result<Self> {
        let file = open_lock_file(database_path)?;
        fs2::FileExt::try_lock_shared(&file).map_err(|error| {
            NuonoError::DatabaseConnection(format!("SQLite 数据库正在恢复，无法打开: {error}"))
        })?;
        Ok(Self {
            file: Arc::new(Mutex::new(Some(file))),
        })
    }

    pub fn acquire_exclusive(database_path: &Path) -> Result<Self> {
        let file = open_lock_file(database_path)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            NuonoError::DatabaseConnection(format!(
                "SQLite 数据库仍被 Nuono 实例使用，拒绝恢复: {error}"
            ))
        })?;
        Ok(Self {
            file: Arc::new(Mutex::new(Some(file))),
        })
    }

    pub(crate) fn release(&self) -> Result<()> {
        self.file
            .lock()
            .map_err(|_| NuonoError::Internal("SQLite 文件锁状态损坏".into()))?
            .take();
        Ok(())
    }
}

/// 从 Nuono 支持的 SQLite 连接串中解析出实际对应的本地数据库文件路径（内存库返回 `Ok(None)`）。
///
/// `sqlite://` 之后紧跟的是本地文件路径而非 URL 权限段（authority）：此处对路径仅做一次百分号解码，
/// 完整保留文件名中的 `+`、`#`、相对路径段以及问号/等号等字符。
/// 刻意拒绝原生 `file:` URI 与自定义 `vfs` 参数，防止 SQLite 底层二次解析路径从而绕过我们加锁的目标文件。
pub fn sqlite_database_path(database_url: &str) -> Result<Option<PathBuf>> {
    let address = if database_url == ":memory:" {
        database_url
    } else {
        database_url
            .strip_prefix("sqlite://")
            .or_else(|| database_url.strip_prefix("sqlite:"))
            .ok_or_else(|| sqlite_url_error("需要 sqlite: 连接串"))?
    };
    let (encoded_path, query) = address.split_once('?').unwrap_or((address, ""));
    let mut in_memory = encoded_path == ":memory:";
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "mode" => match value.as_ref() {
                "memory" => in_memory = true,
                "ro" | "rw" | "rwc" => {}
                _ => return Err(sqlite_url_error("mode 只支持 ro、rw、rwc 或 memory")),
            },
            "cache" if matches!(value.as_ref(), "private" | "shared") => {}
            "immutable" if matches!(value.as_ref(), "true" | "false" | "1" | "0") => {}
            "cache" => return Err(sqlite_url_error("cache 只支持 private 或 shared")),
            "immutable" => return Err(sqlite_url_error("immutable 只支持 true、false、1 或 0")),
            "vfs" => return Err(sqlite_url_error("不支持自定义 SQLite VFS")),
            _ => return Err(sqlite_url_error("包含不支持的查询参数")),
        }
    }

    let path = decode_sqlite_path(encoded_path)?;
    if path.contains('\0') {
        return Err(sqlite_url_error("文件路径不能包含 NUL"));
    }
    if path.starts_with("file:") || path.starts_with("sqlite:") {
        return Err(sqlite_url_error(
            "不支持嵌套 URI，请直接提供 SQLite 文件路径",
        ));
    }
    if path == ":memory:" && encoded_path != ":memory:" {
        return Err(sqlite_url_error(
            "内存数据库请使用字面 :memory: 或 mode=memory",
        ));
    }
    if in_memory {
        return Ok(None);
    }
    if path.is_empty() {
        return Err(sqlite_url_error(
            "文件路径不能为空，内存数据库请使用 mode=memory",
        ));
    }
    Ok(Some(PathBuf::from(path)))
}

fn sqlite_url_error(message: &str) -> NuonoError {
    NuonoError::DatabaseConnection(format!("SQLite 连接串无效: {message}"))
}

fn decode_sqlite_path(encoded: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut remaining = encoded.as_bytes();
    while let Some((&first, rest)) = remaining.split_first() {
        if first == b'%' && remaining.len() >= 3 {
            let high = char::from(remaining[1]).to_digit(16);
            let low = char::from(remaining[2]).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                remaining = &remaining[3..];
                continue;
            }
        }
        decoded.push(first);
        remaining = rest;
    }
    String::from_utf8(decoded).map_err(|_| sqlite_url_error("文件路径必须是有效 UTF-8"))
}

fn open_lock_file(database_path: &Path) -> Result<File> {
    let absolute = if database_path.is_absolute() {
        database_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(file_error)?
            .join(database_path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| NuonoError::DatabaseConnection("SQLite 路径缺少父目录".into()))?;
    std::fs::create_dir_all(parent).map_err(file_error)?;
    let canonical = if absolute.exists() {
        absolute.canonicalize().map_err(file_error)?
    } else {
        parent.canonicalize().map_err(file_error)?.join(
            absolute
                .file_name()
                .ok_or_else(|| NuonoError::DatabaseConnection("SQLite 路径缺少文件名".into()))?,
        )
    };
    let mut name = canonical.as_os_str().to_os_string();
    name.push(".nuono-lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(PathBuf::from(name)).map_err(file_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_paths_preserve_native_filename_semantics() {
        for (url, path) in [
            ("sqlite://data/nuono.db", "data/nuono.db"),
            ("sqlite:data/nuono.db", "data/nuono.db"),
            ("sqlite:///tmp/nuono.db?mode=rwc", "/tmp/nuono.db"),
            (
                "sqlite://../data/nuono.db?mode=rw&cache=shared",
                "../data/nuono.db",
            ),
            ("sqlite:///tmp/a%3Fb%23c%25.db?mode=ro", "/tmp/a?b#c%.db"),
            ("sqlite://data/a+b%2Bc.db?immutable=false", "data/a+b+c.db"),
            ("sqlite://data/a#b.db", "data/a#b.db"),
            ("sqlite://data/%253F.db?immutable=1", "data/%3F.db"),
            ("sqlite://data/%E7%A4%BA%E4%BE%8B.db", "data/示例.db"),
            ("sqlite://data/name%.db", "data/name%.db"),
            ("sqlite://data/%not-an-escape.db", "data/%not-an-escape.db"),
        ] {
            assert_eq!(
                sqlite_database_path(url).unwrap(),
                Some(PathBuf::from(path))
            );
        }
    }

    #[test]
    fn explicit_memory_addresses_never_acquire_disk_locks() {
        for url in [
            ":memory:",
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite://named?mode=memory&cache=private",
            "sqlite://?mode=memory&cache=shared",
            "sqlite://named?%6dode=%6demory",
            "sqlite://named?mode=memory&mode=rw",
        ] {
            assert_eq!(sqlite_database_path(url).unwrap(), None);
        }
        assert_eq!(
            sqlite_database_path("sqlite://data/memory.db?mode=rwc").unwrap(),
            Some(PathBuf::from("data/memory.db"))
        );
    }

    #[test]
    fn ambiguous_paths_and_invalid_parameters_fail_closed() {
        for url in [
            "postgres://localhost/database",
            "data/nuono.db",
            "SQLITE://data/nuono.db",
            " sqlite://data/nuono.db",
            "sqlite:",
            "sqlite://?mode=rwc",
            "sqlite://file:elsewhere.db",
            "sqlite://%66ile:elsewhere.db%3Fmode=memory",
            "sqlite://sqlite://elsewhere.db",
            "sqlite://%3Amemory%3A",
            "sqlite://bad%00.db",
            "sqlite://bad\0.db",
            "sqlite://bad%FF.db",
            "sqlite://name?mode=invalid",
            "sqlite://name?mode=MEMORY",
            "sqlite://name?cache=invalid",
            "sqlite://name?immutable=yes",
            "sqlite://name?vfs=memdb",
            "sqlite://name?journal=WAL",
            "sqlite://name?MODE=memory",
            "sqlite::memory:?unsupported=true",
        ] {
            assert!(sqlite_database_path(url).is_err(), "accepted {url:?}");
        }
    }

    #[test]
    fn percent_encoded_filename_aliases_contend_for_the_same_lock() {
        let dir = std::env::temp_dir().join(format!("nuono-lock-url-{}", uuid::Uuid::new_v4()));
        let path = dir.join("a+b#c.db");
        let url = format!(
            "sqlite://{}",
            path.to_str()
                .unwrap()
                .replace('+', "%2B")
                .replace('#', "%23")
        );
        let parsed = sqlite_database_path(&url).unwrap().unwrap();
        assert_eq!(parsed, path);
        let held = SqliteDatabaseLock::acquire_shared(&parsed).unwrap();
        assert!(SqliteDatabaseLock::acquire_exclusive(&path).is_err());
        drop(held);
        drop(SqliteDatabaseLock::acquire_exclusive(&path).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn resolved_path_matches_the_file_opened_by_sea_orm() {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};

        let dir = std::env::temp_dir().join(format!("nuono-sqlite-url-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let filename = if cfg!(windows) {
            "a+b#%示例.db"
        } else {
            "a+b?c#%示例.db"
        };
        let path = dir.join(filename);
        let encoded = path
            .to_str()
            .unwrap()
            .replace('%', "%25")
            .replace('?', "%3F")
            .replace('#', "%23");
        let url = format!("sqlite://{encoded}?mode=rwc&cache=private");
        let resolved = sqlite_database_path(&url).unwrap().unwrap();
        let held = SqliteDatabaseLock::acquire_shared(&resolved).unwrap();
        let connection = sea_orm::Database::connect(url).await.unwrap();
        let database = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA database_list",
            ))
            .await
            .unwrap()
            .unwrap();
        let opened: String = database.try_get("", "file").unwrap();
        assert_eq!(
            std::fs::canonicalize(opened).unwrap(),
            resolved.canonicalize().unwrap()
        );
        assert!(SqliteDatabaseLock::acquire_exclusive(&path).is_err());
        connection.close().await.unwrap();
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_database_lock_excludes_restore_and_survives_clone() {
        let dir = std::env::temp_dir().join(format!("nuono-lock-{}", uuid::Uuid::new_v4()));
        let path = dir.join("database.db");
        let first = SqliteDatabaseLock::acquire_shared(&path).unwrap();
        let second = SqliteDatabaseLock::acquire_shared(&path).unwrap();
        assert!(SqliteDatabaseLock::acquire_exclusive(&path).is_err());
        let clone = first.clone();
        drop(first);
        drop(second);
        assert!(SqliteDatabaseLock::acquire_exclusive(&path).is_err());
        clone.release().unwrap();
        let restore = SqliteDatabaseLock::acquire_exclusive(&path).unwrap();
        assert!(SqliteDatabaseLock::acquire_shared(&path).is_err());
        drop(restore);
        assert!(SqliteDatabaseLock::acquire_shared(&path).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
