use crate::error::Result;
use dashmap::DashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

/// 消息发送优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SendPriority {
    /// 系统消息（最高优先级）
    System = 0,
    /// 高优先级消息
    High = 1,
    /// 普通消息
    Normal = 5,
    /// 低优先级消息
    Low = 10,
}

/// 发送请求
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// 消息内容
    pub message: serde_json::Value,
    /// 优先级
    pub priority: SendPriority,
    /// 发送时间戳
    pub timestamp: std::time::Instant,
    /// 追踪 ID
    pub trace_id: Option<String>,
}

impl SendRequest {
    pub fn new(message: serde_json::Value) -> Self {
        Self {
            message,
            priority: SendPriority::Normal,
            timestamp: std::time::Instant::now(),
            trace_id: None,
        }
    }

    pub fn with_priority(mut self, priority: SendPriority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
}

/// Bot 会话状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    /// 连接中
    Connecting,
    /// 在线
    Online,
    /// 离线
    Offline,
    /// 断开连接中
    Disconnecting,
}

/// Bot 会话信息
#[derive(Debug, Clone)]
pub struct BotSessionInfo {
    /// 机器人ID
    pub bot_id: i64,
    /// 平台名称
    pub platform: String,
    /// 连接 IP
    pub ip: String,
    /// 会话状态
    pub status: SessionStatus,
    /// 连接时间
    pub connected_at: std::time::Instant,
    /// 最后心跳时间
    pub last_heartbeat: std::time::Instant,
    /// 接收消息计数
    pub received_count: u64,
    /// 发送消息计数
    pub sent_count: u64,
    /// 错误计数
    pub error_count: u64,
    /// 发送队列深度
    pub queue_depth: usize,
}

/// 负责单个 Bot 连接的消息串行发送。
///
/// 每个 actor 拥有容量为 256 的 FIFO 通道；队列满时 [`SendActor::send`] 会等待，
/// 从而把背压传给调用方。`SendRequest::priority` 当前只用于观测，不参与重排，
/// 也没有自动重试：序列化或 WebSocket 写入失败只计数并丢弃该条消息。
/// 注册替换、注销、显式关闭或最后一个 owner 被释放时，后台任务立即取消，
/// 包括正在等待下游 WebSocket 队列容量的发送；队列中尚未转交的消息随之丢弃。
pub struct SendActor {
    /// 机器人ID
    bot_id: i64,
    /// 发送通道
    tx: mpsc::Sender<SendRequest>,
    /// 会话信息
    info: Arc<RwLock<BotSessionInfo>>,
    closed: Arc<AtomicBool>,
    worker: tokio::task::AbortHandle,
}

struct WorkerClosed(Arc<AtomicBool>);

impl Drop for WorkerClosed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl SendActor {
    /// 创建新的 SendActor
    pub fn new(bot_id: i64, ws_sender: mpsc::Sender<nuono_app::transport::TransportFrame>) -> Self {
        Self::with_capacity(bot_id, ws_sender, 256)
    }

    fn with_capacity(
        bot_id: i64,
        ws_sender: mpsc::Sender<nuono_app::transport::TransportFrame>,
        capacity: usize,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<SendRequest>(capacity);

        let info = Arc::new(RwLock::new(BotSessionInfo {
            bot_id,
            platform: String::new(),
            ip: String::new(),
            status: SessionStatus::Connecting,
            connected_at: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now(),
            received_count: 0,
            sent_count: 0,
            error_count: 0,
            queue_depth: 0,
        }));

        let info_clone = info.clone();
        let bot_id_str = bot_id.to_string();
        let closed = Arc::new(AtomicBool::new(false));
        let worker_closed = WorkerClosed(closed.clone());

        let worker = tokio::spawn(async move {
            let _closed = worker_closed;
            while let Some(request) = rx.recv().await {
                let start = std::time::Instant::now();

                {
                    let mut info = info_clone.write().await;
                    info.queue_depth = rx.len();
                }

                let text = match serde_json::to_string(&request.message) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(bot_id = %bot_id, error = %e, "消息序列化失败");
                        let mut info = info_clone.write().await;
                        info.error_count += 1;
                        continue;
                    }
                };

                match ws_sender
                    .send(nuono_app::transport::TransportFrame::Text(text))
                    .await
                {
                    Ok(_) => {
                        let mut info = info_clone.write().await;
                        info.sent_count += 1;
                        info.queue_depth = rx.len();

                        let duration = start.elapsed();
                        debug!(
                            bot_id = %bot_id,
                            priority = ?request.priority,
                            duration_ms = duration.as_millis(),
                            trace_id = ?request.trace_id,
                            "消息发送成功"
                        );

                        metrics::counter!("nuono_messages_sent_total", "bot_id" => bot_id_str.clone())
                            .increment(1);
                        metrics::histogram!("nuono_message_send_duration_seconds", "bot_id" => bot_id_str.clone())
                            .record(duration.as_secs_f64());
                    }
                    Err(e) => {
                        warn!(bot_id = %bot_id, error = %e, "消息发送失败");
                        let mut info = info_clone.write().await;
                        info.error_count += 1;

                        metrics::counter!("nuono_message_send_errors_total", "bot_id" => bot_id_str.clone())
                            .increment(1);
                        // 当前连接实例的底层传输通道一旦关闭便无法恢复，直接退出发送循环。
                        break;
                    }
                }
            }

            debug!(bot_id = %bot_id, "SendActor 已停止");
        });

        Self {
            bot_id,
            tx,
            info,
            closed,
            worker: worker.abort_handle(),
        }
    }

    /// 发送消息
    pub async fn send(&self, request: SendRequest) -> Result<()> {
        if self.is_closed() {
            return Err(self.closed_error());
        }
        self.tx.send(request).await.map_err(|_| {
            crate::error::NuonoError::Connection(format!("Bot {} 发送通道已关闭", self.bot_id))
        })?;
        Ok(())
    }

    /// 立即尝试排队；广播使用此入口，避免一个满队列阻塞其他 Bot。
    pub fn try_send(&self, request: SendRequest) -> Result<()> {
        if self.is_closed() {
            return Err(self.closed_error());
        }
        self.tx.try_send(request).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                crate::error::NuonoError::Conflict(format!("Bot {} 发送队列已满", self.bot_id))
            }
            mpsc::error::TrySendError::Closed(_) => self.closed_error(),
        })
    }

    fn closed_error(&self) -> crate::error::NuonoError {
        crate::error::NuonoError::Connection(format!("Bot {} 发送通道已关闭", self.bot_id))
    }

    /// 关闭当前连接的发送 Actor，并中止正在等待底层通道容量的发送工作协程。
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.worker.abort();
    }

    /// 发送文本消息
    pub async fn send_text(&self, text: impl Into<String>) -> Result<()> {
        let message = serde_json::json!({
            "action": "send_msg",
            "params": {
                "message": text.into()
            }
        });
        self.send(SendRequest::new(message)).await
    }

    /// 获取会话信息
    pub async fn info(&self) -> BotSessionInfo {
        let mut info = self.info.read().await.clone();
        info.queue_depth = self.tx.max_capacity() - self.tx.capacity();
        if self.is_closed() {
            info.status = SessionStatus::Offline;
        }
        info
    }

    /// 更新会话状态
    pub async fn set_status(&self, status: SessionStatus) {
        let mut info = self.info.write().await;
        info.status = status;
    }

    /// 更新平台信息
    pub async fn set_platform(&self, platform: impl Into<String>) {
        let mut info = self.info.write().await;
        info.platform = platform.into();
    }

    /// 更新 IP 信息
    pub async fn set_ip(&self, ip: impl Into<String>) {
        let mut info = self.info.write().await;
        info.ip = ip.into();
    }

    /// 更新心跳时间
    pub async fn heartbeat(&self) {
        let mut info = self.info.write().await;
        info.last_heartbeat = std::time::Instant::now();
    }

    /// 增加接收计数
    pub async fn increment_received(&self) {
        let mut info = self.info.write().await;
        info.received_count += 1;
    }

    /// 获取 Bot ID
    pub fn bot_id(&self) -> i64 {
        self.bot_id
    }

    /// 检查通道是否已关闭
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.tx.is_closed()
    }
}

impl Drop for SendActor {
    fn drop(&mut self) {
        self.close();
    }
}

/// 活跃 Bot 连接的进程内注册表。
///
/// `bot_id` 是唯一键，重复注册会立即替换并关闭旧 actor。`is_online` 仅表示映射存在，不等同于
/// [`SessionStatus::Online`]，断链回收依赖显式 unregister 或 cleanup。
pub struct BotSessionManager {
    /// 活跃会话映射表
    sessions: DashMap<i64, Arc<SendActor>>,
}

impl BotSessionManager {
    /// 创建新的 BotSessionManager
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// 注册新的 Bot 会话
    pub fn register(&self, bot_id: i64, actor: Arc<SendActor>) {
        if let Some(old) = self.sessions.insert(bot_id, actor.clone())
            && !Arc::ptr_eq(&old, &actor)
        {
            old.close();
        }
        info!(bot_id = %bot_id, "Bot 会话已注册");
        metrics::gauge!("nuono_active_bot_sessions").set(self.sessions.len() as f64);
    }

    /// 注销 Bot 会话
    pub fn unregister(&self, bot_id: i64) -> Option<Arc<SendActor>> {
        let actor = self.sessions.remove(&bot_id).map(|(_, actor)| actor);
        if let Some(actor) = &actor {
            actor.close();
            info!(bot_id = %bot_id, "Bot 会话已注销");
            metrics::gauge!("nuono_active_bot_sessions").set(self.sessions.len() as f64);
        }
        actor
    }

    /// 仅当当前注册项仍为调用方持有的那个连接实例时才执行注销，防止旧连接断开清理时误删重连后的新会话。
    pub fn unregister_if_current(
        &self,
        bot_id: i64,
        expected: &Arc<SendActor>,
    ) -> Option<Arc<SendActor>> {
        let actor = self
            .sessions
            .remove_if(&bot_id, |_, current| Arc::ptr_eq(current, expected))
            .map(|(_, actor)| actor);
        if let Some(actor) = &actor {
            actor.close();
            info!(bot_id = %bot_id, "Bot 会话已注销");
            metrics::gauge!("nuono_active_bot_sessions").set(self.sessions.len() as f64);
        }
        actor
    }

    fn snapshot(&self) -> Vec<Arc<SendActor>> {
        self.sessions
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 获取 Bot 会话
    pub fn get(&self, bot_id: i64) -> Option<Arc<SendActor>> {
        self.sessions.get(&bot_id).map(|r| r.value().clone())
    }

    /// 检查 Bot 是否在线
    pub fn is_online(&self, bot_id: i64) -> bool {
        self.sessions.contains_key(&bot_id)
    }

    /// 获取所有活跃的 Bot ID
    pub fn active_bot_ids(&self) -> Vec<i64> {
        self.sessions.iter().map(|r| *r.key()).collect()
    }

    /// 获取活跃会话数量
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    /// 向指定 Bot 发送消息
    pub async fn send_to_bot(&self, bot_id: i64, request: SendRequest) -> Result<()> {
        if let Some(actor) = self.get(bot_id) {
            actor.send(request).await
        } else {
            Err(crate::error::NuonoError::Connection(format!(
                "Bot {} 未连接",
                bot_id
            )))
        }
    }

    /// 向指定 Bot 发送文本消息
    pub async fn send_text_to_bot(&self, bot_id: i64, text: impl Into<String>) -> Result<()> {
        if let Some(actor) = self.get(bot_id) {
            actor.send_text(text).await
        } else {
            Err(crate::error::NuonoError::Connection(format!(
                "Bot {} 未连接",
                bot_id
            )))
        }
    }

    /// 尽力广播到调用时可见的全部会话。
    ///
    /// 满载或已关闭的队列立即拒绝该条，单个失败仅记录日志并继续其余会话。
    /// 成功仅表示进入 actor 队列，不提供原子“全发/全不发”或已送达保证。
    pub async fn broadcast(&self, request: SendRequest) {
        for actor in self.snapshot() {
            if let Err(e) = actor.try_send(request.clone()) {
                let reason = if matches!(&e, crate::error::NuonoError::Conflict(_)) {
                    "queue_full"
                } else {
                    "queue_closed"
                };
                metrics::counter!("nuono_bot_broadcast_rejected_total", "reason" => reason)
                    .increment(1);
                warn!(bot_id = %actor.bot_id(), error = %e, "广播消息发送失败");
            }
        }
    }

    /// 获取所有会话信息
    pub async fn all_sessions_info(&self) -> Vec<BotSessionInfo> {
        let mut infos = Vec::with_capacity(self.sessions.len());
        for actor in self.snapshot() {
            infos.push(actor.info().await);
        }
        infos
    }

    /// 获取指定会话信息
    pub async fn session_info(&self, bot_id: i64) -> Option<BotSessionInfo> {
        if let Some(actor) = self.get(bot_id) {
            Some(actor.info().await)
        } else {
            None
        }
    }

    /// 清理断开的会话
    pub async fn cleanup_disconnected(&self) {
        for actor in self.snapshot() {
            if actor.is_closed() && self.unregister_if_current(actor.bot_id(), &actor).is_some() {
                warn!(bot_id = %actor.bot_id(), "清理断开的会话");
            }
        }
    }

    /// 获取全局统计信息
    pub async fn global_stats(&self) -> GlobalStats {
        let mut stats = GlobalStats::default();

        for actor in self.snapshot() {
            let info = actor.info().await;
            stats.total_sessions += 1;
            stats.total_received += info.received_count;
            stats.total_sent += info.sent_count;
            stats.total_errors += info.error_count;

            match info.status {
                SessionStatus::Online => stats.online_sessions += 1,
                SessionStatus::Offline => stats.offline_sessions += 1,
                SessionStatus::Connecting => stats.connecting_sessions += 1,
                SessionStatus::Disconnecting => stats.disconnecting_sessions += 1,
            }
        }

        stats
    }
}

impl Default for BotSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BotSessionManager {
    fn drop(&mut self) {
        for actor in self.snapshot() {
            actor.close();
        }
    }
}

/// 全局统计信息
#[derive(Debug, Clone, Default)]
pub struct GlobalStats {
    /// 总会话数
    pub total_sessions: usize,
    /// 在线会话数
    pub online_sessions: usize,
    /// 离线会话数
    pub offline_sessions: usize,
    /// 连接中会话数
    pub connecting_sessions: usize,
    /// 断开中会话数
    pub disconnecting_sessions: usize,
    /// 总接收消息数
    pub total_received: u64,
    /// 总发送消息数
    pub total_sent: u64,
    /// 总错误数
    pub total_errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_manager_register_unregister() {
        let manager = BotSessionManager::new();
        let (tx, _rx) = mpsc::channel(10);
        let actor = Arc::new(SendActor::new(123, tx));

        assert!(!manager.is_online(123));
        assert_eq!(manager.active_count(), 0);

        manager.register(123, actor);
        assert!(manager.is_online(123));
        assert_eq!(manager.active_count(), 1);

        manager.unregister(123);
        assert!(!manager.is_online(123));
        assert_eq!(manager.active_count(), 0);
    }

    #[tokio::test]
    async fn test_session_manager_active_bot_ids() {
        let manager = BotSessionManager::new();
        let (tx1, _rx1) = mpsc::channel(10);
        let (tx2, _rx2) = mpsc::channel(10);

        manager.register(100, Arc::new(SendActor::new(100, tx1)));
        manager.register(200, Arc::new(SendActor::new(200, tx2)));

        let mut ids = manager.active_bot_ids();
        ids.sort();
        assert_eq!(ids, vec![100, 200]);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/bot_session_fault_tests.rs"]
mod fault_tests;
