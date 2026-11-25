//! 宿主侧交互按钮的一次性令牌注册表；下发给客户端的回调载荷仅携带随机查询令牌。
use super::{PluginHost, PluginProcessResult};
use crate::{
    error::{NuonoError, Result},
    plugin::{Event, PluginResponse},
};
use nuono_plugin::api::{
    button::ButtonPermission,
    interaction::{
        InteractionAction, InteractionRegistration, PluginInteraction, PluginInteractionPort,
    },
    routed::{RuntimeMessageTarget, RuntimeTargetKind},
    waiter::{WaitManager, WaitSnapshot},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 4096;
const MAX_CLICKS: usize = 16_384;
const MAX_TEXT: usize = 8192;
const MAX_ID: usize = 512;

#[derive(Default)]
pub(super) struct InteractionRegistry(Arc<Mutex<Registry>>);
#[derive(Default)]
struct Registry {
    pending: HashMap<String, Entry>,
    blocked: HashSet<String>,
    used_clicks: HashMap<(crate::core::RuntimeBotRoute, String), Instant>,
    epochs: HashMap<String, u64>,
    wait_manager: Weak<WaitManager>,
}
impl Registry {
    fn cleanup_expired(&mut self, now: Instant) {
        let waits = self.wait_manager.upgrade();
        let mut current_waits = HashMap::new();
        // 只有绑定了等待会话的回复选项按钮才随等待批次（generation）一起过期；指令按钮与自定义回调按钮在对话结束后仍维持各自独立的生命周期。
        // WaitManager 绝不会反向调用本注册表（锁顺序固定为：先 interactions 锁，后 wait_manager 锁）。同一个提示包含多个选项按钮时，按会话缓存一次存活检查结果。
        self.pending.retain(|_, entry| {
            entry.deadline > now
                && (!matches!(entry.request.action, InteractionAction::Reply { .. })
                    || entry.wait.as_ref().is_some_and(|wait| {
                        current_waits
                            .get(&wait.session_id)
                            .copied()
                            .unwrap_or_else(|| {
                                let generation = waits.as_ref().and_then(|waits| {
                                    waits
                                        .snapshot(&wait.session_id)
                                        .map(|current| current.generation)
                                });
                                current_waits.insert(wait.session_id.clone(), generation);
                                generation
                            })
                            == Some(wait.generation)
                    }))
        });
        self.used_clicks.retain(|_, deadline| *deadline > now);
    }
}
#[derive(Clone)]
struct Entry {
    request: InteractionRegistration,
    wait: Option<WaitSnapshot>,
    deadline: Instant,
    epoch: u64,
}
pub(crate) struct ClaimedInteraction {
    pub plugin: String,
    pub action: InteractionAction,
    pub wait: Option<WaitSnapshot>,
    pub permission: ButtonPermission,
    pub reservation: Option<CallbackReservation>,
    pub epoch: InteractionEpoch,
    /// 由宿主合成的兼容层消息事件 JSON，供权限校验与消息路由阶段复用。
    pub message: Value,
}

#[derive(Clone)]
pub(crate) struct InteractionEpoch {
    registry: Arc<Mutex<Registry>>,
    plugin: String,
    epoch: u64,
}
impl InteractionEpoch {
    pub(crate) fn is_current(&self) -> bool {
        let state = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        !state.blocked.contains(&self.plugin)
            && state.epochs.get(&self.plugin).copied().unwrap_or(0) == self.epoch
    }
}

/// 回调令牌预留凭证：仅当真正进入业务处理器时才消费单次令牌；
/// 若在权限或限流准入阶段被拒，析构时会归还令牌以便用户稍后重新点击重试（但本次 interaction_id 仍保持去重）。
pub(crate) struct CallbackReservation {
    registry: Arc<Mutex<Registry>>,
    token: String,
    entry: Option<Entry>,
}
impl CallbackReservation {
    fn commit(mut self) -> bool {
        let Some(entry) = &self.entry else {
            return false;
        };
        let state = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let current = entry.deadline > Instant::now()
            && !state.blocked.contains(&entry.request.plugin)
            && state
                .epochs
                .get(&entry.request.plugin)
                .copied()
                .unwrap_or(0)
                == entry.epoch;
        drop(state);
        if !current {
            return false;
        }
        self.entry.take();
        true
    }
}
impl Drop for CallbackReservation {
    fn drop(&mut self) {
        let Some(entry) = self.entry.take() else {
            return;
        };
        let mut state = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if entry.deadline > Instant::now()
            && !state.blocked.contains(&entry.request.plugin)
            && state.pending.len() < MAX_PENDING
            && state
                .epochs
                .get(&entry.request.plugin)
                .copied()
                .unwrap_or(0)
                == entry.epoch
        {
            state.pending.entry(self.token.clone()).or_insert(entry);
        }
    }
}

impl InteractionRegistry {
    pub(super) fn with_wait_manager(waits: &Arc<WaitManager>) -> Self {
        Self(Arc::new(Mutex::new(Registry {
            wait_manager: Arc::downgrade(waits),
            ..Default::default()
        })))
    }
    fn is_reply(&self, event: &Value) -> bool {
        let Some(token) = event.get("interaction_token").and_then(Value::as_str) else {
            return false;
        };
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.cleanup_expired(Instant::now());
        state.pending.get(token).is_some_and(|entry| {
            entry.deadline > Instant::now()
                && matches!(entry.request.action, InteractionAction::Reply { .. })
        })
    }
    fn owns_token(&self, plugin: &str, token: &str, event: &Value) -> bool {
        let Some(actor) = exact(event, "user_id") else {
            return false;
        };
        let Ok(route) = event.get("route").cloned().ok_or(()).and_then(|route| {
            serde_json::from_value::<crate::core::RuntimeBotRoute>(route).map_err(|_| ())
        }) else {
            return false;
        };
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.cleanup_expired(Instant::now());
        state.pending.get(token).is_some_and(|entry| {
            !state.blocked.contains(plugin)
                && entry.request.plugin == plugin
                && entry.request.target.route == route
                && target_matches(&entry.request, event, &actor)
        })
    }
    pub(super) fn enable(&self, plugin: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocked
            .remove(plugin);
    }
    pub(super) fn revoke(&self, plugin: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.blocked.insert(plugin.into());
        let epoch = state.epochs.entry(plugin.into()).or_default();
        *epoch = epoch.saturating_add(1);
        state
            .pending
            .retain(|_, entry| entry.request.plugin != plugin);
    }
    fn insert(
        &self,
        request: InteractionRegistration,
        wait: Option<WaitSnapshot>,
    ) -> Result<String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state.cleanup_expired(now);
        if state.blocked.contains(&request.plugin) {
            return Err(NuonoError::Validation(
                "plugin interactions are disabled".into(),
            ));
        }
        if state.pending.len() >= MAX_PENDING {
            return Err(NuonoError::Validation(
                "too many pending button interactions".into(),
            ));
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        let deadline = now + Duration::from_secs(u64::from(request.ttl_secs));
        let epoch = state.epochs.get(&request.plugin).copied().unwrap_or(0);
        state.pending.insert(
            token.clone(),
            Entry {
                request,
                wait,
                deadline,
                epoch,
            },
        );
        Ok(token)
    }
    fn claim(
        &self,
        event: &Value,
        actor_permission: crate::core::permission::ActorPermission,
    ) -> Option<ClaimedInteraction> {
        if event.get("post_type").and_then(Value::as_str) != Some("interaction") {
            return None;
        }
        let token = event.get("interaction_token")?.as_str()?;
        if token.len() != 32 || !token.bytes().all(|ch| ch.is_ascii_hexdigit()) {
            return None;
        }
        let click_id = exact(event, "interaction_id")?;
        let actor = exact(event, "user_id")?;
        let route: crate::core::RuntimeBotRoute =
            serde_json::from_value(event.get("route")?.clone()).ok()?;
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state.cleanup_expired(now);
        let click_key = (route.clone(), click_id.clone());
        if state.used_clicks.contains_key(&click_key) || state.used_clicks.len() >= MAX_CLICKS {
            return None;
        }
        let entry = state.pending.get(token)?;
        if state.blocked.contains(&entry.request.plugin)
            || route != entry.request.target.route
            || !target_matches(&entry.request, event, &actor)
        {
            return None;
        }
        if entry
            .wait
            .as_ref()
            .is_some_and(|wait| wait.scope.user_id != actor)
        {
            return None;
        }
        match entry.request.permission {
            ButtonPermission::SpecifiedUser if !entry.request.allowed_user_ids.contains(&actor) => {
                return None;
            }
            ButtonPermission::SpecifiedRole => return None,
            ButtonPermission::Manager
                if !actor_permission
                    .has_permission(crate::core::permission::PermissionLevel::GroupAdmin) =>
            {
                return None;
            }
            _ => {}
        }
        let entry = if matches!(entry.request.action, InteractionAction::Command { .. }) {
            entry.clone()
        } else {
            state.pending.remove(token)?
        };
        state.used_clicks.insert(click_key, entry.deadline);
        let reservation =
            matches!(entry.request.action, InteractionAction::Callback { .. }).then(|| {
                CallbackReservation {
                    registry: self.0.clone(),
                    token: token.into(),
                    entry: Some(entry.clone()),
                }
            });
        let text = match &entry.request.action {
            InteractionAction::Command { command } => command.as_str(),
            InteractionAction::Reply { value } => value.as_str(),
            InteractionAction::Callback { .. } | _ => "",
        };
        let message = security_message(&entry.request.target, &actor, &click_id, text, event);
        let epoch = InteractionEpoch {
            registry: self.0.clone(),
            plugin: entry.request.plugin.clone(),
            epoch: entry.epoch,
        };
        Some(ClaimedInteraction {
            plugin: entry.request.plugin,
            action: entry.request.action,
            wait: entry.wait,
            permission: entry.request.permission,
            reservation,
            epoch,
            message,
        })
    }
}

fn exact(event: &Value, key: &str) -> Option<String> {
    let value = crate::core::runtime_outbound::runtime_identity(event, key)?;
    (!value.trim().is_empty() && value.len() <= MAX_ID).then_some(value)
}

fn target_matches(request: &InteractionRegistration, event: &Value, actor: &str) -> bool {
    let target = &request.target;
    // 飞书卡片回调虽然校验了 chat_id，但回调载荷本身不区分单聊还是群聊，需结合目标路由判断。
    if target.route.platform.eq_ignore_ascii_case("feishu") {
        let Some(chat) = exact(event, "chat_id") else {
            return false;
        };
        if let Some(origin) = &request.origin_chat_id
            && origin != &chat
        {
            return false;
        }
        return match &target.kind {
            RuntimeTargetKind::Private { user_id } => {
                request.origin_chat_id.is_some()
                    && actor == request.actor_id
                    && (actor == user_id || &chat == user_id)
            }
            RuntimeTargetKind::Group { group_id } => &chat == group_id,
            RuntimeTargetKind::Channel { channel_id, .. } => &chat == channel_id,
            _ => false,
        };
    }
    match &target.kind {
        RuntimeTargetKind::Private { user_id } => {
            event.get("message_type").and_then(Value::as_str) == Some("private") && actor == user_id
        }
        RuntimeTargetKind::Group { group_id } => {
            event.get("message_type").and_then(Value::as_str) == Some("group")
                && exact(event, "group_id").as_deref() == Some(group_id)
        }
        RuntimeTargetKind::Channel { channel_id, parent } => {
            event.get("message_type").and_then(Value::as_str) == Some("channel")
                && exact(event, "channel_id").as_deref() == Some(channel_id)
                && parent
                    .as_ref()
                    .is_none_or(|parent| exact(event, "guild_id").as_deref() == Some(parent))
        }
        _ => false,
    }
}

fn security_message(
    target: &RuntimeMessageTarget,
    actor: &str,
    click_id: &str,
    text: &str,
    raw: &Value,
) -> Value {
    let mut event = json!({
        "post_type":"message", "platform": target.route.platform,
        "route": target.route, "self_id": target.route.bot_self_id,
        "user_id":actor, "message_id":click_id, "raw_message":text,
        "message":[{"type":"text","data":{"text":text}}],
        "interaction_id":click_id, "sub_type":"normal",
    });
    match &target.kind {
        RuntimeTargetKind::Private { .. } => {
            event["message_type"] = json!("private");
            event["sub_type"] = json!("friend");
        }
        RuntimeTargetKind::Group { group_id } => {
            event["message_type"] = json!("group");
            event["group_id"] = json!(group_id);
        }
        RuntimeTargetKind::Channel { channel_id, parent } => {
            event["message_type"] = json!("channel");
            event["channel_id"] = json!(channel_id);
            event["group_id"] = json!(channel_id);
            if let Some(parent) = parent {
                event["guild_id"] = json!(parent);
            }
        }
        _ => {}
    }
    // 这些原生会话字段仅从已认证的平台适配器事件中提取，绝不信任客户端回传的令牌载荷。
    for key in [
        "sender",
        "chat_id",
        "event_id",
        "openid",
        "group_openid",
        "qq_target_type",
        "reply_context",
    ] {
        if let Some(value) = raw.get(key) {
            event[key] = value.clone();
        }
    }
    if target.route.platform.eq_ignore_ascii_case("discord")
        && matches!(target.kind, RuntimeTargetKind::Private { .. })
        && let Some(channel) = raw.get("channel_id")
    {
        event["channel_id"] = channel.clone();
    }
    event
}

pub(crate) struct HostInteractionPort {
    host: Weak<PluginHost>,
    config: Arc<crate::config::ConfigManager>,
}
impl HostInteractionPort {
    pub(crate) fn new(host: &Arc<PluginHost>, config: Arc<crate::config::ConfigManager>) -> Self {
        Self {
            host: Arc::downgrade(host),
            config,
        }
    }
}

#[async_trait::async_trait]
impl PluginInteractionPort for HostInteractionPort {
    async fn register(&self, mut request: InteractionRegistration) -> Result<String> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| NuonoError::Internal("plugin host stopped".into()))?;
        request.target.validate()?;
        let target_id = match &request.target.kind {
            RuntimeTargetKind::Private { user_id } => user_id,
            RuntimeTargetKind::Group { group_id } => group_id,
            RuntimeTargetKind::Channel { channel_id, .. } => channel_id,
            _ => {
                return Err(NuonoError::Validation(
                    "unsupported interaction target kind".into(),
                ));
            }
        };
        if request.target.route.platform.eq_ignore_ascii_case("feishu")
            && matches!(request.target.kind, RuntimeTargetKind::Private { .. })
            && request.origin_chat_id.is_none()
        {
            return Err(NuonoError::Validation(
                "Feishu private buttons require the originating chat".into(),
            ));
        }
        if request.plugin.is_empty()
            || request.plugin.len() > MAX_ID
            || request.actor_id.len() > MAX_ID
            || target_id.len() > MAX_ID
            || [
                &request.target.route.platform,
                &request.target.route.instance_id,
                &request.target.route.bot_self_id,
            ]
            .iter()
            .any(|id| id.len() > MAX_ID)
            || matches!(&request.target.kind, RuntimeTargetKind::Channel { parent:Some(parent), .. } if parent.len() > MAX_ID)
            || request.ttl_secs == 0
            || request.ttl_secs > 3600
            || request.allowed_user_ids.len() > 100
            || request
                .allowed_user_ids
                .iter()
                .any(|id| id.is_empty() || id.len() > MAX_ID)
            || request
                .origin_chat_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_ID)
        {
            return Err(NuonoError::Validation(
                "invalid interaction registration".into(),
            ));
        }
        if !host
            .get_plugin_config(&request.plugin)
            .await
            .is_some_and(|config| config.enabled)
        {
            return Err(NuonoError::Validation(
                "plugin interactions are disabled".into(),
            ));
        }
        match request.permission {
            ButtonPermission::SpecifiedUser if request.allowed_user_ids.is_empty() => {
                if request.actor_id.is_empty() {
                    return Err(NuonoError::Validation("button requires an actor".into()));
                }
                request.allowed_user_ids.push(request.actor_id.clone());
            }
            ButtonPermission::SpecifiedRole => {
                return Err(NuonoError::Validation(
                    "button role authorization is unsupported".into(),
                ));
            }
            _ => {}
        }
        let mut wait = None;
        match &mut request.action {
            InteractionAction::Command { command } => {
                *command = host
                    .resolve_interaction_command(&request.plugin, command)
                    .await?;
            }
            InteractionAction::Reply { value } => {
                if value.len() > MAX_TEXT || request.actor_id.is_empty() {
                    return Err(NuonoError::Validation("invalid reply button".into()));
                }
                let event = security_message(
                    &request.target,
                    &request.actor_id,
                    "registration",
                    value,
                    &Value::Null,
                );
                let bot_id = request.target.route.bot_self_id.parse().unwrap_or(0);
                let session =
                    crate::onebot::try_build_session_id(bot_id, &event).ok_or_else(|| {
                        NuonoError::Validation("reply button requires an existing wait".into())
                    })?;
                let snapshot = host
                    .wait_manager
                    .snapshot_scoped(&request.target.route, &session)
                    .filter(|wait| {
                        wait.scope.plugin_name.as_deref() == Some(request.plugin.as_str())
                    })
                    .ok_or_else(|| {
                        NuonoError::Validation(
                            "reply button requires a wait owned by this plugin".into(),
                        )
                    })?;
                wait = Some(snapshot);
            }
            InteractionAction::Callback { callback_id, data } => {
                if callback_id.is_empty()
                    || callback_id.len() > MAX_ID
                    || serde_json::to_vec(data)?.len() > MAX_TEXT
                {
                    return Err(NuonoError::Validation("invalid callback data".into()));
                }
            }
            _ => {
                return Err(NuonoError::Validation(
                    "unsupported interaction action".into(),
                ));
            }
        }
        host.interactions.insert(request, wait)
    }
    async fn message_policy(
        &self,
        route: &crate::core::RuntimeBotRoute,
    ) -> Result<nuono_app::message_interactions::MessagePolicy> {
        Ok(self.config.snapshot().await.message_interactions.resolve(
            &route.platform,
            &route.instance_id,
            &route.bot_self_id,
        ))
    }
    async fn resolve_command(&self, plugin: &str, command: &str) -> Result<String> {
        self.host
            .upgrade()
            .ok_or_else(|| NuonoError::Internal("plugin host stopped".into()))?
            .resolve_interaction_command(plugin, command)
            .await
    }
}

impl PluginHost {
    /// 获取指令按钮对应的插件调用守卫（作用域仅包裹实际执行的指令回调，不跨整个消息管线）。
    pub(crate) async fn lock_interaction_command(
        &self,
        plugin: &str,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<crate::plugin::LoadedPlugin>> {
        let hosted = self.hosted_plugin(plugin).await?;
        let invocation = hosted.invocation();
        let guard = invocation.run(hosted.plugin.clone().read_owned()).await?;
        if !crate::core::interaction::allows_plugin(Some(plugin)) {
            return None;
        }
        Some(guard)
    }
    pub(crate) fn owns_interaction_token(&self, plugin: &str, token: &str, event: &Value) -> bool {
        self.interactions.owns_token(plugin, token, event)
    }
    pub(crate) fn is_registered_reply_interaction(&self, event: &Value) -> bool {
        self.interactions.is_reply(event)
    }
    pub(crate) async fn claim_interaction(&self, event: &Value) -> Option<ClaimedInteraction> {
        let actor_permission = self.permission_manager.evaluate(event).await;
        if actor_permission.is_banned() {
            return None;
        }
        self.interactions.claim(event, actor_permission)
    }
    async fn resolve_interaction_command(&self, plugin: &str, command: &str) -> Result<String> {
        if command.trim().is_empty() || command.len() > MAX_TEXT {
            return Err(NuonoError::Validation("invalid button command".into()));
        }
        let config = self
            .get_plugin_config(plugin)
            .await
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        let prefixes = config.effective_prefixes();
        let already_prefixed = prefixes
            .iter()
            .filter(|prefix| !prefix.is_empty())
            .any(|prefix| {
                command
                    .trim()
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            });
        let resolved = if already_prefixed {
            command.trim().to_owned()
        } else {
            format!(
                "{}{}",
                prefixes
                    .iter()
                    .find(|prefix| !prefix.is_empty())
                    .map(String::as_str)
                    .unwrap_or(""),
                command.trim()
            )
        };
        if !self.message_route(&resolved).await.allows_plugin(plugin) {
            return Err(NuonoError::Validation(
                "button command belongs to another namespace".into(),
            ));
        }
        Ok(resolved)
    }
    pub(crate) async fn interaction_command_is_current(&self, plugin: &str, command: &str) -> bool {
        self.get_plugin_config(plugin)
            .await
            .is_some_and(|config| config.enabled && config.message_text(command).is_some())
            && self.message_route(command).await.allows_plugin(plugin)
    }
    pub(crate) async fn dispatch_interaction(
        &self,
        plugin_name: &str,
        message: &Value,
        interaction: &PluginInteraction,
        reservation: Option<CallbackReservation>,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> PluginProcessResult {
        if !self
            .wait_reply_allowed(Some(plugin_name), None, message)
            .await
        {
            return PluginProcessResult::continue_without_plugin();
        }
        let admission_event = Event::with_domain(message, domain).with_locale_context(locale);
        let guard = match self.admit_plugin_message(plugin_name, &admission_event) {
            super::trigger_control::Admission::Granted(guard) => guard,
            super::trigger_control::Admission::Denied { .. } => {
                return PluginProcessResult::continue_without_plugin();
            }
        };
        let plugins = self.plugin_snapshot().await;
        let Some(invocation) = plugins.iter().find(|plugin| plugin.name() == plugin_name) else {
            return PluginProcessResult::continue_without_plugin();
        };
        let Some(plugin) = invocation.read().await else {
            return PluginProcessResult::continue_without_plugin();
        };
        if !reservation.is_some_and(CallbackReservation::commit) {
            return PluginProcessResult::continue_without_plugin();
        }
        let mut raw = message.clone();
        raw["post_type"] = json!("interaction");
        let event = Event::with_domain(&raw, domain).with_locale_context(locale);
        let result = invocation
            .run(nuono_plugin::plugin::with_invocation_guard(
                plugin_name,
                guard,
                self.diagnostic_metrics(plugin_name).observe(
                    "on_interaction",
                    Self::call_with_catch(plugin.on_interaction(&event, interaction)),
                ),
            ))
            .await;
        match result {
            None => PluginProcessResult::continue_without_plugin(),
            Some(Ok(response)) => PluginProcessResult::new(Some(plugin_name.into()), response)
                .with_wait_invocation(invocation.clone()),
            Some(Err(error)) => {
                tracing::warn!(plugin = plugin_name, error = %error, "plugin interaction failed");
                PluginProcessResult::new(Some(plugin_name.into()), PluginResponse::Continue)
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/interaction_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/interaction_wait_cleanup_tests.rs"]
mod wait_cleanup_tests;
