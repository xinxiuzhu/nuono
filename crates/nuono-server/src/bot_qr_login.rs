//! 按管理员隔离、带数量与有效期限制的机器人扫码登录临时会话（扫码成功后仍需显式提交保存才会创建正式实例）。
use crate::bot_instances::{BotInstanceRuntimeManager, BotInstanceView, CreateBotInstanceRequest};
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_adapter::{
    AdapterDescriptor, AdapterProvider, BotProfile, QrLoginSession, QrLoginStatus,
};
use nuono_core::error::{NuonoError, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex as AsyncMutex, time::Instant};

/// 单个扫码绑定会话的最大存活时长（仅需覆盖扫码与提交保存操作）。
const SESSION_LIFETIME: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_SESSIONS: usize = 64;
const MAX_USER_SESSIONS: usize = 4;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub(crate) struct QrLoginView {
    pub id: String,
    pub status: String,
    pub qr_image: String,
    pub expires_in: u64,
    pub interval: u64,
    pub config: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

enum Phase {
    Starting,
    Pending(Box<dyn QrLoginSession>),
    Ready(Value),
    Expired,
    SaveFailed,
    Saved(Box<BotInstanceView>),
}

struct Flow {
    phase: Phase,
    image: String,
    public_config: Value,
    profile: BotProfile,
    next_poll: Instant,
}

struct Entry {
    owner: i64,
    adapter: String,
    descriptor: AdapterDescriptor,
    fields: Vec<String>,
    deadline: Instant,
    cancelled: AtomicBool,
    flow: AsyncMutex<Flow>,
    expiry: std::sync::OnceLock<tokio::task::AbortHandle>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(expiry) = self.expiry.get() {
            expiry.abort();
        }
    }
}

#[derive(Default)]
pub(crate) struct QrLogins {
    entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    closed: AtomicBool,
}

fn gone() -> NuonoError {
    NuonoError::NotFound("扫码会话已失效，请重新扫码".into())
}

impl QrLogins {
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for entry in self
            .entries
            .lock()
            .expect("QR sessions mutex poisoned")
            .drain()
            .map(|(_, entry)| entry)
        {
            entry.cancelled.store(true, Ordering::Release);
        }
    }

    fn entry(&self, owner: i64, id: &str) -> Result<Arc<Entry>> {
        self.entries
            .lock()
            .expect("QR sessions mutex poisoned")
            .get(id)
            .filter(|entry| {
                entry.owner == owner
                    && !entry.cancelled.load(Ordering::Acquire)
                    && Instant::now() < entry.deadline
            })
            .cloned()
            .ok_or_else(gone)
    }

    pub(crate) async fn start(
        &self,
        owner: i64,
        provider: Arc<dyn AdapterProvider>,
    ) -> Result<QrLoginView> {
        let descriptor = provider.descriptor();
        let fields = provider.qr_login_fields();
        if !provider.supports_instances()
            || fields.is_empty()
            || fields
                .iter()
                .any(|key| !descriptor.fields.iter().any(|field| &field.key == key))
        {
            return Err(NuonoError::Validation("此适配器不支持扫码绑定".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(Entry {
            owner,
            adapter: descriptor.id.clone(),
            descriptor,
            fields,
            deadline: Instant::now() + SESSION_LIFETIME,
            cancelled: AtomicBool::new(false),
            expiry: std::sync::OnceLock::new(),
            flow: AsyncMutex::new(Flow {
                phase: Phase::Starting,
                image: String::new(),
                public_config: json!({}),
                profile: BotProfile::default(),
                next_poll: Instant::now(),
            }),
        });
        {
            let mut entries = self.entries.lock().expect("QR sessions mutex poisoned");
            if self.closed.load(Ordering::Acquire) {
                return Err(gone());
            }
            if entries.len() >= MAX_SESSIONS
                || entries
                    .values()
                    .filter(|entry| entry.owner == owner)
                    .count()
                    >= MAX_USER_SESSIONS
            {
                return Err(NuonoError::Conflict(
                    "扫码会话过多，请关闭已有二维码后重试".into(),
                ));
            }
            entries.insert(id.clone(), entry.clone());
        }
        let entries = Arc::downgrade(&self.entries);
        let expires_id = id.clone();
        let expiry = tokio::spawn(async move {
            tokio::time::sleep(SESSION_LIFETIME).await;
            if let Some(entries) = entries.upgrade()
                && let Some(entry) = entries
                    .lock()
                    .expect("QR sessions mutex poisoned")
                    .remove(&expires_id)
            {
                entry.cancelled.store(true, Ordering::Release);
            }
        });
        let _ = entry.expiry.set(expiry.abort_handle());
        let result = async {
            let session = tokio::time::timeout(Duration::from_secs(15), provider.start_qr_login())
                .await
                .map_err(|_| NuonoError::Validation("申请二维码超时，请重试".into()))??;
            let url = url::Url::parse(session.qr_url())
                .map_err(|_| NuonoError::Validation("扫码授权地址无效".into()))?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.as_str().len() > 2048
            {
                return Err(NuonoError::Validation("扫码授权地址无效".into()));
            }
            let svg = qrcode::QrCode::new(url.as_str())
                .map_err(|_| NuonoError::Validation("无法生成二维码".into()))?
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .build();
            let mut flow = entry.flow.lock().await;
            if entry.cancelled.load(Ordering::Acquire) {
                return Err(gone());
            }
            flow.image = format!("data:image/svg+xml;base64,{}", STANDARD.encode(svg));
            flow.phase = Phase::Pending(session);
            Ok(view(&id, &entry, &flow))
        }
        .await;
        if result.is_err() {
            self.entries
                .lock()
                .expect("QR sessions mutex poisoned")
                .remove(&id);
        }
        result
    }

    pub(crate) async fn poll(&self, owner: i64, id: &str) -> Result<QrLoginView> {
        let entry = self.entry(owner, id)?;
        let mut flow = entry.flow.lock().await;
        if entry.cancelled.load(Ordering::Acquire) || Instant::now() >= entry.deadline {
            return Err(gone());
        }
        if Instant::now() >= flow.next_poll
            && let Phase::Pending(session) = &mut flow.phase
        {
            let result = tokio::time::timeout(Duration::from_secs(15), session.poll()).await;
            let polled_profile = session.profile();
            flow.next_poll = Instant::now() + POLL_INTERVAL;
            if entry.cancelled.load(Ordering::Acquire) || Instant::now() >= entry.deadline {
                return Err(gone());
            }
            match result
                .map_err(|_| NuonoError::Validation("查询扫码结果超时，请重试".into()))??
            {
                QrLoginStatus::Pending => {}
                QrLoginStatus::Expired => {
                    flow.phase = Phase::Expired;
                    flow.image.clear();
                    flow.profile = BotProfile::default();
                }
                QrLoginStatus::Complete(config) => {
                    let values = config
                        .as_object()
                        .filter(|values| {
                            entry.fields.iter().all(|key| values.contains_key(key))
                                && values.keys().all(|key| entry.fields.contains(key))
                        })
                        .ok_or_else(|| NuonoError::Validation("扫码结果缺少必要配置".into()))?;
                    let profile = crate::bot_instances::sanitize_profile(polled_profile);
                    flow.public_config = Value::Object(
                        values
                            .iter()
                            .filter(|(key, _)| !entry.descriptor.secret_fields.contains(key))
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect(),
                    );
                    flow.profile = profile;
                    flow.image.clear();
                    flow.phase = Phase::Ready(config);
                }
                _ => {}
            }
        }
        Ok(view(id, &entry, &flow))
    }

    pub(crate) async fn cancel(&self, owner: i64, id: &str) -> Result<()> {
        let entry = self.entry(owner, id)?;
        entry.cancelled.store(true, Ordering::Release);
        self.entries
            .lock()
            .expect("QR sessions mutex poisoned")
            .remove(id);
        Ok(())
    }

    pub(crate) async fn complete(
        &self,
        owner: i64,
        id: &str,
        mut request: CreateBotInstanceRequest,
        manager: Arc<BotInstanceRuntimeManager>,
    ) -> Result<BotInstanceView> {
        let entry = self.entry(owner, id)?;
        // 一旦进入保存流程，放入独立任务执行，避免浏览器中途断开连接导致已落库的实例状态不一致。
        tokio::spawn(async move {
            let mut flow = entry.flow.lock().await;
            if request.adapter_type != entry.adapter {
                return Err(NuonoError::Validation("扫码平台与实例类型不一致".into()));
            }
            if let Phase::Saved(instance) = &flow.phase {
                return Ok(instance.as_ref().clone());
            }
            if entry.cancelled.load(Ordering::Acquire) || Instant::now() >= entry.deadline {
                return Err(gone());
            }
            if let Phase::SaveFailed = flow.phase {
                return Err(NuonoError::Conflict(
                    "保存结果未确认，请刷新机器人列表核对后再创建".into(),
                ));
            }
            let Phase::Ready(credentials) = &flow.phase else {
                return Err(NuonoError::Conflict("请先完成扫码绑定".into()));
            };
            let config = request
                .config
                .as_object_mut()
                .ok_or_else(|| NuonoError::Validation("实例配置必须是 JSON 对象".into()))?;
            // 扫码会话验证通过后拿到的凭据优先级高于前端表单传入的字段。
            config.extend(
                credentials
                    .as_object()
                    .expect("validated QR result object")
                    .clone(),
            );
            // 若用户未手动填写实例名称，自动回填扫码阶段拉取到的平台官方昵称。
            if request.name.trim().is_empty()
                && let Some(bot_name) = flow.profile.name.clone()
            {
                request.name = bot_name;
            }
            // 创建过程可能已在数据库落库但后续步骤报错；先置为 SaveFailed 防止重复提交创建出重复实例。
            flow.phase = Phase::SaveFailed;
            flow.public_config = json!({});
            flow.profile = BotProfile::default();
            let instance = manager.create(request).await?;
            flow.phase = Phase::Saved(Box::new(instance.clone()));
            flow.public_config = json!({});
            Ok(instance)
        })
        .await
        .map_err(|_| NuonoError::Internal("保存扫码实例失败".into()))?
    }
}

fn view(id: &str, entry: &Entry, flow: &Flow) -> QrLoginView {
    QrLoginView {
        id: id.into(),
        status: match flow.phase {
            Phase::Starting | Phase::Pending(_) => "pending",
            Phase::Ready(_) => "ready",
            Phase::Expired => "expired",
            Phase::SaveFailed => "save_failed",
            Phase::Saved(_) => "saved",
        }
        .into(),
        qr_image: flow.image.clone(),
        expires_in: entry
            .deadline
            .saturating_duration_since(Instant::now())
            .as_secs(),
        interval: POLL_INTERVAL.as_secs(),
        config: flow.public_config.clone(),
        bot_name: flow.profile.name.clone(),
        avatar_url: flow.profile.avatar_url.clone(),
    }
}

#[cfg(test)]
#[path = "../tests/unit/bot_qr_login.rs"]
pub(crate) mod tests;
