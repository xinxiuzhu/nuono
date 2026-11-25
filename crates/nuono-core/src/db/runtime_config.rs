//! 运行期设置的数据库持久化（`global_configs` 的 `runtime.settings.v1`）。
//!
//! 数据库是普通运行设置的权威来源：`ConfigManager` 启动时把 `nuono.toml`（bootstrap）
//! 作为种子导入一次，此后 Dashboard 的修改只写数据库，`nuono.toml` 不再被改写。
//! 写入走 [`Database::update_runtime_config`]：一个事务内完成「读取存储 → 应用补丁 →
//! 完整校验 → 写回提交」，任何一步失败都整体回滚，数据库保持原状。
//!
//! secret 不进这份设置树，由 [`super::secrets`] 的 `runtime_secrets` 表单独管理；
//! 但两者在同一个事务里一起提交，保证普通设置与密钥变更的原子性。

use std::collections::HashMap;

use super::Database;
use crate::config::NuonoConfig;
use crate::error::{NuonoError, Result};
use serde_json::Value;

#[cfg(test)]
#[path = "../../tests/unit/db/runtime_config/agent_output_tests.rs"]
mod agent_output_tests;
#[cfg(test)]
#[path = "../../tests/unit/db/runtime_config/image_output_tests.rs"]
mod image_output_tests;
#[cfg(test)]
#[path = "../../tests/unit/db/runtime_config/message_interactions_tests.rs"]
mod message_interactions_tests;

#[cfg(test)]
const SETTINGS_KEY: &str = "runtime.settings.v1";

pub use nuono_app::settings::RuntimeConfigPatch;
pub(crate) fn runtime_values(config: &NuonoConfig) -> Result<Value> {
    let mut clean = config.clone();
    clean.clear_secrets();
    let mut value =
        serde_json::to_value(clean).map_err(|e| NuonoError::Serialization(e.to_string()))?;
    let object = value.as_object_mut().expect("configuration is an object");
    // host/port/database/build/backup/admin_auth_enabled 是启动参数：只由 `nuono.toml`
    // 决定，不进运行设置，Dashboard 修改它们会被拒绝。
    for key in ["host", "port", "database", "build", "backup"] {
        object.remove(key);
    }
    if let Some(security) = object.get_mut("security").and_then(Value::as_object_mut) {
        security.remove("admin_auth_enabled");
    }
    if let Some(log) = object.get_mut("log").and_then(Value::as_object_mut) {
        log.remove("output");
    }
    Ok(value)
}

use nuono_app::settings::merge_runtime_values as merge_values;

#[async_trait::async_trait]
pub trait ConfigRuntimePersistence {
    async fn initialize_runtime_config(&self, seed: &NuonoConfig) -> Result<()>;
    async fn load_runtime_config(&self, startup: &NuonoConfig) -> Result<NuonoConfig>;
    async fn update_runtime_config(
        &self,
        patch: &RuntimeConfigPatch,
        startup: &NuonoConfig,
    ) -> Result<NuonoConfig>;
}

#[async_trait::async_trait]
impl ConfigRuntimePersistence for Database {
    async fn initialize_runtime_config(&self, seed: &NuonoConfig) -> Result<()> {
        self.initialize_runtime_settings(
            &runtime_values(seed)?,
            &super::secrets::config_secrets(seed),
        )
        .await
    }
    async fn load_runtime_config(&self, startup: &NuonoConfig) -> Result<NuonoConfig> {
        let stored = self.load_runtime_settings().await?;
        let secrets = stored
            .secrets
            .iter()
            .filter_map(|(key, value)| {
                super::SECRET_KEYS
                    .iter()
                    .find(|allowed| **allowed == key)
                    .map(|key| (*key, value.clone()))
            })
            .collect();
        build_candidate(startup, &stored.settings, &secrets)
    }
    async fn update_runtime_config(
        &self,
        patch: &RuntimeConfigPatch,
        startup: &NuonoConfig,
    ) -> Result<NuonoConfig> {
        let effective = nuono_app::settings::RuntimeSettingsStore::update(
            self,
            patch,
            &EffectiveConfigValidator(startup.clone()),
        )
        .await?;
        Ok(serde_json::from_value(effective)?)
    }
}

/// 用 bootstrap 基线、存储的运行设置与目标 secret 组装候选配置。
pub(crate) fn build_candidate(
    startup: &NuonoConfig,
    stored: &Value,
    secrets: &HashMap<&'static str, String>,
) -> Result<NuonoConfig> {
    let mut value =
        serde_json::to_value(startup).map_err(|e| NuonoError::Serialization(e.to_string()))?;
    let mut stored = stored.clone();
    if let Some(object) = stored.as_object_mut() {
        for key in ["host", "port", "database", "build", "backup"] {
            object.remove(key);
        }
        if let Some(security) = object.get_mut("security").and_then(Value::as_object_mut) {
            security.remove("admin_auth_enabled");
        }
    }
    if let Some(log) = stored.get_mut("log").and_then(Value::as_object_mut) {
        log.remove("output");
    }
    merge_values(&mut value, &stored);
    let mut candidate: NuonoConfig =
        serde_json::from_value(value).map_err(|e| NuonoError::ConfigLoad(e.to_string()))?;
    candidate.clear_secrets();
    for key in super::SECRET_KEYS {
        match secrets.get(key) {
            Some(value) => {
                if !super::secrets::write_secret_field(&mut candidate, key, Some(value)) {
                    return Err(NuonoError::ConfigValidation(format!(
                        "未知 secret 键: {key}"
                    )));
                }
            }
            None => {
                super::secrets::write_secret_field(&mut candidate, key, None);
            }
        }
    }
    Ok(candidate)
}

/// 计算「普通设置」的差异补丁：只包含 `previous` 与 `next` 之间发生变化的路径。
///
/// 供 [`crate::config::ConfigManager::update`] 的整快照兼容入口使用；新代码应走
/// 显式补丁（[`RuntimeConfigPatch`]）。
pub(crate) fn runtime_config_diff_patch(
    previous: &NuonoConfig,
    next: &NuonoConfig,
) -> Result<RuntimeConfigPatch> {
    let settings = changed_values(&runtime_values(previous)?, &runtime_values(next)?);
    let before = super::secrets::config_secrets(previous);
    let after = super::secrets::config_secrets(next);
    let secrets = super::SECRET_KEYS
        .iter()
        .filter_map(|key| {
            let old = before.iter().find(|(k, _)| k == key).map(|(_, v)| v);
            let new = after.iter().find(|(k, _)| k == key).map(|(_, v)| v);
            if old == new {
                return None;
            }
            Some(((*key).to_string(), new.cloned()))
        })
        .collect();
    Ok(RuntimeConfigPatch { settings, secrets })
}

/// 计算「普通设置」的差异树：只包含 `previous` 与 `next` 之间发生变化的路径。
pub(crate) fn changed_values(before: &Value, after: &Value) -> Value {
    let mut patch = serde_json::Map::new();
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        for (key, value) in after {
            let old = before.get(key).unwrap_or(&Value::Null);
            if old == value {
                continue;
            }
            if old.is_object() && value.is_object() {
                patch.insert(key.clone(), changed_values(old, value));
            } else {
                patch.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(patch)
}

#[cfg(test)]
mod tests {
    use super::{RuntimeConfigPatch, SETTINGS_KEY};
    use crate::config::{ConfigManager, NuonoConfig};
    use crate::test_support;

    #[tokio::test]
    async fn localization_patch_persists_without_rewriting_bootstrap_or_other_settings() {
        use nuono_app::localization::Locale;
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("locale-settings").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        // 兼容升级新增语言配置字段前已存在的旧版运行时配置记录。
        let mut legacy = super::runtime_values(&NuonoConfig::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("localization");
        db.set_global_config(SETTINGS_KEY, legacy).await.unwrap();
        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();
        assert_eq!(
            manager.snapshot().await.localization.default_locale,
            Locale::ZhCn
        );
        let before = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
        manager
            .update_patch(RuntimeConfigPatch {
                settings: serde_json::json!({"localization": {"default_locale": "ja"}}),
                secrets: vec![],
            })
            .await
            .unwrap();
        let saved = db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap();
        assert_eq!(saved["localization"]["default_locale"], "ja");
        assert_eq!(saved["subscription"], before["subscription"]);
        assert_eq!(saved["core_commands"], before["core_commands"]);
        assert_eq!(
            manager.snapshot().await.localization.default_locale,
            Locale::Ja
        );
        assert!(
            manager
                .update_patch(RuntimeConfigPatch {
                    settings: serde_json::json!({"localization": {"default_locale": "invalid"}}),
                    secrets: vec![]
                })
                .await
                .is_err()
        );
        assert_eq!(
            db.get_global_config(SETTINGS_KEY).await.unwrap().unwrap(),
            saved
        );
        let restarted = ConfigManager::load(Some(path.clone())).await.unwrap();
        restarted.attach_database(db.clone()).await.unwrap();
        assert_eq!(
            restarted.snapshot().await.localization.default_locale,
            Locale::Ja
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn secret_patch(key: &str, value: Option<&str>) -> RuntimeConfigPatch {
        RuntimeConfigPatch {
            settings: serde_json::json!({}),
            secrets: vec![(key.to_string(), value.map(str::to_string))],
        }
    }

    async fn temp_config_dir(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("nuono-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("nuono.toml");
        (root, path)
    }

    #[tokio::test]
    async fn retired_access_address_is_ignored_in_toml_database_and_environment() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("retired-access-address").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let original = format!(
            "public_url = \"not a valid TOML access URL\"\n{}",
            std::fs::read_to_string(&path).unwrap()
        );
        std::fs::write(&path, &original).unwrap();
        db.set_global_config(
            SETTINGS_KEY,
            serde_json::json!({
                "public_url": "not a valid database access URL",
                "log": { "level": "warn" }
            }),
        )
        .await
        .unwrap();

        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();

        let current = manager.snapshot().await;
        assert_eq!(current.log.level, "warn");
        assert!(
            serde_json::to_value(current.as_ref())
                .unwrap()
                .get("public_url")
                .is_none()
        );
        assert!(
            super::runtime_values(current.as_ref())
                .unwrap()
                .get("public_url")
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retired_workflow_settings_do_not_block_existing_database_startup() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("retired-workflow-settings").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let original = std::fs::read(&path).unwrap();
        db.set_global_config(
            SETTINGS_KEY,
            serde_json::json!({
                "ai": { "sop_model_alias": "legacy-workflow-model" },
                "security": {
                    "sop_alert_webhook_url": "https://example.com/legacy-alerts",
                    "sop_command_required_permission": "master"
                },
                "log": { "level": "warn" }
            }),
        )
        .await
        .unwrap();
        db.set_secret("security.sop_alert_webhook_token", "legacy-secret")
            .await
            .unwrap();

        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();
        let current = manager.get().await;
        assert_eq!(current.log.level, "warn");
        let current = serde_json::to_value(current).unwrap();
        assert!(current["ai"].get("sop_model_alias").is_none());
        assert!(current["security"].get("sop_alert_webhook_url").is_none());
        assert!(!current.to_string().contains("legacy-secret"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            db.get_secret("security.sop_alert_webhook_token")
                .await
                .unwrap()
                .as_deref(),
            Some("legacy-secret")
        );
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn runtime_settings_survive_restart_without_rewriting_toml() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-survive").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let original = std::fs::read(&path).unwrap();
        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();
        let mut cfg = manager.get().await;
        cfg.mail.smtp_host = "smtp.example.com".into();
        cfg.mail.smtp_password = Some("test-password".into());
        manager.update(cfg).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let restarted = ConfigManager::load(Some(path.clone())).await.unwrap();
        restarted.attach_database(db.clone()).await.unwrap();
        assert_eq!(restarted.get().await.mail.smtp_host, "smtp.example.com");
        assert_eq!(
            restarted.get().await.mail.smtp_password.as_deref(),
            Some("test-password")
        );
        let stored = db
            .get_global_config("runtime.settings.v1")
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.to_string().contains("test-password"));
        assert!(stored.get("database").is_none());
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_persistence_does_not_publish_config() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let manager = ConfigManager::for_test(
            NuonoConfig::default(),
            std::env::temp_dir().join("unused-config.toml"),
        );
        manager.attach_database(db.clone()).await.unwrap();
        db.shutdown().await.unwrap();
        let mut next = manager.get().await;
        next.mail.smtp_password = Some("must-not-publish".into());
        assert!(manager.update(next).await.is_err());
        assert_eq!(manager.get().await.mail.smtp_password, None);

        // 显式补丁同样必须先落库成功再发布内存快照。
        let patch = RuntimeConfigPatch {
            settings: serde_json::json!({ "mail": { "smtp_host": "must-not-publish" } }),
            secrets: vec![],
        };
        assert!(manager.update_patch(patch).await.is_err());
        assert_eq!(manager.get().await.mail.smtp_host, "");
    }

    #[tokio::test]
    async fn deleted_toml_seed_does_not_resurrect_on_restart() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-seed").await;
        let mut seed = NuonoConfig::default();
        seed.telegram.token = Some("seed-token".into());
        seed.save(&path).await.unwrap();

        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();
        assert_eq!(
            db.get_secret("telegram.token").await.unwrap().as_deref(),
            Some("seed-token")
        );
        // Telegram 未启用，删除是合法操作。
        manager.update_secret("telegram.token", None).await.unwrap();
        assert_eq!(db.get_secret("telegram.token").await.unwrap(), None);
        assert_eq!(manager.get().await.telegram.token, None);
        drop(manager);

        // TOML 里旧种子仍在，但运行设置行已存在，重启不会重新导入。
        let restarted = ConfigManager::load(Some(path.clone())).await.unwrap();
        restarted.attach_database(db.clone()).await.unwrap();
        assert_eq!(restarted.get().await.telegram.token, None);
        assert_eq!(db.get_secret("telegram.token").await.unwrap(), None);

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn deleting_enabled_module_secret_is_rejected_and_db_unchanged() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-kept-token").await;
        let mut seed = NuonoConfig::default();
        seed.telegram.enabled = true;
        seed.telegram.token = Some("tg-token".into());
        seed.save(&path).await.unwrap();

        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();

        // 启用中的模块缺不了必填密钥：补丁删除与直删都拒绝，数据库与内存都不变。
        let patch = secret_patch("telegram.token", None);
        assert!(manager.update_patch(patch).await.is_err());
        assert!(manager.update_secret("telegram.token", None).await.is_err());
        // 空串等价于删除，同样被拒绝。
        assert!(
            manager
                .update_secret("telegram.token", Some("   "))
                .await
                .is_err()
        );
        assert_eq!(
            db.get_secret("telegram.token").await.unwrap().as_deref(),
            Some("tg-token")
        );
        assert_eq!(
            manager.get().await.telegram.token.as_deref(),
            Some("tg-token")
        );

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn update_secret_publishes_transaction_candidate_directly() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-secret-publish").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();

        // 设置：内存发布的就是事务返回的有效配置，落库保留原值（trim 只用于判空）。
        manager
            .update_secret("metrics.token", Some(" padded "))
            .await
            .unwrap();
        assert_eq!(
            manager.get().await.metrics.token.as_deref(),
            Some(" padded ")
        );
        assert_eq!(
            db.get_secret("metrics.token").await.unwrap().as_deref(),
            Some(" padded ")
        );

        // 空串与纯空白等价删除，内存与数据库一致。
        manager
            .update_secret("metrics.token", Some("   "))
            .await
            .unwrap();
        assert_eq!(db.get_secret("metrics.token").await.unwrap(), None);
        assert_eq!(manager.get().await.metrics.token, None);

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_cannot_touch_startup_only_fields() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-startup-keys").await;
        NuonoConfig::default().save(&path).await.unwrap();
        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        manager.attach_database(db.clone()).await.unwrap();

        let startup_patch = |key: &str| RuntimeConfigPatch {
            settings: serde_json::Value::Object(
                [(key.to_string(), serde_json::json!("x"))]
                    .into_iter()
                    .collect(),
            ),
            secrets: vec![],
        };
        for key in ["host", "port", "database", "build", "backup"] {
            assert!(
                manager.update_patch(startup_patch(key)).await.is_err(),
                "{key} 属启动配置，补丁应被拒绝"
            );
        }
        let admin_auth = RuntimeConfigPatch {
            settings: serde_json::json!({ "security": { "admin_auth_enabled": true } }),
            secrets: vec![],
        };
        assert!(manager.update_patch(admin_auth).await.is_err());
        assert!(
            manager
                .update_patch(RuntimeConfigPatch {
                    settings: serde_json::json!({"log":{"output":["file"]}}),
                    secrets: vec![]
                })
                .await
                .is_err()
        );
        assert!(
            manager
                .update_patch(RuntimeConfigPatch {
                    settings: serde_json::json!(null),
                    secrets: vec![]
                })
                .await
                .is_err()
        );

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn bootstrap_load_defers_module_validation_to_attach() {
        let _env_lock = test_support::env_lock().await;
        let db = test_support::test_db(2).await;
        let (root, path) = temp_config_dir("config-bootstrap").await;
        // 手写 TOML 而不是 NuonoConfig::save：save 保留完整校验语义，会拒绝
        // 「启用中但无 token」的配置——这正是本测试要验证的启动引导宽容度。
        std::fs::write(
            &path,
            r#"host = "127.0.0.1"
port = 9923

[log]
level = "INFO"
output = ["stdout", "file"]
module = false

[database]
url = "postgres://nuono:nuono@127.0.0.1:5432/nuono"
max_connections = 5

[upstream]
default_url = "ws://127.0.0.1:9923/ws"
reconnect_interval_secs = 5
heartbeat_timeout_secs = 30

[telegram]
enabled = true
api_base_url = ""
"#,
        )
        .unwrap();

        // NuonoConfig::load 保留完整校验语义：此时报错。
        assert!(NuonoConfig::load(Some(path.clone())).await.is_err());
        // 启动引导只做 bootstrap 校验：放行，交给 attach 读回数据库后再完整校验。
        let manager = ConfigManager::load(Some(path.clone())).await.unwrap();
        let error = manager.attach_database(db.clone()).await.unwrap_err();
        assert!(error.to_string().contains("telegram.token"));
        // 校验失败不发布内存快照。
        assert!(manager.get().await.telegram.enabled);

        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// 运行期配置完整结构校验器。
pub(crate) struct EffectiveConfigValidator(pub NuonoConfig);
#[async_trait::async_trait]
impl nuono_app::settings::RuntimeSettingsValidator for EffectiveConfigValidator {
    async fn validate(&self, stored: nuono_app::settings::RuntimeConfigSnapshot) -> Result<Value> {
        let secrets = stored
            .secrets
            .iter()
            .filter_map(|(key, value)| {
                super::SECRET_KEYS
                    .iter()
                    .find(|allowed| **allowed == key)
                    .map(|key| (*key, value.clone()))
            })
            .collect();
        let candidate = build_candidate(&self.0, &stored.settings, &secrets)?;
        candidate
            .validate()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        Ok(serde_json::to_value(candidate)?)
    }
}
pub(crate) fn runtime_seed(
    config: &NuonoConfig,
) -> Result<nuono_app::settings::RuntimeConfigSnapshot> {
    Ok(nuono_app::settings::RuntimeConfigSnapshot {
        settings: runtime_values(config)?,
        secrets: super::secrets::config_secrets(config)
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    })
}

#[cfg(test)]
mod deployment_tests {
    use super::*;
    #[test]
    fn stored_deployment_fields_cannot_override_bootstrap() {
        let mut startup = NuonoConfig::default();
        startup.build.incremental = true;
        let candidate=build_candidate(&startup,&serde_json::json!({"host":"0.0.0.0","port":1,"database":{"url":"sqlite:wrong"},"build":{"incremental":false},"backup":{"enabled":false},"security":{"admin_auth_enabled":false},"log":{"output":[],"level":"WARN"}}),&HashMap::new()).unwrap();
        assert_eq!(candidate.host, startup.host);
        assert_eq!(candidate.port, startup.port);
        assert_eq!(candidate.database.url, startup.database.url);
        assert_eq!(candidate.build, startup.build);
        assert_eq!(candidate.backup.enabled, startup.backup.enabled);
        assert_eq!(
            candidate.security.admin_auth_enabled,
            startup.security.admin_auth_enabled
        );
        assert_eq!(candidate.log.output, startup.log.output);
        assert_eq!(candidate.log.level, "WARN");
    }

    #[test]
    fn build_settings_never_enter_runtime_values_or_database_seed() {
        let mut config = NuonoConfig::default();
        config.build.incremental = true;
        assert!(runtime_values(&config).unwrap().get("build").is_none());
        assert!(
            runtime_seed(&config)
                .unwrap()
                .settings
                .get("build")
                .is_none()
        );
    }
}
