//! Dashboard 测试会话使用生产事件管线与虚拟输出汇。
//! 会话状态是瞬时的；插件仍使用其正常数据库与外部服务。

mod sink;
#[cfg(test)]
#[path = "../tests/unit/sandbox/tests.rs"]
mod tests;

use futures::FutureExt;
use nuono_core::api::waiter::WaitManager;
use nuono_core::core::platform::{
    MessageType, SenderInfo, UnifiedEvent, UnifiedMessage, UnifiedSegment,
};
use nuono_core::core::{EventKernel, RuntimeActionRouter, RuntimeBotRoute};
use nuono_core::error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use utoipa::ToSchema;

const MAX_SESSIONS: usize = 32;
const MAX_OWNER_SESSIONS: usize = 4;
const MAX_MESSAGES: usize = 200;
const MAX_PROCESSING: usize = 4;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_MEDIA_BYTES: usize = 32 * 1024 * 1024;
const MAX_MEDIA_ITEMS: usize = 64;
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSession {
    pub message_type: String,
    #[serde(default = "default_sender")]
    pub sender_name: String,
    #[serde(default = "default_role")]
    pub group_role: String,
}
fn default_sender() -> String {
    "测试用户".into()
}
fn default_role() -> String {
    "member".into()
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendMessage {
    pub text: String,
    #[serde(default)]
    pub at_bot: bool,
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct SandboxMessage {
    pub seq: u64,
    pub id: String,
    pub direction: String,
    pub segments: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionSnapshot {
    pub id: String,
    pub bot_id: String,
    pub user_id: String,
    pub group_id: Option<String>,
    pub message_type: String,
    pub sender_name: String,
    pub group_role: String,
    pub processing: usize,
    pub messages: Vec<SandboxMessage>,
    pub cursor: u64,
    pub test_mode: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMode {
    Conversation,
    Test,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageReceipt {
    pub id: String,
    pub seq: u64,
}

#[derive(Clone)]
pub(crate) struct CapturedMedia {
    pub bytes: Arc<[u8]>,
    pub content_type: String,
    pub file_name: String,
    pub inline: bool,
}

struct HistoryEntry {
    message: SandboxMessage,
    media: Vec<String>,
}

struct SessionState {
    touched_at: Instant,
    next_seq: u64,
    outgoing_count: u64,
    history: VecDeque<HistoryEntry>,
    media: HashMap<String, CapturedMedia>,
    media_bytes: usize,
    jobs: HashMap<String, tokio::task::AbortHandle>,
}

pub(crate) struct Session {
    id: String,
    owner: i64,
    mode: SessionMode,
    bot_id: i64,
    user_id: i64,
    group_id: Option<i64>,
    options: CreateSession,
    route: RuntimeBotRoute,
    generation: AtomicU64,
    closed: AtomicBool,
    slots: Arc<tokio::sync::Semaphore>,
    state: Mutex<SessionState>,
}

impl Session {
    fn new(owner: i64, options: CreateSession) -> Result<Arc<Self>> {
        Self::with_mode(owner, options, SessionMode::Test)
    }

    fn with_mode(owner: i64, options: CreateSession, mode: SessionMode) -> Result<Arc<Self>> {
        if !matches!(options.message_type.as_str(), "private" | "group") {
            return Err(validation("会话类型只能是 private 或 group"));
        }
        if !matches!(options.group_role.as_str(), "member" | "admin" | "owner") {
            return Err(validation("群身份只能是 member、admin 或 owner"));
        }
        if options.sender_name.trim().is_empty()
            || options.sender_name.len() > 128
            || options.sender_name.chars().any(char::is_control)
        {
            return Err(validation("测试用户名称不能为空，且最多 128 字节"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        // 使用负数虚拟 ID：既不会与真实 QQ 号冲突，又在 JavaScript 双精度安全整数范围内保持精确。
        let base = -((rand::random::<u64>() % 1_000_000_000_000 + 1_000_000_000_000) as i64) * 4;
        let group_id = (options.message_type == "group").then_some(base - 2);
        Ok(Arc::new(Self {
            route: RuntimeBotRoute::new("onebot", format!("web-chat:{id}"), base.to_string()),
            id,
            owner,
            mode,
            bot_id: base,
            user_id: base - 1,
            group_id,
            options,
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_PROCESSING)),
            state: Mutex::new(SessionState {
                touched_at: Instant::now(),
                next_seq: 1,
                outgoing_count: 0,
                history: VecDeque::new(),
                media: HashMap::new(),
                media_bytes: 0,
                jobs: HashMap::new(),
            }),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(validation("测试会话已关闭，请重新创建"))
        } else {
            Ok(())
        }
    }

    fn push(
        &self,
        direction: &str,
        mut segments: Vec<Value>,
        mut action: Option<String>,
        mut error: Option<String>,
        media: Vec<String>,
    ) -> Result<MessageReceipt> {
        if self.mode == SessionMode::Conversation {
            action = None;
            if direction == "system" && error.is_some() {
                segments = vec![
                    json!({"type":"text","data":{"text":"这条消息暂时无法处理，请稍后重试。"}}),
                ];
                error = Some("processing_failed".into());
            }
        }
        let mut state = self.state();
        self.ensure_open()?;
        let seq = state.next_seq;
        state.next_seq += 1;
        let id = format!("-{seq}");
        if direction == "outgoing" {
            state.outgoing_count += 1;
        }
        state.history.push_back(HistoryEntry {
            message: SandboxMessage {
                seq,
                id: id.clone(),
                direction: direction.into(),
                segments,
                action,
                error,
            },
            media,
        });
        while state.history.len() > MAX_MESSAGES {
            if let Some(entry) = state.history.pop_front() {
                for id in entry.media {
                    if let Some(media) = state.media.remove(&id) {
                        state.media_bytes -= media.bytes.len();
                    }
                }
            }
        }
        Ok(MessageReceipt { id, seq })
    }

    fn system(&self, text: impl Into<String>, error: Option<String>) {
        if self.mode == SessionMode::Conversation && error.is_none() {
            return;
        }
        let _ = self.push(
            "system",
            vec![json!({"type":"text","data":{"text":text.into()}})],
            None,
            error,
            vec![],
        );
    }

    fn snapshot(&self, after: u64) -> Result<SessionSnapshot> {
        let mut state = self.state();
        self.ensure_open()?;
        state.touched_at = Instant::now();
        Ok(SessionSnapshot {
            id: self.id.clone(),
            bot_id: self.bot_id.to_string(),
            user_id: self.user_id.to_string(),
            group_id: self.group_id.map(|id| id.to_string()),
            message_type: self.options.message_type.clone(),
            sender_name: self.options.sender_name.clone(),
            group_role: self.options.group_role.clone(),
            processing: state.jobs.len(),
            cursor: state.next_seq - 1,
            test_mode: self.mode == SessionMode::Test,
            messages: state
                .history
                .iter()
                .filter(|entry| entry.message.seq > after)
                .map(|entry| entry.message.clone())
                .collect(),
        })
    }

    fn unified_event(&self, raw: Value, message: &SendMessage) -> UnifiedEvent {
        let mut segments = Vec::new();
        if message.at_bot && self.group_id.is_some() {
            segments.push(UnifiedSegment::At {
                user_id: self.bot_id,
            });
            segments.push(UnifiedSegment::Text { text: " ".into() });
        }
        segments.push(UnifiedSegment::Text {
            text: message.text.clone(),
        });
        UnifiedEvent::Message(UnifiedMessage {
            message_id: raw["message_id"].to_string(),
            self_id: self.bot_id,
            user_id: self.user_id,
            group_id: self.group_id,
            content: message.text.clone(),
            message_type: if self.group_id.is_some() {
                MessageType::Group
            } else {
                MessageType::Private
            },
            sub_type: raw["sub_type"].as_str().unwrap_or_default().into(),
            sender: SenderInfo {
                user_id: self.user_id.to_string(),
                nickname: Some(self.options.sender_name.clone()),
                card: None,
                role: Some(self.options.group_role.clone()),
            },
            time: raw["time"].as_i64().unwrap_or_default(),
            raw,
            segments,
        })
    }
    fn event(&self, receipt: &MessageReceipt, message: &SendMessage) -> Value {
        let mut segments = Vec::new();
        if message.at_bot && self.group_id.is_some() {
            segments.push(json!({"type":"at","data":{"qq":self.bot_id.to_string()}}));
            segments.push(json!({"type":"text","data":{"text":" "}}));
        }
        segments.push(json!({"type":"text","data":{"text":message.text}}));
        let mut event = json!({
            "time":chrono::Utc::now().timestamp(), "post_type":"message", "self_id":self.bot_id,
            "message_type":self.options.message_type, "sub_type": if self.group_id.is_some() { "normal" } else { "friend" },
            "message_id": -(receipt.seq as i64), "user_id":self.user_id, "font":0,
            "raw_message":message.text, "message":segments,
            "sender":{"user_id":self.user_id,"nickname":self.options.sender_name,"card":"", "role": if self.group_id.is_some() { &self.options.group_role } else { "member" }},
            "web_chat_session_id":self.id,
        });
        if let Some(group_id) = self.group_id {
            event["group_id"] = json!(group_id);
        }
        event
    }

    fn wait_key(&self) -> String {
        match self.group_id {
            Some(group) => format!("{}:{}:{}", self.bot_id, group, self.user_id),
            None => format!("{}:private:{}", self.bot_id, self.user_id),
        }
    }

    fn close(&self, router: &RuntimeActionRouter, waits: &WaitManager) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        router.unregister(&self.route, self.generation.load(Ordering::Acquire));
        waits.cancel(&self.wait_key());
        let mut state = self.state();
        for (_, job) in state.jobs.drain() {
            job.abort();
        }
        state.media.clear();
        state.media_bytes = 0;
        state.history.clear();
    }
}

pub(crate) struct SandboxManager {
    kernel: Arc<EventKernel>,
    waits: Arc<WaitManager>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl SandboxManager {
    pub(crate) fn new(kernel: Arc<EventKernel>, waits: Arc<WaitManager>) -> Arc<Self> {
        let manager = Arc::new(Self {
            kernel,
            waits,
            sessions: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                manager.cleanup();
            }
        });
        manager
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cleanup(&self) {
        self.sessions().retain(|_, session| {
            let expired = session.state().touched_at.elapsed() >= SESSION_TTL;
            if expired {
                session.close(&self.kernel.action_router(), &self.waits);
            }
            !expired
        });
    }

    pub(crate) fn create(&self, owner: i64, options: CreateSession) -> Result<SessionSnapshot> {
        self.cleanup();
        let session = Session::new(owner, options)?;
        self.insert(session)
    }

    /// 已登录账号打开或恢复其专属的网页私聊会话（管理员沙盒测试身份则每次独立创建、绝不复用）。
    pub(crate) fn conversation(&self, owner: i64, sender: &str) -> Result<SessionSnapshot> {
        self.cleanup();
        let session = Session::with_mode(
            owner,
            CreateSession {
                message_type: "private".into(),
                sender_name: sender.into(),
                group_role: "member".into(),
            },
            SessionMode::Conversation,
        )?;
        self.insert(session)
    }

    fn insert(&self, session: Arc<Session>) -> Result<SessionSnapshot> {
        let mut sessions = self.sessions();
        if session.mode == SessionMode::Conversation
            && let Some(existing) = sessions.values().find(|existing| {
                existing.owner == session.owner && existing.mode == SessionMode::Conversation
            })
        {
            return existing.snapshot(0);
        }
        if sessions.len() >= MAX_SESSIONS
            || sessions
                .values()
                .filter(|s| s.owner == session.owner)
                .count()
                >= MAX_OWNER_SESSIONS
        {
            return Err(validation("测试会话数量已达上限，请先关闭已有会话"));
        }
        // 即便发生极低概率的随机负数 ID 碰撞，也直接报错拒绝，绝不覆盖另一个活跃会话的虚拟路由。
        if sessions.values().any(|s| s.bot_id == session.bot_id) {
            return Err(validation("测试身份分配失败，请重试"));
        }
        let sink = Arc::new(sink::SandboxSink::new(Arc::downgrade(&session)));
        let generation = self
            .kernel
            .action_router()
            .register(session.route.clone(), sink);
        session.generation.store(generation, Ordering::Release);
        let snapshot = session.snapshot(0)?;
        sessions.insert(session.id.clone(), session);
        Ok(snapshot)
    }

    fn get(&self, owner: i64, id: &str) -> Result<Arc<Session>> {
        let session = self
            .sessions()
            .get(id)
            .cloned()
            .ok_or_else(|| validation("测试会话不存在或已过期，请重新创建"))?;
        if session.owner != owner {
            return Err(NuonoError::PermissionDenied("这不是你的测试会话".into()));
        }
        session.ensure_open()?;
        Ok(session)
    }

    pub(crate) fn snapshot(&self, owner: i64, id: &str, after: u64) -> Result<SessionSnapshot> {
        self.get(owner, id)?.snapshot(after)
    }

    pub(crate) fn require_conversation(&self, owner: i64, id: &str) -> Result<()> {
        let session = self.get(owner, id)?;
        if session.mode != SessionMode::Conversation {
            return Err(NuonoError::PermissionDenied("测试会话需要管理权限".into()));
        }
        Ok(())
    }

    pub(crate) fn delete(&self, owner: i64, id: &str) -> Result<bool> {
        self.delete_mode(owner, id, None)
    }

    pub(crate) fn close_conversation(&self, owner: i64, id: &str) -> Result<bool> {
        self.delete_mode(owner, id, Some(SessionMode::Conversation))
    }

    fn delete_mode(&self, owner: i64, id: &str, mode: Option<SessionMode>) -> Result<bool> {
        let mut sessions = self.sessions();
        let Some(session) = sessions.get(id) else {
            return Ok(true);
        };
        if session.owner != owner {
            return Err(NuonoError::PermissionDenied("这不是你的测试会话".into()));
        }
        if mode.is_some_and(|mode| mode != session.mode) {
            return Err(NuonoError::PermissionDenied("测试会话需要管理权限".into()));
        }
        session.close(&self.kernel.action_router(), &self.waits);
        sessions.remove(id);
        Ok(true)
    }

    pub(crate) fn media(&self, owner: i64, id: &str, media_id: &str) -> Result<CapturedMedia> {
        let session = self.get(owner, id)?;
        let mut state = session.state();
        state.touched_at = Instant::now();
        state
            .media
            .get(media_id)
            .cloned()
            .ok_or_else(|| validation("测试媒体已被清理"))
    }

    pub(crate) fn send(
        &self,
        owner: i64,
        id: &str,
        message: SendMessage,
    ) -> Result<MessageReceipt> {
        if message.text.trim().is_empty() || message.text.len() > MAX_TEXT_BYTES {
            return Err(validation("消息不能为空，且最多 16 KiB"));
        }
        let session = self.get(owner, id)?;
        let slot = session
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| validation("该会话同时处理中消息过多，请稍后再试"))?;
        // 互斥锁仅在登记消息与任务时短暂持有，事件处理必须在锁外并发运行：
        // 因为插件调用 `wait_for_reply` 挂起等待下一条回复时，需要放行后续入站消息。
        let mut state = session.state();
        session.ensure_open()?;
        state.touched_at = Instant::now();
        drop(state);
        let mut segments = vec![];
        if message.at_bot && session.group_id.is_some() {
            segments.push(json!({"type":"at","data":{"qq":session.bot_id.to_string()}}));
        }
        segments.push(json!({"type":"text","data":{"text":message.text}}));
        let receipt = session.push("incoming", segments, None, None, vec![])?;
        let event = session.event(&receipt, &message);
        let unified = session.unified_event(event, &message);
        let kernel = self.kernel.clone();
        let sink = Arc::new(sink::SandboxSink::new(Arc::downgrade(&session)));
        let route = session.route.clone();
        let job_id = receipt.id.clone();
        let job_session = session.clone();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            let _slot = slot;
            let before = job_session.state().outgoing_count;
            let process = tokio::time::timeout(
                PROCESS_TIMEOUT,
                kernel.process_unified_event_with_route(
                    "onebot",
                    unified,
                    Some(sink),
                    Some(&route),
                ),
            );
            let result = std::panic::AssertUnwindSafe(process).catch_unwind().await;
            match result {
                Ok(Ok(Ok(result))) => {
                    let replies = job_session.state().outgoing_count.saturating_sub(before);
                    let consumed = if result.blocked {
                        "已被阶段消费或拦截"
                    } else {
                        "已走完流程"
                    };
                    let detail = if replies == 0 {
                        "尚未收到回复，可继续观察异步结果。".to_owned()
                    } else {
                        format!("期间收到 {replies} 条回复。")
                    };
                    job_session.system(format!("消息 {job_id} {consumed}；{detail}"), None);
                }
                Ok(Ok(Err(error))) => job_session.system(
                    format!("消息 {job_id} 处理失败：{error}"),
                    Some(error.to_string()),
                ),
                Ok(Err(_)) => job_session.system(
                    format!("消息 {job_id} 处理超过 120 秒，已停止本次处理。"),
                    Some("processing_timeout".into()),
                ),
                Err(_) => job_session.system(
                    format!("消息 {job_id} 处理发生异常，已停止本次处理。"),
                    Some("processing_panic".into()),
                ),
            }
            job_session.state().jobs.remove(&job_id);
        });
        let mut state = session.state();
        if let Err(error) = session.ensure_open() {
            task.abort();
            return Err(error);
        }
        state.jobs.insert(receipt.id.clone(), task.abort_handle());
        drop(state);
        let _ = start_tx.send(());
        Ok(receipt)
    }
}

impl Drop for SandboxManager {
    fn drop(&mut self) {
        for session in self.sessions().values() {
            session.close(&self.kernel.action_router(), &self.waits);
        }
    }
}

fn validation(message: impl Into<String>) -> NuonoError {
    NuonoError::Validation(message.into())
}
