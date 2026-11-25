//! 宿主签发的短期媒体链接。
//!
//! HTTP 服务暴露一个按 token 寻址的文件端点。Token 用进程内随机密钥做 HMAC 签名：
//! 重启宿主会使所有未过期链接失效，且密钥永不落盘。只有插件明确要求分享的路径
//! 才会被提供，因此该端点不能用来遍历文件系统。
//!
//! 绝对 URL 按 bot 路由，从网关打开 WebSocket 时使用的 `Host` 头解析，
//! 使另一台机器上的网关（或反向代理之后）也能访问该链接，
//! 而无需强制配置外部 URL。

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use nuono_plugin::RuntimeBotRoute;
use nuono_plugin::media_link::{MediaLink, MediaLinkService};

type HmacSha256 = Hmac<Sha256>;

/// 宿主可签名的最长有效期；避免把意外的长寿命能力写进
/// 插件配置。
const MAX_TTL: Duration = Duration::from_secs(3600);
pub(crate) const TOKEN_PREFIX: &str = "/media/";

static GLOBAL: OnceLock<Arc<HostMediaLinks>> = OnceLock::new();
/// 各 bot 路由到达本宿主时使用的 Base URL，从 WebSocket 升级请求中学习得到。
/// generation 保证旧连接无法清掉新连接写入的条目。
static ROUTE_BASES: LazyLock<DashMap<RuntimeBotRoute, (u64, String)>> = LazyLock::new(DashMap::new);

/// 记住某条 bot 路由对外可见的 Base URL。
pub fn remember_route_base(route: &RuntimeBotRoute, generation: u64, base: impl Into<String>) {
    let base = base.into();
    let base = base.trim_end_matches('/');
    if !base.is_empty() {
        ROUTE_BASES.insert(route.clone(), (generation, base.to_string()));
    }
}

/// 连接关闭时丢弃该路由学到的 Base，避免过期地址
/// 在上报它的网关消失后仍残留，并防止映射无限增长。
pub(crate) fn forget_route_base(route: &RuntimeBotRoute, generation: u64) {
    ROUTE_BASES.remove_if(route, |_, (stored, _)| *stored == generation);
}

fn route_base(route: &RuntimeBotRoute) -> Option<String> {
    ROUTE_BASES.get(route).map(|base| base.1.clone())
}

/// 根据客户端到达本机时使用的请求头推导绝对 Base URL。
///
/// `Host` 是客户端拨号（或反向代理转发）的主机；协议方案来自
/// 存在的 `X-Forwarded-Proto`，否则为明文 `http`——本进程自身终止 HTTP，
/// TLS 由代理负责。
///
/// 该值会嵌入 bot 消息中的 URL，因此必须是真实的 authority：
/// 拒绝 userinfo、路径、查询与 fragment，而不是转义后放行。
pub fn base_from_headers(headers: &http::HeaderMap) -> Option<String> {
    let host = headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|host| !host.is_empty())?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').next().unwrap_or_default().trim())
        .filter(|value| matches!(*value, "http" | "https"))
        .unwrap_or("http");
    let parsed = url::Url::parse(&format!("{scheme}://{host}")).ok()?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_none()
    {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

/// 初始化（每进程一次）并返回全局媒体链接签名器。
///
/// 调用方在没有路由上下文时签发链接会用到 `fallback_base`；它是
/// 宿主自身的监听地址，因此另一台机器上的网关必须使用带路由的形式。
pub fn init_global(fallback_base: String) -> Arc<HostMediaLinks> {
    GLOBAL
        .get_or_init(|| HostMediaLinks::new(fallback_base))
        .clone()
}

/// 若宿主已初始化，则为进程级签名器。
pub fn global() -> Option<Arc<HostMediaLinks>> {
    GLOBAL.get().cloned()
}

/// HMAC 签名的短期有效链接，用于明确共享的本地文件。
pub struct HostMediaLinks {
    key: [u8; 32],
    fallback_base: String,
}

impl HostMediaLinks {
    pub fn new(fallback_base: String) -> Arc<Self> {
        let mut key = [0u8; 32];
        use rand::TryRng;
        rand::rngs::SysRng
            .try_fill_bytes(&mut key)
            .expect("operating system random source must be available");
        Arc::new(Self {
            key,
            fallback_base: fallback_base.trim_end_matches('/').to_string(),
        })
    }

    fn signature(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC accepts a 32-byte key of this fixed length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// 为现有普通文件签发有效期为 `ttl` 的短期下载链接。
    ///
    /// 签名前会先对路径做规范化（canonicalize），确保签名绑定的就是实际提供的真实文件路径；
    /// HTTP 下载处理器在打开文件时还会禁止跟随末级符号链接（O_NOFOLLOW），防止签发后替换软链接偷梁换柱。
    pub fn issue(&self, path: &Path, ttl: Duration) -> Result<MediaLink> {
        ensure!(!ttl.is_zero(), "媒体链接有效期不能为零");
        ensure!(ttl <= MAX_TTL, "媒体链接有效期最长 1 小时");
        let canonical = path
            .canonicalize()
            .with_context(|| format!("媒体文件不可访问: {}", path.display()))?;
        ensure!(canonical.is_file(), "媒体链接只支持普通文件");
        let file = canonical
            .to_str()
            .context("媒体文件路径必须是 UTF-8")?
            .to_owned();
        let expires_at = SystemTime::now() + ttl;
        let expiry = expires_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let payload = format!("{expiry}.{}", URL_SAFE_NO_PAD.encode(file.as_bytes()));
        let signature = self.signature(&payload);
        let relative = format!("{TOKEN_PREFIX}{payload}.{signature}");
        Ok(MediaLink {
            url: format!("{}{relative}", self.fallback_base),
            path: relative,
            file: canonical,
            expires_at,
        })
    }

    /// 校验签名令牌并返回对应的本地文件路径。
    ///
    /// 格式非法、已过期或签名不匹配时统一返回 `None`；调用方应统一映射为 HTTP `404`，防止外部探测区分具体失败原因。
    pub fn verify(&self, token: &str) -> Option<PathBuf> {
        let (payload, signature) = token.rsplit_once('.')?;
        let (expiry, encoded_path) = payload.split_once('.')?;
        let expiry: u64 = expiry.parse().ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if expiry <= now {
            return None;
        }
        let expected = self.signature(payload);
        if !nuono_app::auth::constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(encoded_path).ok()?;
        let path = String::from_utf8(bytes).ok()?;
        let path = PathBuf::from(path);
        path.is_file().then_some(path)
    }

    /// 生成指定 `route` 可访问的签名链接绝对 URL。
    ///
    /// 若传入了具体路由，说明请求来自特定网关连接；此时若尚未记录该路由的外部基地址（base_url），
    /// 则直接报错让调用方回退为内联 Base64，避免把本机回环地址发给远端网关。
    fn url_for(&self, relative: &str, route: Option<&RuntimeBotRoute>) -> Result<String> {
        match route {
            Some(route) => {
                let base = route_base(route).context(
                    "尚未获知该 bot 路由可达的宿主地址；请先从该网关建立 OneBot WebSocket",
                )?;
                Ok(format!("{base}{relative}"))
            }
            None => Ok(format!("{}{relative}", self.fallback_base)),
        }
    }
}

#[async_trait::async_trait]
impl MediaLinkService for HostMediaLinks {
    async fn link(
        &self,
        path: &Path,
        ttl: Duration,
        route: Option<&RuntimeBotRoute>,
    ) -> nuono_error::Result<MediaLink> {
        // HMAC 签名是纯 CPU 计算，路径规范化也只需一次本地元数据查询，直接同步执行即可。
        let mut link = self.issue(path, ttl)?;
        link.url = self.url_for(&link.path, route)?;
        Ok(link)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/media_links/tests.rs"]
mod tests;
