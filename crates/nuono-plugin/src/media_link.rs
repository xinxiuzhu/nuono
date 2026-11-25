//! 由宿主签发的短期本地媒体文件 HTTP 访问链接。
//!
//! 将媒体文件编码为 Base64 内联发送是最通用的 OneBot 传输方式，但在宿主和协议端网关内存中都会产生约原文件 4/3 大小的额外开销。
//! 对于支持直接拉取 HTTP URL 的平台适配器，宿主可以为指定的本机文件签发一个带短期令牌的临时下载 URL，从而只传递轻量链接。
//!
//! 此能力不会扩大插件的文件读取权限（插件本身运行在同一宿主进程内），仅用于避免将大文件字节流直接拷贝进聊天消息报文。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::core::RuntimeBotRoute;
use crate::error::{NuonoError, Result};

/// 未显式指定更短有效期时，临时媒体链接的默认有效时长（10 分钟）。
pub const DEFAULT_MEDIA_LINK_TTL: Duration = Duration::from_secs(600);

/// 已签发的短期本机文件访问链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLink {
    /// 结合目标发送路由解析出的完整 `http(s)` URL，可直接填入消息段发送。
    pub url: String,
    /// 宿主 HTTP 服务上的相对根路径（如 `/media/…`），不含外部基础 URL。
    pub path: String,
    /// 被签名的本机文件绝对路径。
    pub file: PathBuf,
    /// 该临时链接令牌的过期时间点。
    pub expires_at: SystemTime,
}

/// [`PluginContext::media_links`](crate::PluginContext::media_links) 背后的宿主底层接口。
#[async_trait::async_trait]
pub trait MediaLinkService: Send + Sync {
    /// 为本机文件 `path` 签发有效期不超过 `ttl` 的临时访问链接，并按目标 `route` 可达的基础 URL 拼接完整地址；
    /// 若宿主未启用外部媒体链接服务或该路由无可达地址则返回错误。
    async fn link(
        &self,
        path: &Path,
        ttl: Duration,
        route: Option<&RuntimeBotRoute>,
    ) -> Result<MediaLink>;
}

/// 供插件使用的临时媒体链接包装器（宿主未启用该服务时平稳返回 `Err`，绝不 panic）。
#[derive(Clone, Default)]
pub struct PluginMediaLinks {
    inner: Option<Arc<dyn MediaLinkService>>,
}

impl PluginMediaLinks {
    pub fn new(inner: Option<Arc<dyn MediaLinkService>>) -> Self {
        Self { inner }
    }

    /// 当前宿主是否已配置并支持签发临时媒体链接。
    pub fn is_supported(&self) -> bool {
        self.inner.is_some()
    }

    /// 为指定的消息发送路由签发临时媒体链接。
    ///
    /// 若宿主未配置该路由可达的外部基础 URL 则返回错误；此时调用方应回退为内联 Base64 发送，以免向平台发出无法访问的死链。
    pub async fn link_for(&self, route: &RuntimeBotRoute, path: &Path) -> Result<MediaLink> {
        self.link_for_with_ttl(route, path, DEFAULT_MEDIA_LINK_TTL)
            .await
    }

    pub async fn link_for_with_ttl(
        &self,
        route: &RuntimeBotRoute,
        path: &Path,
        ttl: Duration,
    ) -> Result<MediaLink> {
        self.link_with_route(Some(route), path, ttl).await
    }

    /// 签发不绑定特定 Bot 路由的临时媒体链接（仅适用于与宿主部署在同一网络环境下的消费者）。
    pub async fn link(&self, path: &Path) -> Result<MediaLink> {
        self.link_with_route(None, path, DEFAULT_MEDIA_LINK_TTL)
            .await
    }

    pub async fn link_with_ttl(&self, path: &Path, ttl: Duration) -> Result<MediaLink> {
        self.link_with_route(None, path, ttl).await
    }

    async fn link_with_route(
        &self,
        route: Option<&RuntimeBotRoute>,
        path: &Path,
        ttl: Duration,
    ) -> Result<MediaLink> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| NuonoError::NotImplemented("当前宿主不支持短时效媒体链接".into()))?;
        if ttl.is_zero() {
            return Err(NuonoError::Validation("媒体链接有效期不能为零".into()));
        }
        inner.link(path, ttl, route).await
    }
}

impl std::fmt::Debug for PluginMediaLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginMediaLinks")
            .field("supported", &self.is_supported())
            .finish()
    }
}
