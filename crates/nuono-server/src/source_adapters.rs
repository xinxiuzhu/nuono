//! 已安装源码平台适配器的实例配置持久化与运行时生命周期编排。
#[cfg(test)]
#[path = "../tests/unit/source_adapters_qr.rs"]
mod qr_tests;
use crate::{
    adapter_host::SourceAdapterHost,
    bot_instances::{
        BotInstanceIngressView, BotInstanceView, CreateBotInstanceRequest, UpdateBotInstanceRequest,
    },
};
use dashmap::DashMap;
use nuono_adapter::{AdapterDescriptor, AdapterHost, AdapterInstanceConfig, AdapterRuntime};
use nuono_core::error::{NuonoError, Result};
use nuono_core::{
    adapter::{AdapterConfig, AdapterManager},
    core::EventKernel,
    db::Database,
    service::bot_instances::{
        self, BotInstance, NewBotInstance, RUNTIME_STATUS_ERROR, RUNTIME_STATUS_OFFLINE,
        RUNTIME_STATUS_ONLINE, UpdateBotInstance,
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, RwLock, watch};

struct Active {
    adapter_type: String,
    exclusive_key: Option<String>,
    runtime: Arc<dyn AdapterRuntime>,
    host: Arc<SourceAdapterHost>,
    stop: watch::Sender<bool>,
    running: AtomicBool,
    connection_live: AtomicBool,
}
impl Active {
    fn cancel(&self) {
        self.running.store(false, Ordering::Release);
        self.stop.send_replace(true);
        self.host.ingress.cancel();
    }
}
struct RunGuard(Arc<Active>);
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        self.0.connection_live.store(false, Ordering::Release);
        self.0.host.ingress.cancel();
    }
}
struct StartGuard {
    active: Arc<Active>,
    committed: bool,
}
impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.active.cancel();
            self.active.running.store(false, Ordering::Release);
            self.active.connection_live.store(false, Ordering::Release);
        }
    }
}

pub(crate) struct SourceInstances {
    pub(crate) registry: Arc<nuono_adapter::AdapterRegistry>,
    closed: tokio_util::sync::CancellationToken,
    listeners: DashMap<String, tokio_util::sync::CancellationToken>,
    db: Database,
    manager: Arc<AdapterManager>,
    kernel: Arc<EventKernel>,
    active: RwLock<HashMap<String, Arc<Active>>>,
    legacy: RwLock<HashMap<String, BotInstance>>,
    locks: DashMap<String, Arc<Mutex<()>>>,
    claims: Mutex<()>,
}

impl SourceInstances {
    pub(crate) fn new(
        db: Database,
        manager: Arc<AdapterManager>,
        kernel: Arc<EventKernel>,
    ) -> Self {
        let registry = nuono_adapter::running();
        let closed = tokio_util::sync::CancellationToken::new();
        let listeners = registry
            .descriptors()
            .into_iter()
            .filter(|descriptor| {
                registry
                    .get(&descriptor.id)
                    .is_some_and(|provider| provider.legacy_codec().is_some())
            })
            .map(|descriptor| (descriptor.id, closed.child_token()))
            .collect();
        Self {
            registry,
            closed,
            listeners,
            db,
            manager,
            kernel,
            active: RwLock::new(HashMap::new()),
            legacy: RwLock::new(HashMap::new()),
            locks: DashMap::new(),
            claims: Mutex::new(()),
        }
    }
    pub(crate) fn close(&self) {
        self.closed.cancel();
    }
    pub(crate) fn listener_token(&self, id: &str) -> Option<tokio_util::sync::CancellationToken> {
        self.listeners
            .get(id)
            .filter(|token| !token.is_cancelled())
            .map(|token| token.clone())
    }
    fn lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    async fn record(&self, id: &str) -> Result<BotInstance> {
        if let Some(record) = self.legacy.read().await.get(id).cloned() {
            return Ok(record);
        }
        bot_instances::get(&self.db, id)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("Bot 实例 {id}")))
    }
    async fn writable(&self, id: &str) -> Result<()> {
        if self.legacy.read().await.contains_key(id) {
            return Err(NuonoError::Validation("请在原运行配置中管理此实例".into()));
        }
        Ok(())
    }
    pub(crate) async fn get(&self, id: &str) -> Result<BotInstanceView> {
        Ok(self.view(self.record(id).await?).await)
    }
    pub(crate) async fn legacy_views(&self) -> Vec<BotInstanceView> {
        let records: Vec<_> = self.legacy.read().await.values().cloned().collect();
        let mut views = Vec::new();
        for record in records {
            views.push(self.view(record).await);
        }
        views
    }
    pub(crate) async fn start_legacy(
        &self,
        id: &str,
        adapter_type: &str,
        name: &str,
        config: Value,
        secrets: Value,
    ) -> Result<()> {
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        let platform = self
            .registry
            .get(adapter_type)
            .map(|provider| provider.descriptor().platform)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| adapter_type.to_owned());
        self.legacy.write().await.insert(
            id.into(),
            BotInstance {
                id: id.into(),
                name: name.into(),
                platform,
                namespace: String::new(),
                adapter_type: adapter_type.into(),
                native_bot_id: None,
                enabled: true,
                config,
                secrets,
                runtime_status: RUNTIME_STATUS_OFFLINE.into(),
                last_error: None,
                last_connected_at_ms: None,
                revision: 1,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
        );
        self.start_locked(id).await
    }
    pub(crate) async fn create(
        &self,
        request: CreateBotInstanceRequest,
    ) -> Result<BotInstanceView> {
        let provider = self
            .registry
            .get(&request.adapter_type)
            .ok_or_else(|| NuonoError::Validation("适配器未安装，请先安装并重新构建应用".into()))?;
        if !provider.supports_instances() {
            return Err(NuonoError::Validation(
                "此适配器由客户端主动接入，无需创建实例".into(),
            ));
        }
        let descriptor = provider.descriptor();
        let platform = if descriptor.platform.trim().is_empty() {
            request.adapter_type.clone()
        } else {
            descriptor.platform.clone()
        };
        let (config, secrets) = normalize(&descriptor, request.config, &json!({}))?;
        provider.validate_config(&config)?;
        let record = bot_instances::create(
            &self.db,
            NewBotInstance {
                name: request.name,
                platform,
                namespace: request.namespace,
                adapter_type: request.adapter_type,
                native_bot_id: request.native_bot_id,
                enabled: request.enabled,
                config,
                secrets,
                runtime_status: RUNTIME_STATUS_OFFLINE.into(),
            },
        )
        .await?;
        if record.enabled && self.kernel.business_is_running() {
            let _ = self.start(&record.id).await;
        }
        self.get(&record.id).await
    }
    pub(crate) async fn update(
        &self,
        id: &str,
        request: UpdateBotInstanceRequest,
    ) -> Result<BotInstanceView> {
        self.writable(id).await?;
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        let record = self.record(id).await?;
        let enabled = request.enabled.unwrap_or(record.enabled);
        let name = request.name.unwrap_or_else(|| record.name.clone());
        let namespace = request
            .namespace
            .unwrap_or_else(|| record.namespace.clone());
        let native_bot_id = match request.native_bot_id {
            Some(value) => Some(value),
            None => record.native_bot_id.clone(),
        };
        let (config, secrets) = if let Some(update) = request.config {
            let provider = self.registry.get(&record.adapter_type).ok_or_else(|| {
                NuonoError::Validation("修改实例配置前请重新安装对应适配器".into())
            })?;
            let mut merged = record.config.as_object().cloned().unwrap_or_default();
            let update = update
                .as_object()
                .ok_or_else(|| NuonoError::Validation("实例配置必须是 JSON 对象".into()))?;
            merged.extend(update.clone());
            let normalized = normalize(
                &provider.descriptor(),
                Value::Object(merged),
                &record.secrets,
            )?;
            provider.validate_config(&normalized.0)?;
            normalized
        } else {
            (record.config.clone(), json!({}))
        };
        let needs_restart = enabled != record.enabled
            || namespace != record.namespace
            || native_bot_id != record.native_bot_id
            || config != record.config
            || secrets.as_object().is_some_and(|values| !values.is_empty());
        bot_instances::update_config_and_secrets(
            &self.db,
            id,
            UpdateBotInstance {
                name,
                platform: record.platform,
                namespace,
                adapter_type: record.adapter_type,
                native_bot_id,
                enabled,
                config,
                secrets,
                expected_revision: request.expected_revision,
            },
        )
        .await?;
        if !needs_restart {
            return self.get(id).await;
        }
        self.stop_locked(id).await;
        bot_instances::update_status(&self.db, id, RUNTIME_STATUS_OFFLINE, None).await?;
        if enabled && self.kernel.business_is_running() {
            self.start_locked(id).await?;
        }
        self.get(id).await
    }
    pub(crate) async fn start(&self, id: &str) -> Result<BotInstanceView> {
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        self.start_locked(id).await?;
        self.get(id).await
    }
    pub(crate) async fn stop(&self, id: &str) -> Result<BotInstanceView> {
        self.writable(id).await?;
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        let record = self.record(id).await?;
        self.stop_locked(id).await;
        bot_instances::update_config_and_secrets(
            &self.db,
            id,
            UpdateBotInstance {
                name: record.name,
                platform: record.platform,
                namespace: record.namespace,
                adapter_type: record.adapter_type,
                native_bot_id: record.native_bot_id,
                enabled: false,
                config: record.config,
                secrets: json!({}),
                expected_revision: None,
            },
        )
        .await?;
        bot_instances::update_status(&self.db, id, RUNTIME_STATUS_OFFLINE, None).await?;
        self.get(id).await
    }
    pub(crate) async fn restart(&self, id: &str) -> Result<BotInstanceView> {
        self.writable(id).await?;
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        self.record(id).await?;
        self.stop_locked(id).await;
        self.start_locked(id).await?;
        self.get(id).await
    }
    pub(crate) async fn delete(&self, id: &str) -> Result<()> {
        self.writable(id).await?;
        let lock = self.lock(id);
        let _guard = lock.lock().await;
        self.record(id).await?;
        self.stop_locked(id).await;
        bot_instances::delete(&self.db, id).await?;
        // 不从 `self.locks` 中移除该实例的 Mutex：其他并发等待者可能已拿到该 `Arc<Mutex<()>>`，移除会导致后续请求新建另一把锁而破坏互斥。
        Ok(())
    }
    pub(crate) async fn handle_request(
        &self,
        adapter: &str,
        id: &str,
        request: nuono_adapter::AdapterRequest,
    ) -> Result<nuono_adapter::AdapterResponse> {
        let active = self
            .active
            .read()
            .await
            .get(id)
            .cloned()
            .filter(|active| {
                active.adapter_type == adapter && active.running.load(Ordering::Acquire)
            })
            .ok_or_else(|| NuonoError::NotFound("回调实例未运行或适配器未安装".into()))?;
        active.runtime.handle_request(request).await
    }
    pub(crate) async fn cancel_all(&self) {
        for active in self.active.read().await.values() {
            active.cancel();
        }
    }
    pub(crate) async fn suspend_type(&self, adapter: &str) {
        if let Some(token) = self.listeners.get(adapter) {
            token.cancel();
        }
        let ids: Vec<_> = self
            .active
            .read()
            .await
            .iter()
            .filter(|(_, active)| active.adapter_type == adapter)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let lock = self.lock(&id);
            let _guard = lock.lock().await;
            self.stop_locked(&id).await;
        }
    }
    pub(crate) async fn shutdown(&self) {
        self.close();
        let ids: Vec<_> = self.active.read().await.keys().cloned().collect();
        for id in ids {
            let lock = self.lock(&id);
            let _guard = lock.lock().await;
            self.stop_locked(&id).await;
        }
    }
    async fn start_locked(&self, id: &str) -> Result<()> {
        if !self.kernel.business_is_running() {
            return Err(NuonoError::Conflict(
                "业务尚未启动，请先完成管理员设置".into(),
            ));
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), self.start_impl(id))
            .await
            .unwrap_or_else(|_| Err(NuonoError::Conflict("平台实例启动超过 30 秒".into())));
        if let Err(error) = &result {
            let secrets = self
                .record(id)
                .await
                .ok()
                .map(|record| collect_secret_values(&record.secrets))
                .unwrap_or_default();
            let message = format_instance_error("适配器启动失败", error, &secrets);
            tracing::warn!(
                instance_id = id,
                kind = error.kind(),
                detail = %message,
                "适配器启动失败"
            );
            if let Some(record) = self.legacy.write().await.get_mut(id) {
                record.runtime_status = RUNTIME_STATUS_ERROR.into();
                record.last_error = Some(message);
            } else {
                let _ = bot_instances::update_status(
                    &self.db,
                    id,
                    RUNTIME_STATUS_ERROR,
                    Some(&message),
                )
                .await;
            }
        }
        result
    }
    async fn start_impl(&self, id: &str) -> Result<()> {
        if self.closed.is_cancelled() || !self.kernel.business_is_running() {
            return Err(NuonoError::Conflict("宿主业务未运行或正在关闭".into()));
        }
        if self
            .active
            .read()
            .await
            .get(id)
            .is_some_and(|active| active.running.load(Ordering::Acquire))
        {
            return Ok(());
        }
        self.stop_locked(id).await;
        let record = self.record(id).await?;
        let provider = self
            .registry
            .get(&record.adapter_type)
            .ok_or_else(|| NuonoError::Validation("适配器未安装，请先安装并重新构建应用".into()))?;
        let definition = provider.descriptor();
        let (config, _) = normalize(&definition, record.config.clone(), &record.secrets)?;
        provider.validate_config(&config)?;
        let _claims = self.claims.lock().await;
        let key = provider
            .exclusive_key(&config)
            .or_else(|| provider.credential_key(&record.secrets));
        if self.active.read().await.values().any(|active| {
            active.connection_live.load(Ordering::Acquire)
                && active.adapter_type == record.adapter_type
                && ((!definition.supports_multiple_instances)
                    || key
                        .as_ref()
                        .is_some_and(|key| active.exclusive_key.as_ref() == Some(key)))
        }) {
            return Err(NuonoError::Conflict(
                "该适配器账号已有运行实例，请先停止原实例".into(),
            ));
        }
        let host = SourceAdapterHost::new(&definition.platform, id, self.kernel.clone());
        let runtime = provider
            .create(
                AdapterInstanceConfig {
                    id: id.into(),
                    config,
                    secrets: record.secrets.clone(),
                },
                host.clone(),
            )
            .await?;
        if self.closed.is_cancelled() || !self.kernel.business_is_running() {
            return Err(NuonoError::Conflict(
                "宿主业务已停止，取消平台连接启动".into(),
            ));
        }
        let (stop, stop_rx) = watch::channel(false);
        let active = Arc::new(Active {
            adapter_type: record.adapter_type.clone(),
            exclusive_key: key,
            runtime,
            host,
            stop,
            running: AtomicBool::new(true),
            connection_live: AtomicBool::new(true),
        });
        self.active.write().await.insert(id.into(), active.clone());
        let mut guard = StartGuard {
            active: active.clone(),
            committed: false,
        };
        self.manager
            .register(
                AdapterConfig {
                    id: id.into(),
                    adapter_type: record.adapter_type.clone(),
                    enabled: true,
                    config: record.config.clone(),
                },
                active.runtime.platform(),
            )
            .await?;
        self.manager.start(id).await?;
        if !self.legacy.read().await.contains_key(id) {
            bot_instances::update_config_and_secrets(
                &self.db,
                id,
                UpdateBotInstance {
                    name: record.name,
                    platform: record.platform,
                    namespace: record.namespace,
                    adapter_type: record.adapter_type,
                    native_bot_id: record.native_bot_id,
                    enabled: true,
                    config: record.config,
                    secrets: json!({}),
                    expected_revision: None,
                },
            )
            .await?;
            bot_instances::update_status(&self.db, id, RUNTIME_STATUS_ONLINE, None).await?;
        }
        let run_guard = RunGuard(active.clone());
        let task_runtime = active.runtime.clone();
        let db = self.db.clone();
        let task_id = id.to_owned();
        let legacy = self.legacy.read().await.contains_key(id);
        let closed = self.closed.clone();
        let shutdown_active = active.clone();
        let secret_values = collect_secret_values(&record.secrets);
        let task = tokio::spawn(async move {
            let _run = run_guard;
            let run = task_runtime.run(stop_rx);
            tokio::pin!(run);
            let result = tokio::select! { biased; _=closed.cancelled()=>{ shutdown_active.cancel();run.await }, result=&mut run=>result };
            if let Err(error) = result {
                let message = format_instance_error("适配器运行失败", &error, &secret_values);
                if !legacy {
                    let _ = bot_instances::update_status(
                        &db,
                        &task_id,
                        RUNTIME_STATUS_ERROR,
                        Some(&message),
                    )
                    .await;
                }
                tracing::warn!(
                    instance_id = task_id,
                    kind = error.kind(),
                    detail = %message,
                    "适配器运行结束"
                );
            }
        });
        self.manager
            .attach_task(id, task, Some(active.stop.clone()))
            .await;
        guard.committed = true;
        Ok(())
    }
    async fn stop_locked(&self, id: &str) {
        let active = self.active.read().await.get(id).cloned();
        if let Some(active) = active {
            active.cancel();
            let _ = self.manager.stop(id).await;
            active.host.shutdown_ingress().await;
            let _ = self.manager.remove(id).await;
            self.active.write().await.remove(id);
        }
    }
    pub(crate) async fn view(&self, record: BotInstance) -> BotInstanceView {
        let provider = self.registry.get(&record.adapter_type);
        let installed = provider.is_some();
        let secret_keys = provider
            .as_ref()
            .map(|provider| provider.descriptor().secret_fields)
            .unwrap_or_default();
        let callback_path =
            provider.and_then(|provider| provider.callback_path(&record.id, &record.config));
        let active = self.active.read().await.get(&record.id).cloned();
        let running = active
            .as_ref()
            .is_some_and(|active| active.running.load(Ordering::Acquire));
        let status = active
            .as_ref()
            .map(|active| active.runtime.status())
            .unwrap_or_default();
        let profile = active
            .as_ref()
            .map(|active| crate::bot_instances::sanitize_profile(active.runtime.profile()))
            .unwrap_or_default();
        let legacy = self.legacy.read().await.contains_key(&record.id);
        let configured_secrets = record
            .secrets
            .as_object()
            .map(|values| {
                values
                    .iter()
                    .filter(|(_, value)| value.as_str().is_some_and(|value| !value.is_empty()))
                    .map(|(key, _)| key.clone())
                    .collect()
            })
            .unwrap_or_default();
        let connection_mode = record
            .config
            .get("connection_mode")
            .and_then(Value::as_str)
            .unwrap_or(if callback_path.is_some() {
                "webhook"
            } else {
                "native"
            })
            .to_owned();
        let environment = record
            .config
            .get("environment")
            .and_then(Value::as_str)
            .unwrap_or("production")
            .to_owned();
        let mut public = record.config.clone();
        if let Some(values) = public.as_object_mut() {
            for key in secret_keys {
                values.remove(&key);
            }
        }
        if let (Some(public), Some(secrets)) = (public.as_object_mut(), record.secrets.as_object())
        {
            for key in secrets.keys() {
                public.remove(key);
            }
        }
        let state = if !installed {
            "adapter_missing"
        } else if running {
            "running"
        } else if record.runtime_status == RUNTIME_STATUS_ERROR {
            "error"
        } else {
            "stopped"
        };
        let live_runtime_status = if running {
            RUNTIME_STATUS_ONLINE
        } else if record.runtime_status == RUNTIME_STATUS_ERROR {
            RUNTIME_STATUS_ERROR
        } else {
            RUNTIME_STATUS_OFFLINE
        };
        BotInstanceView {
            id: record.id,
            name: record.name,
            platform: record.platform,
            namespace: record.namespace,
            adapter_type: record.adapter_type,
            native_bot_id: record.native_bot_id,
            enabled: record.enabled,
            config: public,
            configured_secrets,
            status: state.into(),
            runtime_status: live_runtime_status.into(),
            connected: running && status.connected,
            connection_mode,
            environment,
            ingress: BotInstanceIngressView {
                state: if !installed {
                    "adapter_missing"
                } else if running {
                    "running"
                } else {
                    "stopped"
                }
                .into(),
                callback_path,
                last_event_at: status.last_event_at,
            },
            avatar_url: profile.avatar_url,
            last_error: record.last_error.or(status.last_error),
            last_connected_at_ms: record.last_connected_at_ms,
            revision: record.revision,
            source: if legacy { "config" } else { "database" }.into(),
            deletable: !legacy,
            created_at: (!legacy).then(|| record.created_at_ms.to_string()),
            updated_at: (!legacy).then(|| record.updated_at_ms.to_string()),
        }
    }
}

fn normalize(
    descriptor: &AdapterDescriptor,
    input: Value,
    existing_secrets: &Value,
) -> Result<(Value, Value)> {
    descriptor.validate()?;
    let input = input
        .as_object()
        .ok_or_else(|| NuonoError::Validation("实例配置必须是 JSON 对象".into()))?;
    let mut config = descriptor
        .defaults()
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut secrets = serde_json::Map::new();
    for (key, value) in input {
        if !descriptor.fields.iter().any(|field| &field.key == key) {
            return Err(NuonoError::Validation(format!(
                "未声明的适配器配置字段: {key}"
            )));
        }
        config.insert(key.clone(), value.clone());
    }
    for field in &descriptor.fields {
        if descriptor.secret_fields.contains(&field.key) {
            let candidate = config.remove(&field.key).unwrap_or_default();
            let value = if candidate.as_str().is_some_and(|value| !value.is_empty()) {
                secrets.insert(field.key.clone(), candidate.clone());
                candidate
            } else {
                existing_secrets
                    .get(&field.key)
                    .cloned()
                    .unwrap_or(Value::String(String::new()))
            };
            if field.required && value.as_str().is_none_or(|value| value.trim().is_empty()) {
                return Err(NuonoError::Validation(format!(
                    "适配器缺少必填字段: {}",
                    field.key
                )));
            }
            nuono_adapter::config_validation::validate_config_field_value(
                &field.key, &value, field,
            )
            .map_err(NuonoError::Validation)?;
        } else {
            let value = &config[&field.key];
            if field.required
                && (value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty()))
            {
                return Err(NuonoError::Validation(format!(
                    "适配器缺少必填字段: {}",
                    field.key
                )));
            }
            nuono_adapter::config_validation::validate_config_field_value(&field.key, value, field)
                .map_err(NuonoError::Validation)?;
        }
    }
    Ok((Value::Object(config), Value::Object(secrets)))
}

const MAX_INSTANCE_ERROR_CHARS: usize = 240;

fn collect_secret_values(value: &Value) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.len() >= 4 {
                    out.push(trimmed.to_owned());
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            Value::Object(map) => {
                for item in map.values() {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    walk(value, &mut values);
    values.sort_by_key(|item| std::cmp::Reverse(item.len()));
    values.dedup();
    values
}

fn format_instance_error(prefix: &str, error: &NuonoError, secret_values: &[String]) -> String {
    let detail = match error {
        NuonoError::Platform(text)
        | NuonoError::Validation(text)
        | NuonoError::Conflict(text)
        | NuonoError::Unauthorized(text)
        | NuonoError::Timeout(text) => Some(text.as_str()),
        _ => None,
    };
    let Some(detail) = detail else {
        return format!("{prefix}（{}）", error.kind());
    };
    let mut cleaned: String = detail
        .chars()
        .filter(|&ch| ch != '\0')
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    for secret in secret_values {
        if !secret.is_empty() && cleaned.contains(secret.as_str()) {
            cleaned = cleaned.replace(secret.as_str(), "[REDACTED]");
        }
    }
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return format!("{prefix}（{}）", error.kind());
    }
    let mut chars = normalized.chars();
    let truncated: String = chars.by_ref().take(MAX_INSTANCE_ERROR_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor {
            id: "fixture".into(),
            platform: "fixture".into(),
            display_name: "Fixture".into(),
            description: String::new(),
            supports_multiple_instances: true,
            fields: vec![
                nuono_adapter::ConfigField::password("token", "Token", "").required(),
                nuono_adapter::ConfigField::text(
                    "endpoint",
                    "Endpoint",
                    "",
                    "https://example.invalid",
                ),
            ],
            secret_fields: vec!["token".into()],
        }
    }
    #[test]
    fn secrets_never_enter_public_config_and_blank_updates_preserve_them() {
        let (public, secrets) =
            normalize(&descriptor(), json!({"token":"synthetic"}), &json!({})).unwrap();
        assert!(public.get("token").is_none());
        assert_eq!(secrets["token"], "synthetic");
        let (public, patch) = normalize(&descriptor(), json!({"token":""}), &secrets).unwrap();
        assert!(public.get("token").is_none());
        assert_eq!(patch, json!({}));
        assert!(normalize(&descriptor(), json!({}), &json!({})).is_err());
    }
    #[test]
    fn unknown_fields_are_rejected_before_persistence() {
        assert!(
            normalize(
                &descriptor(),
                json!({"token":"synthetic", "undeclared":true}),
                &json!({})
            )
            .is_err()
        );
    }
    type StopGate = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
    struct Provider(Option<StopGate>);
    #[async_trait::async_trait]
    impl nuono_adapter::AdapterProvider for Provider {
        fn descriptor(&self) -> AdapterDescriptor {
            descriptor()
        }
        fn credential_key(&self, secrets: &Value) -> Option<String> {
            secrets["token"].as_str().map(str::to_owned)
        }
        fn normalize(
            &self,
            _: &nuono_adapter::BotIdentity,
            _: &nuono_adapter::UnifiedEvent,
            _: chrono::DateTime<chrono::Utc>,
        ) -> std::result::Result<nuono_adapter::DomainEvent, nuono_adapter::DomainModelError>
        {
            unreachable!("no events produced by fixture")
        }
        async fn create(
            &self,
            config: AdapterInstanceConfig,
            host: Arc<dyn AdapterHost>,
        ) -> Result<Arc<dyn AdapterRuntime>> {
            Ok(Arc::new(Runtime {
                id: config.id,
                host,
                stop_gate: self
                    .0
                    .clone()
                    .filter(|_| config.config["endpoint"] == "hold"),
            }))
        }
    }
    struct Runtime {
        id: String,
        host: Arc<dyn AdapterHost>,
        stop_gate: Option<StopGate>,
    }
    #[tokio::test]
    async fn enabled_configuration_waits_for_business_and_cannot_restart_after_shutdown() {
        let fixture = crate::api::contract_tests::Fixture::prepared(false).await;
        let mut source = SourceInstances::new(
            fixture.state.db.clone(),
            Arc::new(AdapterManager::new()),
            fixture.state.manager.event_kernel.clone(),
        );
        let mut registry = nuono_adapter::AdapterRegistry::default();
        registry.register(Arc::new(Provider(None))).unwrap();
        source.registry = Arc::new(registry);
        let instance = source
            .create(CreateBotInstanceRequest {
                name: "waiting-for-setup".into(),
                adapter_type: "fixture".into(),
                enabled: true,
                config: json!({"token":"synthetic-startup-token"}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(instance.enabled);
        assert_eq!(instance.status, "stopped");
        assert!(source.active.read().await.is_empty());
        let updated = source
            .update(
                &instance.id,
                UpdateBotInstanceRequest {
                    config: Some(json!({"endpoint":"https://changed.invalid"})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.status, "stopped");
        assert!(matches!(
            source.start(&instance.id).await,
            Err(NuonoError::Conflict(_))
        ));
        assert!(source.active.read().await.is_empty());

        fixture.state.manager.start_business().await.unwrap();
        source.start(&instance.id).await.unwrap();
        assert_eq!(source.get(&instance.id).await.unwrap().status, "running");
        fixture.state.manager.request_business_shutdown();
        source.shutdown().await;
        assert!(matches!(
            source.start(&instance.id).await,
            Err(NuonoError::Conflict(_))
        ));
        fixture.close().await;
    }
    struct Sink;
    #[async_trait::async_trait]
    impl nuono_adapter::RuntimeActionSink for Sink {
        async fn send_action(&self, _: Value) -> Result<()> {
            Ok(())
        }
        async fn call_action(&self, _: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }
    #[async_trait::async_trait]
    impl AdapterRuntime for Runtime {
        fn platform(&self) -> Arc<dyn nuono_adapter::PlatformAdapter> {
            Arc::new(nuono_adapter_fixture::FixtureAdapter)
        }
        async fn run(&self, mut stop: watch::Receiver<bool>) -> Result<()> {
            let _lease = self.host.register_route(
                nuono_adapter::RuntimeBotRoute::new("fixture", &self.id, "fixture-account"),
                Arc::new(Sink),
            )?;
            while !*stop.borrow_and_update() && stop.changed().await.is_ok() {}
            if let Some((entered, release)) = &self.stop_gate {
                entered.notify_one();
                release.notified().await;
            }
            self.host.shutdown_ingress().await;
            Ok(())
        }
    }
    #[tokio::test]
    async fn lifecycle_keeps_credentials_and_missing_adapter_records_without_exposing_values() {
        let fixture = crate::api::contract_tests::Fixture::new(false).await;
        let mut source = SourceInstances::new(
            fixture.state.db.clone(),
            Arc::new(AdapterManager::new()),
            fixture.state.manager.event_kernel.clone(),
        );
        let mut registry = nuono_adapter::AdapterRegistry::default();
        registry.register(Arc::new(Provider(None))).unwrap();
        source.registry = Arc::new(registry);
        let view = source
            .create(CreateBotInstanceRequest {
                name: "fixture".into(),
                adapter_type: "fixture".into(),
                enabled: false,
                config: json!({"token":"synthetic-secret"}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(view.config.get("token").is_none());
        assert!(
            !serde_json::to_string(&view)
                .unwrap()
                .contains("synthetic-secret")
        );
        source
            .update(
                &view.id,
                UpdateBotInstanceRequest {
                    config: Some(json!({"token":""})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            bot_instances::get(&fixture.state.db, &view.id)
                .await
                .unwrap()
                .unwrap()
                .secrets["token"],
            "synthetic-secret"
        );
        source.start(&view.id).await.unwrap();
        tokio::task::yield_now().await;
        let second = source
            .create(CreateBotInstanceRequest {
                name: "second".into(),
                adapter_type: "fixture".into(),
                enabled: false,
                config: json!({"token":"synthetic-secret"}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(matches!(
            source.start(&second.id).await,
            Err(NuonoError::Conflict(_))
        ));
        let other_account = source
            .create(CreateBotInstanceRequest {
                name: "other-account".into(),
                adapter_type: "fixture".into(),
                enabled: false,
                config: json!({"token":"another-secret"}),
                ..Default::default()
            })
            .await
            .unwrap();
        source.start(&other_account.id).await.unwrap();
        assert!(
            source
                .update(
                    &view.id,
                    UpdateBotInstanceRequest {
                        name: Some("second".into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(source.get(&view.id).await.unwrap().status, "running");
        source.suspend_type("fixture").await;
        assert!(
            bot_instances::get(&fixture.state.db, &view.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            fixture
                .state
                .manager
                .event_kernel
                .action_router()
                .is_empty()
        );
        let missing = SourceInstances::new(
            fixture.state.db.clone(),
            Arc::new(AdapterManager::new()),
            fixture.state.manager.event_kernel.clone(),
        );
        let view = missing.get(&view.id).await.unwrap();
        assert_eq!(view.status, "adapter_missing");
        assert!(view.config.get("token").is_none());
        assert_eq!(view.configured_secrets, vec!["token"]);
        source.shutdown().await;
    }

    #[tokio::test]
    async fn a_stopping_connection_keeps_its_account_until_the_runtime_has_drained() {
        let fixture = crate::api::contract_tests::Fixture::new(false).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut source = SourceInstances::new(
            fixture.state.db.clone(),
            Arc::new(AdapterManager::new()),
            fixture.state.manager.event_kernel.clone(),
        );
        let mut registry = nuono_adapter::AdapterRegistry::default();
        registry
            .register(Arc::new(Provider(Some((entered.clone(), release.clone())))))
            .unwrap();
        source.registry = Arc::new(registry);
        let source = Arc::new(source);
        let first = source
            .create(CreateBotInstanceRequest {
                name: "draining".into(),
                adapter_type: "fixture".into(),
                enabled: false,
                config: json!({"token":"synthetic-secret", "endpoint":"hold"}),
                ..Default::default()
            })
            .await
            .unwrap();
        let second = source
            .create(CreateBotInstanceRequest {
                name: "replacement".into(),
                adapter_type: "fixture".into(),
                enabled: false,
                config: json!({"token":"synthetic-secret"}),
                ..Default::default()
            })
            .await
            .unwrap();
        source.start(&first.id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while fixture
                .state
                .manager
                .event_kernel
                .action_router()
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let worker = source.clone();
        let stopping = tokio::spawn(async move { worker.stop(&first.id).await });
        tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        let conflict = source.start(&second.id).await.unwrap_err();
        assert!(matches!(&conflict, NuonoError::Conflict(_)));
        assert!(!conflict.to_string().contains("synthetic-secret"));
        release.notify_one();
        stopping.await.unwrap().unwrap();
        source.start(&second.id).await.unwrap();
        source.shutdown().await;
    }

    #[test]
    fn format_instance_error_preserves_platform_reason_and_redacts_secrets() {
        let secrets = collect_secret_values(&json!({
            "app_secret": "super-secret-value-1234",
            "nested": {"token": "nested-token-5678"}
        }));
        let message = format_instance_error(
            "适配器运行失败",
            &NuonoError::Platform("QQ Gateway 查询失败：当前出口 IP 不在白名单".into()),
            &secrets,
        );
        assert_eq!(message, "QQ Gateway 查询失败：当前出口 IP 不在白名单");

        let leaked = format_instance_error(
            "适配器运行失败",
            &NuonoError::Platform(
                "连接失败 super-secret-value-1234\0\nwith nested-token-5678".into(),
            ),
            &secrets,
        );
        assert!(!leaked.contains("super-secret-value-1234"));
        assert!(!leaked.contains("nested-token-5678"));
        assert!(!leaked.contains('\0'));
        assert!(leaked.contains("[REDACTED]"));

        let internal = format_instance_error(
            "适配器运行失败",
            &NuonoError::Internal("sql details".into()),
            &secrets,
        );
        assert_eq!(internal, "适配器运行失败（Internal）");
    }
}
