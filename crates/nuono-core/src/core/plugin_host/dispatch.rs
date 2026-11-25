//! 插件宿主调度职责。
use super::*;

impl PluginHost {
    /// 在恢复挂起的多轮等待对话前重新校验当前权限与开关策略；
    /// 曾注册过等待会话并不意味着可以豁免等待期间新生效的封禁、群级停用或插件权限变更。
    pub(crate) async fn wait_reply_allowed(
        &self,
        plugin_name: Option<&str>,
        required: Option<PermissionLevel>,
        event: &Value,
    ) -> bool {
        if event.get("post_type").and_then(Value::as_str) != Some("message") {
            return false;
        }
        let actor_permission = self.permission_manager.evaluate(event).await;
        if actor_permission.is_banned() {
            return false;
        }
        if let Some(required) = required
            && !actor_permission.has_permission(required)
        {
            return false;
        }
        let Some(name) = plugin_name else {
            return true;
        };
        let Some(config) = self.get_plugin_config(name).await else {
            return false;
        };
        let Some(user_id) = crate::core::runtime_outbound::runtime_identity(event, "user_id")
        else {
            return false;
        };
        let group_id = crate::core::runtime_outbound::runtime_identity(event, "group_id");
        let context = PluginFilterContext {
            user_id,
            group_id,
            msg_type: event
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            actor_permission,
        };
        Self::plugin_allows_event(&config, &context)
            && self
                .group_enablement_allows(name, context.group_id.as_deref())
                .await
    }
    /// 将等待回复分发给指定插件
    ///
    /// 这条回调路径原先完全不看开关：`WaitReply` 登记之后，即使插件被全局停用或被这个群
    /// 关掉，下一条消息照样会进 `on_message`。等待窗口可以长达数十秒，跨过一次开关变更
    /// 是常态而不是边角情况，所以这里补上和 [`Self::process_message`] 同一道闸门。
    ///
    /// 群号从回调事件本身取，因为等待的消息可能来自与登记时不同的会话。
    pub async fn dispatch_to_plugin(
        &self,
        plugin_name: &str,
        event: &Value,
        session_id: &str,
        callback_id: &str,
    ) -> PluginResponse {
        let locale =
            crate::localization::resolve_event_or_default(&self.locales, event, None).await;
        self.dispatch_to_plugin_with_context(
            plugin_name,
            event,
            session_id,
            callback_id,
            None,
            locale,
        )
        .await
    }

    pub async fn dispatch_to_plugin_with_context(
        &self,
        plugin_name: &str,
        event: &Value,
        session_id: &str,
        callback_id: &str,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> PluginResponse {
        let evt = Event::new(event);
        if !self
            .wait_reply_allowed(Some(plugin_name), None, event)
            .await
        {
            return PluginResponse::Continue;
        }
        if evt.post_type() == "message"
            && !self
                .message_route(evt.text())
                .await
                .allows_plugin(plugin_name)
        {
            return PluginResponse::Continue;
        }
        // 全局开关走注册表快照，与 `process_message` 同一个来源；查不到配置就当没启用。
        let globally_enabled = self
            .get_plugin_config(plugin_name)
            .await
            .map(|cfg| cfg.enabled)
            .unwrap_or(false);
        if !globally_enabled {
            warn!(plugin = %plugin_name, "插件已停用，丢弃其等待回复回调");
            return PluginResponse::Continue;
        }
        let group_id = crate::core::runtime_outbound::runtime_identity(event, "group_id");
        if !self
            .group_enablement_allows(plugin_name, group_id.as_deref())
            .await
        {
            return PluginResponse::Continue;
        }

        let plugins = self.plugin_snapshot().await;
        if let Some(invocation) = plugins.iter().find(|p| p.name() == plugin_name) {
            let Some(plugin) = invocation.read().await else {
                return PluginResponse::Continue;
            };
            let evt = Event::with_domain(event, domain).with_locale_context(locale);
            match invocation
                .run(self.diagnostic_metrics(plugin_name).observe(
                    "on_message",
                    Self::call_with_catch(async {
                        if let Some(event) = domain {
                            self.db.record_activity(
                                event,
                                plugin_name,
                                nuono_storage::activity::ActivityKind::Reply,
                            );
                        }
                        plugin.on_message(&evt, session_id, callback_id).await
                    }),
                ))
                .await
            {
                Some(Ok(resp)) => resp,
                _ => PluginResponse::Continue,
            }
        } else {
            PluginResponse::Continue
        }
    }

    /// 注册等待回复映射
    pub async fn register_pending_wait(&self, session_id: String, plugin_name: String) {
        self.pending_wait_callbacks
            .write()
            .await
            .insert(session_id, plugin_name);
    }

    /// 获取并移除等待回复的插件名
    pub async fn take_pending_wait(&self, session_id: &str) -> Option<String> {
        self.pending_wait_callbacks.write().await.remove(session_id)
    }

    /// 移除所有等待回复回调中归属给定插件的条目。
    ///
    /// 禁用 / fail-closed 清理后该插件不再可被事件调用，若不清掉这些回调，等待窗口里下一条
    /// 消息仍会命中「会话已登记」的状态，虽然 `dispatch_to_plugin` 会因 registry 停用丢弃，
    /// 但条目本身不该残留（泄漏）。只按插件名清除，同一会话若登记的是别的插件则保留。
    pub(super) async fn purge_pending_wait_callbacks(&self, name: &str) {
        self.interactions.revoke(name);
        self.wait_manager.cancel_plugin(name);
        let mut waits = self.pending_wait_callbacks.write().await;
        waits.retain(|_, plugin_name| plugin_name != name);
    }

    /// 按加载顺序调用允许接收该事件的插件，遇到首个阻断型响应即停止。
    ///
    /// 集合快照保留加载顺序，回调只占用目标插件读锁；禁用会撤销快照并取消回调。
    /// panic 会转成错误并继续下一个插件，但外部副作用不会回滚。
    pub async fn process_message(
        &self,
        _event_json: &str,
        event: &Value,
        filter_ctx: &PluginFilterContext,
    ) -> PluginProcessResult {
        self.process_message_with_domain(_event_json, event, filter_ctx, None)
            .await
    }
    pub async fn process_message_with_domain(
        &self,
        _event_json: &str,
        event: &Value,
        filter_ctx: &PluginFilterContext,
        domain: Option<&crate::domain::DomainEvent>,
    ) -> PluginProcessResult {
        let locale =
            crate::localization::resolve_event_or_default(&self.locales, event, domain).await;
        self.process_message_with_context(event, filter_ctx, domain, locale)
            .await
    }

    pub async fn process_message_with_context(
        &self,
        event: &Value,
        filter_ctx: &PluginFilterContext,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> PluginProcessResult {
        super::trigger_control::with_message_admission(
            self.process_message_admitted(event, filter_ctx, domain, locale),
        )
        .await
    }

    async fn process_message_admitted(
        &self,
        event: &Value,
        filter_ctx: &PluginFilterContext,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> PluginProcessResult {
        let evt = Event::with_domain(event, domain).with_locale_context(locale);
        let route = if evt.post_type() == "message" {
            self.message_route(evt.text()).await
        } else {
            PluginMessageRoute::Unprefixed
        };
        if let Some(message) = route.conflict_message() {
            warn!(route = ?route, "插件消息前缀冲突，拒绝分发");
            return PluginProcessResult::new(None, evt.reply_text(message));
        }
        let plugins = self.plugin_snapshot().await;
        for invocation in &plugins {
            // 在排队获取该插件的实例锁之前，先利用句柄上缓存的插件名快速完成路由过滤。
            if !crate::core::interaction::allows_plugin(Some(invocation.name()))
                || !route.allows_plugin(invocation.name())
            {
                continue;
            }
            let Some(plugin) = invocation.read().await else {
                continue;
            };
            let Some(plugin_cfg) = self.get_plugin_config(plugin.name()).await else {
                continue;
            };
            if !Self::plugin_allows_event(&plugin_cfg, filter_ctx) {
                continue;
            }
            // 插件声明的事件类别先于一切回调：不在类别内的事件连 on_event 都不进。
            if !plugin.event_filter().accepts(evt.post_type()) {
                continue;
            }
            // 顺序有意：同步门先淘汰掉区域、权限、黑白名单不合的插件，剩下的才问群级开关。
            // 命中缓存时这里是一次 DashMap 查找，热路径仍是零查询零文件读。
            if !self
                .group_enablement_allows(plugin.name(), filter_ctx.group_id.as_deref())
                .await
            {
                continue;
            }

            // 声明快照在加载时校验；不在热路径执行插件声明或重新编译正则。
            let matches_listener = self
                .installed_plugins
                .read()
                .await
                .get(plugin.name())
                .and_then(|info| info.message_listener.as_ref())
                .is_some_and(|listener| listener.matches(&evt));
            let listens = evt.post_type() == "message"
                && matches!(route, PluginMessageRoute::Unprefixed)
                && matches_listener
                && self
                    .auto_listening_allows(
                        plugin.name(),
                        &plugin_cfg,
                        filter_ctx.group_id.as_deref(),
                    )
                    .await;
            // 未声明监听器的空前缀插件也会经过 on_event，不能把每条普通聊天消息都算作它的触发准入；
            // 此处仅对已声明消息监听器或命中显式指令前缀的调用扣减准入配额，声明式的空前缀指令留到后续触发器匹配命中后再做准入检查。
            let lease = if listens || (evt.post_type() == "message" && !route.prefix().is_empty()) {
                match self.admit_plugin_message(plugin.name(), &evt) {
                    super::trigger_control::Admission::Granted(lease) => Some(lease),
                    super::trigger_control::Admission::Denied { .. } => continue,
                }
            } else {
                None
            };
            let command_evt = if !listens && evt.post_type() == "message" {
                let Some(body) = plugin_cfg.message_text(evt.text()) else {
                    continue;
                };
                Event::with_domain(event, domain)
                    .with_locale_context(locale)
                    .with_command_context(route.prefix(), body)
            } else {
                Event::with_domain(event, domain).with_locale_context(locale)
            };
            let callback = if listens {
                "on_message_listen"
            } else {
                "on_event"
            };
            if !crate::core::interaction::allows_plugin(Some(plugin.name())) {
                continue;
            }
            let metrics = self.diagnostic_metrics(plugin.name());
            let call = metrics.observe(
                callback,
                Self::call_with_catch(async {
                    if listens {
                        if let Some(event) = domain {
                            self.db.record_activity(
                                event,
                                plugin.name(),
                                nuono_storage::activity::ActivityKind::Listener,
                            );
                        }
                        plugin.on_message_listen(&evt).await
                    } else {
                        plugin.on_event(&command_evt).await
                    }
                }),
            );
            let response = invocation
                .run(async {
                    match lease {
                        Some(guard) => {
                            nuono_plugin::plugin::with_invocation_guard(plugin.name(), guard, call)
                                .await
                        }
                        None => call.await,
                    }
                })
                .await;
            let Some(response) = response else {
                continue;
            };
            // 未声明监听器的空前缀 on_event 仅作透传探测，返回 Continue 不算作一次插件触发。
            if !listens
                && !matches!(&response, Ok(PluginResponse::Continue))
                && let Some(event) = domain
            {
                self.db.record_activity(
                    event,
                    plugin.name(),
                    nuono_storage::activity::ActivityKind::Event,
                );
            }
            match response {
                Ok(PluginResponse::Stop) => {
                    return PluginProcessResult::new(
                        Some(plugin.name().to_string()),
                        PluginResponse::Stop,
                    );
                }
                Ok(PluginResponse::Reply(reply)) => {
                    return PluginProcessResult::new(
                        Some(plugin.name().to_string()),
                        PluginResponse::Reply(reply),
                    );
                }
                Ok(PluginResponse::WaitReply {
                    reply_message,
                    session_id,
                    timeout_secs,
                    callback_id,
                    plugin_name,
                    event_context,
                }) => {
                    return PluginProcessResult::new(
                        Some(plugin.name().to_string()),
                        PluginResponse::WaitReply {
                            reply_message,
                            session_id,
                            timeout_secs,
                            callback_id,
                            plugin_name,
                            event_context,
                        },
                    )
                    .with_wait_invocation(invocation.clone());
                }
                Ok(PluginResponse::Continue) | Ok(_) => {}
                Err(e) => {
                    warn!(plugin = %plugin.name(), callback, error = %e, "插件回调失败 (已捕获)");
                }
            }
        }
        PluginProcessResult::continue_without_plugin()
    }

    pub(super) fn plugin_allows_event(plugin: &Plugins, filter_ctx: &PluginFilterContext) -> bool {
        plugin.enabled
            && plugin.check_area(&filter_ctx.msg_type)
            && plugin.check_permission(filter_ctx.actor_permission)
            && plugin.check_access(&filter_ctx.user_id, filter_ctx.group_id.as_deref())
    }

    async fn auto_listening_allows(
        &self,
        name: &str,
        config: &Plugins,
        group: Option<&str>,
    ) -> bool {
        let Some(default) = config.auto_listen_enabled else {
            return false;
        };
        if nuono_plugin::config::runtime_configuration(name).is_some() {
            return default;
        }
        group.is_none() && default
    }

    /// 群级开关这一层，独立于同步的 [`Self::plugin_allows_event`]。
    ///
    /// 没有并进那个门里：解析是 async，而那个门是对 `Plugins` 快照的纯谓词 —— 改成 async
    /// 会让它必须拿着宿主和数据库才能测，而它现在的价值正是不用。所以拆成两段，调用方
    /// 先过便宜的同步门，只有已经通过的插件才付这一次解析的代价。
    ///
    /// `group_id == None`（私聊、定时任务、启停、Agent 工具等）直接放行：那些路径没有第二层，
    /// 全局开关已经由调用方查过。群号原样按字符串传下去，飞书 `oc_` 之类的非数字 id 不能
    /// 折成 `i64` 或哈希，否则会串号或整群丢失。
    pub(crate) async fn group_enablement_allows(
        &self,
        plugin_name: &str,
        group_id: Option<&str>,
    ) -> bool {
        if self.plugin_role(plugin_name).await.is_service() {
            return true;
        }
        if let Some(configuration) = nuono_plugin::config::runtime_configuration(plugin_name) {
            return configuration.enabled;
        }
        // 缺乏规范化事件上下文的裸群号无法匹配群级覆盖配置，仅在私聊（group_id 为 None）时默认放行。
        group_id.is_none()
    }

    /// 收集所有插件的 Web 路由
    /// 分发 Web 请求到对应插件
    pub async fn handle_web_request(
        &self,
        plugin_name: &str,
        req: &crate::plugin::PluginWebRequest,
    ) -> crate::plugin::PluginWebResponse {
        let Some(route) = self
            .plugin_web_route(plugin_name, &req.method, &req.path)
            .await
        else {
            return crate::plugin::PluginWebResponse::NotFound;
        };
        if req.body.len() > route.max_body_bytes {
            return crate::plugin::PluginWebResponse::Status(413, "插件请求超过大小上限".into());
        }
        let plugins = self.plugin_snapshot().await;
        if let Some(invocation) = plugins.iter().find(|p| p.name() == plugin_name) {
            let Some(plugin) = invocation.read().await else {
                return crate::plugin::PluginWebResponse::NotFound;
            };
            let metrics = self.diagnostic_metrics(plugin_name);
            let callback = format!("web:{} {}", req.method, req.path);
            let guard = metrics.begin(&callback);
            let response = match invocation
                .run(Self::call_with_catch(async {
                    Ok(plugin.handle_web_request(req).await)
                }))
                .await
            {
                Some(Ok(response)) => response,
                Some(Err(_)) => {
                    crate::plugin::PluginWebResponse::Status(500, "插件请求失败".into())
                }
                None => return crate::plugin::PluginWebResponse::NotFound,
            };
            // Web 路由回调不返回 Result，以响应状态码是否为 HTTP 5xx 作为调用失败信号。
            let failed = matches!(
                response,
                crate::plugin::PluginWebResponse::Status(500..=599, _)
                    | crate::plugin::PluginWebResponse::Http {
                        status: 500..=599,
                        ..
                    }
            );
            guard.finish(failed);
            response
        } else {
            crate::plugin::PluginWebResponse::NotFound
        }
    }

    /// 将插件 future 内的 unwind 转成普通错误。
    ///
    /// 这只保护进入该 future 后发生的 Rust panic，不能撤销 panic 前已经完成的
    /// 网络、文件或数据库副作用。插件工厂在实例化边界捕获 panic。
    pub(super) async fn call_with_catch<F, T>(future: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(r) => r,
            Err(panic_err) => {
                let msg = panic_err
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic_err.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                Err(anyhow::anyhow!("插件 panic: {}", msg))
            }
        }
    }
}
