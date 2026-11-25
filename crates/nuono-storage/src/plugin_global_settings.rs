//! 插件全局策略与配置修改：操作者权限复核、乐观锁（CAS）、密钥提取落盘与操作审计在同一事务内原子提交。
use crate::{
    Database,
    entity::{plugins, users},
    error::{NuonoError, Result},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter,
    QuerySelect, Set,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginPolicyPatch {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type=String)]
    pub expected_revision: i64,
    pub enabled: Option<bool>,
    pub allow_user_overrides: Option<bool>,
    pub allow_group_overrides: Option<bool>,
}

pub(crate) async fn lock_administrator(
    tx: &sea_orm::DatabaseTransaction,
    user_id: i64,
) -> Result<users::orm::Model> {
    let mut query = users::orm::Entity::find_by_id(user_id);
    if tx.get_database_backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    let user = query
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
    if user.status != "active" || !matches!(user.role.as_str(), "owner" | "admin") {
        return Err(NuonoError::PermissionDenied("需要有效管理员账号".into()));
    }
    Ok(user)
}

impl Database {
    pub async fn plugin_policy(&self, plugin: &str) -> Result<plugins::Model> {
        plugins::Entity::find_by_id(plugin)
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))
    }
    pub async fn save_plugin_policy(
        &self,
        actor: i64,
        plugin: &str,
        patch: PluginPolicyPatch,
    ) -> Result<plugins::Model> {
        let tx = self.begin_write().await?;
        let actor_user = lock_administrator(&tx, actor).await?;
        let mut query = plugins::Entity::find_by_id(plugin);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let row = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        if row.plugin_role == nuono_app::plugin_role::PluginRole::Service.as_str() {
            if actor_user.role != "owner" {
                return Err(NuonoError::PermissionDenied(
                    "仅部署所有者可管理底层服务插件".into(),
                ));
            }
            if patch.allow_user_overrides == Some(true) || patch.allow_group_overrides == Some(true)
            {
                return Err(NuonoError::Validation(
                    "底层服务插件不支持开放个人或群级配置覆盖".into(),
                ));
            }
        }
        if row.revision != patch.expected_revision {
            return Err(NuonoError::Conflict("插件设置已更新，请刷新".into()));
        }
        let revision = row
            .revision
            .checked_add(1)
            .ok_or_else(|| NuonoError::Conflict("配置版本已耗尽".into()))?;
        let mut row: plugins::ActiveModel = row.into();
        if let Some(value) = patch.enabled {
            row.enabled = Set(value);
        }
        if let Some(value) = patch.allow_user_overrides {
            row.allow_user_overrides = Set(value);
        }
        if let Some(value) = patch.allow_group_overrides {
            row.allow_group_overrides = Set(value);
        }
        row.revision = Set(revision);
        row.updated_at_ms = Set(chrono::Utc::now().timestamp_millis());
        let row = row.update(&tx).await?;
        crate::db::append_audit_on(
            &tx,
            crate::db::AuditEvent {
                actor_user_id: Some(actor),
                actor_plugin_id: None,
                action: "plugin.policy.update",
                result: "success",
                subject_user_id: None,
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"plugin_id":plugin,"revision":revision.to_string()}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(row)
    }
    /// 保存插件全局配置（`actor = None` 仅保留给受信任的插件生命周期与 SDK 内部代码调用；HTTP 管理接口必须传入具体管理员账号 ID）。
    pub async fn save_plugin_configuration_as(
        &self,
        actor: Option<i64>,
        plugin: &str,
        configuration: &crate::plugins::PluginConfiguration,
        secret_fields: &[String],
    ) -> Result<i64> {
        self.save_plugin_configuration_import_as(
            actor,
            plugin,
            configuration,
            secret_fields,
            &std::collections::HashMap::new(),
        )
        .await
    }

    pub async fn save_plugin_configuration_import_as(
        &self,
        actor: Option<i64>,
        plugin: &str,
        configuration: &crate::plugins::PluginConfiguration,
        secret_fields: &[String],
        imported_secrets: &std::collections::HashMap<String, Option<String>>,
    ) -> Result<i64> {
        self.save_plugin_configuration_import_at_version(
            actor,
            plugin,
            configuration,
            secret_fields,
            imported_secrets,
            None,
        )
        .await
    }

    /// 导入或保存指定版本的插件全局配置：仅当数据库中该插件仍处于尚未修改的初始空配置状态（且无任何用户/群组覆盖项）时，才允许直接初始化为导入声明的配置版本号。
    pub async fn save_plugin_configuration_import_at_version(
        &self,
        actor: Option<i64>,
        plugin: &str,
        configuration: &crate::plugins::PluginConfiguration,
        secret_fields: &[String],
        imported_secrets: &std::collections::HashMap<String, Option<String>>,
        imported_version: Option<i64>,
    ) -> Result<i64> {
        let tx = self.begin_write().await?;
        let actor_user = match actor {
            Some(actor) => Some(lock_administrator(&tx, actor).await?),
            None => None,
        };
        let mut query = plugins::Entity::find_by_id(plugin);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let row = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        if row.plugin_role == nuono_app::plugin_role::PluginRole::Service.as_str()
            && actor_user.as_ref().is_some_and(|user| user.role != "owner")
        {
            return Err(NuonoError::PermissionDenied(
                "仅部署所有者可配置底层服务插件".into(),
            ));
        }
        let initialize_version = imported_version == Some(configuration.config_version)
            && configuration.config_version > 0
            && row.revision == 1
            && row.config_json == "{}"
            && row.service_config_json == "{}";
        if initialize_version {
            use sea_orm::PaginatorTrait;
            if crate::entity::plugin_overrides::Entity::find()
                .filter(crate::entity::plugin_overrides::Column::PluginId.eq(plugin))
                .count(&tx)
                .await?
                != 0
            {
                return Err(NuonoError::Conflict(
                    "已有对象覆盖，不能直接改变配置版本".into(),
                ));
            }
        }
        if row.revision != configuration.revision
            || (row.config_version != configuration.config_version && !initialize_version)
        {
            return Err(NuonoError::Conflict("插件配置版本冲突，请刷新".into()));
        }
        let revision = row
            .revision
            .checked_add(1)
            .ok_or_else(|| NuonoError::Conflict("配置版本已耗尽".into()))?;
        let mut values = configuration.values.clone();
        let now = chrono::Utc::now().timestamp_millis();
        store_secrets(&tx, plugin, &mut values, secret_fields).await?;
        for (field, value) in imported_secrets {
            if field.is_empty()
                || !field
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(NuonoError::ConfigValidation("密钥声明键无效".into()));
            }
            let key = format!("plugin.{plugin}.{field}");
            if let Some(value) = value {
                tx.execute_raw(sea_orm::Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_runtime_secrets (key,owner_plugin_id,value,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,$3,1,$4,$4) ON CONFLICT(key) DO UPDATE SET value=excluded.value,revision=nuono_runtime_secrets.revision+1,updated_at_ms=excluded.updated_at_ms",[key.into(),plugin.into(),value.as_bytes().to_vec().into(),now.into()])).await?;
            } else {
                tx.execute_raw(sea_orm::Statement::from_sql_and_values(
                    self.backend(),
                    "DELETE FROM nuono_runtime_secrets WHERE key=$1 AND owner_plugin_id=$2",
                    [key.into(), plugin.into()],
                ))
                .await?;
            }
        }

        let mut row: plugins::ActiveModel = row.into();
        row.config_version = Set(configuration.config_version);
        row.config_json = Set(serde_json::Value::Object(values).to_string());
        row.service_config_json =
            Set(serde_json::Value::Object(configuration.service.clone()).to_string());
        row.revision = Set(revision);
        row.updated_at_ms = Set(now);
        crate::plugins::save_configuration_on(&tx, row).await?;
        crate::db::append_audit_on(&tx,crate::db::AuditEvent{actor_user_id:actor,actor_plugin_id:if actor.is_none(){Some(plugin)}else{None},action:"plugin.config.update",result:"success",subject_user_id:None,group_id:None,operation_id:None,details:serde_json::json!({"plugin_id":plugin,"revision":revision.to_string(),"secret_fields":secret_fields})}).await?;
        tx.commit().await?;
        Ok(revision)
    }
}

pub(crate) async fn hydrate_secrets(
    connection: &impl ConnectionTrait,
    plugin: &str,
    values: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    for (field, value) in values.iter_mut() {
        if let Some(key) = value
            .as_object()
            .and_then(|map| map.get("$secret"))
            .and_then(|key| key.as_str())
        {
            if !key.starts_with(&format!("plugin.{plugin}.config."))
                || !key.ends_with(&format!(".{field}"))
            {
                return Err(NuonoError::ConfigValidation("配置密钥引用归属无效".into()));
            }
            let row = connection
                .query_one_raw(sea_orm::Statement::from_sql_and_values(
                    connection.get_database_backend(),
                    "SELECT value FROM nuono_runtime_secrets WHERE key=$1 AND owner_plugin_id=$2",
                    [key.into(), plugin.into()],
                ))
                .await?
                .ok_or_else(|| NuonoError::ConfigValidation("配置密钥引用缺失".into()))?;
            let bytes: Vec<u8> = row.try_get("", "value")?;
            *value = String::from_utf8(bytes)
                .map_err(|_| NuonoError::ConfigValidation("配置密钥编码无效".into()))?
                .into();
        }
    }
    Ok(())
}

pub(crate) async fn store_secrets(
    tx: &impl ConnectionTrait,
    plugin: &str,
    values: &mut serde_json::Map<String, serde_json::Value>,
    secret_fields: &[String],
) -> Result<()> {
    store_scoped_secrets(tx, plugin, "", values, secret_fields).await
}

pub(crate) async fn store_scoped_secrets(
    tx: &impl ConnectionTrait,
    plugin: &str,
    scope: &str,
    values: &mut serde_json::Map<String, serde_json::Value>,
    secret_fields: &[String],
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    for field in secret_fields {
        let key = format!("plugin.{plugin}.config.{scope}{field}");
        match values.remove(field) {
            Some(value) => {
                let raw = value.as_str().ok_or_else(|| {
                    NuonoError::ConfigValidation(format!("密钥字段 {field} 必须为字符串"))
                })?;
                tx.execute_raw(sea_orm::Statement::from_sql_and_values(tx.get_database_backend(),"INSERT INTO nuono_runtime_secrets (key,owner_plugin_id,value,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,$3,1,$4,$4) ON CONFLICT(key) DO UPDATE SET value=excluded.value,revision=nuono_runtime_secrets.revision+1,updated_at_ms=excluded.updated_at_ms",[key.clone().into(),plugin.into(),raw.as_bytes().to_vec().into(),now.into()])).await?;
                values.insert(field.clone(), serde_json::json!({"$secret":key}));
            }
            None => {
                tx.execute_raw(sea_orm::Statement::from_sql_and_values(
                    tx.get_database_backend(),
                    "DELETE FROM nuono_runtime_secrets WHERE key=$1 AND owner_plugin_id=$2",
                    [key.into(), plugin.into()],
                ))
                .await?;
            }
        }
    }
    Ok(())
}

/// 清理指定插件不再被任何全局配置或用户/群组覆盖项引用的孤儿密钥记录（必须在持有该插件行锁、且完成配置更新或账号合并引用变更后调用）。
pub async fn purge_unused_plugin_secrets_on(
    connection: &impl ConnectionTrait,
    plugin: &str,
) -> Result<()> {
    let mut references = std::collections::HashSet::<String>::new();
    let rows=connection.query_all_raw(sea_orm::Statement::from_sql_and_values(connection.get_database_backend(),"SELECT config_json AS config FROM nuono_plugins WHERE plugin_id=$1 UNION ALL SELECT config_patch_json AS config FROM nuono_plugin_overrides WHERE plugin_id=$1",[plugin.into()])).await?;
    for row in rows {
        let raw: String = row.try_get("", "config")?;
        let values: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)?;
        for value in values.values() {
            if let Some(key) = value.get("$secret").and_then(|value| value.as_str()) {
                references.insert(key.to_owned());
            }
        }
    }
    let rows = connection
        .query_all_raw(sea_orm::Statement::from_sql_and_values(
            connection.get_database_backend(),
            "SELECT key FROM nuono_runtime_secrets WHERE owner_plugin_id=$1",
            [plugin.into()],
        ))
        .await?;
    for row in rows {
        let key: String = row.try_get("", "key")?;
        if key.starts_with(&format!("plugin.{plugin}.config.")) && !references.contains(&key) {
            connection
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    connection.get_database_backend(),
                    "DELETE FROM nuono_runtime_secrets WHERE key=$1 AND owner_plugin_id=$2",
                    [key.into(), plugin.into()],
                ))
                .await?;
        }
    }
    Ok(())
}
