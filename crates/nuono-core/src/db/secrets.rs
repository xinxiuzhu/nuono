use super::Database;
use crate::error::Result;
pub use nuono_storage::db::SECRET_KEYS;

/// 取出 [`NuonoConfig`] 中某个 secret 字段的可变引用。
///
/// 集中在一处映射，避免 seed 与 apply 两条路径出现字段遗漏。未知键返回 `None`。
fn secret_field_mut<'a>(
    cfg: &'a mut crate::config::NuonoConfig,
    key: &str,
) -> Option<SecretField<'a>> {
    Some(match key {
        "security.ws_token" => SecretField::Plain(&mut cfg.security.ws_token),
        "security.registration_otp_pepper" => {
            SecretField::Plain(&mut cfg.security.registration_otp_pepper)
        }
        "telegram.token" => SecretField::Opt(&mut cfg.telegram.token),
        "discord.token" => SecretField::Opt(&mut cfg.discord.token),
        "feishu.app_secret" => SecretField::Opt(&mut cfg.feishu.app_secret),
        "feishu.verification_token" => SecretField::Opt(&mut cfg.feishu.verification_token),
        "feishu.encrypt_key" => SecretField::Opt(&mut cfg.feishu.encrypt_key),
        "qq_official.app_secret" => SecretField::Opt(&mut cfg.qq_official.app_secret),
        "ai.api_key" => SecretField::Opt(&mut cfg.ai.api_key),
        "ai.web.tavily_api_key" => SecretField::Opt(&mut cfg.ai.web.credentials.tavily_api_key),
        "ai.web.exa_api_key" => SecretField::Opt(&mut cfg.ai.web.credentials.exa_api_key),
        "ai.web.firecrawl_api_key" => {
            SecretField::Opt(&mut cfg.ai.web.credentials.firecrawl_api_key)
        }
        "ai.web.jina_api_key" => SecretField::Opt(&mut cfg.ai.web.credentials.jina_api_key),
        "metrics.token" => SecretField::Opt(&mut cfg.metrics.token),
        "mail.smtp_password" => SecretField::Opt(&mut cfg.mail.smtp_password),
        _ => return None,
    })
}

/// secret 字段的两种形态：必填字符串和可选字符串。
enum SecretField<'a> {
    Plain(&'a mut String),
    Opt(&'a mut Option<String>),
}

impl SecretField<'_> {
    fn read(&self) -> Option<String> {
        match self {
            Self::Plain(value) => (!value.trim().is_empty()).then(|| value.to_string()),
            Self::Opt(value) => value
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .map(String::from),
        }
    }

    fn write(&mut self, value: String) {
        match self {
            Self::Plain(target) => **target = value,
            Self::Opt(target) => **target = Some(value),
        }
    }
}

/// 把 secret 键的目标值写入配置字段，供候选配置组装使用。
///
/// `value` 为 `None` 时清空该字段（`Plain` 归零、`Opt` 置 `None`）。未知键返回
/// `false`，由调用方决定如何报错——集中在这里是为了与 seed/apply 共用同一份字段映射，
/// 避免出现第三条会漂移的清单。
pub(crate) fn write_secret_field(
    cfg: &mut crate::config::NuonoConfig,
    key: &str,
    value: Option<&str>,
) -> bool {
    let Some(field) = secret_field_mut(cfg, key) else {
        return false;
    };
    match (field, value) {
        (SecretField::Plain(target), Some(value)) => *target = value.to_string(),
        (SecretField::Opt(target), Some(value)) => *target = Some(value.to_string()),
        (SecretField::Plain(target), None) => *target = String::new(),
        (SecretField::Opt(target), None) => *target = None,
    }
    true
}

pub(crate) fn config_secrets(config: &crate::config::NuonoConfig) -> Vec<(&'static str, String)> {
    let mut probe = config.clone();
    SECRET_KEYS
        .iter()
        .filter_map(|key| {
            secret_field_mut(&mut probe, key)
                .and_then(|field| field.read())
                .map(|value| (*key, value))
        })
        .collect()
}

#[async_trait::async_trait]
pub trait ConfigSecretPersistence {
    async fn seed_secrets_from_config(
        &self,
        cfg: &crate::config::NuonoConfig,
    ) -> Result<Vec<String>>;
    async fn apply_secrets_to_config(&self, cfg: &mut crate::config::NuonoConfig) -> Result<usize>;
}
#[async_trait::async_trait]
impl ConfigSecretPersistence for Database {
    /// 把 `nuono.toml` 里残留的 secret 种子导入数据库。
    ///
    /// 只在数据库尚未持有该键时导入，因此 Dashboard 的后续修改不会被文件覆盖。返回被
    /// 导入的键名，供调用方提示用户从文件中删除。
    async fn seed_secrets_from_config(
        &self,
        cfg: &crate::config::NuonoConfig,
    ) -> Result<Vec<String>> {
        let mut seeded = Vec::new();
        // seed 只读配置，这里克隆一份用于复用同一套字段映射。
        let mut probe = cfg.clone();
        for key in SECRET_KEYS {
            let Some(value) = secret_field_mut(&mut probe, key).and_then(|f| f.read()) else {
                continue;
            };
            if self.get_secret(key).await?.is_some() {
                continue;
            }
            self.set_secret(key, &value).await?;
            seeded.push((*key).to_string());
        }
        Ok(seeded)
    }

    /// 用数据库中的 secret 覆盖内存配置。
    ///
    /// 数据库是 secret 的可信来源，但环境变量优先级更高，所以调用方应在本方法之后再次
    /// 应用环境变量覆盖。
    async fn apply_secrets_to_config(&self, cfg: &mut crate::config::NuonoConfig) -> Result<usize> {
        let rows = self.all_secret_values().await?;
        cfg.clear_secrets();
        let mut applied = 0;
        for (key, value) in rows {
            if value.trim().is_empty() {
                continue;
            }
            if let Some(mut field) = secret_field_mut(cfg, &key) {
                field.write(value);
                applied += 1;
            }
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NuonoConfig;

    async fn test_db() -> Database {
        crate::test_support::test_db(2).await
    }

    #[tokio::test]
    async fn applying_deleted_secret_clears_runtime_value() {
        let db = test_db().await;
        let mut cfg = NuonoConfig::default();
        cfg.mail.smtp_password = Some("stale".into());
        cfg.security.ws_token = "stale".into();
        db.apply_secrets_to_config(&mut cfg).await.unwrap();
        db.shutdown().await.unwrap();
        assert_eq!(cfg.mail.smtp_password, None);
        assert!(cfg.security.ws_token.is_empty());
    }

    #[tokio::test]
    async fn secret_roundtrip_treats_empty_as_absent() {
        let db = test_db().await;
        assert_eq!(db.get_secret("ai.api_key").await.unwrap(), None);

        db.set_secret("ai.api_key", "sk-test").await.unwrap();
        assert_eq!(
            db.get_secret("ai.api_key").await.unwrap().as_deref(),
            Some("sk-test")
        );
        assert_eq!(
            db.list_configured_secret_keys().await.unwrap(),
            vec!["ai.api_key".to_string()]
        );

        // 空值等价于删除，避免残留空串被当作已配置。
        db.set_secret("ai.api_key", "   ").await.unwrap();
        assert_eq!(db.get_secret("ai.api_key").await.unwrap(), None);
        assert!(db.list_configured_secret_keys().await.unwrap().is_empty());

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn seed_does_not_overwrite_dashboard_edits() {
        let db = test_db().await;
        // Dashboard 先写入了新值。
        db.set_secret("telegram.token", "from-dashboard")
            .await
            .unwrap();

        // nuono.toml 里仍残留旧种子值。
        let mut cfg = NuonoConfig::default();
        cfg.telegram.token = Some("stale-from-file".to_string());
        let seeded = db.seed_secrets_from_config(&cfg).await.unwrap();

        assert!(!seeded.contains(&"telegram.token".to_string()));
        assert_eq!(
            db.get_secret("telegram.token").await.unwrap().as_deref(),
            Some("from-dashboard")
        );

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn seed_then_apply_restores_secrets_into_config() {
        let db = test_db().await;
        let mut seed_cfg = NuonoConfig::default();
        seed_cfg.security.ws_token = "ws-secret".to_string();
        seed_cfg.ai.api_key = Some("sk-seed".to_string());

        let mut seeded = db.seed_secrets_from_config(&seed_cfg).await.unwrap();
        seeded.sort();
        assert_eq!(seeded, vec!["ai.api_key", "security.ws_token"]);

        // 模拟重启：配置文件里已没有 secret。
        let mut fresh = NuonoConfig::default();
        assert!(fresh.security.ws_token.is_empty());
        assert_eq!(fresh.ai.api_key, None);

        let applied = db.apply_secrets_to_config(&mut fresh).await.unwrap();
        assert_eq!(applied, 2);
        assert_eq!(fresh.security.ws_token, "ws-secret");
        assert_eq!(fresh.ai.api_key.as_deref(), Some("sk-seed"));

        db.shutdown().await.unwrap();
    }
}
