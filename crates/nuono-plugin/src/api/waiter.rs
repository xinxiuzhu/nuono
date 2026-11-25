use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};

use crate::error::{NuonoError, Result};

/// 宿主全局共享的回复等待注册表最大待处理容量。
pub const DEFAULT_WAIT_CAPACITY: usize = 1024;

// 使用 Tokio 的单调时钟：既能在非异步运行时环境下正常工作，也能在测试中感知 `tokio::time::pause` 暂停的虚拟时间。
fn wait_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// 回复等待的匹配模式：单次仅匹配同一发送者，或多人模式允许同群任意成员回复。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplyWaitMode {
    /// 仅接受发起会话的同一用户回复。
    #[default]
    Single,
    /// 允许当前群组或频道内的任意成员回复。
    Multiple,
}

impl ReplyWaitMode {
    pub fn from_multiply(multiply: bool) -> Self {
        if multiply {
            Self::Multiple
        } else {
            Self::Single
        }
    }

    pub fn is_multiple(self) -> bool {
        matches!(self, Self::Multiple)
    }
}

/// 捕获到的用户回复事件（保留该条新回复自身的入站领域事件与语言快照，不依赖发起等待时的旧消息上下文）。
#[derive(Debug, Clone)]
pub struct WaitEvent {
    pub raw: Value,
    pub domain: Option<crate::domain::DomainEvent>,
    pub locale: crate::LocaleContext,
}
impl WaitEvent {
    pub fn event(&self) -> crate::Event<'_> {
        crate::Event::with_domain(&self.raw, self.domain.as_ref()).with_locale_context(self.locale)
    }
}

/// 等待注册的宿主归属与作用域信息（在回复实际投递前一直保留，以便在运行期权限或插件状态变更后重新校验访问权限）。
#[derive(Debug, Clone, Default)]
pub struct WaitScope {
    pub route: Option<crate::core::RuntimeBotRoute>,
    pub plugin_name: Option<String>,
    pub required_permission: Option<crate::core::command::PermissionLevel>,
    pub bot_id: i64,
    pub user_id: String,
    pub group_id: Option<String>,
    pub multiply: bool,
}
impl WaitScope {
    pub fn from_event(bot_id: i64, event: &Value) -> Self {
        let identity = |key| {
            event.get(key).and_then(|v| match v {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
        };
        Self {
            route: event
                .get("route")
                .cloned()
                .and_then(|route| serde_json::from_value(route).ok()),
            bot_id,
            user_id: identity("user_id").unwrap_or_default(),
            group_id: matches!(
                event.get("message_type").and_then(Value::as_str),
                Some("group" | "channel")
            )
            .then(|| identity("group_id").or_else(|| identity("channel_id")))
            .flatten(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct WaitSnapshot {
    pub session_id: String,
    pub generation: u64,
    pub scope: WaitScope,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

enum WaitSender {
    Raw(oneshot::Sender<Value>),
    Context(oneshot::Sender<WaitEvent>),
}
impl WaitSender {
    fn is_closed(&self) -> bool {
        match self {
            Self::Raw(tx) => tx.is_closed(),
            Self::Context(tx) => tx.is_closed(),
        }
    }
    fn send(self, event: WaitEvent) -> bool {
        match self {
            Self::Raw(tx) => tx.send(event.raw).is_ok(),
            Self::Context(tx) => tx.send(event).is_ok(),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitStatus {
    Waiting,
    Delivered,
    Cancelled,
    Expired,
}
struct Entry {
    sender: Option<WaitSender>,
    status: watch::Sender<WaitStatus>,
    deadline: Instant,
    snapshot: WaitSnapshot,
}
impl Entry {
    fn is_live(&self, now: Instant) -> bool {
        self.deadline > now
            && self
                .sender
                .as_ref()
                .is_some_and(|sender| !sender.is_closed())
    }
    fn deliver(mut self, event: WaitEvent) -> bool {
        if self.deadline > wait_now() && self.sender.take().is_some_and(|sender| sender.send(event))
        {
            self.status.send_replace(WaitStatus::Delivered);
            true
        } else {
            false
        }
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        if *self.status.borrow() == WaitStatus::Waiting {
            self.status.send_replace(if self.deadline <= wait_now() {
                WaitStatus::Expired
            } else {
                WaitStatus::Cancelled
            });
        }
    }
}
#[derive(Default)]
struct RegistryState {
    entries: HashMap<String, Entry>,
    blocked_plugins: HashSet<String>,
}
impl RegistryState {
    fn take_expired(&mut self, now: Instant) -> Vec<Entry> {
        self.entries
            .extract_if(|_, entry| !entry.is_live(now))
            .map(|(_, entry)| entry)
            .collect()
    }
}
struct Registry {
    state: Mutex<RegistryState>,
    capacity: usize,
    next: AtomicU64,
    changes: watch::Sender<u64>,
}
impl Registry {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            capacity,
            next: AtomicU64::new(1),
            changes: watch::channel(0).0,
        }
    }
    fn expire_current(&self, snapshot: &WaitSnapshot) {
        let removed = self.take_current(snapshot);
        if let Some(entry) = &removed {
            entry.status.send_replace(WaitStatus::Expired);
        }
        drop(removed);
    }
    fn take_current(&self, snapshot: &WaitSnapshot) -> Option<Entry> {
        let mut state = self.state.lock().expect("锁已中毒");
        if state
            .entries
            .get(&snapshot.session_id)
            .is_some_and(|entry| entry.snapshot.generation == snapshot.generation)
        {
            state.entries.remove(&snapshot.session_id)
        } else {
            None
        }
    }
}

/// 单次回复等待的注册句柄（严格绑定单一 generation）。
/// 丢弃或取消该 Future 只会清理当前这一代等待，绝不会误删同一会话后续新建的等待注册；即使直接 `.await` 也会受自身截止时间约束。
pub struct WaitRegistration<T> {
    receiver: oneshot::Receiver<T>,
    status: watch::Receiver<WaitStatus>,
    registry: Arc<Registry>,
    snapshot: WaitSnapshot,
    deadline: Instant,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> std::fmt::Debug for WaitRegistration<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitRegistration")
            .field("snapshot", &self.snapshot)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}
impl<T> WaitRegistration<T> {
    pub fn is_active(&self) -> bool {
        let (live, expired) = {
            let mut state = self.registry.state.lock().expect("锁已中毒");
            let current = state.entries.get(&self.snapshot.session_id);
            let same =
                current.is_some_and(|entry| entry.snapshot.generation == self.snapshot.generation);
            let live = same && current.is_some_and(|entry| entry.is_live(wait_now()));
            let expired = (same && !live)
                .then(|| state.entries.remove(&self.snapshot.session_id))
                .flatten();
            (live, expired)
        };
        drop(expired);
        live
    }
    pub fn snapshot(&self) -> &WaitSnapshot {
        &self.snapshot
    }
    /// 回复接收与提示消息准备/发送阶段共用的绝对截止时间点。
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    fn expire(&mut self) {
        self.registry.expire_current(&self.snapshot);
        self.receiver.close();
    }
    /// 在不消费已到达回复的前提下，监听等待是否被取消、被新注册替换、所属插件停用或到达绝对截止时间。
    /// 若回复已在提示发送完成前提前送达，不会误报为取消；但绝对截止时间仍会继续约束尚未完成的提示发送过程。
    pub async fn cancelled(&self) -> NuonoError {
        let mut status = self.status.clone();
        let cancellation = async {
            loop {
                let state = *status.borrow_and_update();
                match state {
                    WaitStatus::Cancelled => return NuonoError::Conflict("等待被取消".into()),
                    WaitStatus::Expired => return NuonoError::Timeout("等待超时".into()),
                    _ => {}
                }
                if status.changed().await.is_err() {
                    if state == WaitStatus::Delivered {
                        std::future::pending::<()>().await;
                    }
                    return NuonoError::Conflict("等待被取消".into());
                }
            }
        };
        tokio::select! {
            biased;
            error = cancellation => error,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.deadline)) => {
                self.registry.expire_current(&self.snapshot);
                NuonoError::Timeout("等待超时".into())
            }
        }
    }
    pub fn try_recv(&mut self) -> std::result::Result<T, oneshot::error::TryRecvError> {
        match self.receiver.try_recv() {
            Err(oneshot::error::TryRecvError::Empty) => {
                if self.deadline <= wait_now() {
                    self.expire();
                } else if matches!(
                    *self.status.borrow(),
                    WaitStatus::Cancelled | WaitStatus::Expired
                ) {
                    self.receiver.close();
                }
                self.receiver.try_recv()
            }
            result => result,
        }
    }
    /// 等待回复并返回强类型的准入拒绝、超时或冲突取消错误。
    pub async fn receive(self, timeout: Duration) -> Result<T> {
        let remaining = self
            .deadline
            .saturating_duration_since(wait_now())
            .min(timeout);
        let deadline = self.deadline;
        let status = self.status.clone();
        match tokio::time::timeout(remaining, self).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(match *status.borrow() {
                WaitStatus::Expired => NuonoError::Timeout("等待超时".into()),
                WaitStatus::Cancelled => NuonoError::Conflict("等待被取消".into()),
                _ if deadline <= wait_now() => NuonoError::Timeout("等待超时".into()),
                _ => NuonoError::Conflict("等待被取消".into()),
            }),
            Err(_) => Err(NuonoError::Timeout("等待超时".into())),
        }
    }
}
impl<T> Future for WaitRegistration<T> {
    type Output = std::result::Result<T, oneshot::error::RecvError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(reply) = Pin::new(&mut self.receiver).poll(cx) {
            return Poll::Ready(reply);
        }
        // 允许在 Tokio 运行时之外先行构造注册句柄；只有当 Future 真正进入 Pending 轮询时才惰性创建睡眠计时器。
        // 在截止时间之前已经送达通道的回复即使稍后轮询也仍能正常取出。
        if self.deadline <= wait_now() {
            self.expire();
            return Pin::new(&mut self.receiver).poll(cx);
        }
        let deadline = self.deadline;
        let timer = self.timer.get_or_insert_with(|| {
            Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                deadline,
            )))
        });
        if timer.as_mut().poll(cx).is_ready() {
            self.expire();
            return Pin::new(&mut self.receiver).poll(cx);
        }
        Poll::Pending
    }
}
impl<T> Drop for WaitRegistration<T> {
    fn drop(&mut self) {
        drop(self.registry.take_current(&self.snapshot));
    }
}

/// 宿主全局共享、带容量上限的单次回复等待管理器。
/// 准入检查、同会话替换与取消均为原子操作；在 `.await` 等待期间或唤醒已被移除注册的接收端时，均不会持有互斥锁。
pub struct WaitManager {
    registry: Arc<Registry>,
}

impl std::fmt::Debug for WaitManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitManager")
            .field("wait_count", &self.wait_count())
            .finish_non_exhaustive()
    }
}
impl Default for WaitManager {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_WAIT_CAPACITY)
    }
}
impl WaitManager {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            registry: Arc::new(Registry::new(capacity)),
        }
    }
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.registry.changes.subscribe()
    }
    fn register<T>(
        &self,
        session_id: String,
        timeout: Duration,
        scope: WaitScope,
        receiver: oneshot::Receiver<T>,
        sender: WaitSender,
    ) -> (WaitRegistration<T>, Option<NuonoError>) {
        let session_id = scope
            .route
            .as_ref()
            .map(|route| Self::scoped_session_id(route, &session_id))
            .unwrap_or(session_id);
        let snapshot = WaitSnapshot {
            session_id: session_id.clone(),
            generation: self.registry.next.fetch_add(1, Ordering::Relaxed),
            scope,
            created_at: chrono::Utc::now(),
        };
        let now = wait_now();
        let checked_deadline = now.checked_add(timeout).filter(|_| !timeout.is_zero());
        let deadline = checked_deadline.unwrap_or(now);
        let (status, status_rx) = watch::channel(WaitStatus::Waiting);
        let mut entry = Some(Entry {
            sender: Some(sender),
            status,
            deadline,
            snapshot: snapshot.clone(),
        });
        let error = if checked_deadline.is_none() {
            Some(NuonoError::Validation(
                "等待超时必须为正且不能超出时钟范围".into(),
            ))
        } else {
            let (expired, error) = {
                let mut state = self.registry.state.lock().expect("锁已中毒");
                let mut expired = state.take_expired(wait_now());
                let error = if snapshot
                    .scope
                    .plugin_name
                    .as_ref()
                    .is_some_and(|plugin| state.blocked_plugins.contains(plugin))
                {
                    Some(NuonoError::Conflict("插件等待已停用".into()))
                } else if state.entries.len() >= self.registry.capacity
                    && !state.entries.contains_key(&session_id)
                {
                    Some(NuonoError::Overloaded(
                        "等待回复登记已满，请稍后重试".into(),
                    ))
                } else {
                    if let Some(previous) = state
                        .entries
                        .insert(session_id, entry.take().expect("登记前已构造待入表项"))
                    {
                        expired.push(previous);
                    }
                    None
                };
                (expired, error)
            };
            drop(expired);
            error
        };
        // 在释放注册表互斥锁之后再销毁被拒绝或被替换的 Sender，从而在锁外唤醒对应的接收端。
        drop(entry);
        if let Some(error) = &error {
            let reason = match error {
                NuonoError::Overloaded(_) => "full",
                NuonoError::Conflict(_) => "disabled",
                _ => "invalid_timeout",
            };
            metrics::counter!("nuono_wait_reply_rejected_total", "reason" => reason).increment(1);
        }
        if error.is_none() {
            self.registry.changes.send_modify(|current| {
                *current = (*current).max(snapshot.generation);
            });
        }
        (
            WaitRegistration {
                receiver,
                status: status_rx,
                registry: self.registry.clone(),
                snapshot,
                deadline,
                timer: None,
            },
            error,
        )
    }
    fn fallible<T>(
        attempt: (WaitRegistration<T>, Option<NuonoError>),
    ) -> Result<WaitRegistration<T>> {
        let (registration, error) = attempt;
        match error {
            Some(error) => Err(error),
            None => Ok(registration),
        }
    }
    /// 旧版兼容入口：若准入被拒绝（如容量已满、超时无效或插件已停用），会返回一个已关闭的注册句柄。
    /// 若需明确区分超载、超时参数非法与插件停用，请使用 `try_register_*` 系列方法。
    pub fn register_wait(&self, session_id: String, timeout: Duration) -> WaitRegistration<Value> {
        self.register_scoped_wait(session_id, timeout, WaitScope::default())
    }
    pub fn try_register_wait(
        &self,
        session_id: String,
        timeout: Duration,
    ) -> Result<WaitRegistration<Value>> {
        self.try_register_scoped_wait(session_id, timeout, WaitScope::default())
    }
    pub fn register_scoped_wait(
        &self,
        session_id: String,
        timeout: Duration,
        scope: WaitScope,
    ) -> WaitRegistration<Value> {
        let (sender, receiver) = oneshot::channel();
        self.register(
            session_id,
            timeout,
            scope,
            receiver,
            WaitSender::Raw(sender),
        )
        .0
    }
    pub fn try_register_scoped_wait(
        &self,
        session_id: String,
        timeout: Duration,
        scope: WaitScope,
    ) -> Result<WaitRegistration<Value>> {
        let (sender, receiver) = oneshot::channel();
        Self::fallible(self.register(
            session_id,
            timeout,
            scope,
            receiver,
            WaitSender::Raw(sender),
        ))
    }
    pub fn register_event_wait(
        &self,
        session_id: String,
        timeout: Duration,
    ) -> WaitRegistration<WaitEvent> {
        self.register_scoped_event_wait(session_id, timeout, WaitScope::default())
    }
    pub fn try_register_event_wait(
        &self,
        session_id: String,
        timeout: Duration,
    ) -> Result<WaitRegistration<WaitEvent>> {
        self.try_register_scoped_event_wait(session_id, timeout, WaitScope::default())
    }
    pub fn register_scoped_event_wait(
        &self,
        session_id: String,
        timeout: Duration,
        scope: WaitScope,
    ) -> WaitRegistration<WaitEvent> {
        let (sender, receiver) = oneshot::channel();
        self.register(
            session_id,
            timeout,
            scope,
            receiver,
            WaitSender::Context(sender),
        )
        .0
    }
    pub fn try_register_scoped_event_wait(
        &self,
        session_id: String,
        timeout: Duration,
        scope: WaitScope,
    ) -> Result<WaitRegistration<WaitEvent>> {
        let (sender, receiver) = oneshot::channel();
        Self::fallible(self.register(
            session_id,
            timeout,
            scope,
            receiver,
            WaitSender::Context(sender),
        ))
    }
    pub fn snapshot(&self, session_id: &str) -> Option<WaitSnapshot> {
        let (snapshot, expired) = {
            let mut state = self.registry.state.lock().expect("锁已中毒");
            let live = state
                .entries
                .get(session_id)
                .is_some_and(|entry| entry.is_live(wait_now()));
            let expired = (!live).then(|| state.entries.remove(session_id)).flatten();
            (
                state
                    .entries
                    .get(session_id)
                    .map(|entry| entry.snapshot.clone()),
                expired,
            )
        };
        drop(expired);
        snapshot
    }
    /// 内部专用的复合会话键（调用方无需解析）：用精确的 Bot 路由为现有的插件会话键加上命名空间隔离。
    fn scoped_session_id(route: &crate::core::RuntimeBotRoute, session: &str) -> String {
        let existing = session.strip_prefix("nuono-wait-v1:").and_then(|value| {
            serde_json::from_str::<(crate::core::RuntimeBotRoute, String)>(value).ok()
        });
        let legacy = existing
            .as_ref()
            .map(|(_, session)| session.as_str())
            .unwrap_or(session);
        format!("nuono-wait-v1:{}", serde_json::json!([route, legacy]))
    }
    pub fn event_session_id(bot_id: i64, event: &Value) -> Option<String> {
        let legacy = crate::onebot::try_build_session_id(bot_id, event)?;
        match event.get("route") {
            Some(route) => {
                let route: crate::core::RuntimeBotRoute =
                    serde_json::from_value(route.clone()).ok()?;
                Some(Self::scoped_session_id(&route, &legacy))
            }
            None => Some(legacy),
        }
    }
    pub fn snapshot_scoped(
        &self,
        route: &crate::core::RuntimeBotRoute,
        session: &str,
    ) -> Option<WaitSnapshot> {
        self.snapshot(&Self::scoped_session_id(route, session))
    }
    /// 携带精确路由的事件只会匹配对应路由命名空间下的等待，绝不会回退去盲目匹配未限定路由的旧版注册。
    pub fn snapshot_for_event(&self, bot_id: i64, event: &Value) -> Option<WaitSnapshot> {
        self.snapshot(&Self::event_session_id(bot_id, event)?)
    }
    pub fn has_wait(&self, session_id: &str) -> bool {
        self.snapshot(session_id).is_some()
    }
    pub fn respond(&self, session_id: &str, event: Value) -> bool {
        self.respond_with_context(
            session_id,
            WaitEvent {
                raw: event,
                domain: None,
                locale: crate::LocaleContext::default(),
            },
        )
    }
    pub fn respond_with_context(&self, session_id: &str, event: WaitEvent) -> bool {
        self.snapshot(session_id)
            .is_some_and(|snapshot| self.respond_if_current(&snapshot, event))
    }
    /// 仅当该快照对应的那一代等待仍处于活跃状态且成功接收了本次回复时，才返回 `true`。
    pub fn respond_if_current(&self, snapshot: &WaitSnapshot, event: WaitEvent) -> bool {
        self.registry
            .take_current(snapshot)
            .is_some_and(|entry| entry.deliver(event))
    }
    pub fn cancel_if_current(&self, snapshot: &WaitSnapshot) -> bool {
        self.registry.take_current(snapshot).is_some()
    }
    pub fn cancel(&self, session_id: &str) -> bool {
        let removed = self
            .registry
            .state
            .lock()
            .expect("锁已中毒")
            .entries
            .remove(session_id);
        removed.is_some()
    }
    pub fn cancel_plugin(&self, plugin: &str) {
        let removed: Vec<_> = {
            let mut state = self.registry.state.lock().expect("锁已中毒");
            state.blocked_plugins.insert(plugin.to_owned());
            state
                .entries
                .extract_if(|_, entry| entry.snapshot.scope.plugin_name.as_deref() == Some(plugin))
                .map(|(_, entry)| entry)
                .collect()
        };
        drop(removed);
    }
    pub fn enable_plugin(&self, plugin: &str) {
        self.registry
            .state
            .lock()
            .expect("锁已中毒")
            .blocked_plugins
            .remove(plugin);
    }
    pub fn clear(&self) {
        let removed = std::mem::take(&mut self.registry.state.lock().expect("锁已中毒").entries);
        drop(removed);
    }
    pub fn cleanup_expired(&self) {
        let removed = self
            .registry
            .state
            .lock()
            .expect("锁已中毒")
            .take_expired(wait_now());
        drop(removed);
    }
    pub fn list(&self) -> Vec<WaitSnapshot> {
        let (mut items, expired) = {
            let mut state = self.registry.state.lock().expect("锁已中毒");
            let expired = state.take_expired(wait_now());
            let items: Vec<_> = state
                .entries
                .values()
                .map(|entry| entry.snapshot.clone())
                .collect();
            (items, expired)
        };
        drop(expired);
        items.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        items
    }
    pub fn wait_count(&self) -> usize {
        self.list().len()
    }
}

#[derive(Debug)]
pub struct Waiter {
    session_id: String,
    wait_manager: Arc<WaitManager>,
    scope: WaitScope,
}
impl Waiter {
    pub fn new(session_id: String, wait_manager: Arc<WaitManager>) -> Self {
        Self {
            session_id,
            wait_manager,
            scope: WaitScope::default(),
        }
    }
    pub fn with_scope(mut self, scope: WaitScope) -> Self {
        self.scope = scope;
        self
    }
    pub fn set_mode(&mut self, mode: ReplyWaitMode) {
        self.scope.multiply = mode.is_multiple();
    }
    pub fn register(&self, timeout: Duration) -> WaitRegistration<Value> {
        self.wait_manager
            .register_scoped_wait(self.session_id.clone(), timeout, self.scope.clone())
    }
    pub fn register_event(&self, timeout: Duration) -> WaitRegistration<WaitEvent> {
        self.wait_manager.register_scoped_event_wait(
            self.session_id.clone(),
            timeout,
            self.scope.clone(),
        )
    }
    pub fn try_register(&self, timeout: Duration) -> Result<WaitRegistration<Value>> {
        self.wait_manager.try_register_scoped_wait(
            self.session_id.clone(),
            timeout,
            self.scope.clone(),
        )
    }
    pub fn try_register_event(&self, timeout: Duration) -> Result<WaitRegistration<WaitEvent>> {
        self.wait_manager.try_register_scoped_event_wait(
            self.session_id.clone(),
            timeout,
            self.scope.clone(),
        )
    }
    pub async fn wait(&self, timeout: Duration) -> Result<Value> {
        self.try_register(timeout)?.receive(timeout).await
    }
    pub async fn wait_event(&self, timeout: Duration) -> Result<WaitEvent> {
        self.try_register_event(timeout)?.receive(timeout).await
    }
    pub async fn wait_or(&self, timeout: Duration, default_event: Value) -> Value {
        self.wait(timeout).await.unwrap_or(default_event)
    }
}

#[cfg(test)]
mod locale_tests {
    use super::*;
    use crate::{Locale, LocaleContext, LocaleSource};
    use serde_json::json;

    #[tokio::test]
    async fn locale_wait_context_and_legacy_json_share_the_original_session_contract() {
        let waits = WaitManager::new();
        let raw = json!({"raw_message":"reply", "route":{"platform":"onebot","instance_id":"instance","bot_self_id":"42"}, "user_id":"opaque-user"});
        let expected = LocaleContext {
            locale: Locale::Ja,
            source: LocaleSource::User,
        };
        let receiver =
            waits.register_event_wait("unchanged-session".into(), Duration::from_secs(1));
        assert!(waits.respond_with_context(
            "unchanged-session",
            WaitEvent {
                raw: raw.clone(),
                domain: None,
                locale: expected
            }
        ));
        let reply = receiver.await.unwrap();
        assert_eq!(reply.event().locale_context(), expected);
        assert_eq!(reply.event().raw(), &raw);
        assert_eq!(reply.event().command_text(), "reply");
        assert_eq!(reply.event().command_prefix(), "");
        assert_eq!(crate::Event::new(&raw).locale(), Locale::ZhCn);
        let legacy = waits.register_wait("unchanged-session".into(), Duration::from_secs(1));
        waits.respond_with_context("unchanged-session", reply);
        assert_eq!(legacy.await.unwrap(), raw);
        assert_eq!(waits.wait_count(), 0);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn exact_routes_namespace_equal_legacy_sessions_and_never_fall_back_to_unscoped() {
        let waits = WaitManager::new();
        let route_a = crate::core::RuntimeBotRoute::new("feishu", "first", "cli_bot");
        let route_b = crate::core::RuntimeBotRoute::new("feishu", "second", "cli_bot");
        let event = |route: &crate::core::RuntimeBotRoute| serde_json::json!({"post_type":"message","route":route,"self_id":0,"message_type":"private","user_id":"ou_actor","raw_message":"reply"});
        let session = "0:private:ou_actor";
        let mut first = waits.register_scoped_wait(
            session.into(),
            Duration::from_secs(30),
            WaitScope::from_event(0, &event(&route_a)),
        );
        let second = waits.register_scoped_wait(
            session.into(),
            Duration::from_secs(30),
            WaitScope::from_event(0, &event(&route_b)),
        );
        let legacy = waits.register_wait(session.into(), Duration::from_secs(30));
        assert!(first.is_active() && second.is_active() && legacy.is_active());
        assert_ne!(first.snapshot().session_id, second.snapshot().session_id);
        let snapshot = waits.snapshot_for_event(0, &event(&route_a)).unwrap();
        assert_eq!(snapshot.generation, first.snapshot().generation);
        assert!(waits.respond_if_current(
            &snapshot,
            WaitEvent {
                raw: event(&route_a),
                domain: None,
                locale: Default::default()
            }
        ));
        assert_eq!(
            first.try_recv().unwrap()["route"],
            serde_json::json!(route_a)
        );
        assert!(second.is_active() && legacy.is_active());
        let unknown = crate::core::RuntimeBotRoute::new("feishu", "third", "cli_bot");
        assert!(
            waits.snapshot_for_event(0, &event(&unknown)).is_none(),
            "a routed reply cannot guess the legacy wait"
        );
    }

    #[tokio::test]
    async fn reusing_a_snapshot_key_does_not_double_namespace_a_scoped_registration() {
        let waits = WaitManager::new();
        let scope = WaitScope {
            route: Some(crate::core::RuntimeBotRoute::new(
                "onebot", "instance", "42",
            )),
            ..Default::default()
        };
        let previous =
            waits.register_scoped_wait("session".into(), Duration::from_secs(30), scope.clone());
        let key = previous.snapshot().session_id.clone();
        let current = waits.register_scoped_wait(key.clone(), Duration::from_secs(30), scope);
        drop(previous);
        assert_eq!(current.snapshot().session_id, key);
        assert!(current.is_active());
    }
    use serde_json::json;

    #[tokio::test]
    async fn old_registration_drop_and_old_delivery_cannot_remove_its_replacement() {
        let waits = WaitManager::new();
        let old = waits.register_wait("session".into(), Duration::from_secs(30));
        let old_snapshot = old.snapshot().clone();
        let next = waits.register_wait("session".into(), Duration::from_secs(30));
        drop(old);
        assert!(!waits.respond_if_current(
            &old_snapshot,
            WaitEvent {
                raw: json!({}),
                domain: None,
                locale: Default::default()
            }
        ));
        assert!(!waits.cancel_if_current(&old_snapshot));
        assert!(waits.has_wait("session"));
        assert!(waits.respond("session", json!({"answer":1})));
        assert_eq!(next.await.unwrap(), json!({"answer":1}));
        assert_eq!(waits.wait_count(), 0);
    }

    #[tokio::test]
    async fn dropping_wait_future_and_timeout_remove_only_their_own_registration() {
        let waits = Arc::new(WaitManager::new());
        let registration = waits.register_wait("cancelled".into(), Duration::from_secs(30));
        let task = tokio::spawn(registration.receive(Duration::from_secs(30)));
        task.abort();
        let _ = task.await;
        assert!(!waits.has_wait("cancelled"));
        let waiter = Waiter::new("timeout".into(), waits.clone());
        assert!(waiter.wait(Duration::ZERO).await.is_err());
        assert_eq!(waits.wait_count(), 0);
    }

    #[tokio::test]
    async fn expired_or_abandoned_waits_do_not_consume_messages() {
        let waits = WaitManager::new();
        let expired = waits.register_wait("expired".into(), Duration::ZERO);
        assert!(!waits.respond("expired", json!({"late":true})));
        assert!(expired.await.is_err());
        drop(waits.register_wait("abandoned".into(), Duration::from_secs(30)));
        assert!(!waits.respond("abandoned", json!({})));
        assert!(waits.list().is_empty());
    }

    #[tokio::test]
    async fn disabling_owner_cancels_waits_and_rejects_late_registration_until_reenabled() {
        let waits = WaitManager::new();
        let scope = WaitScope {
            plugin_name: Some("plugin".into()),
            ..Default::default()
        };
        let active =
            waits.register_scoped_wait("session".into(), Duration::from_secs(30), scope.clone());
        let peer = waits.register_wait("peer".into(), Duration::from_secs(30));
        waits.cancel_plugin("plugin");
        assert!(active.await.is_err());
        assert!(
            waits
                .register_scoped_wait("late".into(), Duration::from_secs(30), scope.clone())
                .await
                .is_err()
        );
        assert!(waits.has_wait("peer"));
        waits.enable_plugin("plugin");
        let fresh = waits.register_scoped_wait("fresh".into(), Duration::from_secs(30), scope);
        assert!(waits.has_wait("fresh"));
        drop((fresh, peer));
    }

    #[tokio::test]
    async fn registration_notifications_reach_every_connection_dispatcher() {
        let waits = WaitManager::new();
        let mut first = waits.subscribe_changes();
        let mut second = waits.subscribe_changes();
        let pending = waits.register_wait("session".into(), Duration::from_secs(30));
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        assert_eq!(*first.borrow(), pending.snapshot().generation);
        assert_eq!(*second.borrow(), pending.snapshot().generation);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/waiter_fault_tests.rs"]
mod fault_tests;
