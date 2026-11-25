//! 提交已筛选候选的状态、安装扩展点，并处理激活失败的补偿。
use super::loading::{PluginLoadPlan, PreparedPlugin, collect_declaration_sync};
use super::*;

impl PluginHost {
    /// 只有安装阶段会修改数据库和声明注册表。DB 批量同步成功后才发布候选，
    /// 按发现顺序准备；业务已开放时立即激活，否则等待 start_business。
    /// 失败清理只作用于本批次，已有实例和配置声明保持不变。
    /// 已提交的业务迁移不随生命周期失败而撤销。
    pub(super) async fn install_load_plan(&self, plan: PluginLoadPlan) -> Result<usize> {
        let actual_versions = plan
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.declaration.name.clone(),
                    candidate.declaration.version.clone(),
                )
            })
            .collect::<Vec<_>>();
        nuono_storage::portable::verify_target_plugin_code(&self.paths.data, &actual_versions)?;
        let actual_storage = plan
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.declaration.name.as_str(),
                    &candidate.declaration.storage,
                )
            })
            .collect::<Vec<_>>();
        nuono_storage::portable::verify_target_plugin_storage(
            &self.paths.data,
            self.db.orm(),
            &actual_storage,
        )
        .await?;
        crate::telemetry::register_console_plugin_names(
            plan.candidates
                .iter()
                .map(|candidate| candidate.declaration.name.as_str())
                .chain(plan.rejected.iter().map(|rejected| rejected.name.as_str())),
        );
        // 目录记录“编入了哪些插件”，不是激活成功的注册表；失败项仍需可见。
        self.remember_install_plan(&plan).await;
        // 在进入 on_load 或清理回调前先从数据库加载慢调用阈值等宿主诊断策略；
        // 该步骤在获取 plugins 集合写锁之前完成，避免阻塞正在读取插件身份的管理端接口。
        for name in plan
            .candidates
            .iter()
            .map(|candidate| candidate.declaration.name.as_str())
            .chain(plan.rejected.iter().map(|rejected| rejected.name.as_str()))
        {
            self.load_plugin_slow_threshold(name).await?;
            self.load_plugin_trigger_policy(name).await?;
        }
        let accepted_names: std::collections::HashSet<&str> = plan
            .candidates
            .iter()
            .map(|candidate| candidate.declaration.name.as_str())
            .collect();
        for rejected in plan.rejected {
            warn!(plugin = %rejected.name, error = %rejected.reason, "插件声明校验失败，已跳过");
            // 同名无效候选不能禁用本批最终选中的有效实例。
            if !accepted_names.contains(rejected.name.as_str()) {
                self.converge_invalid_candidate_disabled(&rejected.name)
                    .await?;
            }
        }
        if plan.candidates.is_empty() {
            nuono_storage::portable::finish_target_plugin_code(&self.paths.data)?;
            return Ok(0);
        }
        let records: Vec<_> = plan
            .candidates
            .iter()
            .map(|candidate| {
                let declaration = &candidate.declaration;
                (
                    declaration.name.clone(),
                    declaration.version.clone(),
                    declaration.config_schema.clone(),
                    declaration.service_config.enabled,
                    declaration.plugin_role,
                )
            })
            .collect();
        // 任一同步失败整体回滚；此时新候选尚未发布 schema、配置或实例。
        let records: Vec<_> = records
            .iter()
            .map(|(name, version, schema, enabled, role)| {
                (name.as_str(), version.as_str(), schema, *enabled, *role)
            })
            .collect();
        self.db.sync_plugins_batch_with_roles(&records).await?;
        for candidate in &plan.candidates {
            let declaration = &candidate.declaration;
            self.db
                .register_plugin_storage(
                    &declaration.name,
                    &declaration.version,
                    &declaration.storage,
                )
                .await?;
        }

        nuono_storage::portable::finish_target_plugin_code(&self.paths.data)?;

        let mut installed: Vec<HostedPlugin> = Vec::new();
        let mut fatal = Vec::new();
        for PreparedPlugin {
            mut plugin,
            mut declaration,
            secret_fields,
        } in plan.candidates
        {
            let plugin_started = std::time::Instant::now();
            let name = declaration.name.clone();
            let name = name.as_str();
            let _operation = self.plugin_operation(name).await;
            let epoch = InvocationEpoch::preparing();
            let mut activation_guard = ActivationGuard::new(epoch.clone());
            self.secret_schemas
                .write()
                .await
                .insert(name.to_owned(), secret_fields);
            let mut enabled = match self.db.is_plugin_enabled(name).await {
                Ok(enabled) => enabled,
                Err(error) => {
                    let cleanup = self.cleanup_failed_activation(name, None).await;
                    fatal.push(activation_failed_error(name, error.to_string(), cleanup));
                    continue;
                }
            };
            if enabled {
                let missing = self
                    .missing_required_dependencies(&declaration.required_dependencies)
                    .await;
                if !missing.is_empty() {
                    warn!(
                        plugin = %name,
                        missing = ?missing,
                        "插件缺少已启用的强依赖插件，已按禁用状态保留"
                    );
                    if let Err(error) = self.db.set_plugin_enabled(name, false).await {
                        let cleanup = self.cleanup_failed_activation(name, None).await;
                        fatal.push(activation_failed_error(name, error.to_string(), cleanup));
                        continue;
                    }
                    enabled = false;
                }
            }
            self.extensions
                .prepare(name, declaration.extensions.clone(), enabled);
            let config = match self.prepare_runtime_config(&declaration).await {
                Ok(config) => config,
                Err(error) => {
                    warn!(plugin = %name, error = %error, "插件配置无效，未执行初始化");
                    if let Err(cleanup) = self.cleanup_failed_activation(name, None).await {
                        fatal.push(cleanup);
                    }
                    continue;
                }
            };
            if enabled {
                let ctx = self
                    .plugin_context(name, config, PluginActivationReason::Startup)
                    .await;
                activation_guard.attach(ctx.tasks().clone());
                let result = self
                    .activate_plugin(&mut plugin, &mut declaration, &ctx, &epoch)
                    .await;
                if let Err(error) = result {
                    epoch.cancel();
                    let cleanup = self
                        .cleanup_failed_activation(name, Some(&mut plugin))
                        .await;
                    match cleanup {
                        Ok(()) => {
                            warn!(plugin = %name, error = %error, "插件激活失败，已清理并跳过")
                        }
                        Err(cleanup) => fatal.push(activation_failed_error(
                            name,
                            error.to_string(),
                            Err(cleanup),
                        )),
                    }
                    continue;
                }
            }
            let mut service_config = self
                .service_config_from_file(name, declaration.service_config.clone())
                .await;
            service_config.enabled = enabled;
            self.register_plugin_config(service_config).await;
            if enabled {
                self.remember_plugin_declaration(&declaration).await;
                self.publish_or_defer_extensions(&declaration).await?;
            }
            let duration = plugin_started.elapsed();
            let status = if !enabled {
                "未启用"
            } else if self.business_is_running() {
                "已加载，运行已启动"
            } else {
                "已加载，等待运行"
            };
            if duration >= std::time::Duration::from_secs(1) {
                info!(target: "nuono::plugins", plugin = %name, duration_ms = duration.as_millis() as u64,
                    "{status}（耗时 {:.2} 秒）", duration.as_secs_f64());
            } else {
                info!(target: "nuono::plugins", plugin = %name, duration_ms = duration.as_millis() as u64, "{status}");
            }
            if !enabled {
                epoch.cancel();
            }
            let hosted = HostedPlugin::with_epoch(plugin, epoch);
            let mut plugins = self.plugins.write().await;
            plugins.push(hosted.clone());
            installed.push(hosted);
            if self.business_is_running() {
                self.commit_activation(activation_guard)?;
            } else {
                activation_guard.prepared();
            }
        }
        if !fatal.is_empty() {
            // 本批构造失败：只收束这次加入的实例，绝不删除或关闭原有插件。
            for hosted in &installed {
                let _operation = self.plugin_operation(hosted.name()).await;
                hosted.retire();
                hosted.quiesce().await;
                let mut plugin = hosted.plugin.write().await;
                if let Err(error) = self
                    .cleanup_failed_activation(hosted.name(), Some(&mut plugin))
                    .await
                {
                    fatal.push(error);
                }
                self.remove_hosted_plugin(hosted).await;
            }
            return Err(crate::error::NuonoError::PluginInit(
                fatal
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        Ok(installed.len())
    }

    /// 启动、手动启用与实例重建共用准备步骤和 panic 边界。
    /// 准备阶段只执行 on_load；业务已经开放时同时执行 on_enable。
    /// commands/config/web_routes 使用预检快照；triggers/tools 仍在 on_load 后读取。
    async fn activate_plugin(
        &self,
        plugin: &mut LoadedPlugin,
        declaration: &mut PluginDeclaration,
        ctx: &PluginContext,
        epoch: &InvocationEpoch,
    ) -> anyhow::Result<()> {
        let metrics = self.diagnostic_metrics(&declaration.name);
        tokio::time::timeout(
            ACTIVATION_TIMEOUT,
            Self::call_with_catch(async {
                self.db
                    .register_plugin_storage(
                        &declaration.name,
                        &declaration.version,
                        &declaration.storage,
                    )
                    .await?;
                metrics.observe("on_load", plugin.on_load(ctx)).await?;
                // 两段式 PluginFactory 在 on_load/mount(ctx) 后产出带运行期状态的扩展、命令与 Web 路由；
                // 此处按统一校验路径刷新快照并更新宿主扩展注册表。
                let post_extensions = plugin.extensions();
                post_extensions.validate()?;
                if post_extensions.service_entitlement.is_some()
                    || post_extensions.capabilities.is_some()
                {
                    declaration.extensions = post_extensions.clone();
                    self.extensions
                        .prepare(&declaration.name, post_extensions, true);
                }
                if declaration.commands.is_empty() {
                    let post_commands = plugin.commands();
                    if !post_commands.is_empty() {
                        declaration.commands = post_commands;
                    }
                }
                let post_routes = plugin.web_routes();
                if !post_routes.is_empty() {
                    declaration.web_routes = super::loading::validated_plugin_web_routes(
                        &declaration.name,
                        post_routes,
                    )?;
                }
                if self.business_is_running() {
                    metrics.observe("on_enable", plugin.on_enable()).await?;
                }
                self.register_scoped_plugin_triggers_in_epoch(
                    plugin,
                    &declaration.commands,
                    Some(epoch.clone()),
                )
                .await?;
                self.replace_plugin_web_declarations(
                    &declaration.name,
                    &declaration.web_routes,
                    &declaration.web_pages,
                )
                .await;
                self.register_scoped_plugin_tools_in_epoch(plugin, Some(epoch.clone()))
                    .await?;
                Ok(())
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("插件激活超过 30 秒"))?
    }

    /// 把无效候选的 DB 启用状态收敛为 `enabled=false`（fail-closed 方向）。
    ///
    /// 收敛失败必须上报：无效候选若在 DB 中仍 `enabled=true`，会造成「宿主未加载、DB 却启用」
    /// 的不一致，init 需要据此中止。写失败返回 `Err`，调用方（init / load 阶段）据此 fatal。
    pub(super) async fn converge_invalid_candidate_disabled(
        &self,
        name: &str,
    ) -> crate::error::Result<()> {
        // 读取当前状态只为日志与避免多余写；读失败不再静默当作未启用，而是保守地仍尝试收敛。
        let already_disabled = matches!(self.db.is_plugin_enabled(name).await, Ok(false));
        if !already_disabled {
            self.db.set_plugin_enabled(name, false).await?;
        }
        Ok(())
    }

    /// 启用一个插件：准备成功后保存启用意图；业务开放时才执行 on_enable 并发布调用入口。
    /// 整个准备及激活过程（含同步的 `triggers()`/`commands()`/
    /// `tools()` 与注册）被包进 panic 边界，任何 error/panic 都走统一清理；提交
    /// `enabled=true` 失败同样补偿。保证不留下 API secret 授权或任何事件可调用实例。插件的
    /// 外部副作用不可事务回滚，已在文档 / 错误语义中如实说明。
    pub(super) async fn enable_plugin(&self, name: &str) -> Result<()> {
        self.enable_plugin_with_declaration(name, None, PluginActivationReason::Enable)
            .await
    }

    /// 重建复用预检快照；再次手动启用仍按新的激活周期收集一份声明。
    pub(super) async fn enable_plugin_with_declaration(
        &self,
        name: &str,
        prepared: Option<PluginDeclaration>,
        reason: PluginActivationReason,
    ) -> Result<()> {
        let Some(hosted) = self.hosted_plugin(name).await else {
            return Err(crate::error::NuonoError::PluginNotFound(name.to_string()));
        };
        if let Some(declaration) = &prepared {
            self.ensure_required_dependencies_enabled(name, &declaration.required_dependencies)
                .await?;
        } else {
            self.ensure_required_dependencies_enabled_for(name).await?;
        }
        hosted.retire();
        self.set_plugin_registry_enabled(name, false).await;
        hosted.quiesce().await;
        let mut plugin = hosted.plugin.write().await;
        let epoch = hosted.renew();
        let mut activation_guard = ActivationGuard::new(epoch.clone());
        self.trigger_registry.unregister_by_plugin(name);
        self.unregister_plugin_tools(name);
        self.unregister_plugin_web_routes(name).await;

        // 1. 收集声明（含 config_fields/service_config/version），
        //    整体包进 panic 边界；secret_fields 由 registry 持有（已验证快照），context 复用。
        let declaration: anyhow::Result<PluginDeclaration> = match prepared {
            Some(declaration) => Ok(declaration),
            None => Self::call_with_catch(async { Ok(collect_declaration_sync(&plugin)?) }).await,
        };
        let mut declaration = match declaration {
            Ok(d) => d,
            Err(e) => {
                epoch.cancel();
                let cleanup = self
                    .cleanup_failed_activation(name, Some(&mut plugin))
                    .await;
                self.remove_hosted_plugin(&hosted).await;
                return Err(activation_failed_error(name, e.to_string(), cleanup));
            }
        };

        self.extensions
            .prepare(name, declaration.extensions.clone(), true);
        if self.extensions.holds_service_policy(name) {
            self.db.set_plugin_enabled(name, true).await?;
        }

        // 2. 准备运行时配置与插件上下文（secret store 复用 registry 快照）。
        let plugin_cfg = match self.prepare_runtime_config(&declaration).await {
            Ok(config) => config,
            Err(error) => {
                let cleanup = self
                    .cleanup_failed_activation(name, Some(&mut plugin))
                    .await;
                self.remove_hosted_plugin(&hosted).await;
                return Err(activation_failed_error(name, error.to_string(), cleanup));
            }
        };
        let ctx = self
            .plugin_context(&declaration.name, plugin_cfg, reason)
            .await;
        activation_guard.attach(ctx.tasks().clone());

        // 3. 生命周期 + 注册，包进同一 panic 边界（on_load/on_enable 异步，triggers/commands/
        //    tools 同步，任何一步 panic 都走同一 fail-closed 清理）。
        let activation_result = self
            .activate_plugin(&mut plugin, &mut declaration, &ctx, &epoch)
            .await;

        if let Err(e) = activation_result {
            epoch.cancel();
            let cleanup = self
                .cleanup_failed_activation(name, Some(&mut plugin))
                .await;
            self.remove_hosted_plugin(&hosted).await;
            return Err(activation_failed_error(name, e.to_string(), cleanup));
        }

        // 4. 提交 enabled=true；失败同样补偿（此时已注册的 triggers/tools 也必须撤销）。
        if let Err(e) = self.db.set_plugin_enabled(name, true).await {
            epoch.cancel();
            let cleanup = self
                .cleanup_failed_activation(name, Some(&mut plugin))
                .await;
            self.remove_hosted_plugin(&hosted).await;
            return Err(activation_failed_error(
                name,
                format!("提交 enabled=true 失败: {e}"),
                cleanup,
            ));
        }

        // 本次激活周期可能重新声明监听范围；与随后发布的服务策略使用同一份快照。
        // 在开启 registry 前更新，避免新实例沿用上一周期的范围或缺失能力。
        self.remember_plugin_declaration(&declaration).await;
        drop(plugin);
        let mut service_config = self
            .service_config_from_file(name, declaration.service_config.clone())
            .await;
        service_config.enabled = true;
        self.register_plugin_config(service_config).await;
        self.set_plugin_registry_enabled(name, true).await;
        hosted.publish(&epoch);
        self.publish_or_defer_extensions(&declaration).await?;
        if self.business_is_running() {
            self.commit_activation(activation_guard)?;
            info!(plugin = %name, "插件已启用，运行已启动");
        } else {
            activation_guard.prepared();
            info!(plugin = %name, "插件已启用，等待框架启动业务");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/listener_lifecycle_tests.rs"]
mod listener_lifecycle_tests;
