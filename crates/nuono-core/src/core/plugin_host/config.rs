//! 插件宿主配置职责。
use super::*;

impl PluginHost {
    /// 校验某 canonical key 当前是否在任何已加载插件的 secret schema 中。
    ///
    /// 供 [`Self::set_plugin_secret`] / [`Self::delete_plugin_secret`] 在 `secret_schemas`
    /// 读锁内调用（见下述原子方法的锁顺序说明）。
    pub(super) fn secret_canonical_key_authorized(
        &self,
        schemas: &std::collections::HashMap<String, Vec<crate::plugin::PluginSecretField>>,
        canonical_key: &str,
    ) -> bool {
        schemas.iter().any(|(plugin_name, fields)| {
            fields.iter().any(|field| {
                crate::plugin::plugin_secret_canonical_key(plugin_name, &field.key)
                    .ok()
                    .as_deref()
                    == Some(canonical_key)
            })
        })
    }

    /// 原子地写入一个插件 secret。
    ///
    /// 授权检查与 DB 写入在 `secret_schemas` **读锁内**完成，保证重载/卸载不能把 schema
    /// 变更插入「校验 → 写入」之间。
    ///
    /// 锁顺序论证（避免反向死锁）：本方法只拿 `secret_schemas` 读锁，从不拿 `plugins` 锁；
    /// 而重载/卸载/关闭一律按 `plugins → secret_schemas` 顺序获取。因此不存在持有
    /// `secret_schemas` 写锁再等待 `plugins` 的路径，也就不会有与重载构成的反向闭环。
    pub async fn set_plugin_secret(
        &self,
        canonical_key: &str,
        value: &str,
    ) -> crate::error::Result<()> {
        let schemas = self.secret_schemas.read().await;
        if !self.secret_canonical_key_authorized(&schemas, canonical_key) {
            return Err(crate::error::NuonoError::PermissionDenied(
                "不支持配置该密钥项".into(),
            ));
        }
        self.db.set_secret(canonical_key, value).await
    }

    /// 原子地删除一个插件 secret（授权与 DB 删除在 `secret_schemas` 读锁内完成，见
    /// [`Self::set_plugin_secret`] 的锁顺序论证）。
    pub async fn delete_plugin_secret(&self, canonical_key: &str) -> crate::error::Result<()> {
        let schemas = self.secret_schemas.read().await;
        if !self.secret_canonical_key_authorized(&schemas, canonical_key) {
            return Err(crate::error::NuonoError::PermissionDenied(
                "不支持配置该密钥项".into(),
            ));
        }
        self.db.delete_secret(canonical_key).await
    }

    /// 列出当前所有已加载插件声明的 secret 字段 schema（含 canonical key 与展示信息）。
    pub async fn plugin_secret_schemas(&self) -> Vec<crate::plugin::PluginSecretSchema> {
        let guard = self.secret_schemas.read().await;
        let mut out: Vec<crate::plugin::PluginSecretSchema> = Vec::new();
        for (plugin_name, fields) in guard.iter() {
            for field in fields {
                if let Ok(key) = crate::plugin::plugin_secret_canonical_key(plugin_name, &field.key)
                {
                    out.push(crate::plugin::PluginSecretSchema {
                        plugin_name: plugin_name.clone(),
                        key,
                        title: field.title.clone(),
                        description: field.description.clone(),
                        required: field.required,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// 所有已声明插件 secret 的 canonical key 列表（供 API 校验允许写入的键）。
    pub async fn plugin_secret_canonical_keys(&self) -> Vec<String> {
        self.plugin_secret_schemas()
            .await
            .into_iter()
            .map(|s| s.key)
            .collect()
    }

    pub async fn get_plugin_config_fields(
        &self,
        name: &str,
    ) -> Vec<crate::core::PluginConfigField> {
        self.config_declarations
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) async fn prepare_runtime_config(
        &self,
        declaration: &PluginDeclaration,
    ) -> Result<Value> {
        let defaults = declaration.config_default.clone();
        let schema = crate::config::plugin_fields_to_schema(&declaration.config_fields)?;
        let config_manager = self.config_manager.read().await.as_ref().cloned();
        let Some(mgr) = config_manager else {
            self.config_declarations
                .write()
                .await
                .insert(declaration.name.clone(), declaration.config_fields.clone());
            return Ok(defaults);
        };
        let previous_resources = mgr.resource_definitions(&declaration.name).await;
        mgr.register_resources(&declaration.name, declaration.config_resources.clone())
            .await?;
        mgr.register_import_secrets(
            &declaration.name,
            self.secret_schemas
                .read()
                .await
                .get(&declaration.name)
                .cloned()
                .unwrap_or_default(),
        )
        .await;
        if let Err(error) = mgr
            .register_with_migration(
                &declaration.name,
                schema,
                Some(&declaration.service_config),
                declaration.config_version,
                declaration.config_migration.as_deref(),
            )
            .await
        {
            mgr.register_resources(&declaration.name, previous_resources)
                .await?;
            return Err(error);
        }
        let config = mgr
            .merge_runtime_config(&declaration.name, defaults)
            .await?;
        // 配置准备成功后再发布字段，避免失败候选覆盖旧实例的声明。
        self.config_declarations
            .write()
            .await
            .insert(declaration.name.clone(), declaration.config_fields.clone());
        if let Some(revision) = mgr.status(&declaration.name).await.revision {
            self.config_revisions
                .write()
                .await
                .insert(declaration.name.clone(), revision);
        }
        Ok(config)
    }

    /// 由 WebUI 保存和文件轮询共用；不重新执行 on_load，不重启插件任务。
    pub async fn refresh_plugin_config(&self, name: &str) -> Result<()> {
        // 将配置准备与等待锁的过程一并纳入超时控制；
        // 在超时 Future 外层记录已尝试的配置版本，避免超时取消抹掉可见的配置错误信息。
        let attempted = std::sync::Mutex::new(None::<(String, bool)>);
        match tokio::time::timeout(
            LIFECYCLE_TIMEOUT,
            self.refresh_plugin_config_inner(name, &attempted),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let message = "插件配置刷新（包括等待）超过 5 秒".to_string();
                let attempt = attempted.into_inner().unwrap_or_else(|e| e.into_inner());
                // 超时 Future 释放操作锁后，可能有另一轮生命周期变更或配置刷新抢先完成；
                // 此处仅在能非阻塞拿到操作锁时记录超时错误，绝不覆盖后来者已经更新的结果。
                if let Some(_operation) = self.try_plugin_operation(name)
                    && let Some((revision, entered)) = attempt
                {
                    let manager = self.config_manager.read().await.as_ref().cloned();
                    if let Some(manager) = manager
                        && manager.status(name).await.revision.as_ref() == Some(&revision)
                        && self.config_revisions.read().await.get(name) != Some(&revision)
                    {
                        if entered {
                            self.config_revisions
                                .write()
                                .await
                                .insert(name.into(), revision);
                        }
                        manager.set_runtime_error(name, Some(message.clone())).await;
                    }
                }
                Err(crate::error::NuonoError::PluginInit(message))
            }
        }
    }

    async fn refresh_plugin_config_inner(
        &self,
        name: &str,
        attempted: &std::sync::Mutex<Option<(String, bool)>>,
    ) -> Result<()> {
        let _admission = self.admit_lifecycle().await?;
        let Some(mgr) = self.config_manager.read().await.as_ref().cloned() else {
            return Ok(());
        };
        mgr.refresh(name).await?;
        let status = mgr.status(name).await;
        let Some(revision) = status.revision else {
            return Ok(());
        };
        *attempted.lock().unwrap_or_else(|e| e.into_inner()) = Some((revision.clone(), false));
        if self.config_revisions.read().await.get(name) == Some(&revision) {
            return match status.runtime_error {
                Some(e) => Err(crate::error::NuonoError::PluginInit(e)),
                None => Ok(()),
            };
        }
        let _operation = self.plugin_operation(name).await;
        if self.config_revisions.read().await.get(name) == Some(&revision) {
            return Ok(());
        }
        let Some(hosted) = self.hosted_plugin(name).await else {
            return Ok(());
        };
        let mut plugin = hosted.plugin.write().await;
        let enabled = self
            .plugin_registry
            .read()
            .await
            .get(name)
            .is_some_and(|c| c.enabled);
        let config = mgr.get_runtime_config(name).await?;
        let callback = if enabled {
            *attempted.lock().unwrap_or_else(|e| e.into_inner()) = Some((revision.clone(), true));
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.diagnostic_metrics(name).observe(
                    "on_config_changed",
                    Self::call_with_catch(plugin.on_config_changed(&config)),
                ),
            )
            .await
            {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("配置变更回调超过 5 秒".into()),
            }
        } else {
            Ok(())
        };
        // 服务能力在加载时声明一次；刷新仅叠加文件，不重新执行插件声明方法。
        let service = self
            .plugin_registry
            .read()
            .await
            .get(name)
            .map(|cfg| cfg.as_ref().clone())
            .ok_or_else(|| crate::error::NuonoError::PluginNotFound(name.to_string()))?;
        let mut service = self.service_config_from_file(name, service).await;
        service.enabled = enabled;
        self.register_plugin_config(service).await;
        self.config_revisions
            .write()
            .await
            .insert(name.into(), revision);
        mgr.set_runtime_error(name, callback.as_ref().err().cloned())
            .await;
        callback.map_err(|e| {
            crate::error::NuonoError::PluginInit(format!("文件已保存，插件配置刷新失败: {e}"))
        })
    }

    pub fn start_config_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let host = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut reported = HashMap::<String, String>::new();
            loop {
                interval.tick().await;
                let Some(host) = host.upgrade() else {
                    break;
                };
                let Some(mgr) = host.config_manager.read().await.as_ref().cloned() else {
                    continue;
                };
                for name in mgr.list_configs().await {
                    if let Err(error) = host.refresh_plugin_config(&name).await {
                        let message = error.to_string();
                        if reported.get(&name) != Some(&message) {
                            warn!(plugin=%name,error=%message,"插件配置刷新失败，保留有效配置；请检查配置状态");
                            reported.insert(name, message);
                        }
                    } else {
                        reported.remove(&name);
                    }
                }
            }
        })
    }

    pub(super) async fn service_config_from_file(&self, name: &str, defaults: Plugins) -> Plugins {
        let mut cfg = defaults;
        cfg.name = name.to_string();

        let values = {
            let mgr_guard = self.config_manager.read().await;
            if let Some(mgr) = mgr_guard.as_ref() {
                mgr.get_values(name).await.unwrap_or_default()
            } else {
                HashMap::new()
            }
        };

        Self::apply_service_values(cfg, values)
    }

    pub(super) fn apply_service_values(
        mut cfg: Plugins,
        values: HashMap<String, ConfigValue>,
    ) -> Plugins {
        for (key, value) in values {
            match (key.as_str(), value) {
                ("prefix", ConfigValue::ListStr(v)) => cfg.prefix = v,
                ("force_prefix", ConfigValue::ListStr(v)) => cfg.force_prefix = v,
                ("alias", ConfigValue::ListStr(v)) => cfg.alias = v,
                ("black_list", ConfigValue::ListStr(v)) => cfg.black_list = v,
                ("white_list", ConfigValue::ListStr(v)) => cfg.white_list = v,
                ("min_permission", ConfigValue::String(v)) => {
                    if let Some(level) = crate::core::permission::PermissionLevel::parse(&v) {
                        cfg.min_permission = level;
                    }
                }
                ("area", ConfigValue::String(v)) => cfg.area = PluginArea::parse_lossy(&v),
                ("disable_force_prefix", ConfigValue::Bool(v)) => cfg.disable_force_prefix = v,
                ("allow_empty_prefix", ConfigValue::Bool(v)) => cfg.allow_empty_prefix = v,
                ("auto_listen_enabled", ConfigValue::Bool(v))
                    if cfg.auto_listen_enabled.is_some() =>
                {
                    cfg.auto_listen_enabled = Some(v)
                }
                _ => {}
            }
        }

        cfg
    }

    pub(super) fn scoped_service_config(config: Arc<Plugins>) -> Arc<Plugins> {
        let Some(scoped) = nuono_plugin::config::runtime_configuration(&config.name) else {
            return config;
        };
        let values =
            serde_json::from_value::<HashMap<String, ConfigValue>>(scoped.values.as_ref().clone())
                .unwrap_or_default();
        let mut config = Self::apply_service_values(config.as_ref().clone(), values);
        config.enabled &= scoped.enabled;
        Arc::new(config)
    }

    pub(crate) async fn event_configuration_scope(
        &self,
        event: Option<&crate::domain::DomainEvent>,
    ) -> Result<HashMap<String, nuono_plugin::config::RuntimePluginConfiguration>> {
        let manager = self
            .config_manager
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| crate::error::NuonoError::ConfigLoad("插件配置管理器不可用".into()))?;
        // 数据库配置解析包含多层嵌套异步查询状态；使用 Box::pin 放到堆上，避免撑大事件或交互调用链的 Future 栈空间。
        Box::pin(crate::service::plugin_configuration::event_configurations(
            &self.db, &manager, event,
        ))
        .await
    }

    pub(super) async fn disabled_commands_from_file(&self, name: &str) -> Vec<String> {
        let values = {
            let mgr_guard = self.config_manager.read().await;
            if let Some(mgr) = mgr_guard.as_ref() {
                mgr.get_values(name).await.unwrap_or_default()
            } else {
                HashMap::new()
            }
        };

        match values.get("disabled_commands") {
            Some(ConfigValue::ListStr(v)) => v.clone(),
            _ => Vec::new(),
        }
    }

    pub async fn set_config_manager(&self, manager: Arc<crate::config::PluginConfigManager>) {
        *self.config_manager.write().await = Some(manager);
    }
}

// 单元测试辅助入口：在独立宿主或处理器测试中模拟 EventKernel 的单次配置快照。
#[cfg(test)]
impl super::PluginHost {
    pub(crate) async fn test_group_snapshot<F: std::future::Future>(
        &self,
        native: Option<&str>,
        future: F,
    ) -> F::Output {
        let manager = self.config_manager.read().await.as_ref().cloned().unwrap();
        let target = if let Some(native) = native {
            Some(nuono_storage::plugin_overrides::OverrideTarget::Group(
                self.db
                    .ensure_group(&nuono_storage::groups::GroupIdentity::onebot(native), "")
                    .await
                    .unwrap()
                    .id,
            ))
        } else {
            None
        };
        let mut values = std::collections::HashMap::new();
        for plugin in manager.list_configs().await {
            let value =
                crate::service::plugin_configuration::resolve(&self.db, &manager, &plugin, target)
                    .await
                    .unwrap_or_else(|_| nuono_plugin::config::RuntimePluginConfiguration {
                        enabled: false,
                        values: std::sync::Arc::new(serde_json::json!({})),
                    });
            values.insert(plugin, value);
        }
        nuono_plugin::config::with_runtime_configurations(values, future).await
    }
    pub(crate) async fn test_group_patch(
        &self,
        native: &str,
        plugin: &str,
        enabled: Option<bool>,
        service: serde_json::Map<String, serde_json::Value>,
    ) -> crate::error::Result<()> {
        use nuono_storage::plugin_overrides::{OverridePatch, OverrideTarget};
        let actor = match self.db.find_user_by_username("test-owner").await? {
            Some(user) => user.id,
            None => {
                self.db
                    .create_user(crate::db::NewUser {
                        username: "test-owner".into(),
                        email: None,
                        password: "Test-owner-password-123!".into(),
                        role: "owner".into(),
                        status: "active".into(),
                        email_verified: false,
                    })
                    .await?
                    .id
            }
        };
        let target = OverrideTarget::Group(
            self.db
                .ensure_group(&nuono_storage::groups::GroupIdentity::onebot(native), "")
                .await?
                .id,
        );
        let policy = self.db.plugin_policy(plugin).await?;
        if !policy.allow_group_overrides {
            self.db
                .save_plugin_policy(
                    actor,
                    plugin,
                    nuono_storage::plugin_global_settings::PluginPolicyPatch {
                        expected_revision: policy.revision,
                        allow_group_overrides: Some(true),
                        ..Default::default()
                    },
                )
                .await?;
        }
        let row = self.db.plugin_override(plugin, target).await?;
        self.db
            .save_plugin_override(
                &nuono_app::configuration::ConfigurationActor {
                    user_id: actor,
                    mode: nuono_app::configuration::ConfigurationMode::Administrator,
                },
                plugin,
                target,
                OverridePatch {
                    expected_id: row.as_ref().map(|v| v.id),
                    expected_revision: row.as_ref().map_or(0, |v| v.revision),
                    config_version: policy.config_version,
                    set_enabled: enabled,
                    set_service: service,
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }
}
