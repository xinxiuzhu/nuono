//! 插件宿主生命周期职责。
use super::*;

impl PluginHost {
    pub async fn toggle_plugin(&self, name: &str) -> crate::error::Result<()> {
        // DB 读取失败时保持现状，不猜测启停方向。
        let current = self.db.is_plugin_enabled(name).await?;
        self.toggle(name, !current).await
    }

    /// 从已链接工厂发现候选，再筛选声明，最后由安装阶段提交并激活。
    #[instrument(skip(self))]
    pub async fn init(&self) -> Result<()> {
        match crate::plugin::source_loader::instantiate_registered()? {
            Some(loaded) => self.init_registered_plugins(loaded).await,
            // 嵌入宿主未提供完整工厂表时，不能把「未知」当成「全部已卸载」。
            None => self.init_with_plugins(vec![]).await,
        }
    }

    /// 仅用于新宿主启动，loaded 必须是当前程序已注册工厂的完整结果。
    pub(super) async fn init_registered_plugins(&self, loaded: Vec<LoadedPlugin>) -> Result<()> {
        if !self.installed_plugins.read().await.is_empty()
            || !self.plugins.read().await.is_empty()
            || self
                .inventory_reconciliation_started
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(crate::error::NuonoError::Validation(
                "插件注册记录清理只能在新宿主加载完整名单时执行".into(),
            ));
        }
        let names = self.load_plugin_batch(loaded).await?;
        // 身份发现或安装发生致命错误时不会进入清理；声明/配置/激活失败的身份仍在名单内。
        let removed = match self.db.prune_plugin_registrations(&names).await {
            Ok(removed) => removed,
            Err(error) => {
                // 清理失败阻止启动，也回收已经加载的受管任务。
                if let Err(shutdown_error) = self.shutdown().await {
                    warn!(%shutdown_error, "插件注册记录清理失败后的宿主收尾失败");
                }
                return Err(error);
            }
        };
        if !removed.is_empty() {
            info!(target: "nuono::plugins", plugins = ?removed, "已清理移除插件的注册记录和群设置");
        }
        Ok(())
    }

    /// 增量注入插件实例，只同步本批身份，不据此删除其他插件的注册记录。
    pub async fn init_with_plugins(&self, loaded: Vec<LoadedPlugin>) -> Result<()> {
        self.load_plugin_batch(loaded).await.map(|_| ())
    }

    async fn load_plugin_batch(&self, loaded: Vec<LoadedPlugin>) -> Result<Vec<String>> {
        let _admission = self.admit_lifecycle().await?;
        let loading_started = std::time::Instant::now();
        let discovered = loaded.len();
        info!(target: "nuono::plugins", "正在加载插件（{discovered} 个已安装）…");
        let plan = self.resolve_load_candidates(loaded).await?;
        let names: Vec<String> = plan
            .candidates
            .iter()
            .map(|candidate| candidate.declaration.name.clone())
            .chain(plan.rejected.iter().map(|rejected| rejected.name.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let installed = self.install_load_plan(plan).await?;
        self.trigger_registry.print_stats();
        let skipped = discovered.saturating_sub(installed);
        let disabled = self
            .list_plugin_configs()
            .await
            .iter()
            .filter(|plugin| names.contains(&plugin.name) && !plugin.enabled)
            .count();
        let enabled = installed.saturating_sub(disabled);
        info!(target: "nuono::plugins", installed, skipped, enabled, disabled,
            "插件加载完成：{enabled} 个已加载，{disabled} 个未启用，{skipped} 个跳过（耗时 {:.2} 秒）",
            loading_started.elapsed().as_secs_f64());
        Ok(names)
    }

    pub(super) async fn plugin_context(
        &self,
        plugin_name: &str,
        config: Value,
        reason: PluginActivationReason,
    ) -> PluginContext {
        let data_dir = self.paths.plugin_data(plugin_name);
        let config_manager = self.config_manager.read().await.as_ref().cloned();
        let config_updates = config_manager
            .as_ref()
            .map(|manager| super::config_updates::for_plugin(manager.clone(), plugin_name))
            .unwrap_or_default();
        let config_handle = match config_manager.as_ref() {
            Some(manager) => manager
                .config_handle(plugin_name)
                .await
                .unwrap_or_else(|| crate::config::PluginConfigHandle::new(config.clone())),
            None => crate::config::PluginConfigHandle::new(config.clone()),
        };
        if let Err(e) = tokio::fs::create_dir_all(&data_dir).await {
            warn!(plugin = %plugin_name, error = %e, "创建插件数据目录失败");
        }

        // 调用方持有 plugin_operation，按插件串行创建新的 on_load 周期。
        // 等待旧任务退出期间保留 map 中的旧 scope：如果调用方被取消，重试仍须
        // 等待同一个退出屏障，不能用空的新 scope 遮住尚未结束的阻塞工作。
        let old = self.task_scopes.read().await.get(plugin_name).cloned();
        if let Some(old) = old {
            old.shutdown().await;
        }
        let scope = Arc::new(
            PluginTaskScope::new(plugin_name)
                .with_activation_reason(reason)
                .with_business_gate(nuono_app::business_runtime::BusinessGate::paused())
                .with_image_output(self.image_output.clone()),
        );
        self.task_scopes
            .write()
            .await
            .insert(plugin_name.to_string(), scope.clone());

        // secret store 复用已验证 snapshot（从 registry 读取，不再调用 secret_fields），
        // 保证与 registry 一致；registry 若缺失则按「无 secret」处理（fail-closed）。
        let secret_fields = self
            .secret_schemas
            .read()
            .await
            .get(plugin_name)
            .cloned()
            .unwrap_or_default();

        let http = crate::plugin::PluginHttpClient::new();
        let assets = nuono_plugin::PluginAssets::new(data_dir.join("assets"), http.clone());
        let embeddings =
            super::embeddings::for_plugin(self.db.clone(), http.clone(), scope.clone());
        let mut database = nuono_plugin::db::Database::for_plugin(self.db.clone(), plugin_name);
        if let Some(manager) = config_manager {
            database =
                database.with_configuration_access(Arc::new(super::configuration_access::Access {
                    db: self.db.clone(),
                    manager,
                    plugin: plugin_name.into(),
                    scope: scope.clone(),
                    authority: self.platform_role_authority.clone(),
                    verifiers: self.platform_role_verifiers.clone(),
                }));
        }
        PluginContext::from_parts(nuono_plugin::plugin::PluginContextParts {
            db: database,
            event_bus: Arc::new(EventBus::for_plugin(self.event_bus.clone(), &scope)),
            permission_manager: self.permission_manager.clone(),
            config,
            config_handle,
            config_updates,
            secrets: crate::plugin::PluginSecretStore::new(
                self.db.clone(),
                plugin_name,
                secret_fields,
            ),
            data_dir,
            bot: Some(Arc::new(
                crate::api::Bot::new(
                    0,
                    self.event_bus.clone(),
                    self.wait_manager.clone(),
                    Arc::new(crate::core::ApiClient::new()),
                    None,
                    None,
                )
                .with_action_router(self.action_router.clone())
                .with_wait_owner(plugin_name, None),
            )),
            http,
            embeddings,
            account_conversations: self.account_conversations(plugin_name, &scope),
            locales: self.locales.clone(),
            assets,
            cooldown_tracker: self.cooldown_tracker.clone(),
            command_stats: self.command_stats.clone(),
            framework_control: self.framework_control.clone(),
            tasks: scope,
            media_links: nuono_plugin::media_link::PluginMediaLinks::new(
                crate::core::media_links::global()
                    .map(|links| links as Arc<dyn nuono_plugin::media_link::MediaLinkService>),
            ),
            services: self.service_registry.clone(),
        })
    }

    /// abort 并等待插件当前全部受管后台任务退出。
    ///
    /// 在 `on_disable` / `on_unload` 之前调用。关闭的作用域不再接受新任务；
    /// 收尾操作应在生命周期回调中直接 await，再次启用时创建新的作用域。
    pub(super) async fn reap_plugin_tasks(&self, name: &str) {
        let scope = self.task_scopes.read().await.get(name).cloned();
        if let Some(scope) = scope {
            scope.shutdown().await;
        }
    }

    pub(super) async fn sync_to_db_with_plugins(
        &self,
        records: Vec<(String, String, Value)>,
    ) -> Result<()> {
        let refs: Vec<(&str, &str, &Value)> = records
            .iter()
            .map(|(n, v, c)| (n.as_str(), v.as_str(), c))
            .collect();
        self.db.sync_plugins_batch(&refs).await
    }

    pub async fn sync_to_db(&self) -> Result<()> {
        let plugins = self.plugins.read().await.clone();
        let declarations = self.config_declarations.read().await.clone();
        let mut records = Vec::new();
        for hosted in &plugins {
            let p = hosted.plugin.read().await;
            // 收集 name/version/config_schema 这组插件声明的同步调用也放进 panic 边界。
            let rec: anyhow::Result<(String, String, Value)> = Self::call_with_catch(async {
                let name = p.name().to_string();
                // 已安装实例使用配置准备时发布的字段快照；嵌入方直接注入的实例兼容旧路径。
                let fields = declarations
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| p.config_fields());
                Ok((
                    name,
                    p.version().to_string(),
                    serde_json::to_value(crate::config::plugin_fields_to_schema(&fields)?)?,
                ))
            })
            .await;
            records.push(rec?);
        }
        self.sync_to_db_with_plugins(records).await
    }

    /// 切换插件运行状态。
    ///
    /// 启用会重新执行 on_load 并注册声明，业务开放后执行 on_enable；禁用执行 on_disable，真正的
    /// `on_unload` 留到实例重建或进程关闭。禁用先撤销准入；启用在激活和注册成功后提交。
    /// 生命周期失败按 fail-closed 清理，插件自己的外部副作用不回滚。
    /// 将管理员策略编辑与插件激活过程串行化，防止随后的启停切换被并发编辑覆盖。
    /// 检查强依赖列表中尚未处于已启用状态的上游插件。
    pub(super) async fn missing_required_dependencies(&self, required: &[String]) -> Vec<String> {
        if required.is_empty() {
            return Vec::new();
        }
        let registry = self.plugin_registry.read().await;
        required
            .iter()
            .filter(|dep| !registry.get(dep.as_str()).is_some_and(|cfg| cfg.enabled))
            .cloned()
            .collect()
    }

    pub(super) async fn ensure_required_dependencies_enabled(
        &self,
        name: &str,
        required: &[String],
    ) -> Result<()> {
        let missing = self.missing_required_dependencies(required).await;
        if !missing.is_empty() {
            return Err(crate::error::NuonoError::Conflict(format!(
                "无法启用插件 {name}：缺少已启用的强依赖插件 {}",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    pub(super) async fn ensure_required_dependencies_enabled_for(&self, name: &str) -> Result<()> {
        let (required, _) = self.plugin_dependencies(name).await;
        self.ensure_required_dependencies_enabled(name, &required)
            .await
    }

    /// 检查是否存在当前仍处于启用状态且强依赖 `name` 的下游插件。
    pub(super) async fn ensure_no_active_required_dependents(&self, name: &str) -> Result<()> {
        let infos = self.installed_plugins.read().await;
        let registry = self.plugin_registry.read().await;
        let mut dependents: Vec<String> = infos
            .iter()
            .filter(|(plugin_id, info)| {
                plugin_id.as_str() != name
                    && registry
                        .get(plugin_id.as_str())
                        .is_some_and(|cfg| cfg.enabled)
                    && info.required_dependencies.iter().any(|dep| dep == name)
            })
            .map(|(plugin_id, _)| plugin_id.clone())
            .collect();
        dependents.sort();
        if !dependents.is_empty() {
            return Err(crate::error::NuonoError::Conflict(format!(
                "无法禁用插件 {name}：以下已启用插件强依赖它（{}），请先停用它们",
                dependents.join(", ")
            )));
        }
        Ok(())
    }

    pub async fn save_policy_and_apply(
        &self,
        actor: i64,
        name: &str,
        patch: nuono_storage::plugin_global_settings::PluginPolicyPatch,
    ) -> Result<()> {
        let _admission = self.admit_lifecycle().await?;
        let _operation = self.plugin_operation(name).await;
        if !self.is_plugin_installed(name).await {
            return Err(crate::error::NuonoError::PluginNotFound(name.into()));
        }
        let enabled = patch.enabled;
        match enabled {
            Some(true) => self.ensure_required_dependencies_enabled_for(name).await?,
            Some(false) => self.ensure_no_active_required_dependents(name).await?,
            None => {}
        }
        self.db.save_plugin_policy(actor, name, patch).await?;
        match enabled {
            Some(true) => self.enable_plugin(name).await,
            Some(false) => self.disable_plugin(name).await,
            None => Ok(()),
        }
    }

    pub async fn toggle(&self, name: &str, enabled: bool) -> Result<()> {
        let _admission = self.admit_lifecycle().await?;
        let _operation = self.plugin_operation(name).await;
        let Some(info) = self.installed_plugin_infos().await.remove(name) else {
            return Err(crate::error::NuonoError::PluginNotFound(name.to_string()));
        };
        // 嵌入方可以直接提供实例；在启用失败可能移除实例前保留它的身份。
        self.remember_plugin_info(name, info).await;
        if enabled {
            self.enable_plugin(name).await
        } else {
            self.disable_plugin(name).await
        }
    }

    /// 统一 fail-closed 清理：激活失败或提交 `enabled=true` 失败后调用。
    ///
    /// 清理宿主可控资源：注销 triggers/tools、收束任务、best-effort `on_disable`/`on_unload`
    /// （失败记录但不中断）、再次收束任务（覆盖 `on_disable`/`on_unload` 里又 `spawn` 的任务）、
    /// 删除该插件的 task scope，从 `secret_schemas`/`plugin_registry` 移除，并写回 DB
    /// `enabled=false`（失败要记录，不静默）。服务策略插件例外：保留启用意图和关闭的准入，
    /// 只有管理员明确停用才释放限制。调用方持有单插件实例写锁，随后按句柄身份
    /// 从集合中移除失败实例，不在集合锁内执行清理。返回组合后的清理
    /// 错误；任何单步失败都可观察，其余清理尽可能继续。
    pub(super) async fn cleanup_failed_activation(
        &self,
        name: &str,
        plugin: Option<&mut LoadedPlugin>,
    ) -> crate::error::Result<()> {
        self.service_registry.unregister_plugin(name);
        self.pending_activations.write().await.remove(name);
        nuono_app::account_merge::unregister_account_merge_participant(name);
        self.extensions.suspend(name);
        let mut errors: Vec<String> = Vec::new();
        self.trigger_registry.unregister_by_plugin(name);
        self.unregister_plugin_tools(name);
        self.unregister_plugin_web_routes(name).await;
        self.reap_plugin_tasks(name).await;
        if let Some(p) = plugin {
            if let Err(e) = self
                .diagnostic_metrics(name)
                .observe("on_disable", Self::lifecycle_call(p.on_disable()))
                .await
            {
                errors.push(format!("on_disable 失败: {e}"));
            }
            if let Err(e) = self
                .diagnostic_metrics(name)
                .observe("on_unload", Self::lifecycle_call(p.on_unload()))
                .await
            {
                errors.push(format!("on_unload 失败: {e}"));
            }
        }
        // on_disable/on_unload 可能又 spawn 了任务，再次收束；随后删除该插件的 task scope。
        self.reap_plugin_tasks(name).await;
        self.service_registry.unregister_plugin(name);
        self.task_scopes.write().await.remove(name);
        self.secret_schemas.write().await.remove(name);
        self.config_declarations.write().await.remove(name);
        self.config_revisions.write().await.remove(name);
        self.plugin_registry.write().await.remove(name);
        self.purge_pending_wait_callbacks(name).await;
        if !self.extensions.holds_service_policy(name)
            && let Err(e) = self.db.set_plugin_enabled(name, false).await
        {
            errors.push(format!("写回 DB enabled=false 失败: {e}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(crate::error::NuonoError::Other(errors.join("; ")))
        }
    }

    /// 禁用一个插件。`on_disable` 的 error/panic 记录主错误但**不中断**后续清理：仍会二次
    /// 收束任务、把 DB 与内存档（registry）都置为 disabled，最后聚合返回。
    /// 实例保持 loaded 但 registry 置 false，因此不可被事件调用。
    pub(super) async fn disable_plugin(&self, name: &str) -> Result<()> {
        self.ensure_no_active_required_dependents(name).await?;
        self.deactivate_plugin(name, true).await
    }

    async fn deactivate_plugin(&self, name: &str, restore_default: bool) -> Result<()> {
        self.service_registry.unregister_plugin(name);
        self.pending_activations.write().await.remove(name);
        nuono_app::account_merge::unregister_account_merge_participant(name);
        self.extensions.suspend(name);
        // 在排队等待活跃回调释放插件实例写锁前，先行撤销该插件注册的所有交互按钮。
        self.interactions.revoke(name);
        // 先撤销该插件的等待会话与调用批次（generation），再去等待该实例的写锁。
        self.wait_manager.cancel_plugin(name);
        let hosted = self.hosted_plugin(name).await;
        if let Some(hosted) = &hosted {
            hosted.retire();
        }
        self.set_plugin_registry_enabled(name, false).await;
        let mut errors: Vec<String> = Vec::new();
        // 只有显式且已持久化落库的停用操作才可解除服务访问限制；
        // 插件重载期间仍保留启用意图与挂起的封闭策略（fail-closed），直到新实例替换成功。
        let mut disabled_persisted = false;
        if restore_default || !self.extensions.holds_service_policy(name) {
            match self.db.set_plugin_enabled(name, false).await {
                Ok(()) => disabled_persisted = true,
                Err(e) => errors.push(format!("写回 DB enabled=false 失败: {e}")),
            }
        }
        if let Some(hosted) = hosted {
            hosted.quiesce().await;
            let mut plugin = hosted.plugin.write().await;
            self.trigger_registry.unregister_by_plugin(name);
            self.unregister_plugin_tools(name);
            self.unregister_plugin_web_routes(name).await;
            self.reap_plugin_tasks(name).await;
            if let Err(e) = self
                .diagnostic_metrics(name)
                .observe("on_disable", Self::lifecycle_call(plugin.on_disable()))
                .await
            {
                errors.push(format!("on_disable 失败: {e}"));
            }
            // on_disable 可能又 spawn 了任务，再次收束。
            self.reap_plugin_tasks(name).await;
            self.service_registry.unregister_plugin(name);
        }
        // 内存档与 DB 保持一致为 disabled。
        self.set_plugin_registry_enabled(name, false).await;
        if restore_default && disabled_persisted {
            self.extensions.disable(name);
        }
        // 该插件不再可被事件调用，清掉其挂起的等待回复回调，避免泄漏。
        self.purge_pending_wait_callbacks(name).await;
        if errors.is_empty() {
            info!(plugin = %name, "插件已禁用");
            Ok(())
        } else {
            Err(crate::error::NuonoError::Other(errors.join("; ")))
        }
    }

    /// 按加载顺序的逆序关闭所有插件，并清空宿主的插件级注册状态。
    ///
    /// 生命周期与 DB 清理错误会被聚合返回（单个插件失败不阻断其余清理）。插件自身的外部副作用
    /// 不可回滚；宿主可控资源（triggers/commands、工具、任务、schema、registry、pending wait）
    /// 都会被清空。`on_disable`/`on_unload` 之后都会再次收束任务（覆盖回调里又 spawn 的任务），
    /// 并在 `on_unload` 之后移除该插件的 task scope。
    pub async fn shutdown(&self) -> Result<()> {
        self.request_business_shutdown();
        let _admission = self.lifecycle_admission.write().await;
        for config in self.list_plugin_configs().await {
            self.wait_manager.cancel_plugin(&config.name);
        }
        let mut errors: Vec<String> = Vec::new();
        let plugins = std::mem::take(&mut *self.plugins.write().await);
        for hosted in &plugins {
            self.service_registry.unregister_plugin(hosted.name());
            nuono_app::account_merge::unregister_account_merge_participant(hosted.name());
            self.extensions.suspend(hosted.name());
            hosted.retire();
        }
        for hosted in plugins.iter().rev() {
            let name = hosted.name().to_string();
            let _operation = self.plugin_operation(&name).await;
            hosted.quiesce().await;
            let mut plugin = hosted.plugin.write().await;
            self.trigger_registry.unregister_by_plugin(&name);
            self.unregister_plugin_tools(&name);
            self.unregister_plugin_web_routes(&name).await;
            self.reap_plugin_tasks(&name).await;
            if let Err(e) = self
                .diagnostic_metrics(&name)
                .observe("on_disable", Self::lifecycle_call(plugin.on_disable()))
                .await
            {
                errors.push(format!("plugin {name} on_disable 失败: {e}"));
            }
            self.reap_plugin_tasks(&name).await;
            if let Err(e) = self
                .diagnostic_metrics(&name)
                .observe("on_unload", Self::lifecycle_call(plugin.on_unload()))
                .await
            {
                errors.push(format!("plugin {name} on_unload 失败: {e}"));
            }
            // on_unload 可能又 spawn 任务，再次收束；随后删除该插件的 task scope。
            self.reap_plugin_tasks(&name).await;
            self.task_scopes.write().await.remove(&name);
            self.unregister_plugin_web_routes(&name).await;
        }
        // 清空 schema、registry、待回复回调、触发器/命令与插件级工具（内置只读工具保留）。
        self.pending_activations.write().await.clear();
        self.secret_schemas.write().await.clear();
        self.web_routes.write().await.clear();
        self.web_pages.write().await.clear();
        self.config_declarations.write().await.clear();
        self.config_revisions.write().await.clear();
        self.plugin_registry.write().await.clear();
        self.pending_wait_callbacks.write().await.clear();
        self.wait_manager.clear();
        self.trigger_registry.clear();
        self.service_registry.clear();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(crate::error::NuonoError::Other(errors.join("; ")))
        }
    }

    /// 重新创建已编译插件的实例。更新源码后须重新构建并重启程序。
    ///
    /// 流程：读取启用状态（DB 读错误立即中止）→ 加载候选新实例（候选必须恰好一个；0 或 >1
    /// 都在替换前拒绝）→ 校验完整声明 → 安装新实例并恢复启用。旧实例与旧
    /// schema 在候选成功前保持原样、不污染集合 / registry。
    pub async fn reload_plugin(&self, name: &str) -> Result<()> {
        let _admission = self.admit_lifecycle().await?;
        let _operation = self.plugin_operation(name).await;
        // 读取启用状态必须尽早完成并失败即中止：DB 故障时旧实例 / 注册状态保持原样。
        let was_enabled = self.db.is_plugin_enabled(name).await?;

        // 1. 加载候选新实例（未加入集合，不影响旧插件）。
        let candidate =
            Self::resolve_reload_candidate(name, Self::discover_plugin_instances()?).await?;

        // 完整预检之后才进入安装与恢复，不重复读取同步声明。
        self.reload_prepared_candidate(name, candidate, was_enabled)
            .await
    }

    /// 完成 reload 的「校验新实例 → 卸载旧实例 → 原子替换 → 恢复启用」。
    ///
    /// 测试注入已编译实例，使用与 [`Self::reload_plugin`] 相同的预检和安装路径。
    /// 身份、secret、配置、命令和 Web 路由声明均在替换前校验，失败保留旧实例。
    #[cfg(test)]
    pub(super) async fn reload_with_candidate(
        &self,
        name: &str,
        candidate: LoadedPlugin,
        was_enabled: bool,
    ) -> Result<()> {
        let _admission = self.admit_lifecycle().await?;
        let _operation = self.plugin_operation(name).await;
        let candidate = Self::resolve_reload_candidate(name, vec![candidate]).await?;
        self.reload_prepared_candidate(name, candidate, was_enabled)
            .await
    }

    async fn reload_prepared_candidate(
        &self,
        name: &str,
        candidate: super::loading::PreparedPlugin,
        was_enabled: bool,
    ) -> Result<()> {
        let super::loading::PreparedPlugin {
            plugin: candidate,
            declaration,
            secret_fields,
        } = candidate;
        // 禁用实例没有激活阶段，仍须安装新配置声明，供表单与后续启用使用。
        if !was_enabled {
            self.prepare_runtime_config(&declaration).await?;
        }

        // 5. 新实例可用，才开始卸载旧实例。
        if was_enabled {
            self.deactivate_plugin(name, false).await?;
        }
        self.trigger_registry.unregister_by_plugin(name);
        self.unregister_plugin_tools(name);
        self.unregister_plugin_web_routes(name).await;

        // 6. 原子替换（并清旧 schema），返回全部已离开集合的同名旧实例。
        let old_plugins = self
            .swap_plugin_instances(name, candidate, secret_fields)
            .await;
        self.remember_plugin_declaration(&declaration).await;

        // 7. 旧实例已离开集合，best-effort 收尾；schema 清理已在 swap 中完成。
        self.reap_plugin_tasks(name).await;
        for hosted in &old_plugins {
            hosted.quiesce().await;
            let mut old = hosted.plugin.write().await;
            if let Err(e) = self
                .diagnostic_metrics(name)
                .observe("on_unload", Self::lifecycle_call(old.on_unload()))
                .await
            {
                warn!(plugin = %name, error = %e, "重载时旧插件 on_unload 失败");
            }
        }
        self.reap_plugin_tasks(name).await;
        self.task_scopes.write().await.remove(name);
        drop(old_plugins);

        // 8. 恢复启用；激活失败走 fail-closed（toggle 已清理新实例），这里返回清楚错误。
        if was_enabled {
            if let Err(e) = self
                .enable_plugin_with_declaration(
                    name,
                    Some(declaration),
                    PluginActivationReason::Rebuild,
                )
                .await
            {
                return Err(crate::error::NuonoError::PluginLoad {
                    plugin: name.to_string(),
                    reason: format!("旧实例已卸载，但新实例激活失败: {e}"),
                });
            }
        } else {
            let mut service_config = self
                .service_config_from_file(name, declaration.service_config)
                .await;
            service_config.enabled = false;
            self.register_plugin_config(service_config).await;
        }

        info!(plugin = %name, "插件已实例重建");
        Ok(())
    }
}
