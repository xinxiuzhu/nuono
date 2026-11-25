//! 插件级原子触发准入与限流控制；状态仅保存在宿主进程内存中，容量有界且绝不保存消息正文。
use super::{PluginHost, PluginTriggerPolicy};
use crate::plugin::Event;
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 16_384;
const NOTICE_INTERVAL: Duration = Duration::from_secs(10);
const IDLE_RETENTION: Duration = Duration::from_secs(60);
const TOKEN: u128 = 60_000_000_000;

tokio::task_local! {
    static MESSAGE_ADMISSION: MessageAdmission;
    static RECEIVED_AT: Instant;
}

/// 在串行工作协程中透传套接字实际收到报文的时间点，确保排队等待不会变相绕过冷却时间（CD）检查。
pub(crate) async fn with_received_at<F: Future>(received_at: Instant, future: F) -> F::Output {
    RECEIVED_AT.scope(received_at, future).await
}

#[derive(Default)]
struct MessageAdmission {
    admitted: Mutex<HashMap<(usize, String), Admission>>,
}

/// 同一条消息在插件 on_event 回调与各条命中触发规则之间共享同一个准入判定上下文。
pub(crate) async fn with_message_admission<F: Future>(future: F) -> F::Output {
    if MESSAGE_ADMISSION.try_with(|_| ()).is_ok() {
        future.await
    } else {
        MESSAGE_ADMISSION
            .scope(MessageAdmission::default(), future)
            .await
    }
}

/// 插件回调与触发器派发阶段已结束；释放准入租约，避免后续无关的 Matcher 或 Agent 阶段继续占用插件并发槽位。
pub(crate) fn finish_message_admission() {
    let _ = MESSAGE_ADMISSION.try_with(|message| {
        message
            .admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    });
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct UserKey {
    plugin: String,
    bot: String,
    conversation: String,
    actor: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct GroupKey {
    plugin: String,
    bot: String,
    conversation: String,
}

impl UserKey {
    fn from_event(plugin: &str, event: &Event<'_>) -> Option<(Self, Option<GroupKey>)> {
        if event.post_type() != "message" {
            return None;
        }
        let (bot, conversation, actor, shared) = if let Some(domain) = event.domain_event() {
            let conversation = domain.conversation.as_ref()?;
            let actor = domain.actor.as_ref()?;
            (
                serde_json::to_string(&domain.bot).ok()?,
                serde_json::to_string(conversation).ok()?,
                serde_json::to_string(actor).ok()?,
                !matches!(conversation, crate::domain::ConversationRef::Direct { .. }),
            )
        } else {
            let actor = event.user_id_str()?;
            let route = event
                .route()
                .map(|route| serde_json::json!(route))
                .unwrap_or_else(|| serde_json::json!([event.platform(), event.bot_self_id()]));
            let raw = event.raw();
            let shared = matches!(event.message_type(), "group" | "channel");
            if shared && event.group_id_str().is_none() && raw.get("channel_id").is_none() {
                return None;
            }
            let conversation = if shared {
                serde_json::json!([
                    event.message_type(),
                    event.group_id_str(),
                    exact_id(raw.get("channel_id")),
                    exact_id(raw.get("guild_id")),
                    exact_id(raw.get("chat_id"))
                ])
            } else {
                serde_json::json!([event.message_type(), actor])
            };
            (route.to_string(), conversation.to_string(), actor, shared)
        };
        let group = shared.then(|| GroupKey {
            plugin: plugin.into(),
            bot: bot.clone(),
            conversation: conversation.clone(),
        });
        Some((
            Self {
                plugin: plugin.into(),
                bot,
                conversation,
                actor,
            },
            group,
        ))
    }
}

fn exact_id(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

struct UserState {
    last_received: Option<Instant>,
    completed_at: Option<Instant>,
    active: u32,
    keep_until: Instant,
    last_notice: Option<Instant>,
}

struct Bucket {
    credits: u128,
    updated_at: Instant,
    rate: u32,
    burst: u32,
}

impl Bucket {
    fn refill(&mut self, at: Instant) {
        let elapsed = at.saturating_duration_since(self.updated_at).as_nanos();
        self.credits = self
            .credits
            .saturating_add(elapsed * u128::from(self.rate))
            .min(u128::from(self.burst) * TOKEN);
        self.updated_at = self.updated_at.max(at);
    }

    fn apply(&mut self, policy: PluginTriggerPolicy, at: Instant) {
        self.refill(at);
        self.rate = policy.group_requests_per_minute;
        self.burst = policy.group_burst;
        self.credits = self.credits.min(u128::from(self.burst) * TOKEN);
    }
}

struct State {
    policies: HashMap<String, PluginTriggerPolicy>,
    users: HashMap<UserKey, UserState>,
    groups: HashMap<GroupKey, Bucket>,
    cleaned_at: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            policies: HashMap::new(),
            users: HashMap::new(),
            groups: HashMap::new(),
            cleaned_at: Instant::now(),
        }
    }
}

impl State {
    fn cleanup(&mut self, now: Instant, oldest_queued: Option<Instant>) {
        let policies = &self.policies;
        self.users.retain(|key, user| {
            user.active > 0
                || now < user.keep_until
                || oldest_queued.is_some_and(|received| {
                    user.completed_at.is_some_and(|end| received <= end)
                        || user.last_received.is_some_and(|last| {
                            received
                                <= last
                                    + Duration::from_millis(
                                        policies
                                            .get(&key.plugin)
                                            .copied()
                                            .unwrap_or_default()
                                            .user_cooldown_ms,
                                    )
                        })
                })
        });
        self.groups.retain(|_, bucket| {
            if bucket.rate == 0 {
                return now.saturating_duration_since(bucket.updated_at) < IDLE_RETENTION;
            }
            bucket.refill(now);
            bucket.credits < u128::from(bucket.burst) * TOKEN
        });
        self.cleaned_at = now;
    }

    fn reject(
        &self,
        control: &Arc<TriggerControl>,
        key: &UserKey,
        reason: RejectionReason,
    ) -> Admission {
        metrics::counter!("nuono_plugin_trigger_rejections_total", "plugin" => key.plugin.clone(), "reason" => reason.label()).increment(1);
        Admission::Denied {
            reason,
            notice: Arc::new(RejectionNotice {
                control: Arc::downgrade(control),
                key: Some(key.clone()),
                pending: AtomicBool::new(true),
            }),
        }
    }
}

#[derive(Default)]
pub(super) struct TriggerControl(Mutex<State>, Mutex<BTreeMap<Instant, usize>>);

/// 已入队事件持有的排队时间水位凭证；
/// 事件处理完毕、连接断开或工作协程被取消时会自动释放该水位记录。
pub(crate) struct QueuedReceipt {
    control: Arc<TriggerControl>,
    received_at: Instant,
}

impl Drop for QueuedReceipt {
    fn drop(&mut self) {
        let mut receipts = self.control.1.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = receipts.get_mut(&self.received_at) {
            *count -= 1;
            if *count == 0 {
                receipts.remove(&self.received_at);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectionReason {
    Cooldown,
    InFlight,
    GroupBudget,
    Capacity,
    Identity,
}

impl RejectionReason {
    fn label(self) -> &'static str {
        match self {
            Self::Cooldown => "user_cooldown",
            Self::InFlight => "in_flight",
            Self::GroupBudget => "group_budget",
            Self::Capacity => "capacity",
            Self::Identity => "identity",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Cooldown => "操作太快，请稍后再试。",
            Self::InFlight => "你在当前会话中的该插件任务仍在处理中，请等待完成。",
            Self::GroupBudget => "本群该插件的请求较多，请稍后再试。",
            Self::Capacity => "当前请求较多，请稍后再试。",
            Self::Identity => "消息缺少有效的用户或会话身份。",
        }
    }
}

pub(crate) struct InvocationLease {
    control: Arc<TriggerControl>,
    key: UserKey,
}

impl Drop for InvocationLease {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut state = self.control.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(user) = state.users.get_mut(&self.key) {
            user.active = user.active.saturating_sub(1);
            user.completed_at = Some(now);
            user.keep_until = user.keep_until.max(now + IDLE_RETENTION);
        }
    }
}

pub(crate) struct RejectionNotice {
    control: Weak<TriggerControl>,
    key: Option<UserKey>,
    pending: AtomicBool,
}

impl RejectionNotice {
    pub(crate) fn claim(&self) -> bool {
        if !self.pending.swap(false, Ordering::Relaxed) {
            return false;
        }
        let (Some(control), Some(key)) = (self.control.upgrade(), self.key.as_ref()) else {
            return false;
        };
        let now = Instant::now();
        let mut state = control.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(user) = state.users.get_mut(key) else {
            return false;
        };
        if user
            .last_notice
            .is_some_and(|last| now.saturating_duration_since(last) < NOTICE_INTERVAL)
        {
            return false;
        }
        user.last_notice = Some(now);
        user.keep_until = user.keep_until.max(now + NOTICE_INTERVAL);
        true
    }
}

#[derive(Clone)]
pub(crate) enum Admission {
    Granted(Arc<InvocationLease>),
    Denied {
        reason: RejectionReason,
        notice: Arc<RejectionNotice>,
    },
}

impl TriggerControl {
    fn policy(&self, plugin: &str) -> PluginTriggerPolicy {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .policies
            .get(plugin)
            .copied()
            .unwrap_or_default()
    }

    fn set_policy(&self, plugin: &str, policy: PluginTriggerPolicy) {
        self.set_policy_at(plugin, policy, Instant::now());
    }

    fn set_policy_at(&self, plugin: &str, policy: PluginTriggerPolicy, now: Instant) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        for (key, bucket) in &mut state.groups {
            if key.plugin == plugin {
                bucket.apply(policy, now);
            }
        }
        for (key, user) in &mut state.users {
            if key.plugin == plugin {
                user.keep_until = user.keep_until.max(now + IDLE_RETENTION);
                if let Some(received) = user.last_received {
                    user.keep_until = user
                        .keep_until
                        .max(received + Duration::from_millis(policy.user_cooldown_ms));
                }
            }
        }
        state.policies.insert(plugin.into(), policy);
    }

    fn admit(
        self: &Arc<Self>,
        key: UserKey,
        group: Option<GroupKey>,
        received: Instant,
        now: Instant,
    ) -> Admission {
        let oldest_queued = self
            .1
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .first_key_value()
            .map(|(at, _)| *at);
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if now.saturating_duration_since(state.cleaned_at) >= IDLE_RETENTION
            || state.users.len() + state.groups.len() >= MAX_ENTRIES
        {
            state.cleanup(now, oldest_queued);
        }
        let policy = state.policies.get(&key.plugin).copied().unwrap_or_default();
        let new_entries = usize::from(!state.users.contains_key(&key))
            + usize::from(
                policy.group_requests_per_minute > 0
                    && group
                        .as_ref()
                        .is_some_and(|key| !state.groups.contains_key(key)),
            );
        if state.users.len() + state.groups.len() + new_entries > MAX_ENTRIES {
            return state.reject(self, &key, RejectionReason::Capacity);
        }
        let user = state.users.entry(key.clone()).or_insert(UserState {
            last_received: None,
            completed_at: None,
            active: 0,
            keep_until: now + IDLE_RETENTION,
            last_notice: None,
        });
        // 若该消息到达套接字时上一条调用尚未结束（即使轮到串行工作协程处理本条消息时上一条已跑完），仍按并发期间重复触发拦截。
        if policy.max_in_flight_per_user > 0
            && (user.active >= policy.max_in_flight_per_user
                || (policy.max_in_flight_per_user == 1
                    && user.completed_at.is_some_and(|end| received < end)))
        {
            return state.reject(self, &key, RejectionReason::InFlight);
        }
        if policy.user_cooldown_ms > 0
            && user.last_received.is_some_and(|last| {
                received.saturating_duration_since(last)
                    < Duration::from_millis(policy.user_cooldown_ms)
            })
        {
            return state.reject(self, &key, RejectionReason::Cooldown);
        }
        if policy.group_requests_per_minute > 0
            && let Some(group) = group
        {
            let bucket = state.groups.entry(group).or_insert(Bucket {
                credits: u128::from(policy.group_burst) * TOKEN,
                updated_at: now,
                rate: policy.group_requests_per_minute,
                burst: policy.group_burst,
            });
            bucket.apply(policy, now);
            if bucket.credits < TOKEN {
                return state.reject(self, &key, RejectionReason::GroupBudget);
            }
            bucket.credits -= TOKEN;
        }
        let user = state.users.get_mut(&key).expect("user inserted atomically");
        user.last_received = Some(received);
        user.active += 1;
        user.keep_until = now + IDLE_RETENTION.max(Duration::from_millis(policy.user_cooldown_ms));
        Admission::Granted(Arc::new(InvocationLease {
            control: self.clone(),
            key,
        }))
    }
}

impl PluginHost {
    /// 路由与生命周期测试桩会连续紧密发送多条测试消息；触发准入限流有独立测试覆盖，此处提供放开限流的测试辅助方法。
    #[cfg(test)]
    pub(crate) fn set_unlimited_trigger_policy_for_test(&self, plugin: &str) {
        self.trigger_control.set_policy(
            plugin,
            PluginTriggerPolicy {
                user_cooldown_ms: 0,
                max_in_flight_per_user: 0,
                group_requests_per_minute: 0,
                ..PluginTriggerPolicy::default()
            },
        );
    }

    pub(crate) fn retain_queued_receipt(&self, received_at: Instant) -> QueuedReceipt {
        *self
            .trigger_control
            .1
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(received_at)
            .or_default() += 1;
        QueuedReceipt {
            control: self.trigger_control.clone(),
            received_at,
        }
    }

    pub fn plugin_trigger_policy(&self, plugin: &str) -> PluginTriggerPolicy {
        self.trigger_control.policy(plugin)
    }

    pub async fn set_plugin_trigger_policy(
        &self,
        plugin: &str,
        policy: PluginTriggerPolicy,
    ) -> crate::error::Result<PluginTriggerPolicy> {
        policy.validate()?;
        let _writer = self.trigger_policy_writer.lock().await;
        if !self.is_plugin_installed(plugin).await {
            return Err(crate::error::NuonoError::PluginNotFound(plugin.into()));
        }
        self.db.set_plugin_trigger_policy(plugin, &policy).await?;
        self.trigger_control.set_policy(plugin, policy);
        Ok(policy)
    }

    pub(super) async fn load_plugin_trigger_policy(
        &self,
        plugin: &str,
    ) -> crate::error::Result<()> {
        let _writer = self.trigger_policy_writer.lock().await;
        // 新发现或校验被拒的插件在同步写入注册表前可能尚无策略记录（此时使用默认策略）；
        // 但若数据库中已存在的持久化策略记录损坏或读取出错，则必须按失败拒绝（fail-closed）处理，绝不能静默回退为默认放行。
        let policy = match self.db.get_plugin_trigger_policy(plugin).await {
            Ok(policy) => policy.unwrap_or_default(),
            Err(crate::error::NuonoError::PluginNotFound(_)) => PluginTriggerPolicy::default(),
            Err(error) => return Err(error),
        };
        self.trigger_control.set_policy(plugin, policy);
        Ok(())
    }

    pub(crate) fn admit_plugin_message(&self, plugin: &str, event: &Event<'_>) -> Admission {
        let now = Instant::now();
        let received = RECEIVED_AT.try_with(|at| *at).unwrap_or(now).min(now);
        let cache_key = (
            Arc::as_ptr(&self.trigger_control) as usize,
            plugin.to_owned(),
        );
        let decide = || match UserKey::from_event(plugin, event) {
            Some((key, group)) => self.trigger_control.admit(key, group, received, now),
            None => Admission::Denied {
                reason: RejectionReason::Identity,
                notice: Arc::new(RejectionNotice {
                    control: Weak::new(),
                    key: None,
                    pending: AtomicBool::new(false),
                }),
            },
        };
        MESSAGE_ADMISSION
            .try_with(|message| {
                message
                    .admitted
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(cache_key)
                    .or_insert_with(decide)
                    .clone()
            })
            .unwrap_or_else(|_| decide())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/trigger_control/tests.rs"]
mod tests;
