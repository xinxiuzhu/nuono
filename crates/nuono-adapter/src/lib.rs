//! 静态链接的源码适配器接口定义，不包含宿主运行时、数据库或 HTTP 具体实现。
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::exhaustive_enums,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::allow_attributes_without_reason
)]

use async_trait::async_trait;
pub use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub use chrono::{DateTime, Utc};
pub use nuono_app::platform::canonical;
pub use nuono_app::{
    adapter::{BackoffPolicy, supervise},
    config_schema::{ConfigFieldOption, ConfigFieldType, PluginConfigField as ConfigField},
    config_validation,
    media::ImageResolver,
    platform::{AdapterCapabilities, PlatformAdapter, UnifiedEvent},
    transport::{RuntimeActionSink, RuntimeBotRoute, TransportFrame},
};
pub use nuono_domain::{BotIdentity, DomainEvent, DomainModelError, Platform};

pub type AdapterFactory = fn() -> Arc<dyn AdapterProvider>;

mod qr_login;
pub use qr_login::{QrLoginSession, QrLoginStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterDescriptor {
    pub id: String,
    pub platform: String,
    pub display_name: String,
    pub description: String,
    pub supports_multiple_instances: bool,
    pub fields: Vec<ConfigField>,
    pub secret_fields: Vec<String>,
}

impl AdapterDescriptor {
    pub fn defaults(&self) -> Value {
        config_validation::config_defaults(&self.fields)
    }

    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) || !valid_id(&self.platform) || self.display_name.trim().is_empty() {
            return Err(NuonoError::Validation(
                "适配器需要合法且稳定的 ID、平台 ID 和名称".into(),
            ));
        }
        // 实例配置表单只是模板：凭据和账号 ID 会在创建实例时填写。
        // 此处仅校验字段类型和 schema 定义，不要求提供可用的默认密钥。
        let mut templates = self.fields.clone();
        for field in &mut templates {
            field.required = false;
            field.ui.required = false;
        }
        config_validation::validate_config_fields(&templates)
            .map_err(|message| NuonoError::Validation(message.replace("插件配置", "适配器配置")))?;
        if self.fields.iter().any(|field| {
            (field.field_type == ConfigFieldType::Password)
                != self.secret_fields.contains(&field.key)
        }) {
            return Err(NuonoError::Validation(
                "密钥字段必须使用 password 类型，并在 secret_fields 中声明".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for key in &self.secret_fields {
            if !seen.insert(key) || !self.fields.iter().any(|field| field.key == *key) {
                return Err(NuonoError::Validation(
                    "适配器密钥必须对应唯一的配置字段".into(),
                ));
            }
            if self
                .fields
                .iter()
                .find(|field| field.key == *key)
                .is_some_and(|field| !field.default.is_null() && field.default.as_str() != Some(""))
            {
                return Err(NuonoError::Validation("适配器不能声明非空默认密钥".into()));
            }
        }
        Ok(())
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && !matches!(id, "hub" | "api" | "plugins" | "adapters")
}

/// 敏感凭据在 `Debug` 输出中自动脱敏，且不会出现在公开的描述信息中。
#[derive(Clone)]
pub struct AdapterInstanceConfig {
    pub id: String,
    pub config: Value,
    pub secrets: Value,
}

impl std::fmt::Debug for AdapterInstanceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterInstanceConfig")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

impl AdapterInstanceConfig {
    pub fn required(&self, key: &str) -> Result<&str> {
        self.secrets
            .get(key)
            .or_else(|| self.config.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| NuonoError::Validation(format!("适配器缺少配置字段: {key}")))
    }
}

/// 释放路由租约时，仅注销当前这一代连接注册的路由。
pub trait RouteLease: Send + Sync {}

#[async_trait]
pub trait AdapterHost: Send + Sync {
    fn images(&self) -> Arc<dyn ImageResolver>;
    fn register_route(
        &self,
        route: RuntimeBotRoute,
        sink: Arc<dyn RuntimeActionSink>,
    ) -> Result<Box<dyn RouteLease>>;
    async fn submit(
        &self,
        event: UnifiedEvent,
        sink: Arc<dyn RuntimeActionSink>,
        route: RuntimeBotRoute,
    ) -> Result<()>;
    async fn shutdown_ingress(&self);
}

/// 保留原始回调请求体字节，供平台完成签名校验。
pub struct AdapterRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub query: String,
    pub body: Vec<u8>,
}

impl AdapterRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl std::fmt::Debug for AdapterRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers_count", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub struct AdapterResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for AdapterResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterResponse")
            .field("status", &self.status)
            .field("headers_count", &self.headers.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl AdapterResponse {
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: vec![],
        }
    }
    pub fn json(status: u16, value: &Value) -> Result<Self> {
        Ok(Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: serde_json::to_vec(value)
                .map_err(|error| NuonoError::Internal(error.to_string()))?,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub connected: bool,
    pub last_event_at: Option<String>,
    pub last_error: Option<String>,
}

/// 机器人账号的平台侧资料（昵称与头像地址），由适配器按平台能力自动获取。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

#[async_trait]
pub trait AdapterRuntime: Send + Sync {
    fn platform(&self) -> Arc<dyn PlatformAdapter>;
    fn status(&self) -> RuntimeStatus {
        RuntimeStatus::default()
    }
    /// 当前运行实例从平台侧获取到的机器人资料（昵称与头像）。
    fn profile(&self) -> BotProfile {
        BotProfile::default()
    }
    /// 持有事件发送端和连接租约，直到取消信号让接收端消费完毕后再释放。
    async fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> Result<()>;
    async fn handle_request(&self, _request: AdapterRequest) -> Result<AdapterResponse> {
        Err(NuonoError::NotFound("适配器没有此回调入口".into()))
    }
}

#[async_trait]
pub trait AdapterProvider: Send + Sync {
    fn descriptor(&self) -> AdapterDescriptor;
    /// 扫码登录成功后能够填充的配置字段列表；为空则表示不支持扫码绑定。
    fn qr_login_fields(&self) -> Vec<String> {
        Vec::new()
    }
    /// 启动扫码绑定流程；临时会话密钥由返回的会话对象持有。
    async fn start_qr_login(&self) -> Result<Box<dyn QrLoginSession>> {
        Err(NuonoError::Validation("此适配器不支持扫码绑定".into()))
    }
    fn callback_path(&self, _instance: &str, _config: &Value) -> Option<String> {
        None
    }
    /// 在落盘保存前校验公开配置项。切勿在错误信息中暴露敏感凭据。
    fn validate_config(&self, _config: &Value) -> Result<()> {
        Ok(())
    }
    /// 同一平台下用于防重的账号唯一标识，确保同一账号不会启动多个活跃连接。
    fn exclusive_key(&self, _config: &Value) -> Option<String> {
        None
    }
    /// 当账号身份隐含在敏感凭据中时，用于排重比对的私有标识。
    /// 宿主绝不会记录或序列化该值；若已提供公开的 `exclusive_key` 则优先使用后者。
    fn credential_key(&self, _secrets: &Value) -> Option<String> {
        None
    }
    /// 是否支持按保存的实例配置主动建立连接（反向监听器由入站连接触发创建会话，无需预置实例）。
    fn supports_instances(&self) -> bool {
        true
    }
    fn normalize(
        &self,
        bot: &BotIdentity,
        event: &UnifiedEvent,
        received_at: chrono::DateTime<chrono::Utc>,
    ) -> std::result::Result<DomainEvent, DomainModelError>;
    fn legacy_codec(&self) -> Option<Arc<dyn PlatformAdapter>> {
        None
    }
    fn avatar_source(
        &self,
        _request: &nuono_app::media::UserAvatarRequest,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    async fn create(
        &self,
        config: AdapterInstanceConfig,
        host: Arc<dyn AdapterHost>,
    ) -> Result<Arc<dyn AdapterRuntime>>;
}

#[derive(Default)]
pub struct AdapterRegistry {
    providers: BTreeMap<String, Arc<dyn AdapterProvider>>,
}

impl std::fmt::Debug for AdapterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        f.debug_struct("AdapterRegistry")
            .field("adapters", &ids)
            .finish()
    }
}

static RUNNING: std::sync::OnceLock<Arc<AdapterRegistry>> = std::sync::OnceLock::new();

/// 在建立任何连接前，由生成的应用在启动入口处一次性完成初始化注册。
pub fn register_running(factories: Vec<AdapterFactory>) -> Result<()> {
    let registry = Arc::new(AdapterRegistry::new(factories)?);
    RUNNING
        .set(registry)
        .map_err(|_| NuonoError::Conflict("适配器注册表已初始化".into()))
}

pub fn running() -> Arc<AdapterRegistry> {
    RUNNING
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(AdapterRegistry::default()))
}

impl AdapterRegistry {
    pub fn new(factories: impl IntoIterator<Item = AdapterFactory>) -> Result<Self> {
        let mut registry = Self::default();
        for factory in factories {
            registry.register(factory())?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, provider: Arc<dyn AdapterProvider>) -> Result<()> {
        let descriptor = provider.descriptor();
        descriptor.validate()?;
        let has_legacy_codec = provider.legacy_codec().is_some();
        if self.providers.contains_key(&descriptor.id)
            || self.providers.values().any(|existing| {
                existing.descriptor().platform == descriptor.platform
                    || (has_legacy_codec && existing.legacy_codec().is_some())
            })
        {
            return Err(NuonoError::Conflict(format!(
                "适配器或平台重复注册: {}",
                descriptor.id
            )));
        }
        self.providers.insert(descriptor.id, provider);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AdapterProvider>> {
        self.providers.get(id).cloned()
    }
    pub fn for_platform(&self, platform: &str) -> Option<Arc<dyn AdapterProvider>> {
        self.providers
            .values()
            .find(|provider| provider.descriptor().platform == platform)
            .cloned()
    }
    /// 查找声明了反向 WebSocket 编解码器的适配器，不依赖具体适配器 ID 名称。
    pub fn legacy_codec_provider(&self) -> Option<Arc<dyn AdapterProvider>> {
        self.providers
            .values()
            .find(|provider| provider.legacy_codec().is_some())
            .cloned()
    }
    pub fn descriptors(&self) -> Vec<AdapterDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor())
            .collect()
    }
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// 使用常量时间比较鉴权信息，避免因前缀匹配短路而引发时序攻击。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.ct_eq(b))
}

pub fn ingress_path(adapter: &str, instance: &str) -> String {
    format!(
        "/ingress/{}/{}",
        urlencoding::encode(adapter),
        urlencoding::encode(instance)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Provider(AdapterDescriptor);
    #[async_trait]
    impl AdapterProvider for Provider {
        fn descriptor(&self) -> AdapterDescriptor {
            self.0.clone()
        }
        fn normalize(
            &self,
            _: &BotIdentity,
            _: &UnifiedEvent,
            _: chrono::DateTime<chrono::Utc>,
        ) -> std::result::Result<DomainEvent, DomainModelError> {
            Err(DomainModelError::InvalidValue {
                field: "platform",
                expected: "fixture event",
            })
        }
        async fn create(
            &self,
            _: AdapterInstanceConfig,
            _: Arc<dyn AdapterHost>,
        ) -> Result<Arc<dyn AdapterRuntime>> {
            Err(NuonoError::Validation(
                "registration does not start runtimes".into(),
            ))
        }
    }
    fn descriptor() -> AdapterDescriptor {
        AdapterDescriptor {
            id: "fixture".into(),
            platform: "fixture".into(),
            display_name: "Fixture".into(),
            description: String::new(),
            supports_multiple_instances: true,
            fields: vec![ConfigField::password("token", "Token", "")],
            secret_fields: vec!["token".into()],
        }
    }
    #[test]
    fn empty_registry_has_no_implicit_platform() {
        let registry = AdapterRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.get("onebot").is_none());
        assert!(registry.descriptors().is_empty());
    }
    #[test]
    fn duplicate_ids_and_platforms_cannot_replace_a_provider() {
        let mut registry = AdapterRegistry::default();
        registry.register(Arc::new(Provider(descriptor()))).unwrap();
        assert!(registry.register(Arc::new(Provider(descriptor()))).is_err());
        let mut other = descriptor();
        other.id = "other".into();
        assert!(registry.register(Arc::new(Provider(other))).is_err());
        assert_eq!(registry.descriptors().len(), 1);
    }
    #[test]
    fn secrets_must_be_declared_once_and_have_no_default_value() {
        let mut value = descriptor();
        value.secret_fields.push("token".into());
        assert!(value.validate().is_err());
        value.secret_fields = vec!["unknown".into()];
        assert!(value.validate().is_err());
        value.secret_fields = vec!["token".into()];
        value.fields[0].default = Value::String("synthetic-credential".into());
        assert!(value.validate().is_err());
    }
    #[test]
    fn invalid_ids_cannot_enter_the_registry() {
        for id in ["", "../telegram", "hub", "plugins", "UPPER", "-bad"] {
            let mut value = descriptor();
            value.id = id.into();
            assert!(value.validate().is_err(), "{id}");
        }
    }
    #[test]
    fn missing_secret_errors_do_not_echo_values() {
        let config = AdapterInstanceConfig {
            id: "fixture".into(),
            config: serde_json::json!({}),
            secrets: serde_json::json!({"token":""}),
        };
        assert!(config.required("token").is_err());
    }

    #[test]
    fn required_instance_credentials_have_empty_templates() {
        let mut value = descriptor();
        value.fields[0] = ConfigField::password("token", "Token", "").required();
        value.validate().unwrap();
        assert_eq!(value.defaults()["token"], "");
    }

    #[test]
    fn legacy_codec_provider_is_resolved_by_capability_not_adapter_id() {
        #[derive(Debug)]
        struct DummyCodec;
        #[async_trait]
        impl PlatformAdapter for DummyCodec {
            fn platform_name(&self) -> &str {
                "OneBot v11"
            }
            fn platform_version(&self) -> &str {
                "v11"
            }
            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities::default()
            }
            fn extract_self_id(&self, _: &str) -> Option<i64> {
                None
            }
            fn parse_incoming(&self, _: &str) -> Result<UnifiedEvent> {
                Err(NuonoError::Validation("unused".into()))
            }
            fn build_action(&self, _: &nuono_app::platform::Action) -> Value {
                Value::Null
            }
            fn build_send_message(
                &self,
                _: &nuono_app::platform::MessageType,
                _: i64,
                _: &str,
            ) -> Value {
                Value::Null
            }
            fn segments_to_protocol(&self, _: &[nuono_app::platform::UnifiedSegment]) -> Value {
                Value::Null
            }
        }
        struct ReverseWsProvider(AdapterDescriptor);
        #[async_trait]
        impl AdapterProvider for ReverseWsProvider {
            fn descriptor(&self) -> AdapterDescriptor {
                self.0.clone()
            }
            fn legacy_codec(&self) -> Option<Arc<dyn PlatformAdapter>> {
                Some(Arc::new(DummyCodec))
            }
            fn normalize(
                &self,
                _: &BotIdentity,
                _: &UnifiedEvent,
                _: chrono::DateTime<chrono::Utc>,
            ) -> std::result::Result<DomainEvent, DomainModelError> {
                Err(DomainModelError::InvalidValue {
                    field: "platform",
                    expected: "fixture event",
                })
            }
            async fn create(
                &self,
                _: AdapterInstanceConfig,
                _: Arc<dyn AdapterHost>,
            ) -> Result<Arc<dyn AdapterRuntime>> {
                Err(NuonoError::Validation("unused".into()))
            }
        }

        let mut registry = AdapterRegistry::default();
        assert!(registry.legacy_codec_provider().is_none());
        let mut desc = descriptor();
        desc.id = "onebotv11".into();
        desc.platform = "onebot".into();
        registry
            .register(Arc::new(ReverseWsProvider(desc.clone())))
            .unwrap();
        let resolved = registry.legacy_codec_provider().unwrap();
        assert_eq!(resolved.descriptor().id, "onebotv11");
        assert!(registry.for_platform("onebot").is_some());

        let mut conflict = desc;
        conflict.id = "another-reverse-ws".into();
        conflict.platform = "other_reverse_ws".into();
        assert!(
            registry
                .register(Arc::new(ReverseWsProvider(conflict)))
                .is_err()
        );
    }
}
