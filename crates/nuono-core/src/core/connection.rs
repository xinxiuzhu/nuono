use crate::api::waiter::WaitManager;
use crate::core::bot_session::{BotSessionManager, SendActor};
use crate::core::config_change::ConfigChangeManager;
use crate::core::event_bus::EventBus;
use crate::core::event_kernel::{EventKernel, EventKernelPipeline};
use crate::core::handler::{CommandDispatchServices, EventHandler};
use crate::core::message_log::{
    OUTBOUND_PARSE_LIMIT, log_send, log_send_outcome, outbound_action_from_ws_message,
    outbound_summary, oversized_outbound_summary,
};
use crate::core::onebot_client::ApiClient;
use crate::core::platform::PlatformAdapter;
use crate::core::plugin_host::PluginHost;
use crate::core::{RuntimeActionSink, RuntimeBotRoute};
use crate::core::{
    filter::FilterHandler,
    framework_control::FrameworkControl,
    matcher::{MatcherRegistry, SessionManager},
    paths::Paths,
    permission::PermissionManager,
    subscribe::SubscribeManager,
    system_monitor::SystemMonitor,
    task_executor::TaskExecutor,
};
use crate::db::Database;
use crate::error::Result;
use crate::service::scheduler::{Scheduler, handle_scheduled_delivery};
use dashmap::DashMap;
use futures::{sink::SinkExt, stream::StreamExt};
use nuono_app::transport::TransportFrame;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

const MSG_CHANNEL_SIZE: usize = 100;
const EVENT_CHANNEL_SIZE: usize = 100;

#[path = "connection_status.rs"]
mod status;

#[derive(Default)]
struct ConnectionSessionState {
    current: Option<RegisteredConnectionSession>,
    closed: bool,
}

struct RegisteredConnectionSession {
    actor: Arc<SendActor>,
    sender: mpsc::Sender<TransportFrame>,
    route: RuntimeBotRoute,
    generation: u64,
}

struct PendingConnectionSession {
    actor: Arc<SendActor>,
    sender: mpsc::Sender<TransportFrame>,
    route: RuntimeBotRoute,
    sink: Arc<dyn RuntimeActionSink>,
    ip: String,
    heartbeat_timeout: std::time::Duration,
}

#[derive(Debug, PartialEq, Eq)]
enum InboundAdmissionError {
    Full,
    Closed,
}

struct ConnectionSessionRegistration {
    manager: Arc<BotSessionManager>,
    bot_txs: Arc<DashMap<i64, mpsc::Sender<TransportFrame>>>,
    bot_states: Arc<crate::core::bot_state::BotStateRegistry>,
    router: Arc<crate::core::RuntimeActionRouter>,
    state: std::sync::Mutex<ConnectionSessionState>,
}

impl ConnectionSessionRegistration {
    /// 在同步代码块内一次性完成连接注册（中间不跨越 `.await`）。
    /// 断开清理与迟到的读取方均通过该状态同步，避免异步取消导致已注销连接被重新发布。
    fn register(&self, pending: PendingConnectionSession) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .expect("connection session mutex poisoned");
        if state.closed || state.current.is_some() {
            pending.actor.close();
            return None;
        }
        let generation = self.router.register(pending.route.clone(), pending.sink);
        state.current = Some(RegisteredConnectionSession {
            actor: pending.actor,
            sender: pending.sender,
            route: pending.route,
            generation,
        });
        let current = state.current.as_ref().expect("registration just inserted");
        let bot_id = current.actor.bot_id();
        self.bot_txs.insert(bot_id, current.sender.clone());
        self.bot_states.register(
            bot_id,
            generation,
            current.sender.clone(),
            pending.ip,
            current.route.platform.clone(),
            pending.heartbeat_timeout,
        );
        self.manager.register(bot_id, current.actor.clone());
        Some(generation)
    }

    fn route(&self) -> Option<(RuntimeBotRoute, u64)> {
        self.state
            .lock()
            .expect("connection session mutex poisoned")
            .current
            .as_ref()
            .map(|current| (current.route.clone(), current.generation))
    }

    /// 事件入队准入与连接清理共用同一把锁，防止持旧路由快照的调用方在连接关闭后继续塞入业务事件。
    fn enqueue_event(
        &self,
        sender: &mpsc::Sender<InboundPostEvent>,
        mut event: InboundPostEvent,
    ) -> std::result::Result<(), InboundAdmissionError> {
        let state = self
            .state
            .lock()
            .expect("connection session mutex poisoned");
        if state.closed || (event.bot_id != 0 && state.current.is_none()) {
            return Err(InboundAdmissionError::Closed);
        }
        event.route = state.current.as_ref().map(|current| current.route.clone());
        let admission = sender.try_send(event);
        drop(state);
        admission.map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => InboundAdmissionError::Full,
            mpsc::error::TrySendError::Closed(_) => InboundAdmissionError::Closed,
        })
    }

    /// 四个注册表在清理时都会比对连接批次号（generation）；
    /// 即使新连接替换与旧连接清理并发执行，旧连接也不会误删新连接注册的条目。
    fn cleanup(&self) -> bool {
        let current = {
            let mut state = self
                .state
                .lock()
                .expect("connection session mutex poisoned");
            state.closed = true;
            state.current.take()
        };
        let Some(current) = current else {
            return false;
        };
        let bot_id = current.actor.bot_id();
        let removed_current = remove_bot_sender_if_current(&self.bot_txs, bot_id, &current.sender);
        self.bot_states.remove(bot_id, current.generation);
        self.router.unregister(&current.route, current.generation);
        crate::core::media_links::forget_route_base(&current.route, current.generation);
        current.actor.close();
        self.manager.unregister_if_current(bot_id, &current.actor);
        removed_current
    }
}

/// 连接主循环（driver）独立于读取协程持有该 RAII 守卫；
/// 一旦主循环被丢弃，即使未能走完异步断开清理流程，也会在析构时同步清理所有进程内注册项。
struct ConnectionSessionGuard {
    registration: Arc<ConnectionSessionRegistration>,
}

impl ConnectionSessionGuard {
    fn new(
        manager: Arc<BotSessionManager>,
        bot_txs: Arc<DashMap<i64, mpsc::Sender<TransportFrame>>>,
        bot_states: Arc<crate::core::bot_state::BotStateRegistry>,
        router: Arc<crate::core::RuntimeActionRouter>,
    ) -> Self {
        Self {
            registration: Arc::new(ConnectionSessionRegistration {
                manager,
                bot_txs,
                bot_states,
                router,
                state: std::sync::Mutex::new(ConnectionSessionState::default()),
            }),
        }
    }
}

impl Drop for ConnectionSessionGuard {
    fn drop(&mut self) {
        self.registration.cleanup();
    }
}

struct InboundPostEvent {
    received_at: std::time::Instant,
    receipt: Option<crate::core::plugin_host::trigger_control::QueuedReceipt>,
    bot_id: i64,
    text: String,
    event: serde_json::Value,
    route: Option<RuntimeBotRoute>,
}

async fn deliver_waiting(kernel: &EventKernel, event: &InboundPostEvent) -> bool {
    let started = std::time::Instant::now();
    let consumed = kernel
        .try_deliver_waiting_post_event(
            event.bot_id,
            &event.text,
            &event.event,
            event.route.as_ref(),
        )
        .await;
    if consumed {
        metrics::counter!("nuono_ws_messages_processed_total", "bot_id" => event.bot_id.to_string()).increment(1);
        metrics::histogram!("nuono_ws_message_duration_seconds", "bot_id" => event.bot_id.to_string()).record(started.elapsed().as_secs_f64());
    }
    consumed
}

async fn run_ordered_events(
    kernel: Arc<EventKernel>,
    mut events: mpsc::Receiver<InboundPostEvent>,
    sender: mpsc::Sender<TransportFrame>,
) -> &'static str {
    let mut pending = std::collections::VecDeque::<InboundPostEvent>::new();
    let mut changes = kernel.wait_registration_changes();
    // 每条连接同一时刻最多运行一个常规业务回调。当准入检查等待数据库连接时，
    // 已在运行的回调仍可继续推进并释放所持有的连接，避免死锁。
    // 若派发器或连接被取消，JoinSet 会自动中止该任务。
    let mut business = tokio::task::JoinSet::new();
    loop {
        let next = match pending.pop_front() {
            Some(event) => event,
            None => match events.recv().await {
                Some(event) => event,
                None => return "event_channel_closed",
            },
        };
        let bot_id = next.bot_id;
        let started = std::time::Instant::now();
        let runtime = kernel.clone();
        let output = sender.clone();
        business.spawn(async move {
            let _receipt = next.receipt;
            if let Err(error) = crate::core::plugin_host::trigger_control::with_received_at(next.received_at, runtime.process_runtime_post_event_with_route(
                bot_id, &next.text, next.event, output, next.route.as_ref(),
            )).await { info!(bot_id, %error, "⚠️ EventKernel runtime 执行失败"); }
            metrics::counter!("nuono_ws_messages_processed_total", "bot_id" => bot_id.to_string()).increment(1);
            metrics::histogram!("nuono_ws_message_duration_seconds", "bot_id" => bot_id.to_string()).record(started.elapsed().as_secs_f64());
        });
        loop {
            tokio::select! {
                result = business.join_next() => {
                    match result {
                        Some(Ok(())) => {},
                        Some(Err(error)) => {
                            warn!(bot_id, %error, "OneBot 事件处理任务异常退出");
                            return "event_worker_failed";
                        }
                        None => return "event_worker_missing",
                    }
                    break;
                }
                changed = changes.changed() => {
                    if changed.is_err() { return "wait_registry_closed"; }
                    // 用户的快速回复可能赶在回调完成等待注册之前就已经入队。
                    // 每次有新等待会话注册时都会唤醒派发器，重新扫描队列中的有界积压事件。
                    let count = pending.len();
                    for _ in 0..count {
                        let event = pending.pop_front().unwrap();
                        if !deliver_waiting(&kernel, &event).await { pending.push_back(event); }
                    }
                }
                next = events.recv() => {
                    let Some(event) = next else { return "event_channel_closed"; };
                    if deliver_waiting(&kernel, &event).await { continue; }
                    if pending.len() >= EVENT_CHANNEL_SIZE {
                        metrics::counter!("nuono_ws_event_queue_overloads_total", "bot_id" => event.bot_id.to_string()).increment(1);
                        warn!(capacity = EVENT_CHANNEL_SIZE, "业务入站等待队列已满，关闭连接");
                        return "event_backlog_full";
                    }
                    pending.push_back(event);
                }
            }
        }
    }
}

/// OneBot 反向 WebSocket 的连接与服务生命周期管理器。
///
/// 每条连接有独立的 socket writer、reader 和串行事件 worker；reader 先识别 bot、
/// 截获 `echo` 回包，再把 `post_type` 事件入队。worker 执行 [`EventKernel`] 的管线，
/// 等待 action 回执时不会阻塞 reader。具体的等待会话、
/// 过滤、插件和触发器顺序由 `EventKernel` 持有，不在连接层重复编排。Telegram、
/// Discord、Feishu 不经过这里，而是直接向同一个 runtime 管线提交 `UnifiedEvent`。
pub struct ConnectionManager {
    /// 已完成 self_id 握手识别的活动连接。尚未识别的 socket 不可被 API 调用寻址；
    /// 相同 self_id 再次注册会覆盖映射，断开时再移除该 key。
    pub bot_txs: Arc<DashMap<i64, mpsc::Sender<TransportFrame>>>,
    pub bot_states: Arc<crate::core::bot_state::BotStateRegistry>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    business_gate: nuono_app::business_runtime::BusinessGate,
    business_start: tokio::sync::Mutex<()>,

    pub db: Database,
    pub paths: Paths,
    pub event_bus: Arc<EventBus>,
    pub permission_manager: Arc<PermissionManager>,
    pub filter_handler: Arc<FilterHandler>,
    pub plugin_host: Arc<PluginHost>,
    pub api_client: Arc<ApiClient>,
    pub wait_manager: Arc<WaitManager>,
    pub adapter: Arc<dyn PlatformAdapter>,
    pub event_handler: Arc<EventHandler>,
    pub event_kernel: Arc<EventKernel>,
    pub scheduler: Arc<Scheduler>,
    pub subscribe_manager: Arc<SubscribeManager>,
    pub matcher_registry: Arc<MatcherRegistry>,
    pub session_manager: Arc<SessionManager>,
    pub system_monitor: Arc<SystemMonitor>,
    pub config: Arc<crate::config::ConfigManager>,
    pub framework_control: FrameworkControl,
    pub core_commands: Arc<crate::core_commands::CoreCommands>,
    pub bot_session_manager: Arc<BotSessionManager>,
    pub task_executor: Arc<TaskExecutor>,
    pub config_change_manager: Arc<ConfigChangeManager>,
}

type AvatarSource =
    dyn Fn(&nuono_app::media::UserAvatarRequest) -> Result<Option<String>> + Send + Sync;

struct OneBotRuntimeActionSink {
    api_client: Arc<ApiClient>,
    sender: mpsc::Sender<TransportFrame>,
    avatar_resolver: Arc<dyn nuono_app::media::ImageResolver>,
    avatar_source: Arc<AvatarSource>,
}

#[async_trait::async_trait]
impl RuntimeActionSink for OneBotRuntimeActionSink {
    async fn send_action(&self, action: serde_json::Value) -> Result<()> {
        let mut action = action;
        let action_name = action
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| crate::error::NuonoError::Validation("action is required".into()))?
            .to_string();
        if action_name == "read_user_avatar" {
            return Err(crate::error::NuonoError::Validation(
                "头像读取需要 call_action 返回结果".into(),
            ));
        }
        if action_name == "send_file" {
            return Err(crate::error::NuonoError::Validation(
                "文件发送需要 call_action 返回结果".into(),
            ));
        }
        if action_name == "read_inbound_file" {
            return Err(crate::error::NuonoError::Validation(
                "文件读取需要 call_action 返回结果".into(),
            ));
        }
        // 取出参数而不是克隆：出站帧可能内嵌数十 MB 的 Base64 媒体。
        let params = action
            .get_mut("params")
            .map(serde_json::Value::take)
            .filter(|value| !value.is_null())
            .unwrap_or_else(|| serde_json::json!({}));
        self.api_client
            .send(&action_name, params, &self.sender)
            .await
            .map_err(crate::error::NuonoError::from)
    }

    async fn call_action(&self, action: serde_json::Value) -> Result<serde_json::Value> {
        let mut action = action;
        let action_name = action
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| crate::error::NuonoError::Validation("action is required".into()))?
            .to_string();
        let params = action
            .get_mut("params")
            .map(serde_json::Value::take)
            .filter(|value| !value.is_null())
            .unwrap_or_else(|| serde_json::json!({}));
        if action_name == "send_file" {
            let request: nuono_app::media::FileSendRequest = serde_json::from_value(params)
                .map_err(|_| {
                    crate::error::NuonoError::Validation("OneBot 文件发送请求无效".into())
                })?;
            request.validate()?;
            // OneBot v11 标准没有通用文件上传 API，不能将该跨平台动作盲目转发给语义各异的协议端扩展接口。
            return Ok(serde_json::json!({"status":"unsupported"}));
        }
        if action_name == "read_user_avatar" {
            return resolve_onebot_avatar(
                &params,
                self.avatar_resolver.as_ref(),
                self.avatar_source.as_ref(),
            )
            .await;
        }
        if action_name == "read_inbound_file" {
            let request: nuono_app::media::InboundFileRequest = serde_json::from_value(params)
                .map_err(|_| {
                    crate::error::NuonoError::Validation("OneBot 入站文件请求无效".into())
                })?;
            request.validate()?;
            // 文件通知事件与协议端特有的文件 ID 需要结合上下文解析，不能将该框架级动作直接当作标准 OneBot API 转发。
            return Ok(serde_json::json!({"status":"unsupported"}));
        }
        self.api_client
            .call(&action_name, params, &self.sender)
            .await
            .map_err(crate::error::NuonoError::from)
    }
}

async fn resolve_onebot_avatar(
    params: &serde_json::Value,
    resolver: &dyn nuono_app::media::ImageResolver,
    source: &AvatarSource,
) -> Result<serde_json::Value> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let request: nuono_app::media::UserAvatarRequest = serde_json::from_value(params.clone())
        .map_err(|_| crate::error::NuonoError::Validation("OneBot 头像请求无效".into()))?;
    request.validate()?;
    let Some(source) = source(&request)? else {
        return Ok(serde_json::json!({"status":"unsupported"}));
    };
    let image = resolver.resolve_image(&source, request.max_bytes).await?;
    if image.bytes.is_empty() || image.bytes.len() > request.max_bytes {
        return Err(crate::error::NuonoError::Validation(
            "OneBot 头像内容为空或超过限制".into(),
        ));
    }
    Ok(
        serde_json::json!({"status":"available","base64":STANDARD.encode(image.bytes),"version":null}),
    )
}

/// 出站帧的单行日志摘要。
///
/// 小帧走已有完整解析；超过 [`OUTBOUND_PARSE_LIMIT`] 的帧（内嵌 Base64 媒体）只做
/// 一次线性扫描，避免为一行 INFO 日志构建巨大的 JSON 树。
async fn outbound_log_summary(
    config: &Arc<crate::config::ConfigManager>,
    platform: &str,
    message: &TransportFrame,
) -> Option<crate::core::message_log::MessageLogSummary> {
    let config = config.snapshot().await;
    if !config.log.show_send {
        return None;
    }
    let max_chars = config.log.message_preview_chars;
    match message {
        TransportFrame::Text(text) if text.len() > OUTBOUND_PARSE_LIMIT => {
            oversized_outbound_summary(platform, text, max_chars)
        }
        _ => outbound_action_from_ws_message(message)
            .and_then(|action| outbound_summary(platform, &action, max_chars)),
    }
}

fn remove_bot_sender_if_current(
    bot_txs: &DashMap<i64, mpsc::Sender<TransportFrame>>,
    bot_id: i64,
    connection_sender: &mpsc::Sender<TransportFrame>,
) -> bool {
    bot_txs
        .remove_if(&bot_id, |_, current| {
            current.same_channel(connection_sender)
        })
        .is_some()
}

async fn send_to_registered_bot(
    bot_txs: &DashMap<i64, mpsc::Sender<TransportFrame>>,
    bot_id: i64,
    text: String,
) -> Result<()> {
    let sender = bot_txs.get(&bot_id).map(|entry| entry.value().clone());
    let Some(sender) = sender else {
        return Err(crate::error::NuonoError::Connection(format!(
            "bot {} 未连接",
            bot_id
        )));
    };
    sender
        .send(TransportFrame::Text(text))
        .await
        .map_err(|_| crate::error::NuonoError::Connection("bot 消息通道已关闭".into()))
}

impl ConnectionManager {
    #[tracing::instrument(skip(db, paths, adapter, config, plugin_config_manager), fields(db.pool_size = tracing::field::Empty))]
    pub async fn new(
        db: Database,
        paths: Paths,
        adapter: Arc<dyn PlatformAdapter>,
        config: Arc<crate::config::ConfigManager>,
        plugin_config_manager: Arc<crate::config::PluginConfigManager>,
    ) -> Result<Arc<Self>> {
        let manager = Self::new_prepared(db, paths, adapter, config, plugin_config_manager).await?;
        manager.start_business().await?;
        Ok(manager)
    }

    /// 准备本地资源与插件声明，等待管理服务和管理员设置完成后显式启动业务。
    pub async fn new_prepared(
        db: Database,
        paths: Paths,
        adapter: Arc<dyn PlatformAdapter>,
        config: Arc<crate::config::ConfigManager>,
        plugin_config_manager: Arc<crate::config::PluginConfigManager>,
    ) -> Result<Arc<Self>> {
        let business_gate = nuono_app::business_runtime::BusinessGate::paused();
        let system_monitor = crate::core::system_monitor::init_global_monitor();

        let permission_manager = Arc::new(PermissionManager::new(db.clone()));
        let filter_handler = Arc::new(FilterHandler::new(db.clone()));
        let api_client = Arc::new(ApiClient::new());
        let wait_manager = Arc::new(WaitManager::new());
        let event_bus = Arc::new(EventBus::new());
        let framework_control = FrameworkControl::new();

        let locales = crate::localization::service(db.clone(), config.clone());
        let plugin_host = PluginHost::new_prepared_with_locales_and_image_output(
            db.clone(),
            event_bus.clone(),
            permission_manager.clone(),
            paths.clone(),
            wait_manager.clone(),
            framework_control.clone(),
            plugin_config_manager.clone(),
            locales,
            crate::core::plugin_host::image_output::for_config(config.clone()),
        )
        .await?;

        let mut event_handler = EventHandler::new(
            db.clone(),
            CommandDispatchServices {
                trigger_registry: plugin_host.trigger_registry(),
                cooldown: plugin_host.cooldown_tracker.clone(),
                stats: plugin_host.command_stats.clone(),
            },
            permission_manager.clone(),
            event_bus.clone(),
            wait_manager.clone(),
            api_client.clone(),
        );
        event_handler.set_plugin_host(plugin_host.clone());
        let event_handler = Arc::new(event_handler);

        let (scheduler, scheduler_rx) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler.with_business_gate(business_gate.clone()));

        let subscribe_manager = SubscribeManager::new(db.clone());
        let session_manager = Arc::new(SessionManager::new());
        let matcher_registry = Arc::new(MatcherRegistry::new(session_manager.clone()));
        let bot_session_manager = Arc::new(BotSessionManager::new());
        let task_executor = Arc::new(TaskExecutor::new(10));
        let config_change_manager = Arc::new(ConfigChangeManager::new(event_bus.clone()));

        let core_commands = Arc::new(
            crate::core_commands::CoreCommands::new(
                config.clone(),
                permission_manager.clone(),
                system_monitor.clone(),
            )
            .with_action_router(plugin_host.action_router())
            .with_status_source(db.clone(), plugin_host.clone(), paths.data.clone()),
        );

        let event_kernel = Arc::new(
            EventKernel::new(
                db.clone(),
                event_bus.clone(),
                permission_manager.clone(),
                plugin_host.clone(),
                task_executor.clone(),
                config.clone(),
                EventKernelPipeline {
                    filter_handler: filter_handler.clone(),
                    wait_manager: wait_manager.clone(),
                    event_handler: event_handler.clone(),
                    matcher_registry: matcher_registry.clone(),
                    adapter: adapter.clone(),
                    core_commands: core_commands.clone(),
                },
            )
            .with_business_gate(business_gate.clone()),
        );
        let cm = Arc::new(Self {
            bot_txs: Arc::new(DashMap::new()),
            bot_states: Arc::new(crate::core::bot_state::BotStateRegistry::new()),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            business_gate,
            business_start: tokio::sync::Mutex::new(()),
            db,
            paths,
            event_bus,
            permission_manager,
            filter_handler,
            plugin_host,
            api_client,
            wait_manager,
            adapter,
            event_handler,
            event_kernel,
            scheduler: scheduler.clone(),
            subscribe_manager,
            matcher_registry,
            session_manager,
            system_monitor,
            config,
            framework_control,
            core_commands,
            bot_session_manager,
            task_executor,
            config_change_manager,
        });
        cm.plugin_host
            .register_platform_role_verifier(Arc::new(
                crate::core::platform_role_verifier::OneBotRoleVerifier {
                    senders: cm.bot_txs.clone(),
                    api: cm.api_client.clone(),
                },
            ))
            .await;

        // 启动调度器事件处理后台任务
        let cm_weak = Arc::downgrade(&cm);
        tokio::spawn(async move {
            Self::scheduler_event_loop(scheduler, scheduler_rx, cm_weak).await;
        });

        Ok(cm)
    }

    /// 管理入口与账号准备完成后启动业务；重复调用不会重复启用插件。
    pub async fn start_business(&self) -> Result<()> {
        let _start = self.business_start.lock().await;
        if self.business_gate.is_closed() {
            return Err(crate::NuonoError::Conflict("业务运行时已关闭".into()));
        }
        if self.business_gate.is_running() {
            return Ok(());
        }
        self.plugin_host.start_business().await?;
        if !self.business_gate.open() {
            return Err(crate::NuonoError::Conflict(
                "业务启动期间已收到停止请求".into(),
            ));
        }
        Ok(())
    }

    /// 是否已允许平台入站、定时任务与插件业务运行。
    pub fn business_is_running(&self) -> bool {
        self.business_gate.is_running()
    }

    /// 立即关闭业务入口，并禁止迟到的初始化任务再次启动业务。
    pub fn request_business_shutdown(&self) {
        self.business_gate.close();
        self.plugin_host.request_business_shutdown();
        self.scheduler.request_shutdown();
    }

    async fn scheduler_event_loop(
        scheduler: Arc<Scheduler>,
        mut event_rx: mpsc::Receiver<crate::service::scheduler::ScheduledDelivery>,
        cm_weak: std::sync::Weak<ConnectionManager>,
    ) {
        let mut stopped = scheduler.shutdown_receiver();
        let startup = async {
            if scheduler.wait_for_business().await {
                scheduler.start().await
            } else {
                Ok(())
            }
        };
        tokio::pin!(startup);
        let ownership_interval = std::time::Duration::from_secs(1);
        // 首次连接归属探测延迟一个周期后再执行，与运行期轮询节奏保持一致；
        // 即使启动失败也保留接收端，以便后续显式重试加载。
        let mut startup_owner_check = tokio::time::interval_at(
            tokio::time::Instant::now() + ownership_interval,
            ownership_interval,
        );
        startup_owner_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = async { let _ = stopped.wait_for(|stopped| *stopped).await; } => return,
                result = &mut startup => {
                    if let Err(error) = result { tracing::error!(%error, "调度器启动失败，保留消费端以便修复后重新加载"); }
                    break;
                }
                _ = startup_owner_check.tick() => {
                    if cm_weak.strong_count() == 0 { scheduler.request_shutdown(); return; }
                }
            }
        }
        loop {
            tokio::select! {
                biased;
                _ = async { let _ = stopped.wait_for(|stopped| *stopped).await; } => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    if cm_weak.strong_count() == 0 { scheduler.request_shutdown(); return; }
                }
                event = event_rx.recv() => {
                    let Some(event) = event else { return; };
                    let Some(cm) = cm_weak.upgrade() else { scheduler.request_shutdown(); return; };
                    let action_router = cm.event_kernel.action_router();
                    tokio::select! {
                        biased;
                        _ = async { let _ = stopped.wait_for(|stopped| *stopped).await; } => return,
                        result = handle_scheduled_delivery(&event, &cm.db, &cm.bot_txs, &action_router) => {
                            if let Err(error) = result { tracing::error!(task=%event.task_name, %error, "调度器事件处理失败"); }
                        }
                    }
                }
            }
        }
    }

    /// 驱动一条 OneBot 反向 WebSocket 直到任一半连接结束。
    ///
    /// bot 生命周期从首个能提取 `self_id` 的帧开始，而不是 TCP/WebSocket 建立时开始。
    /// 注册成功后才写入 sender、action route、会话和在线状态；driver 的同步 RAII guard
    /// 在正常退出、取消和 panic 时按连接批次号清理这些映射，防止误删重连后的新连接。连接未被识别时不会产生历史记录。
    pub async fn handle_inbound_ws<S, E>(self: Arc<Self>, socket: S, ip: String)
    where
        S: futures::Stream<Item = std::result::Result<nuono_app::transport::TransportFrame, E>>
            + futures::Sink<nuono_app::transport::TransportFrame, Error = E>
            + Unpin
            + Send
            + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        self.handle_inbound_ws_with_base(socket, ip, None).await
    }

    /// 与 [`Self::handle_inbound_ws`] 相同，并记住该网关使用的 origin。
    ///
    /// 短期媒体链接相对 `base_url` 解析，使另一台机器上的网关
    /// 拿到可实际访问的 URL，而不是宿主的回环地址。
    pub async fn handle_inbound_ws_with_base<S, E>(
        self: Arc<Self>,
        socket: S,
        ip: String,
        base_url: Option<String>,
    ) where
        S: futures::Stream<Item = std::result::Result<nuono_app::transport::TransportFrame, E>>
            + futures::Sink<nuono_app::transport::TransportFrame, Error = E>
            + Unpin
            + Send
            + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        info!(ip = %ip, "🔗 新的 WebSocket 连接已建立，开始接收消息...");
        let connected_at = std::time::Instant::now();
        let (mut ws_sender, mut ws_receiver) = socket.split();
        let (msg_tx, mut msg_rx) = mpsc::channel::<TransportFrame>(MSG_CHANNEL_SIZE);
        let bot_id_state = Arc::new(AtomicI64::new(0));
        let bot_id_state_send = bot_id_state.clone();
        let config_send = self.config.clone();
        let platform_send = self.adapter.platform_name().to_string();
        let api_client_send = self.api_client.clone();
        let ip_send = ip.clone();
        let mut connection_tasks = JoinSet::new();

        connection_tasks.spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                let summary = outbound_log_summary(&config_send, &platform_send, &msg).await;
                let bot_id = bot_id_state_send.load(Ordering::SeqCst).to_string();
                // 回执可以在 write 返回前到达，因此先按 echo 登记；只保留已脱敏的小摘要。
                let observed = summary.as_ref().is_some_and(|summary| {
                    let Some(echo) = summary.request_id.as_deref() else {
                        return false;
                    };
                    let summary = summary.clone();
                    let bot_id = bot_id.clone();
                    api_client_send.observe_response(echo, move |state, plugin, error| {
                        log_send_outcome(&bot_id, &summary, state, plugin, error);
                    })
                });
                if let Err(error) = ws_sender.send(msg).await {
                    if observed {
                        if let Some(echo) = summary.as_ref().and_then(|s| s.request_id.as_deref()) {
                            api_client_send.report_write_failure(echo, &error.to_string());
                        }
                    } else if let Some(summary) = &summary {
                        log_send_outcome(
                            &bot_id,
                            summary,
                            nuono_plugin::core::action_log::SendState::Failed,
                            None,
                            Some(&error.to_string()),
                        );
                    } else {
                        warn!(bot_id = %bot_id, ip = %ip_send, %error, "OneBot WebSocket 写入失败");
                    }
                    return "socket_write_failed";
                }
                if let Some(summary) = summary {
                    if observed {
                        tracing::debug!(target: "message_flow", bot_id = %bot_id,
                            request_id = summary.request_id.as_deref(), direction = "send",
                            send_state = "submitted", "发送请求已写入连接，等待接口回执");
                    } else {
                        log_send(&bot_id, &summary);
                    }
                }
            }
            "outbound_channel_closed"
        });

        let bot_states = self.bot_states.clone();
        let lifecycle = self.lifecycle.clone();
        let heartbeat_timeout = std::time::Duration::from_secs(
            self.config.snapshot().await.upstream.heartbeat_timeout_secs,
        );
        let db = self.db.clone();
        let adapter = self.adapter.clone();
        let api_client = self.api_client.clone();
        let session_registration = ConnectionSessionGuard::new(
            self.bot_session_manager.clone(),
            self.bot_txs.clone(),
            self.bot_states.clone(),
            self.plugin_host.action_router(),
        );
        let session_registration_recv = session_registration.registration.clone();
        let msg_tx_recv = msg_tx.clone();
        let event_kernel = self.event_kernel.clone();
        let msg_tx_event = msg_tx.clone();
        let (event_tx, event_rx) = mpsc::channel::<InboundPostEvent>(EVENT_CHANNEL_SIZE);
        let bot_id_state_recv = bot_id_state.clone();
        let ip_clone = ip.clone();
        let (identified_tx, identified_rx) = tokio::sync::oneshot::channel();
        let status_client = self.api_client.clone();
        let status_states = self.bot_states.clone();
        let status_db = self.db.clone();
        connection_tasks.spawn(async move {
            if let Ok((bot_id, generation)) = identified_rx.await {
                status::monitor(
                    status_client,
                    status_states,
                    status_db,
                    bot_id,
                    generation,
                    msg_tx,
                    heartbeat_timeout,
                )
                .await;
            }
            "status_monitor_stopped"
        });

        // 有界派发器在保证常规业务回调按序串行执行的同时，
        // 通过独立通道将匹配的回复投递给正在等待的 WaitReply 会话，避免被当前挂起的业务回调阻塞。
        connection_tasks.spawn(run_ordered_events(
            event_kernel.clone(),
            event_rx,
            msg_tx_event,
        ));

        connection_tasks.spawn(async move {
            let mut bot_id: i64 = 0;
            let mut identified_tx = Some(identified_tx);
            let mut exit_reason = "peer_stream_ended";
            while let Some(msg) = ws_receiver.next().await {
                let text = match msg {
                    Ok(TransportFrame::Text(t)) => {
                        let is_heartbeat = t.len() < 200 && t.contains("\"meta_event_type\"");
                        if !is_heartbeat && !t.contains("\"echo\"") {
                            debug!(
                                ip = %ip_clone,
                                frame_chars = t.chars().count(),
                                "收到 WebSocket 文本帧"
                            );
                        }
                        t
                    }
                    Ok(TransportFrame::Binary(b)) => {
                        let s = String::from_utf8_lossy(&b).to_string();
                        info!(ip = %ip_clone, len = s.len(), "📨 收到二进制消息");
                        s
                    }
                    Ok(TransportFrame::Ping(_)) | Ok(TransportFrame::Pong(_)) => continue,
                    Ok(TransportFrame::Close(close)) => {
                        info!(bot_id, ip = %ip_clone, close_code = close.as_ref().map(|close| close.code), close_reason = ?close.as_ref().map(|close| &close.reason), "OneBot 客户端发送 WebSocket 关闭帧");
                        exit_reason = "peer_close";
                        break;
                    }
                    Err(error) => {
                        warn!(bot_id, ip = %ip_clone, %error, "OneBot WebSocket 读取失败");
                        exit_reason = "socket_read_failed";
                        break;
                    }
                };

                match bot_id {
                    0 if let Some(id) = adapter.extract_self_id(&text) => {
                        let _lifecycle = lifecycle.lock().await;
                        bot_id = id;
                        bot_id_state_recv.store(bot_id, Ordering::SeqCst);
                        let route = RuntimeBotRoute::new(
                            adapter.platform_name(),
                            id.to_string(),
                            id.to_string(),
                        );

                        // 注册到 BotSessionManager
                        let actor = Arc::new(crate::core::bot_session::SendActor::new(
                            bot_id,
                            msg_tx_recv.clone(),
                        ));
                        actor.set_platform(adapter.platform_name()).await;
                        actor.set_ip(&ip_clone).await;
                        actor
                            .set_status(crate::core::bot_session::SessionStatus::Online)
                            .await;
                        let Some(generation) =
                            session_registration_recv.register(PendingConnectionSession {
                                actor,
                                sender: msg_tx_recv.clone(),
                                route: route.clone(),
                                sink: Arc::new(OneBotRuntimeActionSink {
                                    api_client: api_client.clone(),
                                    sender: msg_tx_recv.clone(),
                                    avatar_resolver: Arc::new(crate::media::HostImageResolver::new()),
                                    avatar_source:{ let provider=nuono_adapter::running().legacy_codec_provider();Arc::new(move|request|provider.as_ref().map(|provider|provider.avatar_source(request)).transpose().map(Option::flatten)) },
                                }),
                                ip: ip_clone.clone(),
                                heartbeat_timeout,
                            })
                        else {
                            exit_reason = "session_registration_closed";
                            break;
                        };
                        // 只有成功注册发布的连接才能更新该路由的外部访问基地址（base_url），
                        // 被拒绝的重复连接不得覆盖当前活跃网关的地址。
                        if let Some(base_url) = base_url.as_deref() {
                            crate::core::media_links::remember_route_base(
                                &route,
                                generation,
                                base_url,
                            );
                        }

                        let db2 = db.clone();
                        let bid = bot_id.to_string();
                        let platform = adapter.platform_name().to_string();
                        let ip = ip_clone.clone();
                        tokio::spawn(async move {
                            let _ = db2.update_bot_status(&bid, "online").await;
                            let _ = db2
                                .add_log(
                                    "INFO",
                                    &bid,
                                    &format!("Bot {} 连接已接入 [{}] ({})", bid, platform, ip),
                                )
                                .await;
                        });

                        info!(bot_id = %bot_id, self_id = %bot_id, ip = %ip_clone, platform = %adapter.platform_name(), "[BotConnected] Bot 连接已接入");
                    }
                    0 => {
                        warn!(
                            ip = %ip_clone,
                            frame_chars = text.chars().count(),
                            "首条消息无法提取 self_id，跳过 bot 注册"
                        );
                    }
                    _ => {}
                }

                let v: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            ip = %ip_clone,
                            error = %e,
                            frame_chars = text.chars().count(),
                            "WebSocket JSON 解析失败"
                        );
                        continue;
                    }
                };

                let connection_route = session_registration_recv.route();
                if bot_id != 0 && connection_route.is_none() {
                    exit_reason = "session_closed";
                    break;
                }
                if let Some((_, generation)) = connection_route {
                    bot_states.observe(bot_id, generation, &v);
                    if let Some(identified_tx) = identified_tx.take() {
                        let _ = identified_tx.send((bot_id, generation));
                    }
                }

                // OneBot action 回包与事件共用同一 WebSocket。只要存在 echo 就视为
                // ApiClient 的相关回包并终止入站分发，避免响应体被插件当作事件处理。
                if v.get("echo").is_some() {
                    api_client.on_response(&v);
                    continue;
                }

                let bot_id_str = bot_id.to_string();
                metrics::counter!("nuono_ws_messages_received_total", "bot_id" => bot_id_str.clone()).increment(1);

                if let Some(post_type) = v.get("post_type").and_then(|p| p.as_str()) {
                    let is_message = post_type == "message";

                    if !is_message && post_type != "meta_event" {
                        info!(
                            bot_id = %bot_id,
                            post_type,
                            notice_type = v.get("notice_type").and_then(serde_json::Value::as_str),
                            request_type = v.get("request_type").and_then(serde_json::Value::as_str),
                            sub_type = v.get("sub_type").and_then(serde_json::Value::as_str),
                            "📨 收到事件"
                        );
                    }

                    // 若在队列满时阻塞等待，会导致读取协程卡住而无法接收后续的 API 回执（echo）。
                    // 因此当事件队列过载时直接主动断开连接，既不静默丢弃事件，也不无限制生成后台任务导致内存膨胀。
                    let received_at = std::time::Instant::now();
                    let receipt = event_kernel.retain_queued_receipt(received_at);
                    if let Err(error) = session_registration_recv.enqueue_event(&event_tx, InboundPostEvent {
                        received_at,
                        receipt: Some(receipt),
                        bot_id,
                        text,
                        event: v,
                        route: None,
                    }) {
                        exit_reason = "event_channel_closed";
                        if error == InboundAdmissionError::Full {
                            exit_reason = "event_queue_full";
                            metrics::counter!("nuono_ws_event_queue_overloads_total", "bot_id" => bot_id_str).increment(1);
                            warn!(bot_id = %bot_id, capacity = EVENT_CHANNEL_SIZE, "入站事件队列已满，关闭连接");
                        }
                        break;
                    }
                }
            }
            exit_reason
        });

        // 读写任一任务退出或 panic 时，立即取消同一连接下的其他并列任务；
        // 若外层连接驱动协程自身被取消，JoinSet 析构时也会自动中止这些子任务。
        let reason = match connection_tasks.join_next().await {
            Some(Ok(reason)) => reason,
            Some(Err(error)) => {
                warn!(bot_id = bot_id_state.load(Ordering::SeqCst), ip = %ip, %error, "OneBot 连接任务异常退出");
                "connection_task_failed"
            }
            None => "connection_tasks_empty",
        };
        info!(bot_id = bot_id_state.load(Ordering::SeqCst), ip = %ip, reason, connected_secs = connected_at.elapsed().as_secs_f64(), "OneBot WebSocket 连接结束");
        connection_tasks.shutdown().await;

        let bot_id = bot_id_state.load(Ordering::SeqCst);
        if bot_id != 0 {
            let _lifecycle = self.lifecycle.lock().await;
            let removed_current = session_registration.registration.cleanup();
            if !removed_current {
                debug!(bot_id = %bot_id, "旧连接已被同 bot_id 的新连接替代，跳过离线清理");
                return;
            }

            let db2 = self.db.clone();
            let bid = bot_id.to_string();
            tokio::spawn(async move {
                let _ = db2.update_bot_status(&bid, "offline").await;
            });

            info!(bot_id = %bot_id, self_id = %bot_id, reason, "[BotDisconnected] Bot 连接已断开");
        }
    }

    /// 主动探测并刷新账号在线健康状态与身份信息（仅获取到登录身份不能作为账号健康的依据）。
    pub async fn probe_bot_status(&self, bot_id: i64) -> Result<crate::service::BotStatus> {
        let Some((generation, sender)) = self.bot_states.connection(bot_id) else {
            return Ok(crate::core::bot_state::disconnected(bot_id.to_string()));
        };
        tokio::join!(
            status::refresh_status(
                &self.api_client,
                &self.bot_states,
                bot_id,
                generation,
                &sender
            ),
            status::refresh_identity(
                &self.api_client,
                &self.bot_states,
                &self.db,
                bot_id,
                generation,
                &sender
            ),
        );
        Ok(self
            .bot_states
            .snapshot(bot_id)
            .unwrap_or_else(|| crate::core::bot_state::disconnected(bot_id.to_string())))
    }

    /// 将已经序列化的 OneBot action 写入当前活动连接。
    ///
    /// 成功仅表示进入有界 mpsc 队列，不代表 socket 已写出或下游 action 已执行；需要
    /// 业务回包的调用应使用 `ApiClient::call` 的 echo 生命周期。
    pub async fn send_to_bot(&self, bot_id: i64, text: String) -> Result<()> {
        send_to_registered_bot(&self.bot_txs, bot_id, text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AvatarResolver {
        calls: std::sync::Mutex<Vec<(String, usize)>>,
        bytes: Vec<u8>,
    }
    #[async_trait::async_trait]
    impl nuono_app::media::ImageResolver for AvatarResolver {
        async fn resolve_image(
            &self,
            source: &str,
            limit: usize,
        ) -> Result<nuono_app::media::ImageUpload> {
            self.calls.lock().unwrap().push((source.into(), limit));
            Ok(nuono_app::media::ImageUpload {
                bytes: self.bytes.clone(),
                file_name: "avatar.png".into(),
                content_type: "image/png",
            })
        }
    }
    fn avatar_params() -> serde_json::Value {
        serde_json::json!({"identity":{"platform":"one_bot","kind":"user","scheme":"numeric","value":"123456789"},"size":256,"max_bytes":1024})
    }
    #[tokio::test]
    async fn onebot_file_read_is_explicitly_unsupported_without_forwarding_unknown_api() {
        let resolver = Arc::new(AvatarResolver {
            calls: Default::default(),
            bytes: vec![],
        });
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = OneBotRuntimeActionSink {
            api_client: Arc::new(ApiClient::new()),
            sender,
            avatar_resolver: resolver.clone(),
            avatar_source: Arc::new(|_| Ok(Some("fixture:avatar".into()))),
        };
        let action = serde_json::json!({"action":"read_inbound_file","params":{
            "source":"opaque-native-file", "message_id":"001", "max_bytes":1024
        }});
        assert_eq!(
            sink.call_action(action.clone()).await.unwrap(),
            serde_json::json!({"status":"unsupported"})
        );
        assert!(sink.send_action(action).await.is_err());
        assert!(resolver.calls.lock().unwrap().is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    #[tokio::test]
    async fn onebot_file_send_does_not_forward_private_upload_protocols() {
        let resolver = Arc::new(AvatarResolver {
            calls: Default::default(),
            bytes: vec![],
        });
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = OneBotRuntimeActionSink {
            api_client: Arc::new(ApiClient::new()),
            sender,
            avatar_resolver: resolver.clone(),
            avatar_source: Arc::new(|_| Ok(Some("fixture:avatar".into()))),
        };
        let action = serde_json::json!({"action":"send_file","params":{
            "target":{"type":"group","group_id":"001"},"base64":"e30=","file_name":"export.json","content_type":"application/json"
        }});
        assert_eq!(
            sink.call_action(action.clone()).await.unwrap(),
            serde_json::json!({"status":"unsupported"})
        );
        assert!(sink.send_action(action).await.is_err());
        assert!(resolver.calls.lock().unwrap().is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    #[tokio::test]
    async fn onebot_avatar_calls_the_host_reader_without_sending_a_protocol_action() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let resolver = Arc::new(AvatarResolver {
            calls: Default::default(),
            bytes: png.to_vec(),
        });
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = OneBotRuntimeActionSink {
            api_client: Arc::new(ApiClient::new()),
            sender,
            avatar_resolver: resolver.clone(),
            avatar_source: Arc::new(|_| Ok(Some("fixture:avatar".into()))),
        };
        let response = sink
            .call_action(serde_json::json!({"action":"read_user_avatar","params":avatar_params()}))
            .await
            .unwrap();
        assert!(
            sink.send_action(
                serde_json::json!({"action":"read_user_avatar","params":avatar_params()})
            )
            .await
            .is_err()
        );
        assert_eq!(
            response,
            serde_json::json!({"status":"available","base64":STANDARD.encode(png),"version":null})
        );
        assert_eq!(
            resolver.calls.lock().unwrap().as_slice(),
            &[("fixture:avatar".into(), 1024)]
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    #[tokio::test]
    async fn onebot_avatar_rejects_invalid_requests_and_oversize_reader_results() {
        let resolver = AvatarResolver {
            calls: Default::default(),
            bytes: vec![0; 1025],
        };
        assert!(
            resolve_onebot_avatar(&avatar_params(), &resolver, &|_| Ok(Some(
                "fixture:avatar".into()
            )))
            .await
            .is_err()
        );
        let mut params = avatar_params();
        params["identity"]["kind"] = serde_json::json!("group");
        assert!(
            resolve_onebot_avatar(&params, &resolver, &|_| Ok(Some("fixture:avatar".into())))
                .await
                .is_err()
        );
        assert_eq!(resolver.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn stale_connection_cannot_remove_reconnected_bot_sender() {
        let bot_txs = DashMap::new();
        let (old_tx, _old_rx) = mpsc::channel(1);
        let (new_tx, _new_rx) = mpsc::channel(1);
        bot_txs.insert(42, old_tx.clone());
        bot_txs.insert(42, new_tx.clone());

        assert!(!remove_bot_sender_if_current(&bot_txs, 42, &old_tx));
        assert!(
            bot_txs
                .get(&42)
                .is_some_and(|current| current.same_channel(&new_tx))
        );
        assert!(remove_bot_sender_if_current(&bot_txs, 42, &new_tx));
        assert!(!bot_txs.contains_key(&42));
    }

    #[tokio::test]
    async fn full_outbound_queue_does_not_hold_registry_shard_during_send() {
        let bot_txs = DashMap::new();
        let (old_tx, mut old_rx) = mpsc::channel(1);
        old_tx
            .send(TransportFrame::Text("occupied".into()))
            .await
            .unwrap();
        bot_txs.insert(42, old_tx.clone());

        let pending = send_to_registered_bot(&bot_txs, 42, "pending".into());
        tokio::pin!(pending);
        assert!(futures::poll!(pending.as_mut()).is_pending());
        assert!(
            !bot_txs.try_get_mut(&42).is_locked(),
            "a capacity waiter must not retain a DashMap shard guard"
        );

        assert!(remove_bot_sender_if_current(&bot_txs, 42, &old_tx));
        let (new_tx, mut new_rx) = mpsc::channel(1);
        bot_txs.insert(42, new_tx.clone());
        old_rx.recv().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(old_rx.recv().await, Some(TransportFrame::Text(text)) if text == "pending")
        );
        assert!(matches!(
            new_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(!remove_bot_sender_if_current(&bot_txs, 42, &old_tx));
        assert!(remove_bot_sender_if_current(&bot_txs, 42, &new_tx));
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/connection_wait_tests.rs"]
mod wait_tests;

#[cfg(test)]
mod scheduler_loop_tests {
    use super::*;
    use sea_orm::{ConnectionTrait, TransactionTrait};

    #[tokio::test]
    async fn database_blocked_start_releases_scheduler_when_connection_manager_is_gone() {
        let db = crate::test_support::test_db(1).await;
        let transaction = db.orm().begin().await.unwrap();
        let (scheduler, events) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler);
        let worker = tokio::spawn(ConnectionManager::scheduler_event_loop(
            scheduler.clone(),
            events,
            std::sync::Weak::new(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !worker.is_finished(),
            "startup must wait for the occupied pool"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), worker)
            .await
            .expect("lost ownership must cancel startup without waiting for a connection")
            .unwrap();
        assert!(*scheduler.shutdown_receiver().borrow());
        assert_eq!(scheduler.job_count(), 0);
        // 启动流程被中途取消时也必须正确释放生命周期锁；
        // 由于此时数据库连接池仍被占满，这里通过非阻塞的状态检查来验证锁已释放。
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(200), scheduler.reload())
                .await
                .unwrap(),
            Err(crate::error::NuonoError::Conflict(_))
        ));
        transaction.rollback().await.unwrap();
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_start_keeps_the_receiver_alive_for_reload_and_shutdown_wakes_it() {
        let db = crate::test_support::test_db(1).await;
        db.orm()
            .execute_unprepared("ALTER TABLE nuono_scheduled_tasks RENAME TO unavailable_tasks")
            .await
            .unwrap();
        let (scheduler, events) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler);
        let owner = scheduler.clone();
        let worker = tokio::spawn(ConnectionManager::scheduler_event_loop(
            owner,
            events,
            std::sync::Weak::new(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!worker.is_finished());
        db.orm()
            .execute_unprepared("ALTER TABLE unavailable_tasks RENAME TO nuono_scheduled_tasks")
            .await
            .unwrap();
        scheduler.reload().await.unwrap();
        scheduler.shutdown().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn idle_scheduler_worker_exits_when_its_connection_manager_is_gone() {
        let db = crate::test_support::test_db(1).await;
        let (scheduler, events) = Scheduler::new(db.clone());
        let scheduler = Arc::new(scheduler);
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            ConnectionManager::scheduler_event_loop(
                scheduler.clone(),
                events,
                std::sync::Weak::new(),
            ),
        )
        .await
        .unwrap();
        assert!(*scheduler.shutdown_receiver().borrow());
        assert_eq!(scheduler.job_count(), 0);
        db.shutdown().await.unwrap();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/connection_outbound_fault_tests.rs"]
mod outbound_fault_tests;

#[cfg(test)]
mod business_start_tests {
    use super::*;

    #[tokio::test]
    async fn prepared_runtime_rejects_events_and_manual_tasks_until_started() {
        let root =
            std::env::temp_dir().join(format!("nuono-business-start-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let db = crate::test_support::test_db(2).await;
        let config = Arc::new(
            crate::config::ConfigManager::load(Some(root.join("nuono.toml")))
                .await
                .unwrap(),
        );
        let plugin_config = Arc::new(crate::config::PluginConfigManager::with_database(
            root.join("configs"),
            db.clone(),
        ));
        let manager = ConnectionManager::new_prepared(
            db.clone(),
            Paths::with_root(&root),
            Arc::new(nuono_adapter_fixture::FixtureAdapter),
            config,
            plugin_config,
        )
        .await
        .unwrap();
        assert!(!manager.business_is_running());
        assert!(!manager.event_kernel.business_is_running());
        let event = serde_json::json!({"post_type":"message","message_type":"private","self_id":42,"user_id":7,"raw_message":"hello"});
        let (sender, mut output) = mpsc::channel(2);
        let processed = manager
            .event_kernel
            .process_runtime_post_event(42, &event.to_string(), event.clone(), sender)
            .await
            .unwrap();
        assert!(
            processed.blocked,
            "uninitialized runtime must reject business input"
        );
        assert!(output.try_recv().is_err());
        assert!(matches!(
            manager.scheduler.run_once(999, "before-start").await,
            Err(crate::NuonoError::Conflict(_))
        ));
        manager.start_business().await.unwrap();
        manager.start_business().await.unwrap();
        assert!(manager.business_is_running());
        assert!(manager.event_kernel.business_is_running());
        manager.request_business_shutdown();
        assert!(!manager.business_is_running());
        assert!(manager.start_business().await.is_err());
        manager.plugin_host.shutdown().await.unwrap();
        manager.scheduler.shutdown().await.unwrap();
        db.shutdown().await.unwrap();
    }
}
