//! 运行时通用配置与受管密钥（secrets）的持久化：在同一个经过完整校验的写事务中原子更新。
use super::{Database, SECRET_KEYS};
use crate::entity::{global_configs, runtime_secrets};
use crate::error::{NuonoError, Result};
use nuono_app::model_catalog::{AiModelRecord, ModelChange, validate_references};
use nuono_app::settings::{
    RuntimeConfigPatch, RuntimeConfigSnapshot, merge_runtime_values, reject_startup_patch,
};
use sea_orm::{
    ColumnTrait, DbBackend, EntityTrait, QueryFilter, QuerySelect, Set, TryInsertResult,
};
use serde_json::Value;
use std::collections::HashMap;
const SETTINGS_KEY: &str = "runtime.settings.v1";
impl Database {
    pub async fn initialize_runtime_settings(
        &self,
        settings: &Value,
        seeds: &[(&str, String)],
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        super::ai_models::lock_catalog(&tx, self.backend()).await?;
        let inserted = matches!(global_configs::Entity::insert(global_configs::ActiveModel {
            key: Set(SETTINGS_KEY.into()), value: Set(settings.to_string()), updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            created_at_ms:Set(crate::clock::db_now().timestamp_millis()),
            ..Default::default()
        }).on_conflict_do_nothing().exec_without_returning(&tx).await?, TryInsertResult::Inserted(rows) if rows > 0);
        if inserted {
            for (key, value) in seeds {
                runtime_secrets::Entity::insert(runtime_secrets::ActiveModel {
                    key: Set((*key).into()),
                    value: Set(value.as_bytes().to_vec()),
                    updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
                    created_at_ms: Set(crate::clock::db_now().timestamp_millis()),
                    ..Default::default()
                })
                .on_conflict_do_nothing()
                .exec_without_returning(&tx)
                .await?;
            }
            if !seeds.is_empty() {
                tracing::info!(keys=%seeds.iter().map(|(key, _)| *key).collect::<Vec<_>>().join(", "), "已把配置文件中的 secret 种子导入数据库，请从文件中删除这些明文值");
            }
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn all_secret_values(&self) -> Result<HashMap<String, String>> {
        runtime_secrets::Entity::find()
            .filter(runtime_secrets::Column::OwnerPluginId.is_null())
            .all(self.orm())
            .await?
            .into_iter()
            .filter(|row| !row.value.is_empty())
            .map(|row| super::secrets::decode_secret(row.value).map(|value| (row.key, value)))
            .collect::<Result<_>>()
    }

    pub async fn load_runtime_settings(&self) -> Result<RuntimeConfigSnapshot> {
        // 先锁定所有框架配置与密钥更新共用的 runtime.settings.v1 行，
        // 保证紧接着读取的普通设置与密钥表来自同一已提交事务边界，不会读到半途交错的不一致快照。
        let tx = self.begin_write().await?;
        let mut query = global_configs::Entity::find_by_id(SETTINGS_KEY);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let row = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::ConfigLoad("运行设置尚未初始化".into()))?;
        let settings = serde_json::from_str(&row.value)
            .map_err(|error| NuonoError::ConfigLoad(format!("运行设置无效: {error}")))?;
        let secrets = runtime_secrets::Entity::find()
            .filter(runtime_secrets::Column::OwnerPluginId.is_null())
            .all(&tx)
            .await?
            .into_iter()
            .filter(|row| !row.value.is_empty())
            .map(|row| super::secrets::decode_secret(row.value).map(|value| (row.key, value)))
            .collect::<Result<_>>()?;
        tx.commit().await?;
        Ok(RuntimeConfigSnapshot { settings, secrets })
    }
    /// 在同一写事务中合并运行时配置与密钥补丁，并交由调用方提供的 `validate` 闭包做完整校验（含环境变量等部署覆盖项）。
    /// 若校验失败则整个事务回滚，不会写入任何配置或密钥；只有事务成功提交后才将校验闭包生成的候选结果返回给调用方发布到内存。
    pub async fn update_runtime_settings<T, F, Fut>(
        &self,
        patch: &RuntimeConfigPatch,
        validate: F,
    ) -> Result<T>
    where
        F: FnOnce(RuntimeConfigSnapshot) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        self.update_runtime_settings_checked(patch, validate, |_| Ok(()))
            .await
    }

    pub async fn update_runtime_settings_checked<T, F, Fut, Check>(
        &self,
        patch: &RuntimeConfigPatch,
        validate: F,
        check: Check,
    ) -> Result<T>
    where
        F: FnOnce(RuntimeConfigSnapshot) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        Check: FnOnce(&Value) -> Result<()> + Send,
        T: Send,
    {
        self.update_runtime_settings_snapshot_checked(
            patch,
            None,
            |snapshot, _| validate(snapshot),
            |current| check(&current.settings),
        )
        .await
        .map(|(candidate, _)| candidate)
    }

    async fn update_runtime_settings_snapshot_checked<T, F, Fut, Check>(
        &self,
        patch: &RuntimeConfigPatch,
        change: Option<ModelChange<'_>>,
        validate: F,
        check: Check,
    ) -> Result<(T, Option<AiModelRecord>)>
    where
        F: FnOnce(RuntimeConfigSnapshot, Vec<AiModelRecord>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        Check: FnOnce(&RuntimeConfigSnapshot) -> Result<()> + Send,
        T: Send,
    {
        reject_startup_patch(&patch.settings)?;
        let enables_ai = patch
            .settings
            .get("ai")
            .and_then(Value::as_object)
            .is_some_and(|ai| {
                ai.get("enabled").and_then(Value::as_bool) == Some(true)
                    || ai.get("reply_enabled").and_then(Value::as_bool) == Some(true)
                    || ai
                        .get("assistant")
                        .and_then(Value::as_object)
                        .and_then(|a| a.get("enabled"))
                        .and_then(Value::as_bool)
                        == Some(true)
            });
        if change.is_some() || enables_ai {
            self.ensure_ai_module().await?;
        }
        let tx = self.begin_write().await?;
        super::ai_models::lock_catalog(&tx, self.backend()).await?;
        let mut query = global_configs::Entity::find_by_id(SETTINGS_KEY);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let mut stored: Value = serde_json::from_str(
            &query
                .one(&tx)
                .await?
                .ok_or_else(|| NuonoError::ConfigLoad("运行设置尚未初始化".into()))?
                .value,
        )
        .map_err(|error| NuonoError::Serialization(error.to_string()))?;
        let mut original = HashMap::new();
        for key in SECRET_KEYS {
            let mut query = runtime_secrets::Entity::find_by_id(*key);
            if self.backend() == DbBackend::Postgres {
                query = query.lock_exclusive();
            }
            if let Some(value) = query
                .one(&tx)
                .await?
                .map(|row| super::secrets::decode_secret(row.value))
                .transpose()?
                .filter(|value| !value.trim().is_empty())
            {
                original.insert((*key).to_string(), value);
            }
        }
        check(&RuntimeConfigSnapshot {
            settings: stored.clone(),
            secrets: original.clone(),
        })?;
        merge_runtime_values(&mut stored, &patch.settings);
        let mut secrets = original.clone();
        for (key, value) in &patch.secrets {
            if !SECRET_KEYS.contains(&key.as_str()) {
                return Err(NuonoError::ConfigValidation(format!(
                    "未知 secret 键: {key}"
                )));
            }
            match value.as_deref() {
                Some(value) if !value.trim().is_empty() => {
                    secrets.insert(key.clone(), value.to_string());
                }
                _ => {
                    secrets.remove(key);
                }
            }
        }
        let changing_model = change.is_some();
        let model = match change {
            Some(change) => super::ai_models::apply_change(&tx, change)
                .await?
                .map(Into::into),
            None => None,
        };
        let models = super::ai_models::catalog(&tx).await?;
        validate_references(&stored, &models, false).map_err(|error| {
            if changing_model {
                super::ai_models::reference_conflict(error)
            } else {
                error
            }
        })?;
        let candidate = validate(
            RuntimeConfigSnapshot {
                settings: stored.clone(),
                secrets: secrets.clone(),
            },
            models,
        )
        .await?;
        use sea_orm::sea_query::{Expr, ExprTrait};
        global_configs::Entity::update_many()
            .col_expr(
                global_configs::Column::Value,
                Expr::value(stored.to_string()),
            )
            .col_expr(
                global_configs::Column::Revision,
                Expr::col(global_configs::Column::Revision).add(1),
            )
            .col_expr(
                global_configs::Column::UpdatedAt,
                Expr::value(crate::clock::db_now()),
            )
            .filter(global_configs::Column::Key.eq(SETTINGS_KEY))
            .exec(&tx)
            .await?;
        for key in SECRET_KEYS {
            if original.get(*key) == secrets.get(*key) {
                continue;
            }
            if let Some(value) = secrets.get(*key) {
                super::set_secret_on(&tx, key, value).await?;
            } else {
                runtime_secrets::Entity::delete_many()
                    .filter(runtime_secrets::Column::Key.eq(*key))
                    .exec(&tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok((candidate, model))
    }
}

#[async_trait::async_trait]
impl nuono_app::settings::RuntimeSettingsStore for Database {
    async fn initialize(&self, seed: RuntimeConfigSnapshot) -> Result<()> {
        let seeds: Vec<_> = seed
            .secrets
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        self.initialize_runtime_settings(&seed.settings, &seeds)
            .await
    }
    async fn load(&self) -> Result<RuntimeConfigSnapshot> {
        self.load_runtime_settings().await
    }
    async fn update(
        &self,
        patch: &RuntimeConfigPatch,
        validator: &dyn nuono_app::settings::RuntimeSettingsValidator,
    ) -> Result<Value> {
        self.update_runtime_settings_snapshot_checked(patch, None,
            |candidate, models| async move { validator.validate_with_models(candidate, &models).await }, |_| Ok(())
        ).await.map(|(candidate, _)| candidate)
    }
    async fn update_conditional(
        &self,
        patch: &RuntimeConfigPatch,
        validator: &dyn nuono_app::settings::RuntimeSettingsValidator,
        precondition: &dyn nuono_app::settings::RuntimeSettingsPrecondition,
    ) -> Result<Value> {
        self.update_runtime_settings_snapshot_checked(
            patch, None,
            |candidate, models| async move { validator.validate_with_models(candidate, &models).await },
            |current| precondition.check_snapshot(current),
        )
        .await.map(|(candidate, _)| candidate)
    }
    async fn change_model(
        &self,
        change: ModelChange<'_>,
        patch: &RuntimeConfigPatch,
        validator: &dyn nuono_app::settings::RuntimeSettingsValidator,
    ) -> Result<(Value, Option<AiModelRecord>)> {
        self.update_runtime_settings_snapshot_checked(
            patch,
            Some(change),
            |candidate, models| async move {
                let effective = validator.validate(candidate).await?;
                validate_references(&effective, &models, true)
                    .map_err(super::ai_models::reference_conflict)?;
                Ok(effective)
            },
            |_| Ok(()),
        )
        .await
    }
}
