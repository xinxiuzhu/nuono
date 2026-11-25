//! HTTP 管理面的统一安全边界。
//!
//! 中间件按客户端 IP 限流，并为受保护的管理 API 验证会话及双提交 CSRF token。
//! 只有直连 peer 位于 `security.trusted_proxy_cidrs` 时才信任代理写入的 Forwarded/
//! X-Forwarded-* 头；其余请求一律使用直连地址。此处的内存限流只适用于单进程实例。

use crate::AppState;
use crate::api::ApiResponse;
use axum::{
    extract::{Request, State, connect_info::ConnectInfo},
    middleware::Next,
    response::{IntoResponse, Response},
};
use nuono_core::auth::Capability;
use nuono_core::plugin::PluginWebAccessPolicy;
use std::net::{IpAddr, SocketAddr};

use ipnet::IpNet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 内存限流器状态（使用 DashMap 避免全局锁竞争）
#[derive(Clone)]
pub struct RateLimiter {
    pub counters: Arc<dashmap::DashMap<String, (u32, Instant)>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub fn check(&self, ip: &str, max_per_minute: u32) -> bool {
        if max_per_minute == 0 {
            return true;
        }
        let now = Instant::now();
        if self.counters.len() >= 10_000 {
            self.counters
                .retain(|_, (_, start)| now.duration_since(*start) <= Duration::from_secs(120));
            if self.counters.len() >= 10_000 {
                self.evict_oldest();
            }
        }
        let mut entry = self.counters.entry(ip.to_string()).or_insert((0, now));
        let (count, start) = entry.value_mut();
        if now.duration_since(*start) > Duration::from_secs(60) {
            *count = 1;
            *start = now;
        } else {
            *count += 1;
            if *count > max_per_minute {
                return false;
            }
        }
        true
    }

    /// 容量兜底：按窗口起点淘汰最旧的 10%。
    ///
    /// 不能整体 `clear()`：攻击者用大量源 IP 灌满表格即可连带清掉所有计数
    /// （包括针对自己的），限流会随每次灌满整体重置。
    fn evict_oldest(&self) {
        let mut order: Vec<(String, Instant)> = self
            .counters
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().1))
            .collect();
        order.sort_by_key(|&(_, start)| start);
        let keep = 10_000 - 10_000 / 10;
        let evict = order.len().saturating_sub(keep);
        for (ip, _) in order.into_iter().take(evict) {
            self.counters.remove(&ip);
        }
    }
}

/// 根据请求路径区分限流桶与阈值，避免平台回调、媒体拉取与管理接口在同一 IP 下互相挤占。
fn rate_limit_bucket(
    path: &str,
    client_ip: &str,
    security: &nuono_core::config::SecurityConfig,
) -> (String, u32) {
    let is_platform_webhook = path.starts_with("/ingress/")
        || path.starts_with("/webhook/qq/")
        || path == "/webhook/feishu";
    if is_platform_webhook {
        return (
            format!("webhook:{client_ip}"),
            security.webhook_rate_limit_per_minute,
        );
    }
    // 签名媒体链接属于机器间（M2M）拉取流量：网关发送画廊或媒体组时会高频回源，
    // 独立分桶并沿用更宽松的机器限流额度，防止与控制台操作互相触发限流。
    if path.starts_with("/media/") {
        return (
            format!("media:{client_ip}"),
            security.webhook_rate_limit_per_minute,
        );
    }
    (format!("api:{client_ip}"), security.rate_limit_per_minute)
}

/// 无需登录即可访问的管理 API。
///
/// 邮件相关的公开端点各自靠一次性令牌或配置开关约束，且不区分「账号不存在」与
/// 「操作成功」，因此公开它们不会泄露账号是否存在。注意这里放行的请求同时绕过
/// `is_protected` 分支内的 CSRF 校验，所以只能收录靠自身凭据鉴权的路由：需要当前
/// 会话身份的 `/api/account/email` 与 `/api/users` 绝不能进入本表。唯一的例外是
/// `/api/auth/logout`——它豁免会话检查（会话可能已过期），但在下方单独执行同规则
/// 的 CSRF 双提交校验。
pub(crate) fn is_public_api_request(method: &axum::http::Method, path: &str) -> bool {
    if matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD)
        && (crate::api::plugin_pages::is_asset_path(path) || path == "/api/plugin-pages/bridge.js")
    {
        return true;
    }
    matches!(
        (method, path),
        (&axum::http::Method::POST, "/api/auth/login")
            | (&axum::http::Method::GET, "/api/auth/status")
            | (&axum::http::Method::POST, "/api/auth/logout")
            | (&axum::http::Method::GET, "/api/health")
            | (&axum::http::Method::GET, "/api/public/branding")
            | (&axum::http::Method::GET, "/api/public/branding/logo")
            | (&axum::http::Method::GET, "/api/public/auth-wallpaper")
            | (&axum::http::Method::GET, "/api/setup/status")
            | (&axum::http::Method::GET, "/api/auth/registration-status")
            | (&axum::http::Method::POST, "/api/auth/register")
            | (&axum::http::Method::POST, "/api/auth/verify-email")
            | (
                &axum::http::Method::POST,
                "/api/auth/verify-registration-otp"
            )
            | (&axum::http::Method::POST, "/api/auth/resend-verification")
            | (&axum::http::Method::POST, "/api/auth/forgot-password")
            | (&axum::http::Method::POST, "/api/auth/reset-password")
            | (&axum::http::Method::POST, "/api/account/email/confirm")
    )
}

/// 访问某个受保护 `/api/*` 路由所需的能力。
///
/// 取代原先的「普通用户白名单 + 取反」布尔判断。布尔值表达不了 owner 专属这类中间态，
/// 于是那部分规则只能散落进 handler（`account.rs` 里手写的建 admin 判断就是这么来的）。
/// 现在准入条件只有一处，且是有名字的。
///
/// **默认值是最严的能力**：没被显式登记的路径落到 [`Capability::ManageRuntime`]，只有
/// owner/admin 持有。方向必须如此 —— 新增管理接口忘记登记，后果是管理员之外的人被挡在
/// 外面并立刻被发现；反过来（默认放行）后果是提权。让疏漏往安全一侧倒。
///
/// 登记为客户可访问的标准没变：要么只碰调用者自己的账号数据，要么 handler 内部已按
/// [`nuono_core::auth::Scope`] 过滤了归属。`/api/groups` 系列满足第二条 —— `list` 按
/// Scope 过滤，写操作逐个经 `ensure_group_writable` 校验。**去掉那层校验就必须同时把这里
/// 改回管理能力。**
pub(crate) fn route_capability(method: &axum::http::Method, path: &str) -> Capability {
    use axum::http::Method;

    if let Some(rest) = path.strip_prefix("/api/chat/") {
        let parts: Vec<_> = rest.split('/').collect();
        let allowed = match (method, parts.as_slice()) {
            (&Method::POST, ["sessions"]) => true,
            (&Method::GET | &Method::DELETE, ["sessions", id]) => !id.is_empty(),
            (&Method::POST, ["sessions", id, "messages"]) => !id.is_empty(),
            (&Method::GET, ["sessions", id, "media", media]) => !id.is_empty() && !media.is_empty(),
            _ => false,
        };
        return if allowed {
            Capability::ManageOwnAccount
        } else {
            Capability::ManageRuntime
        };
    }

    if (path == "/api/plugins/personal" && *method == Method::GET)
        || (path
            .strip_prefix("/api/plugins/personal/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            && *method == Method::POST)
    {
        return Capability::ManageOwnAccount;
    }

    if let Some(rest) = path.strip_prefix("/api/assistant/") {
        let parts: Vec<_> = rest.split('/').collect();
        let card_id = |id: &str| uuid::Uuid::parse_str(id).is_ok();
        let requirement = match parts.as_slice() {
            // 角色卡写操作在路由层放行给已登录账号，由 companion 处理器结合「允许普通用户自建角色卡」开关与 ManageRuntime 精确鉴权。
            ["cards"] if matches!(*method, Method::GET | Method::POST) => {
                Some(Capability::ManageOwnAccount)
            }
            ["cards", id] if card_id(id) && matches!(*method, Method::GET | Method::PUT) => {
                Some(Capability::ManageOwnAccount)
            }
            ["cards", id, "activate"] if card_id(id) && *method == Method::POST => {
                Some(Capability::ManageOwnAccount)
            }
            ["cards", id, "assets", kind]
                if card_id(id)
                    && matches!(*kind, "avatar" | "portrait" | "reference_audio")
                    && matches!(
                        *method,
                        Method::GET | Method::HEAD | Method::POST | Method::DELETE
                    ) =>
            {
                Some(Capability::ManageOwnAccount)
            }
            ["presets"] if *method == Method::GET => Some(Capability::ManageOwnAccount),
            ["presets", id, "copy"] if !id.is_empty() && *method == Method::POST => {
                Some(Capability::ManageOwnAccount)
            }
            ["data", "clear-conversation" | "clear-all"] if *method == Method::POST => {
                Some(Capability::ManageOwnAccount)
            }
            _ => None,
        };
        if let Some(requirement) = requirement {
            return requirement;
        }
    }

    if matches!(*method, Method::GET | Method::HEAD)
        && (path == "/api/plugin-pages"
            || path.strip_prefix("/api/plugin-pages/").is_some_and(|rest| {
                let parts: Vec<_> = rest.split('/').collect();
                matches!(parts.as_slice(), [plugin, page] if !plugin.is_empty() && !page.is_empty())
            }))
    {
        return Capability::ManageOwnAccount;
    }

    // 带路径参数的群接口用前缀匹配。`/api/groups/sync/{bot_id}` 是 bot 级全量同步、
    // 不针对单个群，没有归属可校验，因此仍归管理能力。
    if let Some(rest) = path.strip_prefix("/api/groups/") {
        if rest.starts_with("sync/") {
            return Capability::ManageAllGroups;
        }

        // 按群的插件设置。必须在下面那组 `contains`/`ends_with` 之前整片接管，原因有两个：
        // `/{gid}/plugins/{name}/toggle` 会被 `rest.ends_with("/toggle")` 误吃（结论恰好
        // 相同，但那是巧合，改动全局开关那条规则就会连带改掉这里）；而 `DELETE .../config`
        // 根本不在那组的 GET|POST 里，会掉进 `ManageRuntime`，客户点「恢复默认」直接 403。
        //
        // 判据用分段精确匹配而不是子串：`plugins-extra` 之类拼错的路径必须落回默认关闭，
        // 子串写法会把它们一起放进来。
        let mut segments = rest.split('/');
        let group_id = segments.next().unwrap_or_default();
        if !group_id.is_empty() && segments.next() == Some("plugins") {
            let tail: Vec<&str> = segments.collect();
            return match (method, tail.as_slice()) {
                // 读：与 `GET /api/groups` 同一档，行范围由 handler 里的 Scope 收窄。
                (&Method::GET, []) => Capability::ViewGroups,
                // 写：归属与服务可用性由 service 逐次校验，所以用客户也持有的 `ManageGroups`。
                (&Method::POST, [name, "toggle"]) if !name.is_empty() => Capability::ManageGroups,
                (&Method::POST | &Method::DELETE, [name, "config"]) if !name.is_empty() => {
                    Capability::ManageGroups
                }
                // 这棵子树已被本分支整片接管，没登记的形状不再往下走，直接默认关闭。
                _ => Capability::ManageRuntime,
            };
        }

        // 单群接口同样按分段精确匹配。原先用 `ends_with("/toggle")` + `contains("/members/")`
        // 这类子串判断，任何以 `/toggle` 结尾或含 `/members/` 的更深路径都会被顺带放行 ——
        // `/{gid}/plugins-extra/Hello/toggle` 就是这么进来的。段数和段名都对上才算登记。
        // 方法集仍是原来的 GET|POST，没有收紧到每条路由的真实方法 —— 那是另一件事，
        // 而且方法不对的请求走到路由层就是 405。这里只修段匹配。
        let parts: Vec<&str> = rest.split('/').collect();
        // 归属指派与按账号列群是跨归属动作，必须先于下面那组客户可达的规则登记。
        // 不显式写出来的话它们会掉进默认的 `ManageRuntime`，今天恰好等价（只有管理侧
        // 持有它），但将来一旦把默认值收紧或放宽，这两条就跟着悄悄变了。
        match parts.as_slice() {
            [gid] if !gid.is_empty() && *method == Method::PATCH => {
                return Capability::ManageAllGroups;
            }
            [gid, "ai-access"] if !gid.is_empty() && *method == Method::PATCH => {
                return Capability::ManageAllGroups;
            }
            [gid, "subscriptions"] if !gid.is_empty() && *method == Method::GET => {
                return Capability::ViewGroups;
            }
            [gid, "subscriptions"] if !gid.is_empty() && *method == Method::DELETE => {
                return Capability::ManageGroups;
            }
            [gid, "authorizations"]
                if !gid.is_empty() && matches!(*method, Method::GET | Method::POST) =>
            {
                return Capability::ManageAllGroups;
            }
            [gid, "authorizations", source]
                if !gid.is_empty() && !source.is_empty() && *method == Method::DELETE =>
            {
                return Capability::ManageAllGroups;
            }
            ["users", user, "authorizations"] if !user.is_empty() && *method == Method::GET => {
                return Capability::ManageAllGroups;
            }
            ["users", user] if !user.is_empty() && *method == Method::GET => {
                return Capability::ViewAllGroups;
            }
            _ => {}
        }
        let is_group_scoped = matches!(method, &Method::GET | &Method::POST)
            && match parts.as_slice() {
                [gid, "toggle"] => !gid.is_empty(),
                [gid, "members" | "send" | "ban" | "kick", bot] => {
                    !gid.is_empty() && !bot.is_empty()
                }
                _ => false,
            };
        if is_group_scoped {
            return Capability::ManageAllGroups;
        }
    }

    match (method, path) {
        (&Method::POST, "/api/account/merges") | (&Method::GET, "/api/account/merges") | (&Method::GET, "/api/account/sessions") => Capability::ManageOwnAccount,
        (method, path) if matches!(*method, Method::GET | Method::DELETE) && path.strip_prefix("/api/account/merges/").is_some_and(|id| !id.is_empty()&&!id.contains('/')) => Capability::ManageOwnAccount,
        (&Method::POST, path) if path.strip_prefix("/api/account/merges/").and_then(|tail|tail.strip_suffix("/confirm")).is_some_and(|id|!id.is_empty()&&!id.contains('/')) => Capability::ManageOwnAccount,
        (&Method::DELETE, path) if path.strip_prefix("/api/account/sessions/").is_some_and(|id|!id.is_empty()&&!id.contains('/')) => Capability::ManageOwnAccount,
        // 自己的会话与账号资料
        (&Method::POST, "/api/account/bindings")
        | (&Method::GET, "/api/account/identities") | (&Method::GET, "/api/account/binding-bots") => Capability::ManageOwnAccount,
        (method, path) if path.strip_prefix("/api/account/bindings/").is_some_and(|id| !id.is_empty() && !id.contains('/'))
            && matches!(*method, Method::GET | Method::POST | Method::DELETE) => Capability::ManageOwnAccount,
        (&Method::DELETE, path) if path.strip_prefix("/api/account/identities/").is_some_and(|id| !id.is_empty() && !id.contains('/')) => Capability::ManageOwnAccount,
        (&Method::GET, "/api/auth/status")
        | (&Method::POST, "/api/auth/logout")
        | (&Method::POST, "/api/auth/password")
        | (&Method::GET, "/api/account/profile")
        | (&Method::PUT, "/api/account/profile")
        | (&Method::POST, "/api/account/email")
        | (&Method::GET, "/api/account/avatar")
        | (&Method::POST, "/api/account/avatar")
        | (&Method::DELETE, "/api/account/avatar")
        | (&Method::GET, "/api/assistant")
        | (&Method::POST, "/api/assistant/messages")
        | (&Method::POST, "/api/assistant/link")
        | (&Method::DELETE, "/api/assistant/link")
        // 两步验证：只碰自己的账号
        | (&Method::POST, "/api/account/totp/enroll")
        | (&Method::POST, "/api/account/totp/confirm")
        | (&Method::POST, "/api/account/totp/disable")
        | (&Method::POST, "/api/account/totp/recovery") => Capability::ManageOwnAccount,

        // 站内通知：只碰自己的收件箱。前缀匹配覆盖集合与子路径，
        // handler 内的 user_id 一律来自会话身份，不做按 id 的裸读写。
        (_, "/api/notifications") | (_, "/api/notifications/") => Capability::ManageOwnAccount,
        _ if path.starts_with("/api/notifications/") => Capability::ManageOwnAccount,

        // 自己名下的群列表
        (&Method::GET, "/api/groups") | (&Method::GET, "/api/groups/") => Capability::ViewGroups,

        // 前端外壳渲染所需的只读信息
        (&Method::GET, "/api/theme")
        | (&Method::GET, "/api/public/branding")
        | (&Method::GET, "/api/public/branding/logo") => Capability::ViewPublicShell,

        // 账号管理。建 admin 那条更严的规则在 handler 内按
        // `Capability::AssignRoles` 判断 —— 中间件只按路由把关，看不到请求体里的 role。
        (_, "/api/users") => Capability::ManageUsers,
        _ if path.starts_with("/api/users/") => Capability::ManageUsers,

        // 服务控制需要运行管理权限；继续经过会话、能力与 CSRF 校验。
        (&Method::POST, "/api/system/shutdown")
        | (&Method::POST, "/api/system/restart") => Capability::ManageRuntime,

        // 可观测数据：日志、指标与事件审计。
        (&Method::GET, "/api/statistics")
        | (&Method::GET, "/api/statistics/")
        | (&Method::GET, "/api/system/operations")
        | (&Method::GET, "/api/system/disk")
        | (&Method::GET, "/api/logs")
        | (&Method::GET, "/api/logs/")
        | (&Method::GET, "/api/logs/settings")
        | (&Method::GET, "/api/events") => Capability::ViewObservability,
        _ if method == Method::GET
            && (path.starts_with("/api/logs/") || path.starts_with("/api/events/")) =>
        {
            Capability::ViewObservability
        }

        _ => Capability::ManageRuntime,
    }
}

/// 初始化向导的写入端点。仅当数据库中还没有任何账号时才放行。
///
/// 与 [`is_public_api_request`] 分开判断，因为放行条件依赖数据库状态而非纯路径匹配：
/// 一旦第一个 owner 创建成功，这里立即变回受保护路由，不会留下长期开放的注册口。
fn is_setup_write_request(method: &axum::http::Method, path: &str) -> bool {
    *method == axum::http::Method::POST && path == "/api/setup"
}

/// 关闭本机管理认证，也不能凭空确定「我的账号」或群归属。
/// 这些接口以及完整数据库连接编辑仍需要真实会话，并沿用能力与 CSRF 校验。
fn management_request_needs_session(
    admin_auth_enabled: bool,
    method: &axum::http::Method,
    path: &str,
    plugin_policy: PluginWebAccessPolicy,
) -> bool {
    let requires_identity = matches!(
        route_capability(method, path),
        Capability::ManageOwnAccount
            | Capability::ViewGroups
            | Capability::ManageGroups
            | Capability::ManageAllGroups
            | Capability::ViewAllGroups
    ) || matches!(
        path.trim_end_matches('/'),
        "/api/config/database" | "/api/statistics"
    );
    path.starts_with("/api/")
        && !is_public_api_request(method, path)
        && !matches!(plugin_policy, PluginWebAccessPolicy::Public)
        && (admin_auth_enabled || requires_identity)
}

fn plugin_web_target(path: &str) -> Option<(&str, String)> {
    let rest = path.strip_prefix("/api/plugin/")?;
    let (plugin, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if plugin.is_empty() {
        return None;
    }
    Some((
        plugin,
        if rest.is_empty() {
            "/".to_string()
        } else {
            format!("/{rest}")
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginWebRequirement {
    Public,
    Authenticated,
    Capability(Capability),
}

fn plugin_web_requirement(policy: PluginWebAccessPolicy) -> PluginWebRequirement {
    match policy {
        PluginWebAccessPolicy::Public => PluginWebRequirement::Public,
        PluginWebAccessPolicy::Authenticated => PluginWebRequirement::Authenticated,
        PluginWebAccessPolicy::ManageRuntime => {
            PluginWebRequirement::Capability(Capability::ManageRuntime)
        }
        _ => PluginWebRequirement::Capability(Capability::ManageRuntime),
    }
}

/// 从请求中提取 token（优先 Cookie，fallback Header）
fn extract_token(req: &axum::extract::Request) -> Option<String> {
    crate::api::auth::extract_token_from_request(req)
}

/// 从 Cookie 中提取 CSRF token
fn extract_csrf_token_from_cookie(req: &axum::extract::Request) -> Option<String> {
    req.headers()
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;

            if key == "csrf_token" {
                Some(value.to_string())
            } else {
                None
            }
        })
        .next()
}

/// 从请求头中提取 CSRF token
fn extract_csrf_token_from_header(req: &axum::extract::Request) -> Option<String> {
    req.headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// 已通过可信代理策略解析出的客户端连接信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientConnection {
    pub ip: IpAddr,
    pub is_https: bool,
}

/// 仅在直接 peer 属于已配置 CIDR 时采纳转发头。
///
/// 代理必须先移除客户端带来的同名头，再写入自己的值；这里的信任边界是直连 socket，不能
/// 仅凭某个 header 自称来自代理。`Forwarded` 优先，`X-Forwarded-For` 仅作兼容 fallback。
///
/// 客户端 IP 从转发链**由右向左**取第一个不属于可信代理网段的地址：nginx
/// `$proxy_add_x_forwarded_for` 这类「只追加不清洗」的代理会把客户端伪造的 XFF 留在
/// 最左侧，取最左值等于让客户端自报地址。
pub fn client_connection(
    headers: &axum::http::HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_cidrs: &[String],
) -> ClientConnection {
    let peer = peer.unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
    let trusted_nets: Vec<IpNet> = trusted_proxy_cidrs
        .iter()
        .filter_map(|cidr| cidr.parse::<IpNet>().ok())
        .collect();
    let is_trusted = |ip: IpAddr| trusted_nets.iter().any(|net| net.contains(&ip));
    if !trusted_nets.iter().any(|net| net.contains(&peer.ip())) {
        return ClientConnection {
            ip: peer.ip(),
            is_https: false,
        };
    }

    // 不能把 `Forwarded` 的 IP 与另一组 X-Forwarded-Proto 混用：当两组头来自不同代理层，
    // 这会把互不对应的来源拼成一个看似可信的请求。选中的 IP 来源同时决定协议头来源。
    let (ip, is_https) = if let Some(entries) = headers
        .get("forwarded")
        .and_then(|value| value.to_str().ok())
        .map(forwarded_entries)
        && !entries.is_empty()
    {
        match entries
            .iter()
            .rev()
            .find(|(ip, _)| ip.is_some_and(|ip| !is_trusted(ip)))
        {
            Some((ip, https)) => (ip.unwrap(), *https),
            // 链上全是可信代理或全部不可解析：按无客户端信息处理。
            None => (peer.ip(), false),
        }
    } else if let Some(ips) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .filter_map(|value| value.trim().parse::<IpAddr>().ok())
                .collect::<Vec<_>>()
        })
        && !ips.is_empty()
    {
        match ips.iter().rev().find(|ip| !is_trusted(**ip)) {
            Some(ip) => (
                *ip,
                headers
                    .get("x-forwarded-proto")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.split(',').next())
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("https")),
            ),
            None => (peer.ip(), false),
        }
    } else {
        (peer.ip(), false)
    };

    ClientConnection { ip, is_https }
}

/// 解析 `Forwarded` 头的每个条目为 `(for=IP, proto=https)`。
///
/// `for` 不可解析（如 `for=_hidden`）的条目 IP 为 `None`，调用方按无客户端信息跳过。
fn forwarded_entries(value: &str) -> Vec<(Option<IpAddr>, bool)> {
    value
        .split(',')
        .map(|entry| {
            let mut ip = None;
            let mut https = false;
            for part in entry.split(';') {
                let Some((name, value)) = part.trim().split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                if name.trim().eq_ignore_ascii_case("for") {
                    let value = value
                        .strip_prefix('[')
                        .and_then(|v| v.split_once(']').map(|(ip, _)| ip))
                        .unwrap_or(value);
                    ip = value.parse().ok();
                } else if name.trim().eq_ignore_ascii_case("proto") {
                    https = value.eq_ignore_ascii_case("https");
                }
            }
            (ip, https)
        })
        .collect()
}

fn extract_client_connection(req: &Request, trusted_proxy_cidrs: &[String]) -> ClientConnection {
    client_connection(
        req.headers(),
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0),
        trusted_proxy_cidrs,
    )
}

/// 依次执行直连 IP 限流、管理会话认证、状态变更 CSRF 校验与 HTTP 指标记录。
///
/// `/api` 之外的 WebSocket、插件 Web 路由和静态文件不由会话认证覆盖；它们必须使用各自
/// 协议认证并在部署层限制暴露。
/// 认证通过后注入请求的调用者身份，供 handler 做数据级过滤。
///
/// 与中间件的路由级授权分工明确：中间件决定「能不能进这个接口」，
/// 这份身份决定「在接口里能看到哪些行」。
///
/// 类型本身定义在 [`nuono_core::auth`]，这里只做转出以免各 handler 的 import 全部改写。
/// 定义下沉到 core 是为了让 `service` 层能直接接 [`nuono_core::auth::Scope`]，
/// 不必每个 handler 手工桥接一次身份到范围 —— 忘了桥就是全量返回。
pub use nuono_core::auth::CallerIdentity;

/// 登出请求是否需要单独执行 CSRF 双提交校验。
///
/// `/api/auth/logout` 在公开豁免表里——豁免的是会话检查（会话可能已过期，登出
/// 仍应能清 Cookie），不能连带豁免 CSRF：否则任意网页可借受害者 Cookie 强制其
/// 登出。`admin_auth_enabled=false` 的本机模式下没有可劫持的会话、也拿不到
/// csrf Cookie，因此不校验。
fn logout_needs_csrf_check(
    admin_auth_enabled: bool,
    method: &axum::http::Method,
    path: &str,
) -> bool {
    admin_auth_enabled && *method == axum::http::Method::POST && path == "/api/auth/logout"
}

pub async fn security_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let cfg = state.config.snapshot().await;
    let client = extract_client_connection(&req, &cfg.security.trusted_proxy_cidrs);
    let client_ip = client.ip.to_string();
    req.extensions_mut().insert(client);

    // Rate limiting —— 跳过静态资源（前端 assets、favicon 等）
    let path = req.uri().path();
    let is_page_asset = matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && crate::api::plugin_pages::is_asset_path(path);
    let is_page_metadata = path.starts_with("/api/plugin-pages")
        && !is_page_asset
        && path != "/api/plugin-pages/bridge.js";
    let is_static = path.starts_with("/assets/")
        || path.starts_with("/wallpapers/")
        || path == "/favicon.svg"
        || path == "/favicon.ico"
        || path.ends_with(".js")
        || path.ends_with(".css")
        || path.ends_with(".svg")
        || path.ends_with(".png")
        || path.ends_with(".ico")
        || path.ends_with(".woff2");

    if !is_static {
        // 平台回调、签名媒体拉取与管理接口按类别独立分桶限流：
        // 既避免未认证回调消耗过多资源，又防止同 IP 下的平台流量与后台操作互相锁死。
        let (bucket_key, limit) = rate_limit_bucket(path, &client_ip, &cfg.security);
        if !state.rate_limiter.check(&bucket_key, limit) {
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                ApiResponse::<()>::bad_request("请求过于频繁，请稍后再试"),
            )
                .into_response();
        }
    }

    let method = req.method();
    let is_state_changing = matches!(
        *method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::DELETE
            | axum::http::Method::PATCH
    );
    let path = req.uri().path();

    // 管理 API 默认需要认证；只显式放行登录状态相关接口和健康检查。
    let plugin_web = if cfg.security.admin_auth_enabled {
        plugin_web_target(path)
    } else {
        None
    };
    let plugin_web_policy = if let Some((plugin, ref sub_path)) = plugin_web {
        state
            .manager
            .plugin_host
            .plugin_web_policy(plugin, method, sub_path)
            .await
    } else {
        PluginWebAccessPolicy::ManageRuntime
    };
    let mut is_protected = management_request_needs_session(
        cfg.security.admin_auth_enabled,
        method,
        path,
        plugin_web_policy,
    );

    // 初始化向导：只在数据库尚无账号时放行 POST /api/setup。
    //
    // 查询失败时保持受保护状态（fail closed），避免数据库异常被利用成无认证创建 owner。
    if is_protected && is_setup_write_request(method, path) {
        is_protected = match state.db.user_count().await {
            Ok(count) => count != 0,
            Err(error) => return ApiResponse::<()>::from_err(&error).into_response(),
        };
    }

    // 登出豁免的是会话检查（会话可能已过期，登出仍应能清 Cookie），不是 CSRF：
    // 状态变更请求的双提交校验照常生效，否则任意网页可借受害者 Cookie 强制其登出。
    // 前端的统一请求封装对所有写请求都带 X-CSRF-Token，登出不受影响。
    if !is_protected
        && is_state_changing
        && logout_needs_csrf_check(cfg.security.admin_auth_enabled, method, path)
    {
        let cookie_csrf = extract_csrf_token_from_cookie(&req);
        let header_csrf = extract_csrf_token_from_header(&req);
        let csrf_valid = matches!((cookie_csrf, header_csrf), (Some(c), Some(h)) if c == h);
        if !csrf_valid {
            return (
                axum::http::StatusCode::FORBIDDEN,
                ApiResponse::<()>::bad_request("CSRF 校验失败: 缺少或无效的 CSRF Token"),
            )
                .into_response();
        }
    }

    if is_protected {
        let Some(token) = extract_token(&req) else {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                ApiResponse::<()>::forbidden("未授权: 缺少认证令牌"),
            )
                .into_response();
        };

        let user = match crate::api::auth::authenticated_user_checked(&state.db, &token).await {
            Ok(Some(user)) => user,
            Err(error) => return ApiResponse::<()>::from_err(&error).into_response(),
            Ok(None) => {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    ApiResponse::<()>::forbidden("未授权: 令牌无效或已过期"),
                )
                    .into_response();
            }
        };

        let caller = CallerIdentity::from_user(&user);

        // 路由级授权：插件 Web 路由使用插件声明的 policy；其它 API 使用静态能力表。
        let requirement = plugin_web
            .map(|_| plugin_web_requirement(plugin_web_policy))
            .unwrap_or(PluginWebRequirement::Capability(route_capability(
                method, path,
            )));
        let required = match requirement {
            PluginWebRequirement::Public | PluginWebRequirement::Authenticated => None,
            PluginWebRequirement::Capability(capability) => Some(capability),
        };
        if let Some(required) = required.filter(|capability| !caller.has(*capability)) {
            // 客户点到管理功能是正常的导航行为（前端导航按能力过滤前尤其常见），
            // 不是攻击信号，所以记 info 不记 warn —— 用 warn 会让真正的异常淹没在噪声里。
            tracing::info!(
                username = %caller.username,
                role = %caller.role,
                capability = %required,
                %method,
                path,
                "拒绝越权访问：调用者不具备该路由所需能力"
            );
            metrics::counter!(
                "nuono_authz_denied_total",
                "role" => caller.role.as_str(),
                "capability" => required.as_str(),
            )
            .increment(1);
            return (
                axum::http::StatusCode::FORBIDDEN,
                ApiResponse::<()>::forbidden(required.denial_message()),
            )
                .into_response();
        }

        // CSRF 校验：认证通过后，对状态变更请求要求 Header 与 Cookie 中的 Token 一致。
        if is_state_changing {
            let cookie_csrf = extract_csrf_token_from_cookie(&req);
            let header_csrf = extract_csrf_token_from_header(&req);

            let csrf_valid = match (cookie_csrf, header_csrf) {
                (Some(c), Some(h)) => c == h,
                _ => false,
            };

            if !csrf_valid {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    ApiResponse::<()>::bad_request("CSRF 校验失败: 缺少或无效的 CSRF Token"),
                )
                    .into_response();
            }
        }

        req.extensions_mut().insert(caller);
    } else if !cfg.security.admin_auth_enabled
        && let Some(token) = extract_token(&req)
        && let Some(user) = crate::api::auth::authenticated_user(&state.db, &token).await
    {
        // 本机无认证运维接口允许匿名调用；有真实会话时仍记录真实身份，供审计使用。
        req.extensions_mut()
            .insert(CallerIdentity::from_user(&user));
    }

    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let start = Instant::now();

    let mut res = next.run(req).await;
    if is_page_metadata {
        res.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    let duration = start.elapsed();
    let status = res.status().as_u16().to_string();

    metrics::counter!("nuono_http_requests_total", "method" => method.clone(), "route" => route.clone(), "status" => status).increment(1);
    metrics::histogram!("nuono_http_request_duration_seconds", "method" => method, "route" => route)
        .record(duration.as_secs_f64());

    // 全局安全响应头。CSP 需要 nonce，只在 SPA HTML 响应处注入（见
    // `main::serve_index_with_csp_nonce`）；nosniff 与 frame-deny 对所有响应统一生效。
    res.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    res.headers_mut().insert(
        axum::http::HeaderName::from_static("x-frame-options"),
        axum::http::HeaderValue::from_static(if is_page_asset { "SAMEORIGIN" } else { "DENY" }),
    );

    // 开发模式下禁止浏览器缓存 HTML，防止 hydration key 不匹配
    if let Some(ct) = res.headers().get("content-type")
        && ct.to_str().unwrap_or("").starts_with("text/html")
    {
        res.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static(
                "no-store, no-cache, must-revalidate, proxy-revalidate",
            ),
        );
    }

    res
}

#[cfg(test)]
mod tests {
    use super::{
        PluginWebRequirement, client_connection, is_public_api_request, is_setup_write_request,
        logout_needs_csrf_check, management_request_needs_session, plugin_web_requirement,
        plugin_web_target, route_capability,
    };
    use axum::http::Method;
    use nuono_core::auth::{Capability, Role};
    use nuono_core::plugin::PluginWebAccessPolicy;

    #[test]
    fn logout_keeps_session_exemption_but_carries_csrf_check() {
        // 公开豁免不变：会话可能已过期，登出仍必须能清 Cookie。
        assert!(is_public_api_request(&Method::POST, "/api/auth/logout"));
        // 但启用管理认证时，登出这一状态变更要过 CSRF 双提交校验。
        assert!(logout_needs_csrf_check(
            true,
            &Method::POST,
            "/api/auth/logout"
        ));
        // 本机无认证模式没有可劫持的会话，不校验（否则无 Cookie 的匿名登出会被 403）。
        assert!(!logout_needs_csrf_check(
            false,
            &Method::POST,
            "/api/auth/logout"
        ));
        // 其它方法/路径不触发。
        assert!(!logout_needs_csrf_check(
            true,
            &Method::GET,
            "/api/auth/logout"
        ));
        assert!(!logout_needs_csrf_check(
            true,
            &Method::POST,
            "/api/auth/login"
        ));
        assert!(!logout_needs_csrf_check(
            true,
            &Method::POST,
            "/api/auth/logout/extra"
        ));
        assert!(!logout_needs_csrf_check(
            true,
            &Method::POST,
            "/api/auth/logoutx"
        ));
    }

    #[test]
    fn native_activity_identifiers_always_require_a_staff_session() {
        for path in ["/api/statistics", "/api/statistics/"] {
            assert_eq!(
                route_capability(&Method::GET, path),
                Capability::ViewObservability
            );
            assert!(management_request_needs_session(
                false,
                &Method::GET,
                path,
                PluginWebAccessPolicy::ManageRuntime,
            ));
        }
        assert!(Role::Owner.has(Capability::ViewObservability));
        assert!(Role::Admin.has(Capability::ViewObservability));
        assert!(!Role::User.has(Capability::ViewObservability));
    }

    #[test]
    fn database_connection_editor_always_requires_an_administrator_session() {
        for method in [Method::GET, Method::POST] {
            for path in ["/api/config/database", "/api/config/database/"] {
                assert!(!is_public_api_request(&method, path));
                assert_eq!(route_capability(&method, path), Capability::ManageRuntime);
                assert!(management_request_needs_session(
                    false,
                    &method,
                    path,
                    PluginWebAccessPolicy::ManageRuntime,
                ));
            }
        }
        assert!(Role::Owner.has(Capability::ManageRuntime));
        assert!(Role::Admin.has(Capability::ManageRuntime));
        assert!(!Role::User.has(Capability::ManageRuntime));
    }

    #[test]
    fn disabled_admin_auth_still_requires_real_identity_for_account_and_group_routes() {
        for (method, path) in [
            (Method::GET, "/api/account/profile"),
            (Method::GET, "/api/account/avatar"),
            (Method::POST, "/api/account/avatar"),
            (Method::DELETE, "/api/account/avatar"),
            (Method::POST, "/api/auth/password"),
            (Method::GET, "/api/notifications"),
            (Method::POST, "/api/notifications/1/read"),
            (Method::GET, "/api/groups"),
            (Method::GET, "/api/groups/"),
            (Method::POST, "/api/groups/1/toggle"),
            (Method::GET, "/api/groups/1/plugins"),
            (Method::POST, "/api/groups/1/plugins/demo/config"),
            (Method::DELETE, "/api/groups/1/plugins/demo/config"),
        ] {
            assert!(
                management_request_needs_session(
                    false,
                    &method,
                    path,
                    PluginWebAccessPolicy::ManageRuntime,
                ),
                "{method} {path} 依赖真实账号，关闭本地管理认证也必须注入已认证身份"
            );
        }
    }

    #[test]
    fn disabled_admin_auth_preserves_anonymous_runtime_and_public_routes() {
        for path in ["/api/system/operations", "/api/stats", "/api/config"] {
            assert!(!management_request_needs_session(
                false,
                &Method::GET,
                path,
                PluginWebAccessPolicy::ManageRuntime,
            ));
            assert!(management_request_needs_session(
                true,
                &Method::GET,
                path,
                PluginWebAccessPolicy::ManageRuntime,
            ));
        }
        for enabled in [false, true] {
            for (method, path) in [
                (Method::GET, "/api/auth/status"),
                (Method::POST, "/api/auth/login"),
                (Method::GET, "/api/setup/status"),
            ] {
                assert!(!management_request_needs_session(
                    enabled,
                    &method,
                    path,
                    PluginWebAccessPolicy::ManageRuntime,
                ));
            }
            assert!(!management_request_needs_session(
                enabled,
                &Method::GET,
                "/api/plugin/demo/public",
                PluginWebAccessPolicy::Public,
            ));
        }
    }

    /// 该路由是否会把普通用户挡在外面。
    ///
    /// 保留这个名字的语义（原来是 `requires_admin_role`）好让下面的用例继续 pin 同一件事：
    /// 重构前后「谁进得来」必须逐条一致。现在的实现是查能力再问角色，而不是查白名单。
    fn requires_admin_role(method: &Method, path: &str) -> bool {
        !Role::User.has(route_capability(method, path))
    }

    #[test]
    fn plugin_web_target_preserves_exact_plugin_relative_path() {
        assert_eq!(
            plugin_web_target("/api/plugin/wutheringwaves/callback"),
            Some(("wutheringwaves", "/callback".to_string()))
        );
        assert_eq!(
            plugin_web_target("/api/plugin/wutheringwaves"),
            Some(("wutheringwaves", "/".to_string()))
        );
        assert_eq!(plugin_web_target("/api/plugin/"), None);
        assert_eq!(plugin_web_target("/api/plugins/wutheringwaves"), None);
    }

    #[test]
    fn plugin_web_access_policy_maps_to_three_distinct_requirements() {
        use nuono_core::plugin::PluginWebAccessPolicy;

        assert_eq!(
            plugin_web_requirement(PluginWebAccessPolicy::Public),
            PluginWebRequirement::Public
        );
        assert_eq!(
            plugin_web_requirement(PluginWebAccessPolicy::Authenticated),
            PluginWebRequirement::Authenticated
        );
        assert_eq!(
            plugin_web_requirement(PluginWebAccessPolicy::ManageRuntime),
            PluginWebRequirement::Capability(Capability::ManageRuntime)
        );
    }

    #[test]
    fn untrusted_peer_cannot_spoof_client_address_or_https() {
        let headers = axum::http::HeaderMap::from_iter([
            (
                "x-forwarded-for".parse().unwrap(),
                "203.0.113.9".parse().unwrap(),
            ),
            (
                "x-forwarded-proto".parse().unwrap(),
                "https".parse().unwrap(),
            ),
        ]);
        let peer = "198.51.100.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        assert_eq!(connection.ip.to_string(), "198.51.100.7");
        assert!(!connection.is_https);
    }

    #[test]
    fn trusted_proxy_can_forward_client_address_and_https() {
        let headers = axum::http::HeaderMap::from_iter([
            (
                "forwarded".parse().unwrap(),
                "for=203.0.113.9;proto=https".parse().unwrap(),
            ),
            (
                "x-forwarded-for".parse().unwrap(),
                "198.51.100.99".parse().unwrap(),
            ),
        ]);
        let peer = "10.0.0.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        assert_eq!(connection.ip.to_string(), "203.0.113.9");
        assert!(connection.is_https);
    }

    #[test]
    fn malformed_trusted_proxy_headers_fall_back_to_peer_address() {
        let headers = axum::http::HeaderMap::from_iter([(
            "forwarded".parse().unwrap(),
            "for=_hidden;proto=http".parse().unwrap(),
        )]);
        let peer = "10.0.0.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        assert_eq!(connection.ip.to_string(), "10.0.0.7");
        assert!(!connection.is_https);
    }

    /// 追加式代理（nginx 默认行为）会把客户端伪造的 XFF 留在最左侧。
    /// 客户端地址必须取最右的非可信 IP，取最左等于让客户端自报地址。
    #[test]
    fn append_mode_xff_takes_rightmost_untrusted_ip() {
        let headers = axum::http::HeaderMap::from_iter([
            (
                "x-forwarded-for".parse().unwrap(),
                "203.0.113.9, 198.51.100.7".parse().unwrap(),
            ),
            (
                "x-forwarded-proto".parse().unwrap(),
                "https".parse().unwrap(),
            ),
        ]);
        let peer = "10.0.0.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        assert_eq!(connection.ip.to_string(), "198.51.100.7");
        assert!(connection.is_https);
    }

    /// 转发链上全是可信代理时按无客户端信息处理，回退直连地址。
    #[test]
    fn fully_trusted_forward_chain_falls_back_to_peer() {
        let headers = axum::http::HeaderMap::from_iter([(
            "x-forwarded-for".parse().unwrap(),
            "10.0.0.8, 10.0.0.9".parse().unwrap(),
        )]);
        let peer = "10.0.0.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        assert_eq!(connection.ip.to_string(), "10.0.0.7");
        assert!(!connection.is_https);
    }

    /// 多层 Forwarded 链同样从右向左取第一个非可信条目，且协议取自同一条目。
    #[test]
    fn forwarded_chain_takes_rightmost_untrusted_entry_with_its_proto() {
        let headers = axum::http::HeaderMap::from_iter([(
            "forwarded".parse().unwrap(),
            "for=203.0.113.9;proto=https, for=10.0.0.9;proto=http"
                .parse()
                .unwrap(),
        )]);
        let peer = "10.0.0.7:443".parse().unwrap();
        let connection = client_connection(&headers, Some(peer), &["10.0.0.0/8".to_string()]);
        // 最右的 10.0.0.9 可信，向左找到 203.0.113.9；proto 用同一条目的 https。
        assert_eq!(connection.ip.to_string(), "203.0.113.9");
        assert!(connection.is_https);
    }

    #[test]
    fn public_api_matching_requires_the_expected_method_and_exact_path() {
        assert!(is_public_api_request(&Method::GET, "/api/public/branding"));
        assert!(is_public_api_request(
            &Method::GET,
            "/api/public/branding/logo"
        ));
        assert!(!is_public_api_request(
            &Method::POST,
            "/api/public/branding"
        ));
        assert!(!is_public_api_request(
            &Method::GET,
            "/api/public/branding/logo/extra"
        ));
        assert!(!is_public_api_request(&Method::GET, "/api/account/avatar"));
    }

    #[test]
    fn existing_public_endpoints_keep_only_their_actual_methods() {
        assert!(is_public_api_request(&Method::POST, "/api/auth/login"));
        assert!(!is_public_api_request(&Method::GET, "/api/auth/login"));
    }

    #[test]
    fn retired_workflow_webhooks_have_no_public_authentication_bypass() {
        for path in [
            "/api/sop/webhooks/demo/external",
            "/api/sop/events/ticket.closed/external",
        ] {
            assert!(!is_public_api_request(&Method::POST, path));
            assert!(!is_public_api_request(&Method::GET, path));
        }
    }

    #[test]
    fn setup_status_is_public_but_setup_write_is_not_path_public() {
        // 读状态可以无条件公开：只返回 needs_setup 布尔值。
        assert!(is_public_api_request(&Method::GET, "/api/setup/status"));

        // 写入端点绝不能进入纯路径放行表，否则建号后会留下永久开放的注册口。
        assert!(!is_public_api_request(&Method::POST, "/api/setup"));
    }

    #[test]
    fn setup_write_matcher_is_exact_on_method_and_path() {
        assert!(is_setup_write_request(&Method::POST, "/api/setup"));
        assert!(!is_setup_write_request(&Method::GET, "/api/setup"));
        assert!(!is_setup_write_request(&Method::POST, "/api/setup/status"));
        assert!(!is_setup_write_request(&Method::POST, "/api/setup/extra"));
        assert!(!is_setup_write_request(&Method::POST, "/api/setupx"));
    }

    #[test]
    fn mail_flow_endpoints_are_public_only_on_their_own_method() {
        for path in [
            "/api/auth/register",
            "/api/auth/verify-email",
            "/api/auth/verify-registration-otp",
            "/api/auth/resend-verification",
            "/api/auth/forgot-password",
            "/api/auth/reset-password",
            "/api/account/email/confirm",
        ] {
            assert!(
                is_public_api_request(&Method::POST, path),
                "{path} 应对 POST 公开"
            );
            assert!(
                !is_public_api_request(&Method::GET, path),
                "{path} 不应对 GET 公开"
            );
        }

        assert!(is_public_api_request(
            &Method::GET,
            "/api/auth/registration-status"
        ));
        assert!(!is_public_api_request(
            &Method::POST,
            "/api/auth/registration-status"
        ));
    }

    #[test]
    fn session_bound_account_endpoints_stay_protected() {
        // 发起改邮箱要读当前会话身份，公开它等于让任何人替他人改邮箱。
        assert!(!is_public_api_request(&Method::POST, "/api/account/email"));
        // 建号与列举账号是 owner/admin 专属。
        assert!(!is_public_api_request(&Method::POST, "/api/users"));
        assert!(!is_public_api_request(&Method::GET, "/api/users"));
    }

    /// 改角色的接口必须先被中间件拦在管理面之外。
    ///
    /// 中间件这一层只认到 `ManageUsers`（admin 也过），「只有 owner 能改角色」由
    /// handler 里的 `AssignRoles` 兜。这道测试 pin 的是前半段：普通用户根本进不来。
    /// 后半段在 `service::users` 的用例里 —— 两层缺任一层，admin 就能自我提权。
    #[test]
    fn role_change_route_is_closed_to_plain_users() {
        let cap = route_capability(&Method::PATCH, "/api/users/7/role");
        assert_eq!(cap, Capability::ManageUsers);
        assert!(!Role::User.has(cap));
        assert!(Role::Owner.has(cap));
    }

    #[test]
    fn admin_surface_is_closed_to_plain_users_by_default() {
        // 抽查各类高危管理面：授权是白名单，这些一律要求 admin。
        for (method, path) in [
            (Method::POST, "/api/database/backup"),
            (Method::GET, "/api/database/tables"),
            (Method::GET, "/api/database/catalog"),
            (
                Method::POST,
                "/api/database/tables/nuono_filter_rules/records",
            ),
            (
                Method::PATCH,
                "/api/database/tables/nuono_filter_rules/records",
            ),
            (
                Method::DELETE,
                "/api/database/tables/nuono_filter_rules/records",
            ),
            (Method::GET, "/api/database/tables/users/schema"),
            (Method::GET, "/api/database/tables/users/rows"),
            (Method::GET, "/api/database/tables/users/export"),
            (Method::GET, "/api/database/backups/example.dump/download"),
            (Method::DELETE, "/api/database/backups/example.dump"),
            (Method::POST, "/api/config"),
            (Method::GET, "/api/config"),
            (Method::GET, "/api/config/message-routes"),
            (Method::GET, "/api/system/info"),
            (Method::GET, "/api/security"),
            (Method::GET, "/api/audit"),
            (Method::GET, "/api/plugins"),
            (Method::GET, "/api/users"),
            (Method::POST, "/api/users"),
            (Method::GET, "/api/logs"),
            (Method::GET, "/api/logs/files"),
            (Method::GET, "/api/logs/files/content"),
            (Method::GET, "/api/bots"),
        ] {
            assert!(
                requires_admin_role(&method, path),
                "{method} {path} 必须要求管理员权限"
            );
        }
    }

    #[test]
    fn user_allowlist_covers_only_own_account_data() {
        for (method, path) in [
            (Method::GET, "/api/auth/status"),
            (Method::POST, "/api/auth/logout"),
            (Method::POST, "/api/auth/password"),
            (Method::GET, "/api/account/profile"),
            (Method::POST, "/api/account/email"),
            (Method::GET, "/api/account/avatar"),
            (Method::POST, "/api/account/avatar"),
            (Method::DELETE, "/api/account/avatar"),
            (Method::GET, "/api/theme"),
            (Method::GET, "/api/public/branding"),
        ] {
            assert!(
                !requires_admin_role(&method, path),
                "{method} {path} 应对普通用户放行"
            );
        }
    }

    #[test]
    fn native_group_controls_require_staff_while_configuration_remains_reachable() {
        assert!(!requires_admin_role(&Method::GET, "/api/groups"));
        for (method, path) in [
            (Method::POST, "/api/groups/123/toggle"),
            (Method::GET, "/api/groups/123/members/bot1"),
            (Method::POST, "/api/groups/123/send/bot1"),
            (Method::POST, "/api/groups/123/ban/bot1"),
            (Method::POST, "/api/groups/123/kick/bot1"),
        ] {
            assert!(
                requires_admin_role(&method, path),
                "{method} {path} 必须要求框架管理员"
            );
        }
    }

    /// 按群插件设置对客户可达。归属与服务可用性由 service 逐次校验，这里只锁路由可达性。
    #[test]
    fn group_plugin_routes_are_reachable_by_customers() {
        for (method, path, expected) in [
            (
                Method::GET,
                "/api/groups/123/plugins",
                Capability::ViewGroups,
            ),
            (
                Method::POST,
                "/api/groups/123/plugins/Hello/toggle",
                Capability::ManageGroups,
            ),
            (
                Method::POST,
                "/api/groups/123/plugins/Hello/config",
                Capability::ManageGroups,
            ),
            (
                Method::DELETE,
                "/api/groups/123/plugins/Hello/config",
                Capability::ManageGroups,
            ),
        ] {
            assert!(
                !requires_admin_role(&method, path),
                "{method} {path} 应对普通用户放行"
            );
            assert_eq!(
                route_capability(&method, path),
                expected,
                "{method} {path} 的能力登记与预期不符"
            );
        }

        // 非数字群号（飞书 `oc_` 之类）走同一条分支，不能因为群号不是数字就掉进默认关闭。
        assert_eq!(
            route_capability(&Method::GET, "/api/groups/oc_abc123/plugins"),
            Capability::ViewGroups
        );
    }

    /// 插件子树不能蹭到既有群路由的规则，既有群路由也不能被插件分支改掉。
    ///
    /// `/plugins/{name}/toggle` 会被 `rest.ends_with("/toggle")` 误吃 —— 两边结论恰好相同，
    /// 所以这里逐条 pin 具体能力而不是只 pin「用户能不能进」，否则任一侧被改动都不会亮红。
    #[test]
    fn group_plugin_paths_and_legacy_group_paths_stay_separated() {
        // 既有映射逐条钉死。
        for (method, path, expected) in [
            (Method::GET, "/api/groups", Capability::ViewGroups),
            (
                Method::POST,
                "/api/groups/123/toggle",
                Capability::ManageAllGroups,
            ),
            (
                Method::GET,
                "/api/groups/123/members/bot1",
                Capability::ManageAllGroups,
            ),
            (
                Method::POST,
                "/api/groups/123/send/bot1",
                Capability::ManageAllGroups,
            ),
            (
                Method::POST,
                "/api/groups/123/ban/bot1",
                Capability::ManageAllGroups,
            ),
            (
                Method::POST,
                "/api/groups/123/kick/bot1",
                Capability::ManageAllGroups,
            ),
            (
                Method::POST,
                "/api/groups/sync/bot1",
                Capability::ManageAllGroups,
            ),
        ] {
            assert_eq!(
                route_capability(&method, path),
                expected,
                "{method} {path} 的既有映射被改动了"
            );
        }

        // 群级插件配置的 DELETE 可达，但群本身的 DELETE 没登记过，必须仍然默认关闭 ——
        // 插件分支只接管 `/{gid}/nuono_plugins/...` 这棵子树。
        assert_eq!(
            route_capability(&Method::DELETE, "/api/groups/123/toggle"),
            Capability::ManageRuntime
        );
        // 全局插件面仍是运维能力，没被群级路由带出可达性。
        assert!(requires_admin_role(&Method::GET, "/api/plugins"));
        assert!(requires_admin_role(
            &Method::POST,
            "/api/plugins/Hello/toggle"
        ));
        assert!(requires_admin_role(
            &Method::POST,
            "/api/plugins/Hello/diagnostics"
        ));
    }

    /// 单群接口的匹配是分段精确的，更深的路径不能靠后缀或子串蹭进来。
    ///
    /// 这条是加插件路由时发现的既有松动：原实现用 `ends_with("/toggle")`，
    /// `/api/groups/123/plugins-extra/Hello/toggle` 因此被放行到了客户侧。
    #[test]
    fn legacy_group_action_matching_is_segment_exact() {
        for (method, path) in [
            (Method::POST, "/api/groups/123/deep/nested/toggle"),
            (Method::GET, "/api/groups/123/members/bot1/extra"),
            (Method::POST, "/api/groups/123/send/bot1/extra"),
            (Method::POST, "/api/groups/123/togglex"),
            (Method::GET, "/api/groups/123/members/"),
        ] {
            assert_eq!(
                route_capability(&method, path),
                Capability::ManageRuntime,
                "{method} {path} 不是已登记的单群接口，必须默认关闭"
            );
        }
    }

    /// 插件子树里没登记过的形状一律默认关闭，包括拼错的段名和多出来的层级。
    #[test]
    fn unregistered_group_plugin_variants_stay_closed() {
        for (method, path) in [
            // 段名拼错：分段精确匹配，子串写法会把它放进来。
            (Method::GET, "/api/groups/123/plugins-extra"),
            (Method::POST, "/api/groups/123/plugins-extra/Hello/toggle"),
            // 动作没登记。
            (Method::POST, "/api/groups/123/plugins/Hello/reset"),
            (Method::POST, "/api/groups/123/plugins"),
            (Method::DELETE, "/api/groups/123/plugins/Hello/toggle"),
            // 层级多了一层。
            (Method::POST, "/api/groups/123/plugins/Hello/config/extra"),
            // 插件名为空。
            (Method::POST, "/api/groups/123/plugins//toggle"),
            // 群号为空。
            (Method::GET, "/api/groups//plugins"),
        ] {
            assert_eq!(
                route_capability(&method, path),
                Capability::ManageRuntime,
                "{method} {path} 未登记时必须默认关闭"
            );
            assert!(
                requires_admin_role(&method, path),
                "{method} {path} 未登记时不能对普通用户放行"
            );
        }
    }

    #[test]
    fn bot_wide_group_sync_stays_admin_only() {
        // 全量同步是 bot 级操作，没有单群归属可校验，不能交给普通用户。
        assert!(requires_admin_role(&Method::POST, "/api/groups/sync/bot1"));
    }

    #[test]
    fn theme_and_branding_writes_stay_admin_only() {
        // 主题和 logo 是全站配置，只读放行、写入收紧。
        assert!(requires_admin_role(&Method::POST, "/api/theme"));
        assert!(requires_admin_role(&Method::POST, "/api/branding/logo"));
        assert!(requires_admin_role(&Method::DELETE, "/api/branding/logo"));
    }

    #[test]
    fn user_allowlist_matching_is_exact() {
        assert!(requires_admin_role(
            &Method::GET,
            "/api/account/profile/extra"
        ));
        assert!(requires_admin_role(&Method::GET, "/api/account/profilex"));
        // 头像放行 GET/POST/DELETE，但父路径 /api/account 不能因此放行。
        assert!(requires_admin_role(&Method::GET, "/api/account"));
    }

    #[test]
    fn public_mail_paths_do_not_leak_via_prefix_or_suffix() {
        // 放行是精确匹配，不能被相邻路径蹭进来。
        assert!(!is_public_api_request(
            &Method::POST,
            "/api/auth/register/extra"
        ));
        assert!(!is_public_api_request(&Method::POST, "/api/auth/registerx"));
        assert!(!is_public_api_request(
            &Method::POST,
            "/api/account/email/confirm/extra"
        ));
        // `/api/account/email/confirm` 公开，但它的父路径不能因此被放行。
        assert!(!is_public_api_request(&Method::POST, "/api/account/email"));
    }

    /// 未登记的路由必须落到管理能力，也就是默认关闭。
    ///
    /// 这是整套授权最重要的一条。`route_capability` 返回的是 `Capability` 而不是
    /// `Option<Capability>`，所以「忘了登记」在类型上就不可能变成「公开」—— 新接口哪怕
    /// 一行没写，也只有管理员进得来。这条测试锁住这个兜底不被后人改成放行。
    #[test]
    fn unregistered_routes_default_to_closed() {
        for path in [
            "/api/something-brand-new",
            "/api/groups/123/unmapped-action",
            "/api/",
        ] {
            assert_eq!(
                route_capability(&Method::POST, path),
                Capability::ManageRuntime,
                "{path} 未登记时必须默认关闭"
            );
            assert!(
                requires_admin_role(&Method::POST, path),
                "{path} 未登记时不能对普通用户放行"
            );
        }
    }

    /// 能力不只是布尔：同为「管理接口」，用户管理、运维和可观测要能分开。
    ///
    /// 这是换掉布尔白名单的实际收益 —— Stage 7 的 owner 专属操作需要这个区分才能表达。
    #[test]
    fn management_routes_carry_distinct_capabilities() {
        for path in ["/api/logs/files", "/api/logs/files/content"] {
            assert_eq!(
                route_capability(&Method::GET, path),
                Capability::ViewObservability
            );
            assert!(!is_public_api_request(&Method::GET, path));
            assert!(requires_admin_role(&Method::GET, path));
        }
        assert_eq!(
            route_capability(&Method::POST, "/api/users"),
            Capability::ManageUsers
        );
        assert_eq!(
            route_capability(&Method::POST, "/api/database/backup"),
            Capability::ManageRuntime
        );
        assert_eq!(
            route_capability(&Method::GET, "/api/logs"),
            Capability::ViewObservability
        );
        assert_eq!(
            route_capability(&Method::GET, "/api/logs/settings"),
            Capability::ViewObservability
        );
        assert_eq!(
            route_capability(&Method::PATCH, "/api/logs/settings"),
            Capability::ManageRuntime
        );
    }

    /// 自助接口归属「管自己的账号」，不是任何管理能力。
    ///
    /// 把它们登记成管理能力会让普通用户被自己的账号页挡住 —— 这正是要修的那类错。
    #[test]
    fn personal_profile_write_uses_authenticated_self_service() {
        assert_eq!(
            route_capability(&Method::PUT, "/api/account/profile"),
            Capability::ManageOwnAccount
        );
        assert!(management_request_needs_session(
            false,
            &Method::PUT,
            "/api/account/profile",
            PluginWebAccessPolicy::ManageRuntime
        ));
        assert!(requires_admin_role(&Method::PUT, "/api/account/profile/2"));
    }

    #[test]
    fn self_service_routes_map_to_own_account_capability() {
        for (method, path) in [
            (Method::GET, "/api/account/profile"),
            (Method::POST, "/api/account/avatar"),
            (Method::POST, "/api/auth/password"),
        ] {
            assert_eq!(
                route_capability(&method, path),
                Capability::ManageOwnAccount,
                "{method} {path} 应归入自助能力"
            );
        }
    }

    /// `Capability::ALL` 必须真的是全集。
    ///
    /// 它只被穷举测试用，漏加一个变体不会导致编译失败，只会让那些测试假绿。这里用三个角色
    /// 的能力并集反查：任何角色持有但 `ALL` 里没有的能力，都是漏登记。
    #[test]
    fn capability_all_covers_every_granted_capability() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            for capability in role.capabilities() {
                assert!(
                    Capability::ALL.contains(capability),
                    "{capability} 已授予 {role} 却不在 Capability::ALL 中"
                );
            }
        }
    }

    /// 管理能力对 owner 必须全开，对普通用户必须全关。
    ///
    /// owner 少任何一项都意味着存在连所有者都做不了的运维动作；用户多任何一项就是越权。
    /// 这里只遍历管理能力而不是 `Capability::ALL`：资源能力（`ViewGroups`/`ManageGroups`）
    /// 三个角色都持有，它只决定接口可达，跨归属与否由 `Scope` 在 SQL 层决定。
    #[test]
    fn management_capabilities_are_owner_open_and_user_closed() {
        for capability in [
            Capability::ViewAllGroups,
            Capability::ManageAllGroups,
            Capability::ManageRuntime,
            Capability::ViewObservability,
            Capability::ManageUsers,
            Capability::AssignRoles,
        ] {
            assert!(Role::Owner.has(capability), "owner 必须持有 {capability}");
            assert!(!Role::User.has(capability), "普通用户不应持有 {capability}");
        }
    }

    /// 每个角色都必须能管自己的账号 —— 这是 orion 那个 bug 的根：用户连自己的资料都
    /// 得从管理接口进，措辞和权限自然都是管理员视角。
    #[test]
    fn every_role_can_manage_its_own_account() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            assert!(
                role.has(Capability::ManageOwnAccount),
                "{role} 应能管自己账号"
            );
        }
    }

    /// 分配角色是 owner 专属：admin 能建 admin 等于任一 admin 可无限自我复制。
    #[test]
    fn assigning_roles_is_owner_only() {
        assert!(Role::Owner.has(Capability::AssignRoles));
        assert!(!Role::Admin.has(Capability::AssignRoles));
        assert!(!Role::User.has(Capability::AssignRoles));
    }

    #[test]
    fn webhook_media_and_api_rate_limit_buckets_are_isolated_per_ip() {
        let security = nuono_core::config::SecurityConfig {
            rate_limit_per_minute: 2,
            webhook_rate_limit_per_minute: 3,
            ..Default::default()
        };

        let (ingress_key, ingress_limit) =
            super::rate_limit_bucket("/ingress/feishu/bot-1", "203.0.113.10", &security);
        let (legacy_key, legacy_limit) =
            super::rate_limit_bucket("/webhook/feishu", "203.0.113.10", &security);
        let (media_key, media_limit) =
            super::rate_limit_bucket("/media/token-abc", "203.0.113.10", &security);
        let (api_key, api_limit) =
            super::rate_limit_bucket("/api/status", "203.0.113.10", &security);

        assert_eq!(ingress_key, legacy_key);
        assert_eq!(ingress_limit, 3);
        assert_eq!(legacy_limit, 3);
        assert_eq!(media_limit, 3);
        assert_eq!(api_limit, 2);
        assert_ne!(ingress_key, api_key);
        assert_ne!(media_key, api_key);
        assert_ne!(ingress_key, media_key);

        let limiter = super::RateLimiter::new();
        for _ in 0..3 {
            assert!(limiter.check(&ingress_key, ingress_limit));
        }
        assert!(
            !limiter.check(&ingress_key, ingress_limit),
            "平台回调超过自身阈值后应被拦截"
        );
        assert!(
            limiter.check(&api_key, api_limit),
            "平台回调打满后不应阻断同 IP 的管理接口请求"
        );
        assert!(
            limiter.check(&media_key, media_limit),
            "平台回调打满后不应阻断同 IP 的签名媒体回源"
        );
    }
}
