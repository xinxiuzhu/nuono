//! 插件注册表持久化。

use super::Database;
use crate::entity::plugins;
use crate::error::{NuonoError, Result};
use crate::plugins::{MAX_SLOW_PLUGIN_THRESHOLD_MS, PluginTriggerPolicy};
use nuono_app::plugin_role::PluginRole;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

#[cfg(test)]
#[path = "../../tests/unit/db/plugins_prune_tests.rs"]
mod prune_tests;

#[cfg(test)]
#[path = "../../tests/unit/db/plugins_threshold_tests.rs"]
mod threshold_tests;

#[cfg(test)]
#[path = "../../tests/unit/db/plugins_trigger_policy_tests.rs"]
mod trigger_policy_tests;

async fn sync_plugin<C: ConnectionTrait>(
    connection: &C,
    name: &str,
    _version: &str,
    _config: &serde_json::Value,
    default_enabled: bool,
    plugin_role: PluginRole,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    plugins::Entity::insert(plugins::ActiveModel {
        plugin_id: Set(name.to_string()),
        plugin_role: Set(plugin_role.as_str().to_owned()),
        enabled: Set(default_enabled),
        allow_user_overrides: Set(false),
        allow_group_overrides: Set(false),
        config_json: Set("{}".into()),
        service_config_json: Set("{}".into()),
        config_version: Set(1),
        revision: Set(1),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(plugins::Column::PluginId)
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(connection)
    .await?;
    // 已有记录时仅在代码声明的角色发生变化时更新；若转为底层服务插件，同步关闭群/个人覆盖开关。
    if plugin_role.is_service() {
        plugins::Entity::update_many()
            .col_expr(plugins::Column::PluginRole, Expr::value("service"))
            .col_expr(plugins::Column::AllowUserOverrides, Expr::value(false))
            .col_expr(plugins::Column::AllowGroupOverrides, Expr::value(false))
            .col_expr(plugins::Column::UpdatedAtMs, Expr::value(now))
            .filter(plugins::Column::PluginId.eq(name))
            .filter(
                plugins::Column::PluginRole
                    .ne("service")
                    .or(plugins::Column::AllowUserOverrides.eq(true))
                    .or(plugins::Column::AllowGroupOverrides.eq(true)),
            )
            .exec(connection)
            .await?;
    } else {
        plugins::Entity::update_many()
            .col_expr(plugins::Column::PluginRole, Expr::value("feature"))
            .col_expr(plugins::Column::UpdatedAtMs, Expr::value(now))
            .filter(plugins::Column::PluginId.eq(name))
            .filter(plugins::Column::PluginRole.ne("feature"))
            .exec(connection)
            .await?;
    }
    Ok(())
}

impl Database {
    /// 读取插件的触发限流策略（数据库列为 `NULL` 时返回 `None`，表示沿用宿主默认策略；若插件未注册或库中 JSON 非法则报错）。
    pub async fn get_plugin_trigger_policy(
        &self,
        name: &str,
    ) -> Result<Option<PluginTriggerPolicy>> {
        let row = plugins::Entity::find()
            .filter(plugins::Column::PluginId.eq(name))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::PluginNotFound(name.into()))?;
        row.trigger_policy_json
            .as_deref()
            .map(|raw| {
                let policy: PluginTriggerPolicy = serde_json::from_str(raw).map_err(|error| {
                    NuonoError::ConfigValidation(format!(
                        "插件 {name} 的触发策略 JSON 无效：{error}"
                    ))
                })?;
                policy.validate().map_err(|error| {
                    NuonoError::ConfigValidation(format!("插件 {name} 的触发策略无效：{error}"))
                })?;
                Ok(policy)
            })
            .transpose()
    }

    /// 仅更新已注册插件的宿主触发限流策略；绝不新建未知插件记录，也不覆盖插件业务配置。
    pub async fn set_plugin_trigger_policy(
        &self,
        name: &str,
        policy: &PluginTriggerPolicy,
    ) -> Result<()> {
        policy.validate()?;
        let raw = serde_json::to_string(policy)?;
        let result = plugins::Entity::update_many()
            .col_expr(
                plugins::Column::Revision,
                Expr::col(plugins::Column::Revision).add(1),
            )
            .col_expr(plugins::Column::TriggerPolicyJson, Expr::value(raw))
            .col_expr(
                plugins::Column::UpdatedAtMs,
                Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(plugins::Column::PluginId.eq(name))
            .exec(self.orm())
            .await?;
        if result.rows_affected == 0 {
            return Err(NuonoError::PluginNotFound(name.into()));
        }
        Ok(())
    }

    /// 已有插件只更新版本与配置，保留启用状态与运营策略。
    pub async fn sync_plugin(
        &self,
        name: &str,
        version: &str,
        config: &serde_json::Value,
    ) -> Result<()> {
        sync_plugin(self.orm(), name, version, config, true, PluginRole::Feature).await
    }

    /// 整批同步在同一事务内提交，失败时不会留下半启用状态。
    pub async fn sync_plugins_batch(
        &self,
        plugins: &[(&str, &str, &serde_json::Value)],
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        for (name, version, config) in plugins {
            sync_plugin(&tx, name, version, config, true, PluginRole::Feature).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 首次登记采用插件声明的启用默认值；已存在记录保留部署者的选择。
    pub async fn sync_plugins_batch_with_defaults(
        &self,
        plugins: &[(&str, &str, &serde_json::Value, bool)],
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        for (name, version, config, enabled) in plugins {
            sync_plugin(&tx, name, version, config, *enabled, PluginRole::Feature).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 首次登记采用插件声明的启用默认值与角色分级；已存在记录保留启停选择并同步最新角色分级。
    pub async fn sync_plugins_batch_with_roles(
        &self,
        plugins: &[(&str, &str, &serde_json::Value, bool, PluginRole)],
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        for (name, version, config, enabled, role) in plugins {
            sync_plugin(&tx, name, version, config, *enabled, *role).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 根据当前可执行文件实际搭载的插件清单同步插件安装状态，但保留已卸载插件的持久化配置与业务数据（数据清理由独立的 purge 流程负责）。
    pub async fn prune_plugin_registrations(
        &self,
        installed_names: &[String],
    ) -> Result<Vec<String>> {
        self.reconcile_plugin_installations(installed_names).await
    }

    /// 读取插件持久化保存的配置差异与服务开关（若插件行尚不存在，则仅初始化一行空配置记录）。
    pub async fn plugin_configuration(
        &self,
        name: &str,
    ) -> Result<crate::plugins::PluginConfiguration> {
        let backend = self.orm().get_database_backend();
        let select = sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT config_json, service_config_json, revision, config_version FROM nuono_plugins WHERE plugin_id = $1",
            [name.into()],
        );
        let row = match self.orm().query_one_raw(select.clone()).await? {
            Some(row) => row,
            None => {
                self.orm().execute_raw(sea_orm::Statement::from_sql_and_values(
                    backend,
                    "INSERT INTO nuono_plugins (plugin_id, created_at_ms, updated_at_ms) VALUES ($1, $2, $2) ON CONFLICT(plugin_id) DO NOTHING",
                    [name.into(), chrono::Utc::now().timestamp_millis().into()],
                )).await?;
                self.orm()
                    .query_one_raw(select)
                    .await?
                    .ok_or_else(|| NuonoError::PluginNotFound(name.into()))?
            }
        };
        let parse = |raw: String| {
            serde_json::from_str(&raw)
                .map_err(|_| NuonoError::ConfigValidation("插件配置不是有效 JSON 对象".into()))
        };
        let mut values = parse(row.try_get("", "config_json")?)?;
        crate::plugin_global_settings::hydrate_secrets(self.orm(), name, &mut values).await?;
        Ok(crate::plugins::PluginConfiguration {
            values,
            service: parse(row.try_get("", "service_config_json")?)?,
            revision: row.try_get("", "revision")?,
            config_version: row.try_get("", "config_version")?,
        })
    }

    /// 校验乐观锁版本号（`revision`）与配置结构版本（`config_version`）匹配后，原子替换已验证的插件配置差异。
    pub async fn save_plugin_configuration(
        &self,
        name: &str,
        configuration: &crate::plugins::PluginConfiguration,
    ) -> Result<i64> {
        self.save_plugin_configuration_as(None, name, configuration, &[])
            .await
    }

    pub async fn set_plugin_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        plugins::Entity::update_many()
            .col_expr(
                plugins::Column::Revision,
                Expr::col(plugins::Column::Revision).add(1),
            )
            .col_expr(plugins::Column::Enabled, Expr::value(enabled))
            .col_expr(
                plugins::Column::UpdatedAtMs,
                Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(plugins::Column::PluginId.eq(name))
            .exec(self.orm())
            .await?;
        Ok(())
    }

    pub async fn set_plugin_customer_toggleable(
        &self,
        name: &str,
        toggleable: bool,
    ) -> Result<bool> {
        if toggleable
            && let Some(row) = plugins::Entity::find_by_id(name).one(self.orm()).await?
            && row.plugin_role == PluginRole::Service.as_str()
        {
            return Err(NuonoError::PermissionDenied(
                "底层服务插件不支持开放群级配置".into(),
            ));
        }
        let result = plugins::Entity::update_many()
            .col_expr(
                plugins::Column::Revision,
                Expr::col(plugins::Column::Revision).add(1),
            )
            .col_expr(
                plugins::Column::AllowGroupOverrides,
                Expr::value(toggleable),
            )
            .col_expr(
                plugins::Column::UpdatedAtMs,
                Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(plugins::Column::PluginId.eq(name))
            .exec(self.orm())
            .await?;
        Ok(result.rows_affected > 0)
    }

    /// 读取宿主管理的诊断策略。未注册或未配置时返回 None，由宿主选择默认阈值。
    pub async fn get_plugin_slow_threshold(&self, name: &str) -> Result<Option<u64>> {
        let row = plugins::Entity::find()
            .filter(plugins::Column::PluginId.eq(name))
            .one(self.orm())
            .await?;
        row.and_then(|row| row.slow_threshold_ms)
            .map(|threshold| {
                u64::try_from(threshold)
                    .ok()
                    .filter(|threshold| *threshold <= MAX_SLOW_PLUGIN_THRESHOLD_MS)
                    .ok_or_else(|| NuonoError::ConfigValidation(format!(
                        "插件 {name} 的慢调用阈值必须为 0 至 {MAX_SLOW_PLUGIN_THRESHOLD_MS} 毫秒"
                    )))
            })
            .transpose()
    }

    /// 不创建历史/未知插件行。语句成功提交后返回该注册记录是否存在。
    pub async fn set_plugin_slow_threshold(&self, name: &str, threshold_ms: u64) -> Result<bool> {
        if threshold_ms > MAX_SLOW_PLUGIN_THRESHOLD_MS {
            return Err(NuonoError::Validation(format!(
                "慢调用阈值必须为 0 至 {MAX_SLOW_PLUGIN_THRESHOLD_MS} 毫秒"
            )));
        }
        let result = plugins::Entity::update_many()
            .col_expr(
                plugins::Column::Revision,
                Expr::col(plugins::Column::Revision).add(1),
            )
            .col_expr(
                plugins::Column::SlowThresholdMs,
                Expr::value(threshold_ms as i64),
            )
            .col_expr(
                plugins::Column::UpdatedAtMs,
                Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(plugins::Column::PluginId.eq(name))
            .exec(self.orm())
            .await?;
        Ok(result.rows_affected > 0)
    }
}
