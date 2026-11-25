//! 启动配置文件中数据库连接地址（`database.url`）的读写；修改仅写盘，不影响当前运行中的连接池。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use toml_edit::{DocumentMut, Item, Table, Value};

use super::{ConfigManager, DatabaseConfig, NuonoError, Result, validate_database_url};

/// 启动配置文件中的数据库地址以及当前进程正在使用的连接地址。
/// 该结构体包含数据库凭据，仅限已认证的管理员 API 返回。
#[derive(Clone, Serialize, Deserialize)]
pub struct DatabaseConnectionSettings {
    pub url: String,
    pub active_url: String,
    pub revision: String,
    pub restart_required: bool,
}

struct StartupFile {
    content: String,
    revision: String,
    permissions: std::fs::Permissions,
}

impl ConfigManager {
    /// 重新读取磁盘上的启动配置文件，以便及时反映外部手动修改，且不影响当前内存配置。
    pub async fn database_settings(&self) -> Result<DatabaseConnectionSettings> {
        let _guard = self.writes.lock().await;
        let file = read_startup_file(&self.path).await?;
        let document = parse_document(&file.content)?;
        let url = document_url(&document)?;
        let active_url = self.snapshot().await.database.url.clone();
        Ok(settings(url, active_url, file.revision))
    }

    /// 仅将新的 `database.url` 写回启动配置文件，当前运行中的配置快照与数据库连接池保持不变。
    pub async fn save_database_settings(
        &self,
        url: &str,
        expected_revision: &str,
    ) -> Result<DatabaseConnectionSettings> {
        validate_address(url)?;
        let _guard = self.writes.lock().await;
        let file = read_startup_file(&self.path).await?;
        require_revision(&file.revision, expected_revision)?;
        let mut document = parse_document(&file.content)?;
        let previous_url = document_url(&document)?;
        let active_url = self.snapshot().await.database.url.clone();
        if previous_url == url {
            return Ok(settings(url.to_owned(), active_url, file.revision));
        }

        set_document_url(&mut document, url)?;
        let content = document.to_string();
        replace_startup_file(&self.path, &file, content.as_bytes()).await?;
        Ok(settings(
            url.to_owned(),
            active_url,
            revision(content.as_bytes()),
        ))
    }
}

fn settings(url: String, active_url: String, revision: String) -> DatabaseConnectionSettings {
    DatabaseConnectionSettings {
        restart_required: url != active_url,
        url,
        active_url,
        revision,
    }
}

fn validate_address(url: &str) -> Result<()> {
    if url.trim().is_empty() || url.len() > 8192 || url.chars().any(char::is_control) {
        return Err(NuonoError::ConfigValidation(
            "数据库连接不能为空、包含控制字符或超过 8192 字节".into(),
        ));
    }
    let mut errors = Vec::new();
    validate_database_url(url, &mut errors);
    if !errors.is_empty() {
        // TOML 解析器的原始报错可能夹带含密码的输入片段，不直接对外透传。
        return Err(NuonoError::ConfigValidation(
            "数据库连接格式无效，请填写 PostgreSQL 或 SQLite 连接地址".into(),
        ));
    }
    Ok(())
}

fn revision(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

fn require_revision(current: &str, expected: &str) -> Result<()> {
    if current != expected {
        return Err(NuonoError::Conflict(
            "启动配置已被修改，请重新读取后再保存数据库连接".into(),
        ));
    }
    Ok(())
}

fn parse_document(content: &str) -> Result<DocumentMut> {
    content
        .parse()
        .map_err(|_| NuonoError::ConfigLoad("启动配置不是有效的 TOML，请修正文件后重新读取".into()))
}

fn document_url(document: &DocumentMut) -> Result<String> {
    let Some(database) = document.get("database") else {
        return Ok(DatabaseConfig::default().url);
    };
    let database = database.as_table_like().ok_or_else(database_table_error)?;
    match database.get("url") {
        None => Ok(DatabaseConfig::default().url),
        Some(url) => url
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| NuonoError::ConfigLoad("启动配置 database.url 必须是字符串".into())),
    }
}

fn database_table_error() -> NuonoError {
    NuonoError::ConfigLoad("启动配置 database 必须是普通表或内联表".into())
}

fn set_document_url(document: &mut DocumentMut, url: &str) -> Result<()> {
    if document.get("database").is_none() {
        document["database"] = Item::Table(Table::new());
    }
    let database = document
        .get_mut("database")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(database_table_error)?;
    let mut value = Value::from(url);
    if let Some(old) = database.get("url").and_then(Item::as_value) {
        // 替换字符串字面量时保留原有的前置空白与行尾注释。
        *value.decor_mut() = old.decor().clone();
    }
    if let Some(existing) = database.get_mut("url") {
        // 仅原地替换值节点；若重新插入键值对会导致现有键被重排格式，并丢失挂在引号键或点分键上的注释。
        *existing = Item::Value(value);
    } else {
        database.insert("url", Item::Value(value));
    }
    Ok(())
}

async fn read_startup_file(path: &Path) -> Result<StartupFile> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| file_read_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(NuonoError::ConfigLoad(
            "启动配置必须是普通文件；软链接和特殊文件请在服务器上修改".into(),
        ));
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| file_read_error(path, error))?;
    Ok(StartupFile {
        revision: revision(content.as_bytes()),
        content,
        permissions: metadata.permissions(),
    })
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        // 尽力清理临时文件（包括异步请求在 await 期间被取消的场景）。
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn replace_startup_file(path: &Path, original: &StartupFile, content: &[u8]) -> Result<()> {
    if original.permissions.readonly() {
        return Err(NuonoError::FileWrite {
            path: path.display().to_string(),
            reason: "启动配置是只读文件".into(),
        });
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary =
        TemporaryFile(parent.join(format!(".nuono-database-{}.tmp", uuid::Uuid::new_v4())));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary.0)
        .await
        .map_err(|error| file_write_error(path, error))?;
    file.write_all(content)
        .await
        .map_err(|error| file_write_error(path, error))?;
    file.flush()
        .await
        .map_err(|error| file_write_error(path, error))?;
    file.set_permissions(original.permissions.clone())
        .await
        .map_err(|error| file_write_error(path, error))?;
    file.sync_all()
        .await
        .map_err(|error| file_write_error(path, error))?;
    drop(file);

    // API 保存请求通过 writes 互斥锁串行化；但在落盘替换前仍需立即重读文件核对版本，因为外部编辑器不会参与进程内加锁。
    let current = read_startup_file(path).await?;
    require_revision(&current.revision, &original.revision)?;
    tokio::fs::rename(&temporary.0, path)
        .await
        .map_err(|error| file_write_error(path, error))?;
    Ok(())
}

fn file_read_error(path: &Path, error: std::io::Error) -> NuonoError {
    NuonoError::FileRead {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

fn file_write_error(path: &Path, error: std::io::Error) -> NuonoError {
    NuonoError::FileWrite {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/config/database_settings/tests.rs"]
mod tests;
