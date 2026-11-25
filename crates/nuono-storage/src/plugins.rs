//! 持久化插件注册表和管理投影。
use sea_orm::{ActiveModelTrait, ActiveValue, ConnectionTrait, EntityTrait};

use crate::db::Database;
use crate::entity::plugins;
use crate::error::{NuonoError, Result};

mod trigger_policy;
pub use trigger_policy::PluginTriggerPolicy;

/// 未配置的插件使用一秒阈值；0 明确表示关闭慢调用识别。
pub const DEFAULT_SLOW_PLUGIN_THRESHOLD_MS: u64 = 1_000;
pub const MAX_SLOW_PLUGIN_THRESHOLD_MS: u64 = 3_600_000;

/// 数据库中持久化的插件配置差异（未覆盖字段的默认值仍由插件代码自身的配置 schema 声明决定）。
#[derive(Debug, Clone)]
pub struct PluginConfiguration {
    pub values: serde_json::Map<String, serde_json::Value>,
    pub service: serde_json::Map<String, serde_json::Value>,
    pub revision: i64,
    pub config_version: i64,
}

/// 在合并配置资源并抽离密钥字段之后，校验最终落库的完整序列化 JSON 单元格大小不超过可移植上限（8 MiB）。
pub(crate) async fn save_configuration_on(
    connection: &impl ConnectionTrait,
    row: plugins::ActiveModel,
) -> Result<()> {
    for (field, value) in [
        ("config_json", &row.config_json),
        ("service_config_json", &row.service_config_json),
    ] {
        if let ActiveValue::Set(json) | ActiveValue::Unchanged(json) = value
            && json.len() > crate::portable::MAX_CELL_BYTES
        {
            return Err(NuonoError::ConfigValidation(format!(
                "插件完整配置 {field} 超过单值 8 MiB 限制"
            )));
        }
    }
    row.update(connection).await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    #[schema(value_type = String, example = "feature")]
    pub plugin_role: nuono_app::plugin_role::PluginRole,
    pub enabled: bool,
    /// 客户能否在自己管理的群里开关这个插件。false = 由管理员统一管理。
    pub customer_toggleable: bool,
    pub allow_user_overrides: bool,
}

pub async fn get_plugins(db: &Database) -> Result<Vec<PluginInfo>> {
    let rows = plugins::Entity::find().all(db.orm()).await?;

    Ok(rows
        .into_iter()
        .map(|r| PluginInfo {
            id: r.plugin_id.clone(),
            name: r.plugin_id,
            plugin_role: nuono_app::plugin_role::PluginRole::parse(&r.plugin_role)
                .unwrap_or_default(),
            enabled: r.enabled,
            customer_toggleable: r.allow_group_overrides,
            allow_user_overrides: r.allow_user_overrides,
        })
        .collect())
}

/// 同步插件信息到数据库（转发到 `Database::sync_plugin`）。
pub async fn sync_plugin(
    db: &Database,
    name: &str,
    version: &str,
    config: &serde_json::Value,
) -> Result<()> {
    db.sync_plugin(name, version, config).await
}

/// 更新插件启用状态（转发到 `Database::set_plugin_enabled`）。
pub async fn set_plugin_enabled(db: &Database, name: &str, enabled: bool) -> Result<()> {
    db.set_plugin_enabled(name, enabled).await
}

/// 查询插件是否启用（转发到 `Database::is_plugin_enabled`）。
pub async fn is_plugin_enabled(db: &Database, name: &str) -> Result<bool> {
    db.is_plugin_enabled(name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn audit_count(db: &Database) -> i64 {
        db.orm()
            .query_one_raw(sea_orm::Statement::from_string(
                db.backend(),
                "SELECT COUNT(*) AS count FROM nuono_audit_events",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "count")
            .unwrap()
    }

    #[tokio::test]
    async fn resource_updates_bound_merged_and_escaped_configuration_without_mutation() {
        use nuono_app::configuration_resources::ConfigurationResourceDefinition;

        let db = crate::test_support::test_db(2).await;
        let plugin = "resource_size_probe";
        db.sync_plugin(plugin, "1", &serde_json::json!({}))
            .await
            .unwrap();
        let limit = crate::portable::MAX_CELL_BYTES;
        let first = ConfigurationResourceDefinition {
            key: "first".into(),
            label: "First".into(),
            description: String::new(),
            version: 1,
            default: serde_json::json!([]),
            max_bytes: limit,
            validator: |_| Ok(()),
        };
        let second = ConfigurationResourceDefinition {
            key: "second".into(),
            ..first.clone()
        };
        let tx = db.begin_write().await.unwrap();
        crate::configuration_resources::save_on(
            &tx,
            plugin,
            &first
                .validated_write(plugin, 0, serde_json::json!(["a".repeat(limit / 2)]))
                .unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let before = db.plugin_policy(plugin).await.unwrap();
        let audits = audit_count(&db).await;
        for write in [
            second
                .validated_write(plugin, 0, serde_json::json!(["b".repeat(limit / 2)]))
                .unwrap(),
            first
                .validated_write(plugin, 1, serde_json::json!(["\\".repeat(limit / 4)]))
                .unwrap(),
        ] {
            let tx = db.begin_write().await.unwrap();
            let error = crate::configuration_resources::save_on(&tx, plugin, &write)
                .await
                .unwrap_err();
            assert!(matches!(&error, NuonoError::ConfigValidation(_)));
            assert!(error.to_string().contains("config_json"));
            assert_eq!(
                plugins::Entity::find_by_id(plugin)
                    .one(&tx)
                    .await
                    .unwrap()
                    .unwrap(),
                before,
                "an oversized resource must not change configuration or revision"
            );
            tx.commit().await.unwrap();
            assert_eq!(db.plugin_policy(plugin).await.unwrap(), before);
            assert_eq!(audit_count(&db).await, audits);
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_global_and_service_configuration_roll_back_secrets_and_audit() {
        let db = crate::test_support::test_db(2).await;
        let plugin = "global_size_probe";
        db.sync_plugin(plugin, "1", &serde_json::json!({}))
            .await
            .unwrap();
        let secret_fields = vec!["token".to_owned()];
        let mut initial = db.plugin_configuration(plugin).await.unwrap();
        initial.values.insert("token".into(), "original".into());
        db.save_plugin_configuration_as(None, plugin, &initial, &secret_fields)
            .await
            .unwrap();
        let before = db.plugin_policy(plugin).await.unwrap();
        let secret_key = format!("plugin.{plugin}.config.token");
        let secret_before = crate::entity::runtime_secrets::Entity::find_by_id(secret_key.clone())
            .one(db.orm())
            .await
            .unwrap();
        let audits = audit_count(&db).await;
        for field in ["config_json", "service_config_json"] {
            let mut configuration = db.plugin_configuration(plugin).await.unwrap();
            configuration
                .values
                .insert("token".into(), "changed".into());
            let values = if field == "config_json" {
                &mut configuration.values
            } else {
                &mut configuration.service
            };
            values.insert(
                "payload".into(),
                "界".repeat(crate::portable::MAX_CELL_BYTES / 3).into(),
            );
            let error = db
                .save_plugin_configuration_as(None, plugin, &configuration, &secret_fields)
                .await
                .unwrap_err();
            assert!(matches!(&error, NuonoError::ConfigValidation(_)));
            assert!(error.to_string().contains(field));
            assert_eq!(db.plugin_policy(plugin).await.unwrap(), before);
            assert_eq!(
                crate::entity::runtime_secrets::Entity::find_by_id(secret_key.clone())
                    .one(db.orm())
                    .await
                    .unwrap(),
                secret_before,
                "the earlier secret write must roll back with oversized configuration"
            );
            assert_eq!(audit_count(&db).await, audits);
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn plugin_settings_default_to_closed_overrides_and_survive_discovery() {
        let db = crate::test_support::test_db(2).await;
        assert!(get_plugins(&db).await.unwrap().is_empty());
        sync_plugin(&db, "legacy", "1.0", &serde_json::json!({}))
            .await
            .unwrap();
        let original = get_plugins(&db).await.unwrap().remove(0);
        let legacy = get_plugins(&db).await.unwrap().remove(0);
        assert!(legacy.enabled);
        assert!(!legacy.customer_toggleable);
        assert!(!legacy.allow_user_overrides);

        set_plugin_enabled(&db, "legacy", false).await.unwrap();
        db.set_plugin_customer_toggleable("legacy", false)
            .await
            .unwrap();
        sync_plugin(&db, "legacy", "2.0", &serde_json::json!({}))
            .await
            .unwrap();
        let updated = get_plugins(&db).await.unwrap().remove(0);
        assert_eq!(updated.id, original.id);
        assert_eq!(updated.name, "legacy");
        assert!(!updated.enabled);
        assert!(!updated.customer_toggleable);
        assert!(!is_plugin_enabled(&db, "legacy").await.unwrap());
        assert!(is_plugin_enabled(&db, "missing").await.unwrap());
        db.shutdown().await.unwrap();
    }
}
