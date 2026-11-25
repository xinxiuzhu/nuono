//! 插件宿主注册表的职责。
use super::*;

enum BusinessStart {
    Immediate,
    Deferred,
}

impl PluginHost {
    pub async fn new(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        paths: crate::core::paths::Paths,
        wait_manager: Arc<WaitManager>,
        framework_control: FrameworkControl,
        config_manager: Arc<crate::config::PluginConfigManager>,
    ) -> Result<Arc<Self>> {
        let locales = nuono_app::localization::LocaleService::new(Arc::new(db.clone()));
        Self::new_with_locales(
            db,
            event_bus,
            permission_manager,
            paths,
            wait_manager,
            framework_control,
            config_manager,
            locales,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_locales(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        paths: crate::core::paths::Paths,
        wait_manager: Arc<WaitManager>,
        framework_control: FrameworkControl,
        config_manager: Arc<crate::config::PluginConfigManager>,
        locales: nuono_app::localization::LocaleService,
    ) -> Result<Arc<Self>> {
        Self::new_with_locales_and_image_output(
            db,
            event_bus,
            permission_manager,
            paths,
            wait_manager,
            framework_control,
            config_manager,
            locales,
            nuono_plugin::image_output::PluginImageOutput::default(),
        )
        .await
    }

    /// 在插件初始化并开始渲染图片卡片之前，先挂载实时图片输出策略。
    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_locales_and_image_output(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        paths: crate::core::paths::Paths,
        wait_manager: Arc<WaitManager>,
        framework_control: FrameworkControl,
        config_manager: Arc<crate::config::PluginConfigManager>,
        locales: nuono_app::localization::LocaleService,
        image_output: nuono_plugin::image_output::PluginImageOutput,
    ) -> Result<Arc<Self>> {
        Self::construct_with_runtime(
            db,
            event_bus,
            permission_manager,
            paths,
            wait_manager,
            framework_control,
            config_manager,
            locales,
            image_output,
            BusinessStart::Immediate,
        )
        .await
    }

    /// 准备插件及其声明，等待管理入口开放后显式启动业务。
    #[allow(clippy::too_many_arguments)]
    pub async fn new_prepared_with_locales_and_image_output(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        paths: crate::core::paths::Paths,
        wait_manager: Arc<WaitManager>,
        framework_control: FrameworkControl,
        config_manager: Arc<crate::config::PluginConfigManager>,
        locales: nuono_app::localization::LocaleService,
        image_output: nuono_plugin::image_output::PluginImageOutput,
    ) -> Result<Arc<Self>> {
        Self::construct_with_runtime(
            db,
            event_bus,
            permission_manager,
            paths,
            wait_manager,
            framework_control,
            config_manager,
            locales,
            image_output,
            BusinessStart::Deferred,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn construct_with_runtime(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        paths: crate::core::paths::Paths,
        wait_manager: Arc<WaitManager>,
        framework_control: FrameworkControl,
        config_manager: Arc<crate::config::PluginConfigManager>,
        locales: nuono_app::localization::LocaleService,
        image_output: nuono_plugin::image_output::PluginImageOutput,
        start: BusinessStart,
    ) -> Result<Arc<Self>> {
        let tool_registry = Arc::new(ToolRegistry::new());
        BuiltinToolRegistry::register_read_only_into(&tool_registry)?;
        let interactions = interactions::InteractionRegistry::with_wait_manager(&wait_manager);

        let host = Arc::new(Self {
            platform_role_authority: Default::default(),
            platform_role_verifiers: Default::default(),
            extensions: Default::default(),
            locales,
            image_output,
            db: db.clone(),
            event_bus,
            permission_manager,
            paths,
            plugins: RwLock::new(Vec::new()),
            lifecycle_operations: Default::default(),
            lifecycle_admission: RwLock::new(()),
            closing: std::sync::atomic::AtomicBool::new(false),
            business_running: std::sync::atomic::AtomicBool::new(matches!(
                start,
                BusinessStart::Immediate
            )),
            business_publication: Default::default(),
            pending_activations: Default::default(),
            registration_writer: Default::default(),
            installed_plugins: RwLock::new(HashMap::new()),
            inventory_reconciliation_started: std::sync::atomic::AtomicBool::new(false),
            trigger_registry: Arc::new(TriggerRegistry::new()),
            plugin_registry: RwLock::new(HashMap::new()),
            cooldown_tracker: Arc::new(CooldownTracker::new()),
            command_stats: Arc::new(CommandStatsManager::new()),
            wait_manager,
            tool_registry,
            framework_control,
            action_router: Arc::new(crate::core::RuntimeActionRouter::with_audit(Arc::new(
                db.clone(),
            ))),
            config_manager: RwLock::new(Some(config_manager)),
            task_scopes: RwLock::new(HashMap::new()),
            agent_runtime: Default::default(),
            pending_wait_callbacks: RwLock::new(HashMap::new()),
            secret_schemas: RwLock::new(HashMap::new()),
            web_routes: RwLock::new(HashMap::new()),
            web_pages: RwLock::new(HashMap::new()),
            web_page_generation: Default::default(),
            config_declarations: RwLock::new(HashMap::new()),
            config_revisions: RwLock::new(HashMap::new()),
            diagnostics: diagnostics::DiagnosticsRegistry::default(),
            diagnostic_policy_writer: tokio::sync::Mutex::new(()),
            trigger_control: Arc::new(trigger_control::TriggerControl::default()),
            trigger_policy_writer: tokio::sync::Mutex::new(()),
            interactions,
            service_registry: crate::plugin::PluginServiceRegistry::new(),
        });

        host.refresh_required_service_providers().await?;
        host.init().await?;
        Ok(host)
    }

    pub async fn register_plugin_config(&self, plugin_cfg: Plugins) {
        let name = plugin_cfg.name.clone();
        if plugin_cfg.enabled {
            self.wait_manager.enable_plugin(&name);
            self.interactions.enable(&name);
        } else {
            self.wait_manager.cancel_plugin(&name);
            self.interactions.revoke(&name);
        }
        let arc = Arc::new(plugin_cfg);
        self.plugin_registry.write().await.insert(name, arc);
    }

    /// 注册插件的 secret schema，并在加载阶段校验声明合法性。
    ///
    /// 提取 [`Plugin::secret_fields`] **一次**并校验（插件名即使字段为空也须合法；字段 key
    /// 空值 / 非法字符 / 重复都会报错并拒绝加载该插件）。schema 按插件名存储，重复注册
    /// （实例重建）会整片替换，不会残留旧字段。
    #[cfg(test)]
    pub(crate) async fn register_plugin_secret_schema(
        &self,
        plugin: &LoadedPlugin,
    ) -> crate::error::Result<()> {
        let fields =
            Self::call_with_catch(async { Ok(validated_plugin_secret_fields(plugin)?) }).await?;
        self.secret_schemas
            .write()
            .await
            .insert(plugin.name().to_string(), fields);
        Ok(())
    }

    /// 原子替换插件集合与 secret schema。
    ///
    /// 同时在 `plugins` 写锁与 `secret_schemas` 写锁内完成「移除全部同名旧实例 + 清旧 schema +
    /// 加入单个新实例 + 注册其 schema」，使重载对并发 API 而言不可见中间态。返回所有被移除的
    /// 同名旧实例，由调用方在其离开集合后 best-effort 执行 `on_unload`（回调失败不再影响
    /// schema 清理）。
    ///
    /// 锁顺序：本方法按 `plugins → secret_schemas` 获取，与 init/关闭一致。
    pub(super) async fn swap_plugin_instances(
        &self,
        name: &str,
        new_plugin: LoadedPlugin,
        new_schema: Vec<crate::plugin::PluginSecretField>,
    ) -> Vec<HostedPlugin> {
        let new_plugin = HostedPlugin::from(new_plugin);
        new_plugin.retire();
        let mut plugins = self.plugins.write().await;
        let mut schemas = self.secret_schemas.write().await;
        let mut old = Vec::new();
        let mut i = 0;
        while i < plugins.len() {
            if plugins[i].name() == name {
                let retired = plugins.remove(i);
                retired.retire();
                old.push(retired);
            } else {
                i += 1;
            }
        }
        schemas.remove(name);
        schemas.insert(name.to_string(), new_schema);
        plugins.push(new_plugin);
        old
    }

    /// 在读取 HTTP 请求体之前，先查找已启用插件的匹配 Web 路由声明。
    ///
    /// 路由声明可包含 `:name` 参数段；字面段最多的最具体模式优先，并列时按声明顺序决胜。
    /// 请求体大小限制与访问权限策略均从同一次匹配结果读取，杜绝「按路由 A 鉴权却按路由 B 派发」的不一致。
    pub async fn plugin_web_route(
        &self,
        plugin_name: &str,
        method: &http::Method,
        path: &str,
    ) -> Option<crate::plugin::PluginWebRoute> {
        if !self
            .get_plugin_config(plugin_name)
            .await
            .is_some_and(|config| config.enabled)
        {
            return None;
        }
        self.web_routes
            .read()
            .await
            .get(plugin_name)
            .and_then(|items| find_route(items, method, path))
            .cloned()
    }

    pub async fn plugin_web_policy(
        &self,
        plugin_name: &str,
        method: &http::Method,
        path: &str,
    ) -> crate::plugin::PluginWebAccessPolicy {
        let routes = self.web_routes.read().await;
        routes
            .get(plugin_name)
            .and_then(|items| find_route(items, method, path))
            .map(|route| route.policy)
            .unwrap_or(crate::plugin::PluginWebAccessPolicy::ManageRuntime)
    }

    pub async fn get_plugin_config(&self, name: &str) -> Option<Arc<Plugins>> {
        self.plugin_registry
            .read()
            .await
            .get(name)
            .cloned()
            .map(Self::scoped_service_config)
    }

    pub async fn list_plugin_configs(&self) -> Vec<Arc<Plugins>> {
        self.plugin_registry
            .read()
            .await
            .values()
            .cloned()
            .map(Self::scoped_service_config)
            .collect()
    }

    pub async fn get_plugin_info(&self, name: &str) -> PluginInfoView {
        self.installed_plugin_infos()
            .await
            .remove(name)
            .unwrap_or_else(|| PluginInfoView {
                display_name: name.to_string(),
                ..PluginInfoView::default()
            })
    }

    pub async fn get_sv_list(
        &self,
        plugin_name: &str,
    ) -> Vec<(Arc<crate::core::SV>, Vec<Arc<crate::core::Trigger>>)> {
        let mut grouped: HashMap<String, Vec<Arc<crate::core::Trigger>>> = HashMap::new();
        for trigger in self.trigger_registry.get_all_triggers() {
            if trigger.plugin_name.as_deref() != Some(plugin_name) {
                continue;
            }
            let sv_name = trigger
                .sv_name
                .clone()
                .unwrap_or_else(|| plugin_name.to_string());
            grouped.entry(sv_name).or_default().push(trigger);
        }

        grouped
            .into_iter()
            .map(|(sv_name, triggers)| (Arc::new(crate::core::SV::new(sv_name)), triggers))
            .collect()
    }

    pub async fn all_plugins(&self) -> Vec<String> {
        let plugins = self.plugins.read().await;
        plugins.iter().map(|p| p.name().to_string()).collect()
    }

    /// 即使插件代码已卸载，数据表归属信息仍以持久化的归属台账为权威来源。
    pub async fn database_table_declarations(&self) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        let mut declarations = Vec::new();
        for plugin in nuono_storage::portable::registered_plugins(self.db.orm()).await? {
            declarations.push((
                plugin.plugin_id,
                nuono_storage::portable::manifest_tables(&plugin.manifest)?
                    .into_iter()
                    .collect(),
            ));
        }
        Ok(declarations)
    }

    /// 从同一份插件快照中收集数据表归属与记录管理策略；源码插件回调仍在统一的 catch_unwind 边界内执行。
    pub async fn database_management_declarations(
        &self,
    ) -> anyhow::Result<
        Vec<(
            String,
            Vec<String>,
            Vec<nuono_plugin::db::DatabaseTablePolicy>,
        )>,
    > {
        let plugins = self.plugins.read().await.clone();
        let retained = nuono_storage::portable::registered_plugins(self.db.orm()).await?;
        let mut declarations = Vec::new();
        for registered in retained {
            let tables = nuono_storage::portable::manifest_tables(&registered.manifest)?;
            let mut policies = Vec::new();
            if let Some(hosted) = plugins
                .iter()
                .find(|plugin| plugin.name() == registered.plugin_id)
            {
                let plugin = hosted.plugin.read().await;
                policies = Self::call_with_catch(async { Ok(plugin.database_management()) })
                    .await.unwrap_or_else(|_| {
                        tracing::warn!(plugin = %registered.plugin_id, "插件数据库管理回调失败，保留结构检查");
                        Vec::new()
                    });
                if policies
                    .iter()
                    .any(|policy| !tables.contains(&policy.table))
                {
                    anyhow::bail!(
                        "插件 {} 的记录管理策略超出持久清单归属",
                        registered.plugin_id
                    );
                }
            }
            declarations.push((registered.plugin_id, tables.into_iter().collect(), policies));
        }
        Ok(declarations)
    }

    #[cfg(test)]
    pub(super) async fn register_scoped_plugin_triggers(
        &self,
        plugin: &LoadedPlugin,
        commands: &[crate::core::command::Command],
    ) -> Result<()> {
        self.register_scoped_plugin_triggers_in_epoch(plugin, commands, None)
            .await
    }

    pub(super) async fn register_scoped_plugin_triggers_in_epoch(
        &self,
        plugin: &LoadedPlugin,
        commands: &[crate::core::command::Command],
        epoch: Option<InvocationEpoch>,
    ) -> Result<()> {
        // 仅同步声明提取与注册表写入阶段需要跨插件互斥。
        let registration = self.registration_writer.lock().await;
        // 用 RAII guard 清空 scope：即使 `plugin.triggers()`/`commands()` 或注册过程中 panic，
        // Drop 也会把 scope 还原为 None，不会把失败的插件名泄漏给后续注册。
        self.trigger_registry
            .set_registration_scope(Some(plugin.name()));
        let scope_guard = std::mem::DropGuard::new(&self.trigger_registry, |registry| {
            registry.set_registration_scope(None);
        });

        let result: Result<()> = async {
            let triggers = plugin.triggers();
            let trigger_count = triggers.len();
            let command_count = commands.len();
            let metrics = self.diagnostic_metrics(plugin.name());

            for trigger in triggers {
                self.trigger_registry.register(diagnostics::observe_trigger(
                    trigger,
                    metrics.clone(),
                    epoch.clone(),
                ));
            }
            for command in commands.iter().cloned() {
                self.trigger_registry
                    .register_command(diagnostics::observe_command(
                        command,
                        metrics.clone(),
                        epoch.clone(),
                    ));
            }

            debug!(
                plugin = %plugin.name(),
                trigger_count,
                command_count,
                "插件声明已注册"
            );
            Ok(())
        }
        .await;

        drop(scope_guard);
        drop(registration);
        if result.is_ok() {
            let disabled_commands = self.disabled_commands_from_file(plugin.name()).await;
            self.trigger_registry
                .apply_disabled_commands(plugin.name(), &disabled_commands);
        }

        result
    }

    pub(super) async fn replace_plugin_web_declarations(
        &self,
        plugin_name: &str,
        routes: &[crate::plugin::PluginWebRoute],
        pages: &[crate::plugin::PluginWebPage],
    ) {
        self.web_routes
            .write()
            .await
            .insert(plugin_name.to_string(), routes.to_vec());
        self.web_pages.write().await.insert(
            plugin_name.to_string(),
            web_pages::RegisteredPages {
                generation: self
                    .web_page_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1,
                pages: pages.iter().cloned().map(Arc::new).collect(),
            },
        );
    }

    pub(super) async fn unregister_plugin_web_routes(&self, plugin_name: &str) {
        self.web_routes.write().await.remove(plugin_name);
        self.web_pages.write().await.remove(plugin_name);
    }

    #[cfg(test)]
    pub(super) async fn register_scoped_plugin_tools(&self, plugin: &LoadedPlugin) -> Result<()> {
        self.register_scoped_plugin_tools_in_epoch(plugin, None)
            .await
    }

    pub(super) async fn register_scoped_plugin_tools_in_epoch(
        &self,
        plugin: &LoadedPlugin,
        epoch: Option<InvocationEpoch>,
    ) -> Result<()> {
        let skills = plugin.agent_skills();
        let skill_count = skills.len();
        let tools = agent_skills::scoped_tools(plugin.name(), plugin.tools(), skills)?;
        let tool_count = tools.len();
        // 在向注册表发布该插件的任何工具之前，先校验全部工具声明的合法性。
        if tools
            .iter()
            .any(|tool| self.tool_registry.get(tool.name()).is_some())
        {
            return Err(crate::error::NuonoError::ConfigValidation(format!(
                "插件 {} 的 Agent 工具已注册",
                plugin.name()
            )));
        }
        let mut registered: Vec<String> = Vec::with_capacity(tool_count);
        let metrics = self.diagnostic_metrics(plugin.name());
        for tool in tools {
            let name = tool.name().to_string();
            if let Err(error) = self.tool_registry.register_arc(diagnostics::observe_tool(
                tool,
                metrics.clone(),
                epoch.clone(),
            )) {
                for name in registered {
                    self.tool_registry.unregister(&name);
                }
                return Err(error);
            }
            registered.push(name);
        }
        debug!(plugin = %plugin.name(), tool_count, skill_count, "插件工具及工作流已注册");
        Ok(())
    }

    pub(super) fn unregister_plugin_tools(&self, plugin_name: &str) -> usize {
        self.tool_registry
            .unregister_by_prefix(&format!("plugin.{plugin_name}."))
    }

    /// 更新全局启用档快照（`plugin_registry`）里该插件的 enabled 标记。
    pub(super) async fn set_plugin_registry_enabled(&self, name: &str, enabled: bool) {
        if enabled {
            self.wait_manager.enable_plugin(name);
        } else {
            self.wait_manager.cancel_plugin(name);
        }
        let mut reg = self.plugin_registry.write().await;
        if let Some(cfg) = reg.get(name) {
            let mut new_cfg = cfg.as_ref().clone();
            new_cfg.enabled = enabled;
            reg.insert(name.to_string(), Arc::new(new_cfg));
        }
    }

    pub fn trigger_registry(&self) -> Arc<TriggerRegistry> {
        self.trigger_registry.clone()
    }
}

/// 同一方法下最具体的匹配声明：字面段多者优先，同分取先声明的那条。
///
/// 加载阶段已按归一化形式拒绝同形重复（`/groups/:a` 与 `/groups/:b`），所以同分只可能
/// 来自「字面段数相同但写法不同」的模式，此时声明顺序是调用方唯一能预期的规则。
/// 显式写成循环而不是 `max_by_key`：后者在并列时返回**最后**一个，与这里的规则相反。
fn find_route<'a>(
    routes: &'a [crate::plugin::PluginWebRoute],
    method: &http::Method,
    path: &str,
) -> Option<&'a crate::plugin::PluginWebRoute> {
    let mut best: Option<&crate::plugin::PluginWebRoute> = None;
    for route in routes {
        if route.method != *method || !super::route_pattern::matches(&route.path, path) {
            continue;
        }
        let more_specific = best.is_none_or(|current| {
            super::route_pattern::literal_count(&route.path)
                > super::route_pattern::literal_count(&current.path)
        });
        if more_specific {
            best = Some(route);
        }
    }
    best
}
