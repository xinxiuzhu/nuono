//! 宿主托管的进程内通用跨插件服务调用总线。
//!
//! 插件之间无需共享第三方 Cargo 协议包，也无需依赖 `TypeId` 版本锁定：
//! - `service` 角色插件在 `on_load` 时通过 `PluginContext::register_service` 注册处理器；
//! - 下游插件在每次业务执行时通过 `PluginContext::call_service` / `call_service_typed`
//!   或经由 `PluginContext::services()` 实时路由，不在全局结构体中缓存上游插件实例；
//! - 上游插件停用、重载、激活失败或宿主关闭时，宿主立即摘除其服务处理器，
//!   后续调用即刻返回 `PluginServiceError::ServiceUnavailable`；
//! - 结构化错误 `PluginServiceError` 显式区分 `Unauthorized`（凭据退登或上游确认失效）、
//!   `RateLimited`（限流退避）、`Upstream`（网络或业务瞬态异常）与 `ServiceUnavailable`，
//!   避免下游在限流或网络抖动时误判凭据失效。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// 跨插件服务调用的统一结构化错误。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginServiceError {
    /// 目标服务插件未安装、未启用或尚未注册处理器。
    ServiceUnavailable { plugin_id: String },
    /// 服务插件不支持请求的动作名称。
    ActionNotFound { plugin_id: String, action: String },
    /// 调用方传入的参数校验或反序列化失败。
    InvalidArgument { message: String },
    /// 目标资源不存在。
    NotFound { message: String },
    /// 业务凭据已退出登录、被撤销或经上游确认失效（例如返回 220），下游可据此触发自动暂停等联动。
    Unauthorized {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i64>,
    },
    /// 上游或共享限流池触发限流退避，应稍后重试，不得误判为凭据失效。
    RateLimited {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// 上游接口返回普通业务异常或网络传输失败（非凭据失效）。
    Upstream {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i64>,
    },
    /// 服务插件内部存储或运行错误。
    Internal { message: String },
}

impl PluginServiceError {
    /// 构造服务不可用错误。
    #[must_use]
    pub fn service_unavailable(plugin_id: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            plugin_id: plugin_id.into(),
        }
    }

    /// 构造未知动作错误。
    #[must_use]
    pub fn action_not_found(plugin_id: impl Into<String>, action: impl Into<String>) -> Self {
        Self::ActionNotFound {
            plugin_id: plugin_id.into(),
            action: action.into(),
        }
    }

    /// 构造参数校验错误。
    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }

    /// 构造资源不存在错误。
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }

    /// 构造凭据退登或确认失效错误。
    #[must_use]
    pub fn unauthorized(message: impl Into<String>, code: Option<i64>) -> Self {
        Self::Unauthorized {
            message: message.into(),
            code,
        }
    }

    /// 构造限流退避错误。
    #[must_use]
    pub fn rate_limited(message: impl Into<String>, retry_after_ms: Option<u64>) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after_ms,
        }
    }

    /// 构造普通上游或网络错误。
    #[must_use]
    pub fn upstream(message: impl Into<String>, code: Option<i64>) -> Self {
        Self::Upstream {
            message: message.into(),
            code,
        }
    }

    /// 构造插件内部错误。
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// 是否属于目标服务插件未启用或不可用。
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::ServiceUnavailable { .. })
    }

    /// 是否属于凭据退登或经上游确认失效。
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Unauthorized { .. })
    }

    /// 是否属于限流或退避。
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }
}

impl fmt::Display for PluginServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceUnavailable { plugin_id } => write!(f, "服务插件 {plugin_id} 不可用"),
            Self::ActionNotFound { plugin_id, action } => {
                write!(f, "服务插件 {plugin_id} 不支持动作 {action}")
            }
            Self::InvalidArgument { message }
            | Self::NotFound { message }
            | Self::Unauthorized { message, .. }
            | Self::RateLimited { message, .. }
            | Self::Upstream { message, .. }
            | Self::Internal { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for PluginServiceError {}

/// 跨插件服务调用的统一返回类型。
pub type PluginServiceResult<T = Value> = std::result::Result<T, PluginServiceError>;

/// 由底层支撑插件实现并注册到宿主的跨插件服务动作处理器。
#[async_trait::async_trait]
pub trait PluginServiceHandler: Send + Sync + 'static {
    /// 处理来自其他插件的命名动作请求。
    async fn handle(&self, action: &str, payload: Value) -> PluginServiceResult<Value>;
}

type HandlerMap = HashMap<String, Arc<dyn PluginServiceHandler>>;

/// 线程安全的插件服务注册表（宿主持有并在 `PluginContext` 间共享）。
#[derive(Clone, Default)]
pub struct PluginServiceRegistry {
    entries: Arc<RwLock<HandlerMap>>,
}

impl fmt::Debug for PluginServiceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.entries.read().map(|guard| guard.len()).unwrap_or(0);
        f.debug_struct("PluginServiceRegistry")
            .field("entry_count", &count)
            .finish_non_exhaustive()
    }
}

impl PluginServiceRegistry {
    /// 创建空的服务注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 以 `plugin_id` 为归属者注册服务处理器；重复注册会替换旧处理器。
    pub fn register(&self, plugin_id: &str, handler: Arc<dyn PluginServiceHandler>) {
        self.entries
            .write()
            .expect("插件服务注册表写锁已中毒")
            .insert(plugin_id.to_owned(), handler);
    }

    /// 检查指定插件当前是否已注册可用服务。
    #[must_use]
    pub fn has_service(&self, plugin_id: &str) -> bool {
        self.entries
            .read()
            .expect("插件服务注册表读锁已中毒")
            .contains_key(plugin_id)
    }

    /// 实时查找并调用指定插件的命名服务动作；若插件未启用或已卸载则返回 `ServiceUnavailable`。
    pub async fn call(
        &self,
        plugin_id: &str,
        action: &str,
        payload: Value,
    ) -> PluginServiceResult<Value> {
        let handler = {
            let guard = self.entries.read().expect("插件服务注册表读锁已中毒");
            guard.get(plugin_id).cloned()
        };
        let Some(handler) = handler else {
            return Err(PluginServiceError::service_unavailable(plugin_id));
        };
        handler.handle(action, payload).await
    }

    /// 强类型封装的跨插件服务调用辅助方法。
    pub async fn call_typed<Req, Resp>(
        &self,
        plugin_id: &str,
        action: &str,
        request: &Req,
    ) -> PluginServiceResult<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let payload = serde_json::to_value(request).map_err(|err| {
            PluginServiceError::invalid_argument(format!("请求序列化失败: {err}"))
        })?;
        let raw = self.call(plugin_id, action, payload).await?;
        serde_json::from_value(raw)
            .map_err(|err| PluginServiceError::internal(format!("服务响应解析失败: {err}")))
    }

    /// 摘除指定插件名下的服务处理器，返回移除条目数（0 或 1）。
    pub fn unregister_plugin(&self, plugin_id: &str) -> usize {
        usize::from(
            self.entries
                .write()
                .expect("插件服务注册表写锁已中毒")
                .remove(plugin_id)
                .is_some(),
        )
    }

    /// 清空全部已注册服务处理器（用于宿主关闭）。
    pub fn clear(&self) {
        self.entries
            .write()
            .expect("插件服务注册表写锁已中毒")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PluginServiceError, PluginServiceHandler, PluginServiceRegistry, PluginServiceResult,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::sync::Arc;

    struct DemoHandler;

    #[async_trait::async_trait]
    impl PluginServiceHandler for DemoHandler {
        async fn handle(&self, action: &str, payload: Value) -> PluginServiceResult<Value> {
            match action {
                "ping" => Ok(json!({ "echo": payload["msg"] })),
                "expired" => Err(PluginServiceError::unauthorized("凭据已失效", Some(220))),
                "throttled" => Err(PluginServiceError::rate_limited("请求频繁", Some(3000))),
                "network" => Err(PluginServiceError::upstream("上游连接超时", None)),
                other => Err(PluginServiceError::action_not_found("demo", other)),
            }
        }
    }

    #[derive(Serialize)]
    struct PingReq {
        msg: String,
    }

    #[derive(Deserialize)]
    struct PingResp {
        echo: String,
    }

    #[tokio::test]
    async fn register_call_distinguish_errors_and_unregister() {
        let registry = PluginServiceRegistry::new();
        assert!(!registry.has_service("demo"));
        let err = registry
            .call("demo", "ping", json!({}))
            .await
            .expect_err("未注册服务时应返回不可用");
        assert!(err.is_unavailable());

        registry.register("demo", Arc::new(DemoHandler));
        assert!(registry.has_service("demo"));

        let resp: PingResp = registry
            .call_typed(
                "demo",
                "ping",
                &PingReq {
                    msg: "ok".to_owned(),
                },
            )
            .await
            .expect("已注册服务应成功返回响应");
        assert_eq!(resp.echo, "ok");

        let unauth = registry
            .call("demo", "expired", json!({}))
            .await
            .unwrap_err();
        assert!(unauth.is_unauthorized());
        assert!(!unauth.is_rate_limited());
        assert_eq!(
            unauth,
            PluginServiceError::Unauthorized {
                message: "凭据已失效".to_owned(),
                code: Some(220),
            }
        );

        let limited = registry
            .call("demo", "throttled", json!({}))
            .await
            .unwrap_err();
        assert!(limited.is_rate_limited());
        assert!(!limited.is_unauthorized());

        let upstream = registry
            .call("demo", "network", json!({}))
            .await
            .unwrap_err();
        assert!(!upstream.is_unauthorized());
        assert!(!upstream.is_rate_limited());
        assert!(matches!(upstream, PluginServiceError::Upstream { .. }));

        assert_eq!(registry.unregister_plugin("demo"), 1);
        assert!(!registry.has_service("demo"));
        assert!(
            registry
                .call("demo", "ping", json!({}))
                .await
                .unwrap_err()
                .is_unavailable()
        );
    }
}
