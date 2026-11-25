//! 插件配置版本升级：在同一写事务内对插件全局配置差异及所有用户/群组级配置覆盖项执行原子格式迁移。
use crate::{
    Database,
    entity::{plugin_overrides, plugins},
    error::{NuonoError, Result},
};
use nuono_app::config_schema::ConfigurationTarget;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set,
};
use serde_json::{Map, Value};

impl Database {
    pub async fn upgrade_plugin_configuration<F>(
        &self,
        plugin: &str,
        version: i64,
        secret_fields: &[String],
        mut convert: F,
    ) -> Result<()>
    where
        F: FnMut(
                i64,
                ConfigurationTarget,
                &Map<String, Value>,
                &Map<String, Value>,
            ) -> Result<Map<String, Value>>
            + Send,
    {
        if version <= 0 {
            return Err(NuonoError::ConfigValidation(
                "插件配置格式版本必须大于零".into(),
            ));
        }
        self.plugin_configuration(plugin).await?;
        let tx = self.begin_write().await?;
        let mut query = plugins::Entity::find_by_id(plugin);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let global = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        if version < global.config_version {
            return Err(NuonoError::ConfigValidation(
                "插件代码配置版本低于已保存版本，拒绝降级解释".into(),
            ));
        }
        let parse = |raw: &str| {
            serde_json::from_str::<Map<String, Value>>(raw)
                .map_err(|_| NuonoError::ConfigValidation("配置差异必须是 JSON 对象".into()))
        };
        let mut values = parse(&global.config_json)?;
        crate::plugin_global_settings::hydrate_secrets(&tx, plugin, &mut values).await?;
        let service = parse(&global.service_config_json)?;
        let mut converted = convert(
            global.config_version,
            ConfigurationTarget::Global,
            &values,
            &service,
        )?;
        let mut rows = plugin_overrides::Entity::find()
            .filter(plugin_overrides::Column::PluginId.eq(plugin))
            .order_by_asc(plugin_overrides::Column::Id);
        if self.backend() == DbBackend::Postgres {
            rows = rows.lock_exclusive();
        }
        let mut replacements = Vec::new();
        for row in rows.all(&tx).await? {
            let target = match (row.user_id, row.group_id) {
                (Some(id), None) => ConfigurationTarget::User(id),
                (None, Some(id)) => ConfigurationTarget::Group(id),
                _ => return Err(NuonoError::ConfigValidation("覆盖对象约束无效".into())),
            };
            let mut values = parse(&row.config_patch_json)?;
            crate::plugin_global_settings::hydrate_secrets(&tx, plugin, &mut values).await?;
            let next = convert(
                global.config_version,
                target,
                &values,
                &parse(&row.service_patch_json)?,
            )?;
            replacements.push((row, next));
        }
        if global.config_version != version {
            crate::plugin_global_settings::store_secrets(
                &tx,
                plugin,
                &mut converted,
                secret_fields,
            )
            .await?;
            let previous_version = global.config_version;
            let now = chrono::Utc::now().timestamp_millis();
            for (row, mut values) in replacements {
                let scope = if let Some(id) = row.user_id {
                    format!("user.{id}.")
                } else {
                    format!("group.{}.", row.group_id.expect("group target"))
                };
                crate::plugin_global_settings::store_scoped_secrets(
                    &tx,
                    plugin,
                    &scope,
                    &mut values,
                    secret_fields,
                )
                .await?;
                let revision = row
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| NuonoError::Conflict("覆盖版本已耗尽".into()))?;
                let mut row: plugin_overrides::ActiveModel = row.into();
                row.config_patch_json = Set(Value::Object(values).to_string());
                row.revision = Set(revision);
                row.updated_at_ms = Set(now);
                row.update(&tx).await?;
            }
            let revision = global
                .revision
                .checked_add(1)
                .ok_or_else(|| NuonoError::Conflict("全局配置版本已耗尽".into()))?;
            let mut global: plugins::ActiveModel = global.into();
            global.config_json = Set(Value::Object(converted).to_string());
            global.config_version = Set(version);
            global.revision = Set(revision);
            global.updated_at_ms = Set(now);
            crate::plugins::save_configuration_on(&tx, global).await?;
            crate::db::append_audit_on(&tx,crate::db::AuditEvent{actor_user_id:None,actor_plugin_id:Some(plugin),action:"plugin.config.upgrade",result:"success",subject_user_id:None,group_id:None,operation_id:None,details:serde_json::json!({"from_version":previous_version.to_string(),"to_version":version.to_string()})}).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
