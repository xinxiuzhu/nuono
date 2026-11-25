use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, trace};

use crate::api::Bot;
use crate::core::{
    CommandStatsManager, CooldownTracker, EventBus, PermissionManager,
    event_kernel::RuntimeActionSink,
    plugin_host::PluginHost,
    plugin_service::{PluginArea, Plugins},
    trigger::{MatchData, Trigger},
    trigger_registry::TriggerRegistry,
};
use crate::db::Database;
use crate::error::{NuonoError, Result};
use crate::onebot::parse_message;
use crate::plugin::Event;
use serde_json::Value;

/// 事件处理管线 — 对应 gsuid_core 的 handler.py
///
/// 管道顺序：
/// 1. 全局暂停/黑名单/权限/冷却
/// 2. 插件声明的业务前缀匹配
/// 3. 触发器匹配（双层过滤：Plugins + SV）
/// 4. 优先级排序 + 顺序执行
pub struct EventHandler {
    pub db: Database,
    pub trigger_registry: Arc<TriggerRegistry>,
    pub cooldown: Arc<CooldownTracker>,
    pub stats: Arc<CommandStatsManager>,
    pub permission_manager: Arc<PermissionManager>,
    pub event_bus: Arc<EventBus>,
    pub wait_manager: Arc<crate::api::WaitManager>,
    pub api_client: Arc<crate::core::ApiClient>,
    /// 插件宿主（动态查询插件配置）
    pub plugin_host: Option<Arc<PluginHost>>,
    pub global_blacklist: RwLock<Vec<String>>,
    pub is_handle: RwLock<bool>,
}

/// 事件处理结果
#[derive(Debug, Clone, Copy)]
pub struct HandleResult {
    /// 是否有触发器匹配并执行
    pub matched: bool,
    /// 是否阻断后续处理（消息历史保存等）
    pub blocked: bool,
}

/// 供各事件派发器复用的触发器匹配、冷却追踪与指令统计服务集合。
pub struct CommandDispatchServices {
    pub trigger_registry: Arc<TriggerRegistry>,
    pub cooldown: Arc<CooldownTracker>,
    pub stats: Arc<CommandStatsManager>,
}

impl EventHandler {
    pub fn new(
        db: Database,
        commands: CommandDispatchServices,
        permission_manager: Arc<PermissionManager>,
        event_bus: Arc<EventBus>,
        wait_manager: Arc<crate::api::WaitManager>,
        api_client: Arc<crate::core::ApiClient>,
    ) -> Self {
        let CommandDispatchServices {
            trigger_registry,
            cooldown,
            stats,
        } = commands;
        Self {
            db,
            trigger_registry,
            cooldown,
            stats,
            permission_manager,
            event_bus,
            wait_manager,
            api_client,
            plugin_host: None,
            global_blacklist: RwLock::new(Vec::new()),
            is_handle: RwLock::new(true),
        }
    }

    /// 设置 PluginHost 引用（用于动态查询插件配置）
    pub fn set_plugin_host(&mut self, host: Arc<PluginHost>) {
        self.plugin_host = Some(host);
    }

    /// 该插件在这个群里是否生效。
    ///
    /// 解析口在宿主上（它持有数据库和配置管理器），这里只做转发，避免本文件出现第二套
    /// 判据。没有宿主时放行：那种配置下连全局开关都读不到，此处收紧只会让触发器凭空失效，
    /// 而真正的闸门在有宿主的运行时里。
    async fn group_enablement_allows(&self, plugin_name: &str, group_id: Option<&str>) -> bool {
        match self.plugin_host.as_ref() {
            Some(host) => host.group_enablement_allows(plugin_name, group_id).await,
            None => true,
        }
    }

    /// 获取插件配置（优先从 PluginHost 查询）
    async fn get_plugin_cfg(&self, name: &str) -> Option<Arc<Plugins>> {
        if let Some(ref host) = self.plugin_host {
            host.get_plugin_config(name).await
        } else {
            None
        }
    }

    /// 🎯 核心事件处理
    pub async fn handle_event(
        &self,
        bot_id: i64,
        event_json: &str,
        bot_sender: Option<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
    ) -> Result<HandleResult> {
        let event: Value = serde_json::from_str(event_json).unwrap_or(Value::Null);
        let locales = self
            .plugin_host
            .as_ref()
            .map(|host| host.locales.clone())
            .unwrap_or_else(|| {
                nuono_app::localization::LocaleService::new(Arc::new(self.db.clone()))
            });
        let locale = crate::localization::resolve_event_or_default(&locales, &event, None).await;
        self.handle_event_with_context(bot_id, event_json, bot_sender, action_sink, None, locale)
            .await
    }

    /// 运行时管线入口，语言上下文与平台原生身份已在入站入口处解析完毕。
    pub async fn handle_event_with_context(
        &self,
        bot_id: i64,
        event_json: &str,
        bot_sender: Option<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> Result<HandleResult> {
        crate::core::plugin_host::trigger_control::with_message_admission(
            self.handle_event_admitted(bot_id, event_json, bot_sender, action_sink, domain, locale),
        )
        .await
    }

    async fn handle_event_admitted(
        &self,
        bot_id: i64,
        event_json: &str,
        bot_sender: Option<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> Result<HandleResult> {
        if !*self.is_handle.read().await {
            return Ok(HandleResult {
                matched: false,
                blocked: false,
            });
        }

        let event_value: Value = match serde_json::from_str(event_json) {
            Ok(v) => v,
            Err(_) => {
                return Ok(HandleResult {
                    matched: false,
                    blocked: false,
                });
            }
        };

        let user_id = get_identity(&event_value, "user_id")?;
        let group_id = get_identity_opt(&event_value, "group_id")?;
        let self_id = get_identity(&event_value, "self_id")?;
        let raw_text = get_text(&event_value, "raw_message");
        let msg_type = event_value
            .get("message_type")
            .and_then(|m| m.as_str())
            .unwrap_or("private");

        self.stats.record_receive(&bot_id.to_string(), &self_id);

        // 2. 全局黑名单 + Banned 用户检查
        {
            let bl = self.global_blacklist.read().await;
            if bl.contains(&user_id) {
                return Ok(HandleResult {
                    matched: false,
                    blocked: true,
                });
            }
            if let Some(ref gid) = group_id
                && bl.contains(gid)
            {
                return Ok(HandleResult {
                    matched: false,
                    blocked: true,
                });
            }
        }

        // 3. 权限 —— 统一 5 档正序权限体系（Everyone < GroupAdmin < GroupOwner < BotAdmin < BotOwner）
        let actor_permission = self.permission_manager.evaluate(&event_value).await;

        if actor_permission.is_banned() {
            debug!(user = %user_id, "用户已被封禁，跳过处理");
            return Ok(HandleResult {
                matched: false,
                blocked: true,
            });
        }

        // 插件 prefix 直接从原始消息匹配；框架不再定义全局命令起始符。
        let processed_text = raw_text.trim().to_string();
        let message_route = match &self.plugin_host {
            Some(host) => Some(host.message_route(&raw_text).await),
            None => None,
        };

        debug!("🔍 处理消息: raw_text='{}'", raw_text);

        // 6. 触发器匹配 + 双层过滤
        let all_triggers = self.trigger_registry.get_all_triggers();
        trace!("📋 触发器数量: {}", all_triggers.len());
        let mut matches: Vec<TriggerResult> = Vec::new();

        // 群级开关的本轮结论按插件名记一次，避免同一插件的多个触发器重复解析配置。
        let mut group_enablement: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();

        for trigger in &all_triggers {
            if !super::interaction::allows_plugin(trigger.plugin_name.as_deref()) {
                continue;
            }
            // 显式插件入口只归属对应插件；空前缀规则或无归属规则不能抢走消息。
            if let Some(route) = &message_route
                && route.is_explicit()
                && !trigger
                    .plugin_name
                    .as_deref()
                    .is_some_and(|name| route.allows_plugin(name))
            {
                continue;
            }
            if self.trigger_registry.is_trigger_command_disabled(trigger) {
                debug!(trigger = %trigger.pattern, "命令已禁用，跳过 native/legacy 触发器");
                continue;
            }

            let mut plugin_cfg = None;

            if let Some(ref plugin_name) = trigger.plugin_name {
                if let Some(plugin) = self.get_plugin_cfg(plugin_name).await {
                    if !plugin.enabled {
                        debug!(plugin = %plugin_name, "插件已禁用，跳过触发器");
                        continue;
                    }
                    // 群级开关：客户在自己群里关掉的插件，命令同样不该响应。
                    // 私聊（group_id 为 None）没有这一层，直接跟随上面的全局开关。
                    let group_allows = match group_enablement.get(plugin_name.as_str()) {
                        Some(cached) => *cached,
                        None => {
                            let allowed = self
                                .group_enablement_allows(plugin_name, group_id.as_deref())
                                .await;
                            group_enablement.insert(plugin_name.clone(), allowed);
                            allowed
                        }
                    };
                    if !group_allows {
                        debug!(plugin = %plugin_name, "插件在本群已停用，跳过触发器");
                        continue;
                    }
                    match plugin.area {
                        PluginArea::Group if msg_type != "group" => {
                            debug!(plugin = %plugin_name, "插件仅限群聊，跳过");
                            continue;
                        }
                        PluginArea::Direct if msg_type != "private" => {
                            debug!(plugin = %plugin_name, "插件仅限私聊，跳过");
                            continue;
                        }
                        _ => {}
                    }
                    if !plugin.check_permission(actor_permission) {
                        debug!(
                            plugin = %plugin_name,
                            actor = ?actor_permission,
                            required = ?plugin.min_permission,
                            "权限不足，跳过触发器"
                        );
                        continue;
                    }
                    if plugin.black_list.contains(&user_id) {
                        debug!(plugin = %plugin_name, user = %user_id, "用户在黑名单中");
                        continue;
                    }
                    if let Some(ref gid) = group_id
                        && plugin.black_list.contains(gid)
                    {
                        debug!(plugin = %plugin_name, group = %gid, "群组在黑名单中");
                        continue;
                    }
                    if !plugin.white_list.is_empty() {
                        let in_whitelist = plugin.white_list.contains(&user_id)
                            || group_id
                                .as_ref()
                                .map(|g| plugin.white_list.contains(g))
                                .unwrap_or(false);
                        if !in_whitelist {
                            debug!(plugin = %plugin_name, "不在白名单中，跳过");
                            continue;
                        }
                    }

                    plugin_cfg = Some(plugin);
                } else if self.plugin_host.is_some() {
                    debug!(plugin = %plugin_name, "插件运行时配置不存在，拒绝分发触发器");
                    continue;
                }
            }

            let trigger_ref = trigger.clone();
            if let Some(required) = trigger_ref.required_permission
                && !actor_permission.has_permission(required)
            {
                debug!(
                    trigger = %trigger_ref.pattern,
                    actor = ?actor_permission,
                    required = ?required,
                    "命令权限不足，跳过触发器"
                );
                continue;
            }
            let candidate_text = match plugin_cfg.as_deref() {
                Some(plugin) => plugin.message_text(&raw_text),
                None => Some(raw_text.trim()),
            };
            if let Some(candidate_text) = candidate_text {
                if trigger_ref.is_event_command() {
                    // 跨平台 Event 触发器（含 Command 以及 Keyword/Prefix/Suffix/Regex/FullMatch/File/Message）
                    // 直接在规范 Event 上匹配，不经过 OneBot parse_message。
                    let runtime_event = Event::with_domain(&event_value, domain);
                    if let Some(md) = trigger_ref.matches_event(&runtime_event, candidate_text) {
                        matches.push(TriggerResult {
                            trigger: trigger_ref.clone(),
                            match_data: md,
                            command_text: candidate_text.to_string(),
                        });
                    }
                } else {
                    // 基于原始事件克隆，只替换 raw_message 为候选文本。
                    // 保持所有其他字段完整，确保 parse_message 不会失败。
                    let mut test_value = event_value.clone();
                    if let Some(obj) = test_value.as_object_mut() {
                        obj.insert(
                            "raw_message".to_string(),
                            serde_json::Value::String(candidate_text.to_string()),
                        );
                    }

                    let parser_event = onebot_parser_event(&test_value);
                    let test_json = match serde_json::to_string(&parser_event) {
                        Ok(s) => s,
                        Err(e) => {
                            error!(trigger = %trigger_ref.pattern, error = %e, "序列化测试消息失败");
                            continue;
                        }
                    };
                    match parse_message(&test_json) {
                        Ok(msg) => {
                            if let Some(md) = trigger_ref.matches(&msg) {
                                matches.push(TriggerResult {
                                    trigger: trigger_ref.clone(),
                                    match_data: md,
                                    command_text: candidate_text.to_string(),
                                });
                            }
                        }
                        Err(e) => {
                            error!(
                                trigger = %trigger_ref.pattern,
                                error = %e,
                                "触发器匹配时消息解析失败"
                            );
                        }
                    }
                }
            }
        }

        if matches.is_empty() {
            debug!(
                "没有触发器匹配: raw_text='{}', processed_text='{}', triggers_count={}",
                raw_text,
                processed_text,
                all_triggers.len()
            );
            return Ok(HandleResult {
                matched: false,
                blocked: false,
            });
        }

        // 7. 前缀规则 → 规则优先级；相同排序值保持本轮匹配顺序。
        matches.sort_by_key(TriggerResult::priority_key);

        // 8. 顺序执行
        let mut blocked = false;
        for result in matches {
            // 已认领的指令按钮点击不得执行插件重载或停用前旧生命周期快照中的触发规则。
            let _interaction_lifecycle = if super::interaction::is_command() {
                let Some(host) = self.plugin_host.as_ref() else {
                    continue;
                };
                let Some(plugin) = result.trigger.plugin_name.as_deref() else {
                    continue;
                };
                let Some(guard) = host.lock_interaction_command(plugin).await else {
                    blocked = true;
                    break;
                };
                Some(guard)
            } else {
                None
            };
            let mut bot = Bot::new(
                bot_id,
                self.event_bus.clone(),
                self.wait_manager.clone(),
                self.api_client.clone(),
                bot_sender.clone(),
                action_sink.clone(),
            );
            if let Some(plugin_host) = &self.plugin_host {
                bot = bot.with_action_router(plugin_host.action_router());
            }
            if let Some(plugin) = result.trigger.plugin_name.as_deref() {
                bot = bot.with_wait_owner(plugin, result.trigger.required_permission);
            }
            // both 分支都要能 reply：current message 从原始 runtime JSON 提取（字符串
            // IDs 被 MessageContext 转成 i64，仅用于 reply 定位，不影响 Event 处理器 读取）。
            if let Some(ctx) = crate::api::MessageContext::from_onebot_json(&event_value) {
                bot.set_current_message(ctx);
            }

            // 同一插件在同一条消息下命中的多条规则共享同一个触发准入租约；
            // 若被限流拒绝，仍按该规则自身的 block 属性决定是否阻断后续规则，但不计为一次已执行的调用。
            let lease = match (
                self.plugin_host.as_ref(),
                result.trigger.plugin_name.as_deref(),
            ) {
                (Some(host), Some(plugin)) => {
                    use crate::core::plugin_host::trigger_control::Admission;
                    match host
                        .admit_plugin_message(plugin, &Event::with_domain(&event_value, domain))
                    {
                        Admission::Granted(lease) => Some((plugin, lease)),
                        Admission::Denied { reason, notice } => {
                            if notice.claim() {
                                let _ = bot.send(reason.message()).await;
                            }
                            if result.trigger.block {
                                blocked = true;
                                break;
                            }
                            continue;
                        }
                    }
                }
                _ => None,
            };
            self.stats.record_command(
                &bot_id.to_string(),
                &self_id,
                &result.trigger.pattern,
                &user_id,
                group_id.as_deref(),
            );

            // 指令：handler 直接拿到 `Event`（借自原始 runtime `Value`），不经
            // OneBot 数值投影 / 哈希，字符串 ID、route()、raw_event()、raw segments 保真。
            // 接收 Message 的指令：仍走 `parse_message`。两者共享以上同样的门控、排序与 block 语义。
            let legacy_message = if result.trigger.is_event_command() {
                None
            } else {
                let parser_event = onebot_parser_event(&event_value);
                let event_json_str = match serde_json::to_string(&parser_event) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(trigger = %result.trigger.pattern, error = %e, "序列化事件消息失败");
                        continue;
                    }
                };
                match parse_message(&event_json_str) {
                    Ok(message) => Some(message),
                    Err(e) => {
                        error!(bot_id = %bot_id, trigger_type = ?result.trigger.trigger_type,
                            trigger = %result.trigger.pattern, propagation_stopped = result.trigger.block,
                            error = %e, "触发器执行时消息解析失败；仍按 block 配置决定后续传播");
                        continue;
                    }
                }
            };
            let handle = async {
                // 仅统计通过参数解析与准入检查、真正进入执行的回调；
                // 统计仅记录声明的触发模式元数据，不包含运行期消息正文、正则捕获、参数或执行结果。
                if !result.trigger.is_event_command() || result.trigger.event_handler.is_some() {
                    if let (Some(event), Some(plugin)) =
                        (domain, result.trigger.plugin_name.as_deref())
                    {
                        let kind = if result.trigger.origin_command.is_some()
                            || matches!(
                                result.trigger.trigger_type,
                                nuono_plugin::core::TriggerType::Command
                                    | nuono_plugin::core::TriggerType::EventCommand
                            ) {
                            nuono_storage::activity::ActivityKind::Command
                        } else {
                            nuono_storage::activity::ActivityKind::Rule
                        };
                        self.db.record_activity(event, plugin, kind);
                    }
                    record_status_command_entry(
                        &self.db,
                        domain,
                        bot_id,
                        &user_id,
                        &result.trigger,
                    );
                }
                if result.trigger.is_event_command() {
                    match result.trigger.event_handler.as_ref() {
                        Some(handler) => {
                            handler
                                .handle(
                                    &bot,
                                    &Event::with_domain(&event_value, domain)
                                        .with_locale_context(locale)
                                        .with_command_context(
                                            message_route
                                                .as_ref()
                                                .map(|route| route.prefix())
                                                .unwrap_or(""),
                                            &result.command_text,
                                        ),
                                    result.match_data,
                                )
                                .await
                        }
                        None => Err(anyhow::anyhow!(
                            "Command 触发器 {} 缺少 event_handler",
                            result.trigger.pattern
                        )),
                    }
                } else {
                    result
                        .trigger
                        .handler
                        .handle(
                            &bot,
                            legacy_message.as_ref().expect("legacy message parsed"),
                            result.match_data,
                        )
                        .await
                }
            };
            let handle_outcome = match lease {
                Some((plugin, guard)) => {
                    nuono_plugin::plugin::with_invocation_guard(plugin, guard, handle).await
                }
                None => handle.await,
            };

            match handle_outcome {
                Ok(()) => {
                    let propagation = if result.trigger.block {
                        "停止后续处理"
                    } else {
                        "继续后续处理"
                    };
                    debug!(
                        target: "message_flow",
                        bot_id = %bot_id,
                        plugin = result.trigger.plugin_name.as_deref().unwrap_or(""),
                        trigger_type = ?result.trigger.trigger_type,
                        trigger = %result.trigger.pattern,
                        propagation_stopped = result.trigger.block,
                        "触发器处理完成：{:?} {}，{}",
                        result.trigger.trigger_type,
                        result.trigger.pattern,
                        propagation
                    );
                }
                Err(e) => {
                    error!(
                        bot_id = %bot_id,
                        plugin = result.trigger.plugin_name.as_deref().unwrap_or(""),
                        trigger_type = ?result.trigger.trigger_type,
                        trigger = %result.trigger.pattern,
                        propagation_stopped = result.trigger.block,
                        error = %e,
                        "触发器处理失败；仍按 block 配置决定后续传播"
                    );
                }
            }

            if result.trigger.block {
                blocked = true;
                break;
            }
        }

        Ok(HandleResult {
            matched: true,
            blocked,
        })
    }
}

fn record_status_command_entry(
    db: &Database,
    domain: Option<&crate::domain::DomainEvent>,
    bot_id: i64,
    user_id: &str,
    trigger: &Trigger,
) {
    // 并发队列满时直接放弃本次可选的统计写入，绝不阻塞或影响业务回调，同时避免后台任务无限制堆积。
    static PENDING: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let pending = PENDING.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(64)));
    let Ok(permit) = pending.clone().try_acquire_owned() else {
        tracing::warn!("指令统计队列已满，跳过本次元数据写入");
        return;
    };
    let db = db.clone();
    let bot_id = domain
        .map(|domain| domain.bot.account_id.value().as_str().to_owned())
        .unwrap_or_else(|| bot_id.to_string());
    let user_id = user_id.to_owned();
    let command: String = trigger
        .plugin_name
        .as_deref()
        .unwrap_or("framework")
        .chars()
        .chain(std::iter::once(':'))
        .chain(trigger.pattern.chars())
        .take(nuono_storage::status_commands::MAX_STATUS_COMMAND_CHARS)
        .collect();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) =
            nuono_storage::status_commands::record_status_command(&db, &bot_id, &user_id, &command)
                .await
        {
            tracing::warn!(error = %error, "指令统计元数据写入失败");
        }
    });
}

struct TriggerResult {
    trigger: Arc<Trigger>,
    match_data: MatchData,
    command_text: String,
}

impl TriggerResult {
    fn priority_key(&self) -> (bool, i32) {
        (self.trigger.prefix.is_empty(), self.trigger.priority)
    }
}

/// 为旧 OneBot 强类型 parser 构造数值身份副本；原事件仍保留平台原生字符串 ID。
fn onebot_parser_event(event: &Value) -> Value {
    let mut projected = event.clone();
    let Some(object) = projected.as_object_mut() else {
        return projected;
    };

    // 旧版 OneBot 解析器只认识私聊和群聊两种变体；这里将频道（channel）事件临时映射为群聊结构供其解析，而业务处理器仍读取原始频道事件。
    if object.get("message_type").and_then(Value::as_str) == Some("channel") {
        object.insert("message_type".to_string(), serde_json::json!("group"));
        if let Some(channel) = object.get("channel_id").cloned() {
            object.insert("group_id".to_string(), channel);
        }
    }

    let native_self_id = object.get("self_id").cloned();
    let projected_self_id = native_self_id
        .as_ref()
        .and_then(|value| legacy_parser_i64("bot", value));
    if let Some(id) = projected_self_id {
        object.insert("self_id".to_string(), serde_json::json!(id));
    }
    for (key, scope) in [("user_id", "actor"), ("group_id", "group")] {
        if let Some(value) = object.get(key).cloned()
            && let Some(id) = legacy_parser_i64(scope, &value)
        {
            object.insert(key.to_string(), serde_json::json!(id));
        }
    }
    if let Some(value) = object.get("message_id").cloned()
        && let Some(id) = legacy_parser_i32("message", &value)
    {
        object.insert("message_id".to_string(), serde_json::json!(id));
    }
    if let Some(sender) = object.get_mut("sender").and_then(Value::as_object_mut)
        && let Some(value) = sender.get("user_id").cloned()
        && let Some(id) = legacy_parser_i64("actor", &value)
    {
        sender.insert("user_id".to_string(), serde_json::json!(id));
    }
    if let (Some(native_self_id), Some(projected_self_id), Some(segments)) = (
        native_self_id.as_ref().and_then(identity_value),
        projected_self_id,
        object.get_mut("message").and_then(Value::as_array_mut),
    ) {
        for segment in segments {
            let Some(qq) = segment
                .get_mut("data")
                .and_then(Value::as_object_mut)
                .and_then(|data| data.get_mut("qq"))
            else {
                continue;
            };
            if identity_value(qq).as_deref() == Some(native_self_id.as_str()) {
                *qq = serde_json::json!(projected_self_id.to_string());
            }
        }
    }

    projected
}

fn identity_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn legacy_parser_i64(scope: &str, value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| Some(stable_legacy_id(scope, &number.to_string()))),
        Value::String(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| Some(stable_legacy_id(scope, value.trim()))),
        _ => None,
    }
}

fn legacy_parser_i32(scope: &str, value: &Value) -> Option<i32> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .or_else(|| Some(stable_legacy_id(scope, &number.to_string()) as i32)),
        Value::String(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<i32>()
            .ok()
            .or_else(|| Some(stable_legacy_id(scope, value.trim()) as i32)),
        _ => None,
    }
}

fn stable_legacy_id(scope: &str, value: &str) -> i64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in scope.bytes().chain(std::iter::once(0)).chain(value.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let id = hash % i32::MAX as u64;
    id.max(1) as i64
}

fn get_text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn get_identity(v: &Value, key: &str) -> Result<String> {
    get_identity_opt(v, key)?
        .ok_or_else(|| NuonoError::EventProcessing(format!("OneBot 事件缺少必需身份字段 {key}")))
}

fn get_identity_opt(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::Number(value)) => Ok(Some(value.to_string())),
        Some(Value::String(_)) => Err(NuonoError::EventProcessing(format!(
            "OneBot 身份字段 {key} 不能为空"
        ))),
        Some(_) => Err(NuonoError::EventProcessing(format!(
            "OneBot 身份字段 {key} 必须是 number 或 string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginConfigManager;
    use crate::core::command::PermissionLevel;
    use crate::core::plugin_service::PluginArea;
    use crate::core::trigger::TriggerType;
    use sea_orm::ConnectionTrait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingTriggerHandler {
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::core::trigger::TriggerHandler for CountingTriggerHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            _event: &crate::onebot::Message,
            _match_data: MatchData,
        ) -> anyhow::Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct TriggerTestPlugin(String);

    #[async_trait::async_trait]
    impl crate::plugin::Plugin for TriggerTestPlugin {
        fn name(&self) -> &str {
            &self.0
        }

        fn version(&self) -> &str {
            "1.0.0"
        }

        fn description(&self) -> &str {
            "群开关触发器测试插件"
        }

        fn service_config(&self) -> Plugins {
            let mut config = Plugins::new(&self.0).with_area(PluginArea::All);
            config.allow_empty_prefix = true;
            config
        }
    }

    struct PathBHarness {
        handler: EventHandler,
        host: Arc<crate::core::plugin_host::PluginHost>,
        db: Database,
        root: std::path::PathBuf,
        count: Arc<AtomicUsize>,
        tag: String,
        /// 逐用例唯一的插件名。
        plugin: String,
    }

    impl PathBHarness {
        /// path B 走的是完整的 `handle_event`，所以必须搭出真实的 handler + 宿主组合，
        /// 而不是直接调过滤块 —— 那个块没有独立入口，绕过它就等于没测。
        async fn new(name: &str) -> Self {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let root = std::env::temp_dir().join(format!("nuono-pathb-{}-{}", name, id));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let db = crate::test_support::test_db(2).await;

            let event_bus = Arc::new(EventBus::new());
            let permission_manager = Arc::new(PermissionManager::new(db.clone()));
            let wait_manager = Arc::new(crate::api::WaitManager::new());
            let host = crate::core::plugin_host::PluginHost::new(
                db.clone(),
                event_bus.clone(),
                permission_manager.clone(),
                crate::core::paths::Paths::with_root(&root),
                wait_manager.clone(),
                crate::core::FrameworkControl::new(),
                Arc::new(PluginConfigManager::with_database(
                    root.join("plugin-config"),
                    db.clone(),
                )),
            )
            .await
            .unwrap();

            let plugin = format!("TriggerGated{}", &id[..8]);
            host.init_with_plugins(vec![crate::plugin::LoadedPlugin {
                plugin: Box::new(TriggerTestPlugin(plugin.clone())),
            }])
            .await
            .unwrap();
            let mut cfg = Plugins::new(&plugin);
            cfg.enabled = true;
            cfg.area = PluginArea::All;
            cfg.allow_empty_prefix = true;
            host.register_plugin_config(cfg).await;

            let count = Arc::new(AtomicUsize::new(0));
            let mut trigger = Trigger::new(
                TriggerType::FullMatch,
                "ping",
                "",
                true,
                false,
                Arc::new(CountingTriggerHandler {
                    count: count.clone(),
                }),
            );
            trigger.plugin_name = Some(plugin.clone());
            host.trigger_registry().register(trigger);

            let mut handler = EventHandler::new(
                db.clone(),
                CommandDispatchServices {
                    trigger_registry: host.trigger_registry(),
                    cooldown: host.cooldown_tracker.clone(),
                    stats: host.command_stats.clone(),
                },
                permission_manager,
                event_bus,
                wait_manager,
                Arc::new(crate::core::ApiClient::new()),
            );
            handler.set_plugin_host(host.clone());

            Self {
                handler,
                host,
                db,
                root,
                count,
                tag: format!("g{}", &id[..8]),
                plugin,
            }
        }

        fn group(&self, suffix: &str) -> String {
            format!("{}-{}", self.tag, suffix)
        }

        async fn matched_for(&self, group_id: Option<&str>) -> bool {
            let event = json!({
                "time": 1,
                "post_type": "message",
                "message_type": if group_id.is_some() { "group" } else { "private" },
                "sub_type": "normal",
                "self_id": 42,
                "user_id": 7,
                "group_id": group_id,
                "message_id": 1,
                "message": [{"type": "text", "data": {"text": "ping"}}],
                "raw_message": "ping",
                "sender": {"user_id": 7, "nickname": "tester"}
            });
            self.host
                .test_group_snapshot(
                    group_id,
                    self.handler
                        .handle_event(42, &event.to_string(), None, None),
                )
                .await
                .unwrap()
                .matched
        }

        async fn set_group_enabled(&self, group_id: &str, enabled: bool) {
            self.host
                .test_group_patch(
                    group_id,
                    &self.plugin,
                    Some(enabled),
                    serde_json::Map::new(),
                )
                .await
                .unwrap();
        }

        async fn cleanup(self) {
            let Self { host, db, root, .. } = self;
            drop(host);
            let _ = db.shutdown().await;
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    /// 2. path B：群 X 关掉 → 触发器不匹配；群 Y → 照常执行。
    ///
    /// 与 path A 分开测：两条路径各有一段自己的插件级过滤，只补一边不会被另一边的用例发现。
    #[tokio::test]
    async fn trigger_path_respects_group_override() {
        let h = PathBHarness::new("scoped").await;
        let blocked = h.group("blocked");
        let other = h.group("other");
        h.set_group_enabled(&blocked, false).await;

        assert!(
            !h.matched_for(Some(&blocked)).await,
            "本群已关，命令不该响应"
        );
        assert_eq!(h.count.load(Ordering::SeqCst), 0);

        assert!(h.matched_for(Some(&other)).await, "别的群必须照常响应");
        assert_eq!(h.count.load(Ordering::SeqCst), 1);

        h.cleanup().await;
    }

    /// 3. path B 的私聊：没有群号，跟随全局。
    #[tokio::test]
    async fn trigger_path_private_message_follows_global() {
        let h = PathBHarness::new("private").await;
        h.set_group_enabled(&h.group("x"), false).await;

        assert!(h.matched_for(None).await, "私聊无群号，应当跟随全局启用");
        assert_eq!(h.count.load(Ordering::SeqCst), 1);

        h.cleanup().await;
    }

    /// 4. path B 的非数字群号。
    #[tokio::test]
    async fn trigger_path_handles_non_numeric_group_id() {
        let h = PathBHarness::new("textid").await;
        let feishu = format!("oc_chat_{}", h.tag);
        h.set_group_enabled(&feishu, false).await;

        assert!(
            !h.matched_for(Some(&feishu)).await,
            "飞书群号必须原样命中覆盖，不能被 stable_legacy_id 折成数字"
        );
        assert!(h.matched_for(Some(&h.group("plain"))).await);

        h.cleanup().await;
    }

    /// 5. path B：全局停用压住群级开启。
    #[tokio::test]
    async fn trigger_path_global_disable_beats_group_enable() {
        let h = PathBHarness::new("global").await;
        let group = h.group("eager");
        h.set_group_enabled(&group, true).await;

        crate::service::plugins::set_plugin_enabled(&h.db, &h.plugin, false)
            .await
            .unwrap();
        let mut cfg = Plugins::new(&h.plugin);
        cfg.enabled = false;
        cfg.area = PluginArea::All;
        h.host.register_plugin_config(cfg).await;

        assert!(
            !h.matched_for(Some(&group)).await,
            "全局停用必须压住群级开启"
        );
        assert_eq!(h.count.load(Ordering::SeqCst), 0);

        h.cleanup().await;
    }

    /// 6. path B：解析出错 → 失败即拒绝，且 `handle_event` 仍正常返回。
    #[tokio::test]
    async fn trigger_path_resolution_error_fails_closed() {
        let h = PathBHarness::new("dberr").await;
        let group = h.group("broken");
        // 测试桩：通过显式 DDL 删除 `plugins` 表，模拟查询该表失败的场景。
        h.db.orm()
            .execute_raw(sea_orm::Statement::from_string(
                h.db.backend(),
                "ALTER TABLE nuono_plugins RENAME COLUMN config_json TO unavailable_config_json",
            ))
            .await
            .unwrap();

        assert!(
            !h.matched_for(Some(&group)).await,
            "解析失败时不该放行触发器"
        );
        assert_eq!(h.count.load(Ordering::SeqCst), 0);

        h.cleanup().await;
    }

    #[test]
    fn onebot_identity_fields_accept_numbers_and_strings() {
        let event = json!({
            "user_id": 10001,
            "self_id": "20002",
            "group_id": 30003,
        });

        assert_eq!(get_identity(&event, "user_id").unwrap(), "10001");
        assert_eq!(get_identity(&event, "self_id").unwrap(), "20002");
        assert_eq!(
            get_identity_opt(&event, "group_id").unwrap().as_deref(),
            Some("30003")
        );
    }

    #[test]
    fn parser_projection_converts_native_ids_without_mutating_source() {
        let event = json!({
            "self_id": "bot-app-id",
            "user_id": "user-openid",
            "group_id": "group-openid",
            "message_id": "message-native-id",
            "sender": {"user_id": "user-openid"},
            "message": [{"type": "at", "data": {"qq": "bot-app-id"}}]
        });

        let projected = onebot_parser_event(&event);
        for key in ["self_id", "user_id", "group_id", "message_id"] {
            assert!(projected[key].is_number(), "{key} was not projected");
        }
        assert_eq!(projected["sender"]["user_id"], projected["user_id"]);
        assert_eq!(
            projected["message"][0]["data"]["qq"],
            projected["self_id"].as_i64().unwrap().to_string()
        );
        assert_eq!(event["user_id"], "user-openid");
        assert_eq!(event["group_id"], "group-openid");
        assert_eq!(event["message"][0]["data"]["qq"], "bot-app-id");
        assert_eq!(onebot_parser_event(&event)["user_id"], projected["user_id"]);
    }

    #[test]
    fn parser_projection_maps_channels_only_in_the_legacy_matcher_view() {
        let event = json!({
            "post_type":"message", "message_type":"channel", "sub_type":"normal",
            "self_id":"42", "user_id":"7", "message_id":"click", "sender":{"user_id":"7"},
            "channel_id":"18446744073709551000", "guild_id":"18446744073709551001",
            "raw_message":"ping", "message":[{"type":"text","data":{"text":"ping"}}],
            "route":{"platform":"discord","instance_id":"fixture","bot_self_id":"42"}
        });
        let projected = onebot_parser_event(&event);
        assert_eq!(projected["message_type"], "group");
        assert!(projected["group_id"].is_number());
        assert!(parse_message(&projected.to_string()).is_ok());
        assert_eq!(event["message_type"], "channel");
        assert!(event.get("group_id").is_none());
        assert_eq!(projected["channel_id"], event["channel_id"]);
        assert_eq!(projected["guild_id"], event["guild_id"]);
        assert_eq!(projected["route"], event["route"]);
    }

    #[test]
    fn onebot_identity_fields_do_not_silently_become_empty() {
        let missing = json!({});
        let empty = json!({"user_id": ""});
        let invalid = json!({"group_id": []});

        assert!(get_identity(&missing, "user_id").is_err());
        assert!(get_identity(&empty, "user_id").is_err());
        assert!(get_identity_opt(&invalid, "group_id").is_err());
        assert_eq!(get_identity_opt(&missing, "group_id").unwrap(), None);
    }

    /// 指令分发 harness：与 PathBHarness 同一套真实 host+handler，但登记 native
    /// 命令并捕获 handler 实际收到的 `Event`（clone 出来断言字符串 ID / route / raw 保真）。
    struct NativeHarness {
        handler: EventHandler,
        host: Arc<crate::core::plugin_host::PluginHost>,
        db: Database,
        manager: Arc<PluginConfigManager>,
        root: std::path::PathBuf,
        plugin: String,
        /// 每次 register_native 创建独立的捕获 handler。
        natives: std::sync::Mutex<Vec<(String, Arc<CaptureNativeHandler>)>>,
        order: Arc<std::sync::Mutex<Vec<String>>>,
        legacy_count: Arc<AtomicUsize>,
    }

    struct CaptureNativeHandler {
        label: String,
        order: Arc<std::sync::Mutex<Vec<String>>>,
        captured: std::sync::Mutex<Option<Value>>,
        args: std::sync::Mutex<Option<String>>,
        command_context: std::sync::Mutex<Option<(String, String)>>,
        err: bool,
    }

    #[async_trait::async_trait]
    impl crate::core::command::CommandHandler for CaptureNativeHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            event: &Event<'_>,
            match_data: MatchData,
        ) -> anyhow::Result<()> {
            self.order.lock().unwrap().push(self.label.clone());
            *self.captured.lock().unwrap() = Some(event.raw().clone());
            *self.args.lock().unwrap() = Some(match_data.args);
            *self.command_context.lock().unwrap() = Some((
                event.command_prefix().to_owned(),
                event.command_text().to_owned(),
            ));
            if self.err {
                anyhow::bail!("handler error");
            }
            Ok(())
        }
    }

    impl NativeHarness {
        async fn new(name: &str) -> Self {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let root = std::env::temp_dir().join(format!("nuono-native-{}-{}", name, id));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let db = crate::test_support::test_db(2).await;

            let manager = Arc::new(PluginConfigManager::with_database(
                root.join("plugin-config"),
                db.clone(),
            ));
            let event_bus = Arc::new(EventBus::new());
            let permission_manager = Arc::new(PermissionManager::new(db.clone()));
            let wait_manager = Arc::new(crate::api::WaitManager::new());
            let host = crate::core::plugin_host::PluginHost::new(
                db.clone(),
                event_bus.clone(),
                permission_manager.clone(),
                crate::core::paths::Paths::with_root(&root),
                wait_manager.clone(),
                crate::core::FrameworkControl::new(),
                manager.clone(),
            )
            .await
            .unwrap();

            let plugin = format!("Native{}", &id[..8]);
            db.sync_plugin(&plugin, "1.0.0", &serde_json::json!({}))
                .await
                .unwrap();
            manager
                .register_with_service_config(
                    &plugin,
                    Default::default(),
                    Some(&Plugins::new(&plugin)),
                )
                .await
                .unwrap();
            host.register_plugin_config(Plugins::new(&plugin)).await;
            // 这些测试桩需要连续发送消息验证路由与触发匹配；触发准入限流已有专门的宿主集成测试覆盖，此处放开限流以免干扰捕获断言。
            host.set_unlimited_trigger_policy_for_test(&plugin);

            let mut handler = EventHandler::new(
                db.clone(),
                CommandDispatchServices {
                    trigger_registry: host.trigger_registry(),
                    cooldown: host.cooldown_tracker.clone(),
                    stats: host.command_stats.clone(),
                },
                permission_manager,
                event_bus,
                wait_manager,
                Arc::new(crate::core::ApiClient::new()),
            );
            handler.set_plugin_host(host.clone());

            Self {
                handler,
                host,
                db,
                manager,
                root,
                plugin,
                natives: Default::default(),
                order: Arc::new(std::sync::Mutex::new(Vec::new())),
                legacy_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// 注册本插件的运行时配置（决定 area/pm/黑白名单/前缀 门控）。
        async fn set_plugin_cfg(&self, cfg: impl FnOnce(Plugins) -> Plugins) {
            let mut c = Plugins::new(&self.plugin);
            c.enabled = true;
            c.area = PluginArea::All;
            c.allow_empty_prefix = true;
            let c = cfg(c);
            self.manager
                .register_with_service_config(&self.plugin, Default::default(), Some(&c))
                .await
                .unwrap();
            self.host.register_plugin_config(c).await;
        }

        fn register_native(&self, pattern: &str, block: bool, err: bool) {
            self.register_native_perm(pattern, block, err, PermissionLevel::Everyone)
        }

        /// 登记带权限要求的 接收 Event 的指令（`register_native` 默认 Everyone）。
        fn register_native_perm(
            &self,
            pattern: &str,
            block: bool,
            err: bool,
            permission: PermissionLevel,
        ) {
            let handler = Arc::new(CaptureNativeHandler {
                label: pattern.to_string(),
                order: self.order.clone(),
                captured: std::sync::Mutex::new(None),
                args: std::sync::Mutex::new(None),
                command_context: Default::default(),
                err,
            });
            let cmd = crate::core::command::Command::new(pattern, handler.clone())
                .with_block(block)
                .with_priority(2)
                .with_plugin(&self.plugin)
                .with_permission(permission);
            self.host.trigger_registry().register_command(cmd);
            self.natives
                .lock()
                .unwrap()
                .push((pattern.to_string(), handler));
        }

        fn register_message_legacy(&self, block: bool, priority: i32) {
            let count = self.legacy_count.clone();
            let order = self.order.clone();
            let handler = std::sync::Arc::new(OrderMessageHandler { count, order });
            let mut trigger = Trigger::new(TriggerType::Message, "", "", block, false, handler);
            trigger.priority = priority;
            trigger.plugin_name = Some(self.plugin.clone());
            self.host.trigger_registry().register(trigger);
        }

        fn capture(&self, pattern: &str) -> Option<Value> {
            let natives = self.natives.lock().unwrap();
            natives
                .iter()
                .find(|(p, _)| p == pattern)
                .and_then(|(_, h)| h.captured.lock().unwrap().clone())
        }

        fn args(&self, pattern: &str) -> Option<String> {
            let natives = self.natives.lock().unwrap();
            natives
                .iter()
                .find(|(p, _)| p == pattern)
                .and_then(|(_, h)| h.args.lock().unwrap().clone())
        }

        async fn dispatch(&self, event: &Value) -> HandleResult {
            // 每次分发前清空上一条事件留下的捕获，避免跨事件读到「未命中但历史命中」的
            // 陈旧值（Event 处理器 是注册期创建的常驻 Arc，捕获字段跨 dispatch 累计）。
            self.clear_captures();
            let group = Event::new(event).group_id_str();
            self.host
                .test_group_snapshot(
                    group.as_deref(),
                    self.handler
                        .handle_event(42, &event.to_string(), None, None),
                )
                .await
                .unwrap()
        }

        fn clear_captures(&self) {
            let natives = self.natives.lock().unwrap();
            for (_, h) in natives.iter() {
                *h.captured.lock().unwrap() = None;
                *h.args.lock().unwrap() = None;
                *h.command_context.lock().unwrap() = None;
            }
        }

        async fn cleanup(self) {
            let Self { host, db, root, .. } = self;
            drop(host);
            let _ = db.shutdown().await;
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    struct OrderMessageHandler {
        count: Arc<AtomicUsize>,
        order: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::core::trigger::TriggerHandler for OrderMessageHandler {
        async fn handle(
            &self,
            _bot: &Bot,
            _event: &crate::onebot::Message,
            _data: MatchData,
        ) -> anyhow::Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.order.lock().unwrap().push("message".to_string());
            Ok(())
        }
    }

    fn native_private(
        platform: &str,
        self_id: &str,
        user_id: &str,
        message_id: &str,
        text: &str,
    ) -> Value {
        json!({
            "platform": platform,
            "post_type": "message",
            "message_type": "private",
            "sub_type": "friend",
            "self_id": self_id,
            "user_id": user_id,
            "message_id": message_id,
            "raw_message": text,
            "message": [{ "type": "text", "data": { "text": text } }],
            "sender": { "user_id": user_id, "nickname": "tester", "role": "member" },
            "route": { "platform": platform, "instance_id": platform, "bot_self_id": self_id },
            "raw": { "event_id": message_id, "platform": platform },
        })
    }

    /// 接收 Event 的指令的 handler 直接拿到原始 runtime JSON：Discord snowflake（远超 i64）、
    /// QQ 数字串、飞书 oc_ openid 都保持字符串，route/raw 也能直接读，不被 OneBot 投影折叠。
    #[tokio::test]
    async fn native_dispatch_preserves_string_ids_route_raw() {
        let h = NativeHarness::new("ids").await;

        // > i64 的 Discord snowflake：数值投影会折成 0 / 哈希，native 必须保留原字符串。
        let snowflake = "4358232439484032348234234";
        let event = native_private(
            "discord",
            "bot-app-id",
            snowflake,
            "msg-snowflake-1",
            "ping",
        );
        h.register_native("ping", true, false);
        let result = h.dispatch(&event).await;
        assert!(result.matched);

        let captured = h.capture("ping").unwrap();
        assert_eq!(captured["platform"], "discord");
        assert_eq!(captured["user_id"], snowflake);
        assert_eq!(captured["message_id"], "msg-snowflake-1");
        assert_eq!(captured["raw_message"], "ping");
        assert_eq!(captured["route"]["bot_self_id"], "bot-app-id");
        assert_eq!(captured["raw"]["event_id"], "msg-snowflake-1");
        assert_eq!(
            Event::new(&captured).user_id_str().as_deref(),
            Some(snowflake)
        );
        assert_eq!(
            Event::new(&captured).message_id_str().as_deref(),
            Some("msg-snowflake-1")
        );
        assert_eq!(
            Event::new(&captured).route().unwrap().bot_self_id,
            "bot-app-id"
        );
        h.cleanup().await;
    }

    #[tokio::test]
    async fn activity_counts_executed_commands_but_not_unmatched_or_disabled_commands() {
        use crate::core::platform::PlatformAdapter;
        let h = NativeHarness::new("activity-admission").await;
        h.register_native("ping", true, false);
        for (text, enabled, matched) in [
            ("ping", true, true),
            ("unknown", true, false),
            ("ping", false, false),
        ] {
            h.set_plugin_cfg(|mut config| {
                config.enabled = enabled;
                config
            })
            .await;
            let mut raw = native_private("onebot", "42", "7", "1", text);
            raw["self_id"] = json!(42);
            raw["user_id"] = json!(7);
            let incoming = nuono_adapter_fixture::FixtureAdapter
                .parse_incoming(&raw.to_string())
                .unwrap();
            let bot = crate::core::runtime_transport::route_identity(
                &crate::core::RuntimeBotRoute::new("onebot", "42", "42"),
            )
            .unwrap();
            let domain = crate::domain::DomainEventNormalizer::new(bot)
                .normalize(&incoming, crate::clock::db_now())
                .unwrap();
            let result = h
                .handler
                .handle_event_with_context(
                    42,
                    &raw.to_string(),
                    None,
                    None,
                    Some(&domain),
                    Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(result.matched, matched);
        }
        let report = h.db.activity_report(Default::default()).await.unwrap();
        assert_eq!(report.totals.commands, 1);
        assert_eq!(report.totals.plugin_calls, 1);
        assert_eq!(report.plugins[0].label, h.plugin);
        assert_eq!(report.users[0].label, "7");
        // 入站消息总计数由管线入口统一负责，指令派发器内部不应重复累加消息数。
        assert_eq!(report.totals.messages, 0);
        h.cleanup().await;
    }

    /// Event 处理器 读到的 raw message segments 原样保留：reply/at 这类自定义消息段里的
    /// 字符串 ID（reply id、at 目标）不参与 OneBot 数值投影 / 哈希，`Event::message()`
    /// 返回的就是原始 JSON 段数组。
    #[tokio::test]
    async fn native_dispatch_preserves_raw_custom_segments() {
        let h = NativeHarness::new("segments").await;
        let reply_id = "reply-snowflake-88293";
        let at_target = "oc_at_openid_fei_shu_0001";

        let event = json!({
            "platform": "feishu",
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "self_id": "bot-app-id",
            "user_id": "oc_user_openid_0001",
            "group_id": "oc_group_openid_0001",
            "message_id": "msg-openid-1",
            "raw_message": "echo",
            "message": [
                { "type": "reply", "data": { "id": reply_id } },
                { "type": "at", "data": { "qq": at_target } },
                { "type": "text", "data": { "text": "echo" } },
            ],
            "sender": { "user_id": "oc_user_openid_0001", "nickname": "测试", "role": "member" },
            "route": {
                "platform": "feishu",
                "instance_id": "feishu",
                "bot_self_id": "bot-app-id",
            },
            "raw": { "event_id": "msg-openid-1", "platform": "feishu" },
        });

        h.register_native("echo", true, false);
        assert!(h.dispatch(&event).await.matched);

        let captured = h.capture("echo").unwrap();
        let event = Event::new(&captured);
        let segments = event.message().unwrap();
        // 三段原始消息段保真：reply.id / at.qq 仍是原字符串（未被折成 0 或哈希）。
        assert_eq!(segments[0]["type"], "reply");
        assert_eq!(segments[0]["data"]["id"], reply_id);
        assert_eq!(segments[1]["type"], "at");
        assert_eq!(segments[1]["data"]["qq"], at_target);
        assert_eq!(segments[2]["type"], "text");
        h.cleanup().await;
    }

    /// 无全局命令前缀；alias 和命令边界由插件自己的 prefix 语义决定。
    #[tokio::test]
    async fn command_alias_and_boundary() {
        let h = NativeHarness::new("boundary").await;

        h.register_native("ping", true, false);
        // alias 用 register_native 单独登记同 plugin，pattern=p，与 ping 共用捕获。
        h.register_native("p", true, false);

        // 未加前缀的功能命令直接匹配 pattern。
        assert!(
            h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
                .await
                .matched
        );
        assert_eq!(h.args("ping").as_deref(), Some(""));

        // 边界：/pingX 不在 ping 的边界内，不应命中。
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-2", "pingX"))
                .await
                .matched
        );
        assert!(h.capture("ping").is_none());

        // alias "p" 命中并保留 args="100"。
        assert!(
            h.dispatch(&native_private("qq", "bot", "u-1", "m-3", "p 100"))
                .await
                .matched
        );
        assert_eq!(h.args("p").as_deref(), Some("100"));

        h.cleanup().await;
    }

    #[tokio::test]
    async fn plugin_prefix_is_the_only_command_prefix() {
        let h = NativeHarness::new("plugin-prefix").await;
        h.set_plugin_cfg(|mut config| {
            config.prefix = vec!["ww".to_string()];
            config.allow_empty_prefix = false;
            config
        })
        .await;
        h.register_native("查询", true, false);

        assert!(
            h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "ww查询 123"))
                .await
                .matched
        );
        assert_eq!(h.args("查询").as_deref(), Some("123"));
        assert!(
            h.dispatch(&native_private("qq", "bot", "u-1", "m-2", "ww 查询 123"))
                .await
                .matched
        );
        assert_eq!(h.args("查询").as_deref(), Some("123"));
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-3", "/ww查询 123"))
                .await
                .matched
        );
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-4", "/查询 123"))
                .await
                .matched
        );
        h.cleanup().await;
    }

    #[tokio::test]
    async fn every_rule_uses_the_plugin_gate_and_passes_routed_body_with_raw_event() {
        let h = NativeHarness::new("all-rule-prefixes").await;
        h.set_plugin_cfg(|mut config| {
            config.prefix = vec!["ww".into()];
            // 即使历史配置中开启了 allow_empty_prefix，也不会恢复旧版正则/消息触发器绕过前缀限制的行为。
            config.allow_empty_prefix = true;
            config
        })
        .await;
        for (kind, pattern) in [
            (TriggerType::Prefix, "查询"),
            (TriggerType::Suffix, "AbC"),
            (TriggerType::Keyword, "查询"),
            (TriggerType::FullMatch, "查询 AbC"),
            (TriggerType::Regex, "^查询 (?P<token>.+)$"),
            (TriggerType::Message, ""),
            (TriggerType::File, "json"),
        ] {
            let capture = Arc::new(CaptureNativeHandler {
                label: format!("{kind:?}"),
                order: h.order.clone(),
                captured: Default::default(),
                args: Default::default(),
                command_context: Default::default(),
                err: false,
            });
            h.host.trigger_registry().register(
                Trigger::new_event(kind, pattern, false, false, capture.clone())
                    .with_plugin_name(&h.plugin),
            );
            for (text, expected) in [
                ("查询 AbC", false),
                ("url查询 AbC", false),
                ("WW查询 AbC", true),
            ] {
                let mut event = native_private("qq", "bot", "u-1", "m-1", text);
                event["message"] = json!([
                    {"type":"text", "data":{"text":text}},
                    {"type":"file", "data":{"file":"native-file-id", "name":"records.JSON"}}
                ]);
                assert_eq!(
                    h.dispatch(&event).await.matched,
                    expected,
                    "{kind:?}: {text}"
                );
                if expected {
                    assert_eq!(*capture.captured.lock().unwrap(), Some(event));
                    assert_eq!(
                        *capture.command_context.lock().unwrap(),
                        Some(("WW".into(), "查询 AbC".into()))
                    );
                } else {
                    assert!(capture.captured.lock().unwrap().is_none());
                }
            }
            h.host.trigger_registry().unregister_by_plugin(&h.plugin);
        }
        h.cleanup().await;
    }

    #[tokio::test]
    async fn message_rules_still_require_permission_and_at_me() {
        let h = NativeHarness::new("message-rule-gates").await;
        h.set_plugin_cfg(|config| config.with_prefix(vec!["ww".into()]))
            .await;
        let capture = Arc::new(CaptureNativeHandler {
            label: "message".into(),
            order: h.order.clone(),
            captured: Default::default(),
            args: Default::default(),
            command_context: Default::default(),
            err: false,
        });
        let mut rule = Trigger::new_event(TriggerType::Message, "", false, false, capture.clone())
            .with_plugin_name(&h.plugin);
        rule.required_permission = Some(PermissionLevel::BotAdmin);
        h.host.trigger_registry().register(rule);
        let event = native_private("qq", "bot", "u-1", "m-1", "ww测试");
        assert!(!h.dispatch(&event).await.matched);
        h.host.trigger_registry().unregister_by_plugin(&h.plugin);
        h.host.trigger_registry().register(
            Trigger::new_event(TriggerType::Message, "", false, true, capture)
                .with_plugin_name(&h.plugin),
        );
        let mut group = event;
        group["message_type"] = json!("group");
        group["group_id"] = json!("g-1");
        assert!(!h.dispatch(&group).await.matched);
        group["message"] = json!([
            {"type":"at", "data":{"qq":"bot"}},
            {"type":"text", "data":{"text":"ww测试"}}
        ]);
        assert!(h.dispatch(&group).await.matched);
        h.cleanup().await;
    }

    #[tokio::test]
    async fn explicit_namespace_excludes_other_plugins_even_with_higher_rule_priority() {
        let h = NativeHarness::new("namespace-isolation").await;
        h.set_plugin_cfg(|config| config.with_prefix(vec!["url".into()]))
            .await;
        h.register_native("QQ音乐登录", false, false);
        let count = Arc::new(AtomicUsize::new(0));
        for (name, prefixes) in [("Ambient", vec![]), ("Short", vec!["u".into()])] {
            h.host
                .register_plugin_config(Plugins::new(name).with_prefix(prefixes))
                .await;
            h.host.trigger_registry().register(
                Trigger::new(
                    TriggerType::Message,
                    "",
                    "",
                    true,
                    false,
                    Arc::new(CountingTriggerHandler {
                        count: count.clone(),
                    }),
                )
                .with_plugin_name(name)
                .with_priority(-100),
            );
        }
        let result = h
            .dispatch(&native_private(
                "qq",
                "bot",
                "u-1",
                "m-1",
                "URLqq音乐登录 TokenAbC",
            ))
            .await;
        assert!(result.matched);
        assert!(!result.blocked);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(h.args("QQ音乐登录").as_deref(), Some("TokenAbC"));
        h.host
            .register_plugin_config(Plugins::new("Conflict").with_prefix(vec!["URL".into()]))
            .await;
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-2", "urlQQ音乐登录"))
                .await
                .matched
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        h.cleanup().await;
    }

    /// Dashboard/API 或配置禁用命令后，EventHandler 的生产分发路径必须跳过 Event 处理器。
    #[tokio::test]
    async fn command_toggle_disables_event_handler_dispatch() {
        let h = NativeHarness::new("toggle").await;
        h.register_native("ping", true, false);
        assert!(h.host.trigger_registry().toggle_command_scoped(
            Some(&h.plugin),
            None,
            "ping",
            false,
        ));

        let result = h
            .dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
            .await;

        assert!(
            !result.matched,
            "被禁用的 接收 Event 的指令不应进入匹配结果"
        );
        assert!(h.capture("ping").is_none(), "被禁用的 handler 不应执行");
        h.cleanup().await;
    }

    #[tokio::test]
    async fn native_prefix_and_regex_preserve_event_and_obey_command_toggle() {
        let h = NativeHarness::new("native-match-type").await;
        h.set_plugin_cfg(|mut config| {
            config.prefix = vec!["ww".into()];
            config.allow_empty_prefix = false;
            config
        })
        .await;
        for (kind, pattern, text) in [
            (TriggerType::Prefix, "设置阈值", "ww设置阈值235"),
            (TriggerType::Regex, "^(?P<name>.+)面板$", "ww今汐面板"),
        ] {
            let handler = Arc::new(CaptureNativeHandler {
                label: pattern.into(),
                order: h.order.clone(),
                captured: Default::default(),
                args: Default::default(),
                command_context: Default::default(),
                err: false,
            });
            let command = crate::core::command::Command::new(pattern, handler.clone())
                .with_match_type(kind)
                .with_plugin(&h.plugin);
            h.host.trigger_registry().register_command(command);
            h.natives.lock().unwrap().push((pattern.into(), handler));
            let event = native_private(
                "discord",
                "bot-native",
                "4358232439484032348234234",
                "message-native",
                text,
            );
            assert!(h.dispatch(&event).await.matched);
            assert_eq!(h.capture(pattern).unwrap(), event);
            if kind == TriggerType::Prefix {
                assert_eq!(h.args(pattern).as_deref(), Some("235"));
            }
            assert!(h.host.trigger_registry().toggle_command_scoped(
                Some(&h.plugin),
                None,
                pattern,
                false
            ));
            assert!(!h.dispatch(&event).await.matched);
            assert!(h.capture(pattern).is_none());
        }
        h.cleanup().await;
    }

    /// 命令权限（required_permission）关键路径：BotAdmin 对普通成员拒绝，Everyone 放行。
    #[tokio::test]
    async fn command_permission_gate() {
        let h = NativeHarness::new("perm").await;
        h.set_plugin_cfg(|mut c| {
            c.allow_empty_prefix = true;
            c
        })
        .await;

        h.register_native_perm("super", true, false, PermissionLevel::BotAdmin);
        h.register_native("everyone", true, false);
        // super 需要 BotAdmin；普通成员 Everyone 不够 → 不会命中。
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "super"))
                .await
                .matched
        );
        assert!(h.capture("super").is_none());

        assert!(
            h.dispatch(&native_private("qq", "bot", "u-1", "m-2", "everyone"))
                .await
                .matched
        );
        assert!(h.capture("everyone").is_some());
        h.cleanup().await;
    }

    /// 跨平台非命令触发器（Keyword / Regex / Suffix）直接基于 Event 匹配，不依赖 OneBot 解析。
    #[tokio::test]
    async fn cross_platform_non_command_triggers_match_event_directly() {
        let h = NativeHarness::new("cross-platform-triggers").await;
        h.set_plugin_cfg(|mut c| {
            c.allow_empty_prefix = true;
            c
        })
        .await;
        let capture = Arc::new(CaptureNativeHandler {
            label: "kw".into(),
            order: h.order.clone(),
            captured: Default::default(),
            args: Default::default(),
            command_context: Default::default(),
            err: false,
        });
        h.host.trigger_registry().register(
            Trigger::new_event(
                TriggerType::Keyword,
                "活动公告",
                true,
                false,
                capture.clone(),
            )
            .with_plugin_name(&h.plugin),
        );
        h.natives.lock().unwrap().push(("kw".into(), capture));

        let event = native_private(
            "discord",
            "bot-discord",
            "discord-user-abc",
            "msg-discord-1",
            "刚看到活动公告更新了",
        );
        let outcome = h.dispatch(&event).await;
        assert!(outcome.matched);
        assert!(outcome.blocked);
        assert_eq!(h.capture("kw").unwrap(), event);
        h.cleanup().await;
    }

    /// 插件级 area / 黑白名单关键路径：命中前的 plugin 门控必须同样作用于 接收 Event 的指令。
    #[tokio::test]
    async fn command_area_black_white_gate() {
        // area=Group + 私聊 => 跳过。
        let h = NativeHarness::new("area").await;
        h.set_plugin_cfg(|mut c| {
            c.area = PluginArea::Group;
            c.allow_empty_prefix = true;
            c
        })
        .await;
        h.register_native("ping", true, false);
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
                .await
                .matched
        );
        assert!(h.capture("ping").is_none());
        h.cleanup().await;

        // black_list 命中 => 跳过。
        let h = NativeHarness::new("black").await;
        h.set_plugin_cfg(|mut c| {
            c.black_list = vec!["u-1".to_string()];
            c.allow_empty_prefix = true;
            c
        })
        .await;
        h.register_native("ping", true, false);
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
                .await
                .matched
        );
        assert!(h.capture("ping").is_none());
        h.cleanup().await;

        // white_list 不含当前用户 => 跳过。
        let h = NativeHarness::new("white").await;
        h.set_plugin_cfg(|mut c| {
            c.white_list = vec!["other".to_string()];
            c.allow_empty_prefix = true;
            c
        })
        .await;
        h.register_native("ping", true, false);
        assert!(
            !h.dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
                .await
                .matched
        );
        assert!(h.capture("ping").is_none());
        h.cleanup().await;
    }

    /// Event 处理器 返回错误仍按 block 阻断：后续触发器（legacy Message）不得执行。
    #[tokio::test]
    async fn event_handler_error_still_blocks() {
        let h = NativeHarness::new("errblock").await;
        h.register_native("ping", true, true);
        h.register_message_legacy(true, 10);

        let result = h
            .dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
            .await;
        assert!(result.blocked, "block=true 的 Event 处理器 出错必须阻断");
        assert_eq!(
            h.legacy_count.load(Ordering::SeqCst),
            0,
            "阻断后 legacy 触发器不得执行"
        );
        h.cleanup().await;
    }

    /// native 与 legacy 触发器并存且按 priority 确定性执行。
    #[tokio::test]
    async fn native_and_legacy_dispatch_in_priority_order() {
        let h = NativeHarness::new("order").await;
        // Message 触发器（priority 1）先于 native（priority 2）执行。
        h.register_message_legacy(false, 1);
        h.register_native("ping", false, false);

        let result = h
            .dispatch(&native_private("qq", "bot", "u-1", "m-1", "ping"))
            .await;
        assert!(result.matched);
        let order = h.order.lock().unwrap().clone();
        assert_eq!(
            order,
            vec!["message", "ping"],
            "priority 小者先执行，native/legacy 可并存且顺序确定"
        );
        h.cleanup().await;
    }

    #[test]
    fn rule_priority_order_preserves_prefix_signed_values_and_stable_ties() {
        let handler = Arc::new(CountingTriggerHandler {
            count: Arc::new(AtomicUsize::new(0)),
        });
        let mut matches: Vec<_> = [
            ("early-rule", "", -100),
            ("large-rule", "", 400),
            ("tie-first", "", 3),
            ("prefixed", "/", 400),
            ("negative-rule", "", -10),
            ("tie-second", "", 3),
        ]
        .into_iter()
        .map(|(name, prefix, rule_priority)| TriggerResult {
            trigger: Arc::new(
                Trigger::new(
                    TriggerType::Command,
                    name,
                    prefix,
                    false,
                    false,
                    handler.clone(),
                )
                .with_priority(rule_priority),
            ),
            match_data: MatchData::default(),
            command_text: String::new(),
        })
        .collect();

        matches.sort_by_key(TriggerResult::priority_key);
        assert_eq!(
            matches
                .iter()
                .map(|result| result.trigger.pattern.as_str())
                .collect::<Vec<_>>(),
            vec![
                "prefixed",
                "early-rule",
                "negative-rule",
                "tie-first",
                "tie-second",
                "large-rule"
            ]
        );
    }

    #[tokio::test]
    async fn cross_plugin_dispatch_uses_rule_priority_and_preserves_block() {
        let h = NativeHarness::new("rule-priority").await;
        h.set_plugin_cfg(|config| config).await;
        let peer_name = format!("{}Peer", h.plugin);
        let mut peer_config = Plugins::new(&peer_name).with_area(PluginArea::All);
        peer_config.allow_empty_prefix = true;
        h.host.register_plugin_config(peer_config).await;

        for (plugin_name, label, rule_priority, block) in [
            (h.plugin.as_str(), "late", 400, false),
            (peer_name.as_str(), "early", -100, false),
            (h.plugin.as_str(), "middle", -10, true),
        ] {
            let handler = Arc::new(CaptureNativeHandler {
                label: label.into(),
                order: h.order.clone(),
                captured: Default::default(),
                args: Default::default(),
                command_context: Default::default(),
                err: false,
            });
            h.host.trigger_registry().register_command(
                crate::core::command::Command::new("ping", handler)
                    .with_plugin(plugin_name)
                    .with_priority(rule_priority)
                    .with_block(block),
            );
        }

        let event = native_private("qq", "bot", "u-1", "m-1", "ping");
        let result = h.dispatch(&event).await;
        assert!(result.matched && result.blocked);
        assert_eq!(*h.order.lock().unwrap(), vec!["early", "middle"]);
        h.cleanup().await;
    }
}
