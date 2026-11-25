//! 统一管理所有已注册平台适配器的机器人实例持久化与运行时生命周期。
use nuono_core::{
    adapter::AdapterManager,
    config::QqOfficialConfig,
    core::EventKernel,
    db::Database,
    error::{NuonoError, Result},
    service::bot_instances,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct CreateBotInstanceRequest {
    pub name: String,
    pub adapter_type: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub native_bot_id: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct UpdateBotInstanceRequest {
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub native_bot_id: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<Value>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub expected_revision: Option<i64>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BotInstanceIngressView {
    pub state: String,
    pub callback_path: Option<String>,
    pub last_event_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BotInstanceView {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub namespace: String,
    pub adapter_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_bot_id: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub configured_secrets: Vec<String>,
    pub status: String,
    pub runtime_status: String,
    pub connected: bool,
    pub connection_mode: String,
    pub environment: String,
    pub ingress: BotInstanceIngressView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    pub last_error: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "nuono_domain::scalar::optional_i64_string"
    )]
    #[schema(value_type = Option<String>)]
    pub last_connected_at_ms: Option<i64>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub revision: i64,
    pub source: String,
    pub deletable: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// 过滤适配器上报的机器人展示资料，避免超长昵称或非 HTTPS 头像地址流出到管理端。
pub(crate) fn sanitize_profile(profile: nuono_adapter::BotProfile) -> nuono_adapter::BotProfile {
    let name = profile
        .name
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && value.chars().count() <= 100);
    let avatar_url = profile
        .avatar_url
        .map(|value| value.trim().to_owned())
        .filter(|value| valid_https_avatar_url(value));
    nuono_adapter::BotProfile { name, avatar_url }
}

fn valid_https_avatar_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.as_str().len() <= 2048
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct BotInstanceTypeInfo {
    pub adapter_type: String,
    pub display_name: String,
    pub description: String,
    pub supports_multiple_instances: bool,
    pub defaults: Value,
    pub fields: Vec<nuono_core::core::config_schema::PluginConfigField>,
    pub qr_login_fields: Vec<String>,
}

pub struct BotInstanceRuntimeManager {
    pub(crate) source: crate::source_adapters::SourceInstances,
    pub(crate) qr_logins: crate::bot_qr_login::QrLogins,
    db: Database,
}
impl BotInstanceRuntimeManager {
    pub fn new(db: Database, adapters: Arc<AdapterManager>, kernel: Arc<EventKernel>) -> Self {
        Self {
            source: crate::source_adapters::SourceInstances::new(db.clone(), adapters, kernel),
            qr_logins: crate::bot_qr_login::QrLogins::default(),
            db,
        }
    }
    pub fn instance_types() -> Vec<BotInstanceTypeInfo> {
        let registry = nuono_adapter::running();
        registry
            .descriptors()
            .into_iter()
            .filter(|d| registry.get(&d.id).is_some_and(|p| p.supports_instances()))
            .map(|d| BotInstanceTypeInfo {
                adapter_type: d.id.clone(),
                display_name: d.display_name.clone(),
                description: d.description.clone(),
                supports_multiple_instances: d.supports_multiple_instances,
                defaults: d.defaults(),
                fields: d.fields,
                qr_login_fields: registry
                    .get(&d.id)
                    .map(|p| p.qr_login_fields())
                    .unwrap_or_default(),
            })
            .collect()
    }
    pub async fn start_legacy_qq(&self, config: &QqOfficialConfig) -> Result<()> {
        self.source.start_legacy("config:qq_official","qq_official","QQ 官方机器人（运行配置）",json!({
            "app_id":config.app_id.as_deref().unwrap_or_default(),"connection_mode":config.connection_mode,"environment":config.environment,"api_base_url":config.api_base_url,"token_url":config.token_url,"intents":config.intents.min(i64::MAX as u64) as i64
        }),json!({"app_secret":config.app_secret.as_deref().unwrap_or_default()})).await
    }
    pub async fn start_enabled(&self) {
        match bot_instances::list(&self.db).await {
            Ok(records) => {
                for record in records.into_iter().filter(|r| r.enabled) {
                    if let Err(error) = self.source.start(&record.id).await {
                        tracing::warn!(
                            instance_id = record.id,
                            kind = error.kind(),
                            "实例自动启动失败"
                        );
                    }
                }
            }
            Err(error) => tracing::warn!(kind = error.kind(), "读取 Bot 实例失败"),
        }
    }
    pub async fn list(&self) -> Result<Vec<BotInstanceView>> {
        let mut views = self.source.legacy_views().await;
        for record in bot_instances::list(&self.db).await? {
            views.push(self.source.view(record).await);
        }
        Ok(views)
    }
    pub async fn get(&self, id: &str) -> Result<Option<BotInstanceView>> {
        match self.source.get(id).await {
            Ok(view) => Ok(Some(view)),
            Err(NuonoError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub async fn create(&self, request: CreateBotInstanceRequest) -> Result<BotInstanceView> {
        self.source.create(request).await
    }
    pub async fn update(
        &self,
        id: &str,
        request: UpdateBotInstanceRequest,
    ) -> Result<BotInstanceView> {
        self.source.update(id, request).await
    }
    pub async fn start(&self, id: &str) -> Result<BotInstanceView> {
        self.source.start(id).await
    }
    pub async fn stop(&self, id: &str) -> Result<BotInstanceView> {
        self.source.stop(id).await
    }
    pub async fn restart(&self, id: &str) -> Result<BotInstanceView> {
        self.source.restart(id).await
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.source.delete(id).await
    }
    pub async fn shutdown_all(&self) {
        self.qr_logins.close();
        self.source.shutdown().await;
    }
}
fn default_enabled() -> bool {
    true
}
