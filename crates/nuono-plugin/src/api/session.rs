use crate::api::bot::{Bot, SendableMessage};
use crate::api::button::ButtonOne;
use crate::api::segment::MessageSegment;
use crate::error::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[cfg(test)]
#[path = "../../tests/unit/api/session_wait_fault_tests.rs"]
mod wait_fault_tests;

type SessionInstance = (Arc<Mutex<Bot>>, std::time::Instant, u64);
#[derive(Default)]
struct SessionState {
    instances: HashMap<String, SessionInstance>,
    multiply_instances: HashMap<String, SessionInstance>,
    multiply_map: HashMap<String, String>,
    next: u64,
}
/// 兼容旧接口的交互会话实例索引（通过短临界区互斥锁保证普通会话与多人会话两个索引的一致性）。
/// 实际的活跃回复等待状态及其插件归属均由 `WaitManager` 统一维护。
#[derive(Default)]
pub struct SessionManager {
    state: Arc<std::sync::Mutex<SessionState>>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager").finish_non_exhaustive()
    }
}
struct SessionRegistration {
    state: Arc<std::sync::Mutex<SessionState>>,
    session: String,
    generation: u64,
    multiply: bool,
}
impl Drop for SessionRegistration {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("锁已中毒");
        let map = if self.multiply {
            &mut state.multiply_instances
        } else {
            &mut state.instances
        };
        if map
            .get(&self.session)
            .is_some_and(|(_, _, generation)| *generation == self.generation)
        {
            map.remove(&self.session);
            if self.multiply {
                state
                    .multiply_map
                    .retain(|_, session| session != &self.session);
            }
        }
    }
}
impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn session_id(bot_id: i64, group_id: Option<i64>, user_id: i64) -> String {
        match group_id {
            Some(group) => format!("{bot_id}:{group}:{user_id}"),
            None => format!("{bot_id}:private:{user_id}"),
        }
    }
    fn insert(&self, session: String, bot: Bot, temp: Option<String>) -> u64 {
        let mut state = self.state.lock().expect("锁已中毒");
        state.next = state.next.wrapping_add(1);
        let generation = state.next;
        let entry = (
            Arc::new(Mutex::new(bot)),
            std::time::Instant::now(),
            generation,
        );
        if let Some(temp) = temp {
            state.multiply_instances.insert(session.clone(), entry);
            state
                .multiply_map
                .retain(|_, existing| existing != &session);
            state.multiply_map.insert(temp, session);
        } else {
            state.instances.insert(session, entry);
        }
        generation
    }
    fn register_guard(&self, session: String, bot: Bot, multiply: bool) -> SessionRegistration {
        let temp = multiply.then(|| format!("{session}:multiply"));
        let generation = self.insert(session.clone(), bot, temp);
        SessionRegistration {
            state: self.state.clone(),
            session,
            generation,
            multiply,
        }
    }
    pub async fn register_instance(&self, session_id: String, bot: Bot) {
        self.insert(session_id, bot, None);
    }
    pub async fn register_multiply_instance(&self, session_id: String, temp_gid: String, bot: Bot) {
        self.insert(session_id, bot, Some(temp_gid));
    }
    pub async fn get_instance(&self, session: &str) -> Option<Arc<Mutex<Bot>>> {
        self.state
            .lock()
            .expect("锁已中毒")
            .instances
            .get(session)
            .map(|entry| entry.0.clone())
    }
    pub async fn get_multiply_instance(&self, session: &str) -> Option<Arc<Mutex<Bot>>> {
        self.state
            .lock()
            .expect("锁已中毒")
            .multiply_instances
            .get(session)
            .map(|entry| entry.0.clone())
    }
    pub async fn get_multiply_instance_by_gid(&self, temp: &str) -> Option<Arc<Mutex<Bot>>> {
        let state = self.state.lock().expect("锁已中毒");
        state
            .multiply_map
            .get(temp)
            .and_then(|session| state.multiply_instances.get(session))
            .map(|entry| entry.0.clone())
    }
    pub async fn remove_instance(&self, session: &str) -> Option<Arc<Mutex<Bot>>> {
        self.state
            .lock()
            .expect("锁已中毒")
            .instances
            .remove(session)
            .map(|entry| entry.0)
    }
    pub async fn remove_multiply_instance(&self, session: &str) -> Option<Arc<Mutex<Bot>>> {
        let mut state = self.state.lock().expect("锁已中毒");
        state.multiply_map.retain(|_, existing| existing != session);
        state
            .multiply_instances
            .remove(session)
            .map(|entry| entry.0)
    }
    pub async fn clear_all(&self) {
        let mut state = self.state.lock().expect("锁已中毒");
        state.instances.clear();
        state.multiply_instances.clear();
        state.multiply_map.clear();
    }
    pub async fn cleanup_expired(&self, max_age_secs: u64) {
        let mut state = self.state.lock().expect("锁已中毒");
        let max_age = std::time::Duration::from_secs(max_age_secs);
        state
            .instances
            .retain(|_, (_, time, _)| time.elapsed() < max_age);
        state
            .multiply_instances
            .retain(|_, (_, time, _)| time.elapsed() < max_age);
        let alive: std::collections::HashSet<_> =
            state.multiply_instances.keys().cloned().collect();
        state
            .multiply_map
            .retain(|_, session| alive.contains(session));
    }
    pub async fn instance_count(&self) -> (usize, usize) {
        let state = self.state.lock().expect("锁已中毒");
        (state.instances.len(), state.multiply_instances.len())
    }
}

/// 交互式会话 — 用于多轮对话
///
/// 对应 gsuid_core 的 Bot.receive_resp / Bot.receive_mutiply_resp
pub struct InteractiveSession {
    session_id: String,
    bot: Arc<Mutex<Bot>>,
    session_manager: Arc<SessionManager>,
    is_multiply: bool,
}

impl std::fmt::Debug for InteractiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveSession")
            .field("session_id", &self.session_id)
            .field("is_multiply", &self.is_multiply)
            .finish_non_exhaustive()
    }
}

impl InteractiveSession {
    /// 创建新的交互式会话
    pub fn new(
        session_id: String,
        bot: Bot,
        session_manager: Arc<SessionManager>,
        is_multiply: bool,
    ) -> Self {
        Self {
            session_id,
            bot: Arc::new(Mutex::new(bot)),
            session_manager,
            is_multiply,
        }
    }

    /// 在发出提示消息前预先注册回复接收器与会话实例。
    /// 无论正常收到回复、发送失败、超时、取消还是调用方直接丢弃 Future，RAII 守卫都只会清理当前这一代（generation）注册。
    pub async fn send_and_wait(
        &self,
        message: impl Into<SendableMessage>,
        buttons: Option<Vec<Vec<ButtonOne>>>,
        timeout_secs: u64,
    ) -> Result<Option<Value>> {
        let bot = self.bot.lock().await;

        let mut segments = Vec::new();
        if let Some(mut btns) = buttons {
            // 未显式指定动作类型的按钮在此处默认作为当前等待的交互回复；显式声明的指令按钮仍保持为指令。
            for button in btns.iter_mut().flatten() {
                if button.action == crate::api::button::ButtonAction::Auto {
                    button.action = crate::api::button::ButtonAction::Reply;
                }
            }
            segments.push(MessageSegment::buttons(btns));
        }

        let msg = message.into();
        match msg {
            SendableMessage::Text(s) | SendableMessage::Json(s) => {
                segments.push(MessageSegment::text(s));
            }
            SendableMessage::Segment(seg) => segments.push(seg),
            SendableMessage::Segments(segs) => segments.extend(segs),
        }

        let timeout = std::time::Duration::from_secs(timeout_secs);
        let registration = bot.prepare_reply_wait(
            timeout,
            crate::api::waiter::ReplyWaitMode::from_multiply(self.is_multiply),
        )?;
        let _instance = self.session_manager.register_guard(
            self.session_id.clone(),
            bot.clone(),
            self.is_multiply,
        );
        // 注册的截止时间同时涵盖提示消息的预处理与发送阶段；
        // 一旦等待被取消或超时，即便底层传输层仍卡在发送中，也会立即退出并由守卫清理等待注册与会话索引。
        tokio::select! {
            biased;
            error = registration.cancelled() => return Err(error),
            sent = bot.send(SendableMessage::Segments(segments)) => sent?,
        }
        let reply = registration.receive(timeout).await?;

        Ok(Some(reply))
    }

    /// 发送选项消息（不等待回复）
    ///
    /// 对应 gsuid_core 的 Bot.send_option
    pub async fn send_options(
        &self,
        message: impl Into<SendableMessage>,
        buttons: Vec<Vec<ButtonOne>>,
    ) -> Result<()> {
        let bot = self.bot.lock().await;

        let mut segments = vec![MessageSegment::buttons(buttons)];
        let msg = message.into();
        match msg {
            SendableMessage::Text(s) | SendableMessage::Json(s) => {
                segments.push(MessageSegment::text(s));
            }
            SendableMessage::Segment(seg) => segments.push(seg),
            SendableMessage::Segments(segs) => segments.extend(segs),
        }

        bot.send(SendableMessage::Segments(segments)).await
    }
}

/// 全局会话管理器单例
static SESSION_MANAGER: once_cell::sync::Lazy<Arc<SessionManager>> =
    once_cell::sync::Lazy::new(|| Arc::new(SessionManager::new()));

/// 获取全局会话管理器实例
pub fn global_session_manager() -> Arc<SessionManager> {
    SESSION_MANAGER.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{MessageContext, waiter::WaitManager},
        core::{event_bus::EventBus, event_kernel::RuntimeActionSink, onebot_client::ApiClient},
    };
    use serde_json::json;
    struct PromptSink {
        waits: Arc<WaitManager>,
        reply: bool,
        fail: bool,
    }
    #[async_trait::async_trait]
    impl RuntimeActionSink for PromptSink {
        async fn send_action(&self, _: Value) -> Result<()> {
            assert!(
                self.waits.has_wait("42:private:7"),
                "must register before prompt is visible"
            );
            if self.fail {
                return Err(crate::error::NuonoError::Other("send failed".into()));
            }
            if self.reply {
                assert!(
                    self.waits
                        .respond("42:private:7", json!({"raw_message":"instant"}))
                );
            }
            Ok(())
        }
    }
    fn fixture(
        reply: bool,
        fail: bool,
        multiply: bool,
    ) -> (
        Arc<WaitManager>,
        Arc<SessionManager>,
        Arc<InteractiveSession>,
    ) {
        let waits = Arc::new(WaitManager::new());
        let sink = Arc::new(PromptSink {
            waits: waits.clone(),
            reply,
            fail,
        });
        let mut bot = Bot::new(
            42,
            Arc::new(EventBus::new()),
            waits.clone(),
            Arc::new(ApiClient::new()),
            None,
            Some(sink),
        )
        .with_wait_owner("fixture", None);
        bot.set_current_message(
            MessageContext::from_onebot_json(
                &json!({"post_type":"message","message_type":"private","self_id":42,"user_id":7}),
            )
            .unwrap(),
        );
        let sessions = Arc::new(SessionManager::new());
        let session = Arc::new(InteractiveSession::new(
            "42:private:7".into(),
            bot,
            sessions.clone(),
            multiply,
        ));
        (waits, sessions, session)
    }
    #[tokio::test]
    async fn immediate_reply_is_received_and_send_failure_cleans_both_indices() {
        for multiply in [false, true] {
            let (waits, sessions, session) = fixture(true, false, multiply);
            assert_eq!(
                session
                    .send_and_wait("prompt", None, 5)
                    .await
                    .unwrap()
                    .unwrap()["raw_message"],
                "instant"
            );
            assert_eq!(waits.wait_count(), 0);
            assert_eq!(sessions.instance_count().await, (0, 0));
            let (waits, sessions, session) = fixture(false, true, multiply);
            assert!(session.send_and_wait("prompt", None, 5).await.is_err());
            assert_eq!(waits.wait_count(), 0);
            assert_eq!(sessions.instance_count().await, (0, 0));
        }
    }
    #[tokio::test]
    async fn cancelled_interaction_cleans_real_wait_and_instance_indices() {
        let (waits, sessions, session) = fixture(false, false, true);
        let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 60).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while waits.wait_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(waits.wait_count(), 0);
        assert_eq!(sessions.instance_count().await, (0, 0));
    }
    #[tokio::test]
    async fn native_channel_registration_uses_the_same_key_as_inbound_delivery() {
        let waits = Arc::new(WaitManager::new());
        let event = json!({"post_type":"message","message_type":"channel","self_id":42,"user_id":"native-user","channel_id":"native-channel"});
        let mut bot = Bot::new(
            42,
            Arc::new(EventBus::new()),
            waits.clone(),
            Arc::new(ApiClient::new()),
            None,
            None,
        )
        .with_wait_owner("fixture", None);
        bot.set_current_message(MessageContext::from_onebot_json(&event).unwrap());
        let pending = bot
            .prepare_reply_wait(
                std::time::Duration::from_secs(10),
                crate::api::waiter::ReplyWaitMode::Single,
            )
            .unwrap();
        assert_eq!(
            pending.snapshot().scope.group_id.as_deref(),
            Some("native-channel")
        );
        assert!(waits.respond(&crate::onebot::build_session_id(42, &event), event.clone()));
        assert_eq!(pending.await.unwrap(), event);
    }
}
