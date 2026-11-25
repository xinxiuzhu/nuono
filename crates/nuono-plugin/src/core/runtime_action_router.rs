use super::event_kernel::RuntimeActionSink;
use crate::core::{HostLogBridge, HostLogRecord};
use crate::error::{NuonoError, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub use nuono_app::transport::RuntimeBotRoute;

/// 路由动作调用的标准化回执（供需要获取平台原生消息 ID 列表与降级信息的调用方使用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeActionReceipt {
    #[serde(default)]
    pub message_ids: Vec<String>,
    #[serde(default)]
    pub degradations: Vec<String>,
    pub raw: Value,
}

impl RuntimeActionReceipt {
    pub fn from_value(value: Value) -> Self {
        let mut message_ids: Vec<String> =
            if let Some(ids) = value.get("message_ids").and_then(Value::as_array) {
                ids.iter().filter_map(value_as_exact_string).collect()
            } else {
                value
                    .get("message_id")
                    .and_then(value_as_exact_string)
                    .into_iter()
                    .collect()
            };
        let mut seen = std::collections::HashSet::new();
        message_ids.retain(|id| seen.insert(id.clone()));
        let raw = value.get("raw").cloned().unwrap_or_else(|| value.clone());
        let degradations = value
            .get("degradations")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            message_ids,
            degradations,
            raw,
        }
    }
}

fn value_as_exact_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

#[derive(Clone)]
struct RegisteredRuntimeActionSink {
    generation: u64,
    sink: Arc<dyn RuntimeActionSink>,
}

/// 将平台宿执行固定到注册该路由的 Tokio 运行时。
///
/// 调用方可以来自其他 Tokio runtime；具体平台 sink 始终在拥有其 HTTP/WebSocket
/// 连接的 runtime 上执行，避免跨 runtime 使用连接资源。
struct HostRuntimeActionSink {
    handle: tokio::runtime::Handle,
    inner: Arc<dyn RuntimeActionSink>,
}

/// 直接丢弃 `JoinHandle` 只会让后台任务脱离管控（detach）。
/// 此守卫在自身被 drop 时主动调用 `abort()`，使派发到宿主运行时的任务与调用方生命周期保持绑定（但注意：若平台侧已接收并执行了该动作，中止任务无法回滚外部副作用）。
struct AbortHostCallOnDrop(tokio::task::AbortHandle);

impl Drop for AbortHostCallOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[async_trait::async_trait]
impl RuntimeActionSink for HostRuntimeActionSink {
    async fn call_native_file(
        &self,
        request: nuono_app::media::NativeFileSendRequest,
    ) -> Result<nuono_app::media::FileSendResponse> {
        let inner = self.inner.clone();
        let plugin = super::log_context::current_plugin();
        let task = self
            .handle
            .spawn(super::log_context::scope(plugin, async move {
                inner.call_native_file(request).await
            }));
        let _abort_on_drop = AbortHostCallOnDrop(task.abort_handle());
        task.await
            .map_err(|error| NuonoError::Internal(format!("host file task failed: {error}")))?
    }

    async fn validate_native_target(&self, target: &crate::domain::DeliveryTarget) -> Result<()> {
        let inner = self.inner.clone();
        let target = target.clone();
        let plugin = super::log_context::current_plugin();
        let task = self
            .handle
            .spawn(super::log_context::scope(plugin, async move {
                inner.validate_native_target(&target).await
            }));
        let _abort_on_drop = AbortHostCallOnDrop(task.abort_handle());
        task.await
            .map_err(|error| NuonoError::Internal(format!("host action task failed: {error}")))?
    }

    async fn send_action(&self, action: Value) -> Result<()> {
        let inner = self.inner.clone();
        let plugin = super::action_log::current_plugin();
        let task = self
            .handle
            .spawn(super::action_log::scope(plugin, async move {
                inner.send_action(action).await
            }));
        let _abort_on_drop = AbortHostCallOnDrop(task.abort_handle());
        task.await
            .map_err(|error| NuonoError::Internal(format!("host action task failed: {error}")))?
    }

    async fn call_action(&self, action: Value) -> Result<Value> {
        let inner = self.inner.clone();
        let plugin = super::action_log::current_plugin();
        let task = self
            .handle
            .spawn(super::action_log::scope(plugin, async move {
                inner.call_action(action).await
            }));
        let _abort_on_drop = AbortHostCallOnDrop(task.abort_handle());
        task.await
            .map_err(|error| NuonoError::Internal(format!("host action task failed: {error}")))?
    }
}

/// 进程级全局路由器：将精确的 Bot 路由映射到对应的平台动作接收端（ActionSink）。
///
/// 路由注册具备代际（generation）隔离保护：覆盖同名路由会分配新的代际编号，
/// 旧连接断开时的清理逻辑不会误删新连接注册的路由。
pub struct RuntimeActionRouter {
    registration: std::sync::Mutex<()>,
    next_generation: AtomicU64,
    routes: DashMap<RuntimeBotRoute, RegisteredRuntimeActionSink>,
    host_log: HostLogBridge,
    audit: Option<Arc<dyn nuono_app::action::ActionAudit>>,
    typed_connections: nuono_app::transport::ConnectionRegistry,
    interaction_port: crate::api::interaction::InteractionPortSource,
}

impl std::fmt::Debug for RuntimeActionRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeActionRouter")
            .field("route_count", &self.routes.len())
            .finish_non_exhaustive()
    }
}

impl Default for RuntimeActionRouter {
    fn default() -> Self {
        Self {
            registration: std::sync::Mutex::new(()),
            next_generation: AtomicU64::new(1),
            routes: DashMap::new(),
            host_log: HostLogBridge::capture_current(),
            audit: None,
            typed_connections: nuono_app::transport::ConnectionRegistry::new(),
            interaction_port: Arc::new(std::sync::RwLock::new(None)),
        }
    }
}

impl RuntimeActionRouter {
    pub fn set_interaction_port(
        &self,
        port: Arc<dyn crate::api::interaction::PluginInteractionPort>,
    ) {
        *self.interaction_port.write().expect("锁已中毒") = Some(port);
    }

    pub fn interaction_port(
        &self,
    ) -> Option<Arc<dyn crate::api::interaction::PluginInteractionPort>> {
        self.interaction_port.read().expect("锁已中毒").clone()
    }
    #[doc(hidden)]
    pub fn message_policy_source(&self) -> crate::api::interaction::InteractionPortSource {
        self.interaction_port.clone()
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_audit(audit: Arc<dyn nuono_app::action::ActionAudit>) -> Self {
        Self {
            audit: Some(audit),
            ..Self::default()
        }
    }
    /// 注册或覆盖一条 Bot 路由，并返回供断开连接时安全注销的代际编号（`generation`）。
    pub fn register(&self, route: RuntimeBotRoute, sink: Arc<dyn RuntimeActionSink>) -> u64 {
        let sink: Arc<dyn RuntimeActionSink> = match tokio::runtime::Handle::try_current() {
            Ok(handle) => Arc::new(HostRuntimeActionSink {
                handle,
                inner: sink,
            }),
            Err(_) => sink,
        };
        let _registration = self.registration.lock().expect("锁已中毒");
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(transport) =
            super::runtime_transport::SinkTransport::new(&route, generation, sink.clone())
                .map(|transport| transport.with_message_policy_source(self.message_policy_source()))
            && let Ok(bot) = super::runtime_transport::route_identity(&route)
        {
            let key = nuono_app::transport::BotInstanceKey::from_bot(
                &bot,
                nuono_app::transport::ActionTransport::protocol(&transport).clone(),
            );
            let transport = if let Some(audit) = &self.audit {
                transport.with_audit(audit.clone())
            } else {
                transport
            };
            self.typed_connections
                .register(key, generation, Arc::new(transport));
        }
        self.routes
            .insert(route, RegisteredRuntimeActionSink { generation, sink });
        generation
    }

    /// 仅当该路由当前仍处于指定的注册代际（`generation`）时才将其移除。
    pub fn unregister(&self, route: &RuntimeBotRoute, generation: u64) -> bool {
        let _registration = self.registration.lock().expect("锁已中毒");
        let removed = self
            .routes
            .remove_if(route, |_, current| current.generation == generation)
            .is_some();
        if removed {
            for (key, connection) in self.typed_connections.list() {
                if connection.generation == generation {
                    self.typed_connections.remove_if_current(
                        &key,
                        &connection.connection_id,
                        generation,
                    );
                }
            }
        }
        removed
    }

    pub fn contains(&self, route: &RuntimeBotRoute) -> bool {
        self.routes.contains_key(route)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// 返回当前已连接的全部精确 Bot 路由列表（已按字典序排序）。
    pub fn connected_routes(&self) -> Vec<RuntimeBotRoute> {
        let mut routes: Vec<_> = self
            .routes
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        routes.sort();
        routes
    }

    /// 通过宿主预先捕获的 `tracing` 分发器输出插件结构化日志。
    pub fn log(&self, record: HostLogRecord) {
        self.host_log.emit(record);
    }

    fn typed_request(
        &self,
        route: &RuntimeBotRoute,
        action: &mut Value,
    ) -> Result<
        Option<(
            Arc<dyn nuono_app::transport::ActionTransport>,
            nuono_app::transport::TransportAction,
        )>,
    > {
        let _registration = self.registration.lock().expect("锁已中毒");
        let generation = self.routes.get(route).map(|entry| entry.generation);
        let Some((_, connection)) = self
            .typed_connections
            .list()
            .into_iter()
            .find(|(_, connection)| Some(connection.generation) == generation)
        else {
            return Ok(None);
        };
        let bot = super::runtime_transport::route_identity(route)
            .map_err(|e| NuonoError::Platform(e.to_string()))?;
        // 路由原始动作 API 是显式的协议透传通道，需完整保留平台特定的全部参数。
        let name = action
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| NuonoError::Platform("action name required".into()))?
            .to_string();
        // 使用 `take` 移出 `params` 的所有权而非深拷贝：原始消息负载可能包含数十 MB 的内联 Base64 媒体数据。
        let params = match action.get_mut("params").map(Value::take) {
            None | Some(Value::Null) => json!({}),
            Some(params) if params.is_object() => params,
            Some(_) => {
                return Err(NuonoError::Platform(
                    "action params must be an object".into(),
                ));
            }
        };
        let outbound = crate::domain::OutboundAction::RawPlatform {
            action: name,
            params,
        };
        Ok(Some((
            connection.transport,
            nuono_app::transport::TransportAction::new(bot, outbound),
        )))
    }
    pub async fn notify(&self, route: &RuntimeBotRoute, mut action: Value) -> Result<()> {
        if let Some((transport, request)) = self.typed_request(route, &mut action)? {
            return nuono_app::action::notify_action(transport.as_ref(), request)
                .await
                .map_err(|e| NuonoError::Platform(e.to_string()));
        }
        let sink = self.sink(route)?;
        sink.send_action(action).await
    }

    fn typed_transport(
        &self,
        route: &RuntimeBotRoute,
        bot: &crate::domain::BotIdentity,
    ) -> Result<Arc<dyn nuono_app::transport::ActionTransport>> {
        let expected = super::runtime_transport::route_identity(route)
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        if &expected != bot {
            return Err(NuonoError::Validation(
                "native action bot does not match exact route".into(),
            ));
        }
        let _registration = self.registration.lock().expect("锁已中毒");
        let generation = self.routes.get(route).map(|entry| entry.generation);
        self.typed_connections
            .list()
            .into_iter()
            .find(|(key, connection)| {
                key.instance_id == bot.instance_id
                    && key.account_id == bot.account_id
                    && key.protocol.family.as_str() == route.platform
                    && Some(connection.generation) == generation
            })
            .map(|(_, connection)| connection.transport)
            .ok_or_else(|| NuonoError::Connection("native action route is not connected".into()))
    }

    /// 通过调用方指定的精确路由及其当前活跃连接代际发送强类型动作通知。
    pub async fn notify_typed_routed(
        &self,
        route: &RuntimeBotRoute,
        request: nuono_app::transport::TransportAction,
    ) -> Result<()> {
        let transport = self.typed_transport(route, &request.bot)?;
        nuono_app::action::notify_action(transport.as_ref(), request)
            .await
            .map_err(native_action_error)
    }

    pub async fn call_typed_routed(
        &self,
        route: &RuntimeBotRoute,
        request: nuono_app::transport::TransportAction,
    ) -> Result<RuntimeActionReceipt> {
        self.call_typed_routed_receipt(route, request)
            .await
            .map(|receipt| RuntimeActionReceipt {
                degradations: receipt.degradations,
                message_ids: receipt
                    .message_ids
                    .into_iter()
                    .map(|id| id.into_inner())
                    .collect(),
                raw: RuntimeActionReceipt::from_value(receipt.raw).raw,
            })
    }

    /// 执行强类型路由动作并返回完整的领域回执（保留完成状态与原生领域身份，供需要核验查询结果的调用方使用）。
    pub async fn call_typed_routed_receipt(
        &self,
        route: &RuntimeBotRoute,
        request: nuono_app::transport::TransportAction,
    ) -> Result<crate::domain::ActionReceipt> {
        let transport = self.typed_transport(route, &request.bot)?;
        nuono_app::action::call_action(transport.as_ref(), request)
            .await
            .map_err(native_action_error)
    }

    /// 供宿主使用的兼容入口：按已注册的 `BotIdentity`（含平台别名）反查唯一活跃路由；
    /// 绝不根据标准化的平台枚举名凭空拼凑路由地址。
    pub async fn notify_typed(&self, request: nuono_app::transport::TransportAction) -> Result<()> {
        let mut routes = self.connected_routes().into_iter().filter(|route| {
            super::runtime_transport::route_identity(route).is_ok_and(|bot| bot == request.bot)
        });
        let route = routes
            .next()
            .ok_or_else(|| NuonoError::Connection("Agent 回复目标已断开".into()))?;
        if routes.next().is_some() {
            return Err(NuonoError::Validation(
                "native bot has multiple routes; exact route required".into(),
            ));
        }
        self.notify_typed_routed(&route, request).await
    }
    pub async fn call(&self, route: &RuntimeBotRoute, action: Value) -> Result<Value> {
        self.call_as(
            route,
            action,
            nuono_app::action::ActionAuthority::Runtime {
                stage: "routed_api".into(),
            },
        )
        .await
    }
    pub async fn call_as(
        &self,
        route: &RuntimeBotRoute,
        mut action: Value,
        authority: nuono_app::action::ActionAuthority,
    ) -> Result<Value> {
        if let Some((transport, request)) = self.typed_request(route, &mut action)? {
            return nuono_app::action::call_action(
                transport.as_ref(),
                request.with_authority(authority),
            )
            .await
            .map(|receipt| receipt.raw)
            .map_err(|e| NuonoError::Platform(e.to_string()));
        }
        authority
            .authorize()
            .map_err(|e| NuonoError::PermissionDenied(e.to_string()))?;
        let sink = self.sink(route)?;
        sink.call_action(action).await
    }

    pub async fn call_receipt(
        &self,
        route: &RuntimeBotRoute,
        action: Value,
    ) -> Result<RuntimeActionReceipt> {
        self.call(route, action)
            .await
            .map(RuntimeActionReceipt::from_value)
    }

    pub async fn notify_action(
        &self,
        route: &RuntimeBotRoute,
        action: &str,
        params: Value,
    ) -> Result<()> {
        self.notify(route, json!({"action": action, "params": params}))
            .await
    }

    pub async fn call_action(
        &self,
        route: &RuntimeBotRoute,
        action: &str,
        params: Value,
    ) -> Result<Value> {
        self.call(route, json!({"action": action, "params": params}))
            .await
    }

    pub async fn call_action_receipt(
        &self,
        route: &RuntimeBotRoute,
        action: &str,
        params: Value,
    ) -> Result<RuntimeActionReceipt> {
        self.call_receipt(route, json!({"action": action, "params": params}))
            .await
    }

    fn sink(&self, route: &RuntimeBotRoute) -> Result<Arc<dyn RuntimeActionSink>> {
        self.routes
            .get(route)
            .map(|registered| registered.sink.clone())
            .ok_or_else(|| {
                NuonoError::Connection(format!(
                    "runtime bot route is not connected: platform={}, instance_id={}, bot_self_id={}",
                    route.platform, route.instance_id, route.bot_self_id
                ))
            })
    }
}

fn native_action_error(error: crate::domain::ActionError) -> NuonoError {
    use crate::domain::ActionErrorKind;
    match &error.kind {
        ActionErrorKind::InvalidTarget | ActionErrorKind::Rejected => {
            NuonoError::Validation(error.message)
        }
        ActionErrorKind::UnsupportedCapability => NuonoError::NotImplemented(error.message),
        ActionErrorKind::Unauthorized => NuonoError::PermissionDenied(error.message),
        _ => NuonoError::Platform(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingSink {
        calls: AtomicUsize,
        response: Value,
    }

    #[async_trait::async_trait]
    impl RuntimeActionSink for RecordingSink {
        async fn send_action(&self, _action: Value) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn call_action(&self, _action: Value) -> Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.response.clone())
        }
    }

    fn route() -> RuntimeBotRoute {
        RuntimeBotRoute::new("qq_official", "instance-a", "app-id-not-numeric")
    }

    #[tokio::test]
    async fn cancelling_routed_call_drops_the_polled_host_future_without_retry() {
        #[derive(Default)]
        struct BlockingSink {
            started: tokio::sync::Notify,
            dropped: tokio::sync::Notify,
            release: tokio::sync::Notify,
            calls: AtomicUsize,
            active: AtomicUsize,
            completed: AtomicUsize,
        }
        struct ActiveCall<'a>(&'a BlockingSink);
        impl Drop for ActiveCall<'_> {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
                self.0.dropped.notify_one();
            }
        }
        #[async_trait::async_trait]
        impl RuntimeActionSink for BlockingSink {
            async fn send_action(&self, _: Value) -> Result<()> {
                panic!("a routed call must use call_action");
            }
            async fn call_action(&self, _: Value) -> Result<Value> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.active.fetch_add(1, Ordering::SeqCst);
                let _active = ActiveCall(self);
                self.started.notify_one();
                self.release.notified().await;
                self.completed.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            }
        }

        let router = Arc::new(RuntimeActionRouter::new());
        let sink = Arc::new(BlockingSink::default());
        router.register(route(), sink.clone());
        let caller = tokio::spawn(async move {
            router
                .call_action(&route(), "read_user_avatar", json!({}))
                .await
        });
        // 收到 started 通知说明宿主运行时上的内部 Future 确实已经被首次轮询（poll）。
        tokio::time::timeout(std::time::Duration::from_secs(2), sink.started.notified())
            .await
            .expect("native call did not start");
        assert_eq!(sink.active.load(Ordering::SeqCst), 1);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), sink.dropped.notified())
            .await
            .expect("cancelled caller left a detached native call");
        assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.active.load(Ordering::SeqCst), 0);
        assert_eq!(sink.completed.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cross_runtime_notify_keeps_results_and_cancels_the_native_future_with_its_caller() {
        #[derive(Default)]
        struct BlockingSink {
            owner: std::sync::Mutex<Option<std::thread::ThreadId>>,
            started: tokio::sync::Notify,
            dropped: tokio::sync::Notify,
            release: tokio::sync::Notify,
            calls: AtomicUsize,
            active: AtomicUsize,
            completed: AtomicUsize,
        }
        struct ActiveSend<'a>(&'a BlockingSink);
        impl Drop for ActiveSend<'_> {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
                self.0.dropped.notify_one();
            }
        }
        #[async_trait::async_trait]
        impl RuntimeActionSink for BlockingSink {
            async fn send_action(&self, action: Value) -> Result<()> {
                assert_eq!(
                    Some(std::thread::current().id()),
                    *self.owner.lock().unwrap()
                );
                self.calls.fetch_add(1, Ordering::SeqCst);
                match action["action"].as_str() {
                    Some("fixture_error") => {
                        Err(NuonoError::Internal("fixture send failed".into()))
                    }
                    Some("fixture_success") => {
                        self.completed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                    _ => {
                        self.active.fetch_add(1, Ordering::SeqCst);
                        let _active = ActiveSend(self);
                        self.started.notify_one();
                        self.release.notified().await;
                        self.completed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }
            }
            async fn call_action(&self, _: Value) -> Result<Value> {
                panic!(
                    "notifications must exercise send_action, not the already guarded call path"
                );
            }
        }

        let router = Arc::new(RuntimeActionRouter::new());
        let sink = Arc::new(BlockingSink::default());
        let owner_router = router.clone();
        let owner_sink = sink.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let owner = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                *owner_sink.owner.lock().unwrap() = Some(std::thread::current().id());
                owner_router.register(route(), owner_sink);
                ready.send(()).unwrap();
                let _ = shutdown.await;
            });
        });
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_ne!(
            Some(std::thread::current().id()),
            *sink.owner.lock().unwrap()
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                router
                    .notify_action(&route(), "fixture_success", json!({}))
                    .await
                    .unwrap();
                let error = router
                    .notify_action(&route(), "fixture_error", json!({}))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("fixture send failed"));
                assert_eq!(sink.completed.load(Ordering::SeqCst), 1);
                let caller_router = router.clone();
                let caller = tokio::spawn(async move {
                    caller_router
                        .notify_action(&route(), "send_msg", json!({}))
                        .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), sink.started.notified())
                    .await
                    .expect("native notify did not start");
                assert_eq!(sink.active.load(Ordering::SeqCst), 1);
                caller.abort();
                assert!(caller.await.unwrap_err().is_cancelled());
                tokio::time::timeout(std::time::Duration::from_secs(2), sink.dropped.notified())
                    .await
                    .expect("cancelled caller detached native notify");
                assert_eq!(sink.active.load(Ordering::SeqCst), 0);
                assert_eq!(sink.calls.load(Ordering::SeqCst), 3);
                assert_eq!(sink.completed.load(Ordering::SeqCst), 1);
            });
        stop.send(()).unwrap();
        owner.join().unwrap();
    }

    #[test]
    fn routed_calls_preserve_owning_runtime_results_and_errors() {
        struct ThreadBoundSink(std::thread::ThreadId);
        #[async_trait::async_trait]
        impl RuntimeActionSink for ThreadBoundSink {
            async fn send_action(&self, _: Value) -> Result<()> {
                panic!("a routed call must use call_action");
            }
            async fn call_action(&self, action: Value) -> Result<Value> {
                assert_eq!(std::thread::current().id(), self.0);
                if action["action"] == "fixture_error" {
                    return Err(NuonoError::Internal("fixture call failed".into()));
                }
                Ok(json!({"message_id":"exact-001", "raw":{"status":"available"}}))
            }
        }

        let router = Arc::new(RuntimeActionRouter::new());
        let owner_router = router.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let owner = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                owner_router.register(
                    route(),
                    Arc::new(ThreadBoundSink(std::thread::current().id())),
                );
                ready.send(()).unwrap();
                let _ = shutdown.await;
            });
        });
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let receipt = router
                    .call_action_receipt(&route(), "read_user_avatar", json!({}))
                    .await
                    .unwrap();
                assert_eq!(receipt.message_ids, vec!["exact-001"]);
                assert_eq!(receipt.raw, json!({"status":"available"}));
                let error = router
                    .call_action(&route(), "fixture_error", json!({}))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("fixture call failed"));
            });
        stop.send(()).unwrap();
        owner.join().unwrap();
    }

    #[tokio::test]
    async fn stale_generation_cannot_remove_replacement() {
        let router = RuntimeActionRouter::new();
        let old = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let new = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let old_generation = router.register(route(), old.clone());
        let new_generation = router.register(route(), new.clone());

        assert!(!router.unregister(&route(), old_generation));
        router
            .notify_action(&route(), "send_msg", json!({}))
            .await
            .unwrap();
        assert_eq!(old.calls.load(Ordering::SeqCst), 0);
        assert_eq!(new.calls.load(Ordering::SeqCst), 1);
        assert!(router.unregister(&route(), new_generation));
        assert!(router.is_empty());
    }

    #[test]
    fn connected_routes_snapshot_is_sorted_and_tracks_live_registrations() {
        let router = RuntimeActionRouter::new();
        let later = RuntimeBotRoute::new("onebot", "200", "200");
        let earlier = RuntimeBotRoute::new("onebot", "100", "100");
        let sink = || {
            Arc::new(RecordingSink {
                calls: AtomicUsize::new(0),
                response: Value::Null,
            }) as Arc<dyn RuntimeActionSink>
        };

        let later_generation = router.register(later.clone(), sink());
        let earlier_generation = router.register(earlier.clone(), sink());
        assert_eq!(
            router.connected_routes(),
            vec![earlier.clone(), later.clone()]
        );

        assert!(router.unregister(&earlier, earlier_generation));
        assert_eq!(router.connected_routes(), vec![later.clone()]);
        assert!(router.unregister(&later, later_generation));
        assert!(router.connected_routes().is_empty());
    }

    #[tokio::test]
    async fn typed_replies_use_exact_instance_and_current_generation() {
        use nuono_domain::{
            DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, MessageSegment,
            OutboundAction, Platform,
        };
        let router = RuntimeActionRouter::new();
        let old = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let current = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let unrelated = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let old_generation = router.register(route(), old.clone());
        let generation = router.register(route(), current.clone());
        router.register(
            RuntimeBotRoute::new("qq_official", "instance-b", "app-id-not-numeric"),
            unrelated.clone(),
        );
        assert!(!router.unregister(&route(), old_generation));
        let bot = super::super::runtime_transport::route_identity(&route()).unwrap();
        let action = OutboundAction::SendMessage {
            target: DeliveryTarget::Actor {
                actor: IdentityRef::new(
                    Platform::QqOfficial,
                    IdentityKind::User,
                    IdentityScheme::OpenId,
                    "User_ExactId",
                    Some("app-id-not-numeric"),
                )
                .unwrap(),
            },
            segments: vec![MessageSegment::Text {
                text: "continued".into(),
            }],
            reply_to: None,
        };
        let request = nuono_app::transport::TransportAction::new(bot, action).with_authority(
            nuono_app::action::ActionAuthority::Runtime {
                stage: "AgentRuntimeResume".into(),
            },
        );
        router.notify_typed(request.clone()).await.unwrap();
        assert_eq!(old.calls.load(Ordering::SeqCst), 0);
        assert_eq!(current.calls.load(Ordering::SeqCst), 1);
        assert_eq!(unrelated.calls.load(Ordering::SeqCst), 0);
        assert!(router.unregister(&route(), generation));
        assert!(router.notify_typed(request).await.is_err());
        assert_eq!(unrelated.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelling_native_call_and_notify_drops_host_work_once() {
        #[derive(Default)]
        struct PendingSink {
            started: tokio::sync::Notify,
            dropped: tokio::sync::Notify,
            calls: AtomicUsize,
        }
        struct Pending<'a>(&'a PendingSink);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.dropped.notify_one();
            }
        }
        #[async_trait::async_trait]
        impl RuntimeActionSink for PendingSink {
            async fn send_action(&self, _: Value) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _pending = Pending(self);
                self.started.notify_one();
                std::future::pending().await
            }
            async fn call_action(&self, action: Value) -> Result<Value> {
                self.send_action(action).await?;
                Ok(Value::Null)
            }
        }
        for notify in [false, true] {
            let router = Arc::new(RuntimeActionRouter::new());
            let sink = Arc::new(PendingSink::default());
            let route = RuntimeBotRoute::new("onebot", "native", "700");
            router.register(route.clone(), sink.clone());
            let request = nuono_app::transport::TransportAction::new(
                super::super::runtime_transport::route_identity(&route).unwrap(),
                crate::domain::OutboundAction::SendMessage {
                    target: crate::domain::DeliveryTarget::actor(
                        crate::domain::IdentityRef::new(
                            crate::domain::Platform::OneBot,
                            crate::domain::IdentityKind::User,
                            crate::domain::IdentityScheme::Native,
                            "00123",
                            None::<String>,
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                    segments: vec![crate::domain::MessageSegment::text("native")],
                    reply_to: None,
                },
            );
            let caller = tokio::spawn(async move {
                if notify {
                    router.notify_typed_routed(&route, request).await
                } else {
                    router.call_typed_routed(&route, request).await.map(|_| ())
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), sink.started.notified())
                .await
                .unwrap();
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            tokio::time::timeout(std::time::Duration::from_secs(2), sink.dropped.notified())
                .await
                .unwrap();
            assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn native_alias_call_uses_exact_route_generation_and_authority() {
        use crate::domain::{
            DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, MessageSegment,
            OutboundAction, Platform,
        };
        let router = RuntimeActionRouter::new();
        let alias = RuntimeBotRoute::new("qq", "instance-a", "app-id-not-numeric");
        let old = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let current = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: json!({"message_id":"00042"}),
        });
        let unrelated = Arc::new(RecordingSink {
            calls: AtomicUsize::new(0),
            response: Value::Null,
        });
        let old_generation = router.register(alias.clone(), old.clone());
        let generation = router.register(alias.clone(), current.clone());
        router.register(
            RuntimeBotRoute::new("qq", "instance-b", "app-id-not-numeric"),
            unrelated.clone(),
        );
        assert!(!router.unregister(&alias, old_generation));
        let request = nuono_app::transport::TransportAction::new(
            super::super::runtime_transport::route_identity(&alias).unwrap(),
            OutboundAction::SendMessage {
                target: DeliveryTarget::actor(
                    IdentityRef::new(
                        Platform::QqOfficial,
                        IdentityKind::User,
                        IdentityScheme::OpenId,
                        "same",
                        Some("app-id-not-numeric"),
                    )
                    .unwrap(),
                )
                .unwrap(),
                segments: vec![MessageSegment::text("native")],
                reply_to: None,
            },
        );
        let denied = request
            .clone()
            .with_authority(nuono_app::action::ActionAuthority::Runtime {
                stage: String::new(),
            });
        assert!(router.call_typed_routed(&alias, denied).await.is_err());
        let receipt = router
            .call_typed_routed(&alias, request.clone())
            .await
            .unwrap();
        assert_eq!(receipt.message_ids, ["00042"]);
        router.notify_typed(request.clone()).await.unwrap();
        assert!(
            router
                .call_typed_routed(&route(), request.clone())
                .await
                .is_err()
        );
        assert_eq!(old.calls.load(Ordering::SeqCst), 0);
        assert_eq!(current.calls.load(Ordering::SeqCst), 2);
        assert_eq!(unrelated.calls.load(Ordering::SeqCst), 0);
        assert!(router.unregister(&alias, generation));
        assert!(router.call_typed_routed(&alias, request).await.is_err());
    }

    #[tokio::test]
    async fn receipt_preserves_exact_string_ids_and_raw_payload() {
        let router = RuntimeActionRouter::new();
        router.register(
            route(),
            Arc::new(RecordingSink {
                calls: AtomicUsize::new(0),
                response: json!({
                    "message_id": "snowflake-001",
                    "message_ids": ["snowflake-002", 3, "snowflake-002"],
                    "raw": {"platform": "response"}
                }),
            }),
        );

        let receipt = router
            .call_action_receipt(&route(), "send_msg", json!({}))
            .await
            .unwrap();
        assert_eq!(receipt.message_ids, vec!["snowflake-002", "3"]);
        assert_eq!(receipt.raw, json!({"platform": "response"}));
    }
}
