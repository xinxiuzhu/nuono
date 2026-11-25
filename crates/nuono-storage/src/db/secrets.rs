//! 运行期 secret 的持久化。
//!
//! secret（token、API key、app secret）以数据库为唯一可信来源，由 Dashboard 维护，
//! 永不回写 `nuono.toml`。TOML 中的 secret 只在首次启动时作为种子导入，之后即可从
//! 文件中删除。
//!
//! 这里只做存取，不做加密：能连接 PostgreSQL 的攻击者通常也能取得应用所需的任何
//! 解密权限，落库加密不能替代数据库访问控制。真正的防护依赖网络隔离、部署隔离和最小
//! 权限运行。因此不要在日志或 API 响应中回显 secret 值，只回显键名和是否已配置。

use super::Database;
use crate::entity::runtime_secrets;
use crate::error::Result;
use sea_orm::{ConnectionTrait, EntityTrait};

pub(crate) async fn set_secret_on(
    connection: &impl ConnectionTrait,
    key: &str,
    value: &str,
) -> Result<()> {
    if key.is_empty() || key.len() > 256 || key.contains('\0') {
        return Err(crate::error::NuonoError::Validation(
            "secret key无效".into(),
        ));
    }
    if value.trim().is_empty() {
        runtime_secrets::Entity::delete_by_id(key)
            .exec(connection)
            .await?;
    } else {
        let owner = key
            .strip_prefix("plugin.")
            .and_then(|rest| rest.split_once('.'))
            .map(|(id, _)| id);
        let now = crate::clock::db_now().timestamp_millis();
        connection.execute_raw(sea_orm::Statement::from_sql_and_values(connection.get_database_backend(),
            "INSERT INTO nuono_runtime_secrets (key,owner_plugin_id,value,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,$3,1,$4,$4) ON CONFLICT(key) DO UPDATE SET value=excluded.value, revision=nuono_runtime_secrets.revision+1, updated_at_ms=excluded.updated_at_ms",
            [key.into(),owner.into(),value.as_bytes().to_vec().into(),now.into()])).await?;
    }
    Ok(())
}

/// 受管 secret 的键名。
///
/// 与 effective settings 中的 secret 字段一一对应，用于在启动时把数据库值
/// 覆盖进内存配置。
pub const SECRET_KEYS: &[&str] = &[
    "security.ws_token",
    "security.registration_otp_pepper",
    "telegram.token",
    "discord.token",
    "feishu.app_secret",
    "feishu.verification_token",
    "feishu.encrypt_key",
    "qq_official.app_secret",
    "ai.api_key",
    "ai.web.tavily_api_key",
    "ai.web.exa_api_key",
    "ai.web.firecrawl_api_key",
    "ai.web.jina_api_key",
    "metrics.token",
    "mail.smtp_password",
];

impl Database {
    /// 读取单个 secret。不存在或为空字符串时返回 `None`。
    pub async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        self.timed("get_secret", "nuono_runtime_secrets", async {
            let value = runtime_secrets::Entity::find_by_id(key)
                .one(self.orm())
                .await?
                .map(|row| decode_secret(row.value))
                .transpose()?;
            Ok(value.filter(|value| !value.trim().is_empty()))
        })
        .await
    }

    /// 写入或更新 secret。空值等价于删除，避免残留空串被当作已配置。
    pub async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        if value.trim().is_empty() {
            return self.delete_secret(key).await;
        }

        self.timed("set_secret", "nuono_runtime_secrets", async {
            set_secret_on(self.orm(), key, value).await
        })
        .await
    }

    /// 删除 secret。不存在时视为成功。
    pub async fn delete_secret(&self, key: &str) -> Result<()> {
        self.timed("delete_secret", "nuono_runtime_secrets", async {
            runtime_secrets::Entity::delete_by_id(key)
                .exec(self.orm())
                .await?;
            Ok(())
        })
        .await
    }

    /// 列出已配置的 secret 键名。
    ///
    /// 只返回键名，不返回值，供 Dashboard 显示「已配置 / 未配置」状态。
    pub async fn list_configured_secret_keys(&self) -> Result<Vec<String>> {
        self.timed("list_secret_keys", "nuono_runtime_secrets", async {
            let rows = runtime_secrets::Entity::find().all(self.orm()).await?;
            let mut keys: Vec<String> = rows
                .into_iter()
                .filter(|m| !m.value.is_empty())
                .map(|m| m.key)
                .collect();
            keys.sort();
            Ok(keys)
        })
        .await
    }
}

pub(crate) fn decode_secret(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map_err(|_| crate::error::NuonoError::ConfigLoad("文本secret包含非UTF-8数据".into()))
}
