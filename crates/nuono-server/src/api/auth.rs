//! Dashboard 账号认证与会话生命周期，对 owner/admin/user 三种角色一视同仁。
//!
//! 会话 token 只以 SHA-256 派生键持久化，浏览器侧使用 HttpOnly session cookie，并由
//! 安全中间件对状态变更请求执行 CSRF 校验。`Secure` 属性只依据该中间件从可信代理解析的
//! HTTPS 状态设置；直连请求伪造 `X-Forwarded-Proto` 不会影响 cookie。

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use nuono_core::auth::{CallerIdentity, Capability};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use utoipa_axum::{router::OpenApiRouter, routes};

static LOGIN_ATTEMPTS: LazyLock<dashmap::DashMap<IpAddr, LoginAttempt>> =
    LazyLock::new(dashmap::DashMap::new);
/// 按账号（规范化后的登录名）维度的失败计数，与 IP 维度并行生效。
///
/// 只按 IP 锁定时，攻击者轮换源 IP（或藏在大 NAT 后面）就能对同一账号持续慢速
/// 暴破；账号维度补上这条边。代价是知悉用户名的攻击者可故意把他人账号锁满
/// ban 时长，这是标准权衡：窗口短（默认 15 分钟）、成功登录即清零。
static ACCOUNT_LOGIN_ATTEMPTS: LazyLock<dashmap::DashMap<String, LoginAttempt>> =
    LazyLock::new(dashmap::DashMap::new);
const MAX_LOGIN_ATTEMPT_ENTRIES: usize = 10_000;
/// 登录名超出此长度不进账号锁定表：真实用户名/邮箱远到不了这个量级（RFC 5321
/// 邮箱上限 254），而没有上限的话攻击者可用超长随机名白嫖内存。
const MAX_ACCOUNT_LOCKOUT_KEY_LEN: usize = 256;

#[derive(Debug, Clone, Copy)]
struct LoginAttempt {
    failures: u32,
    first_failure: Instant,
    banned_until: Option<Instant>,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(login))
        .routes(routes!(status))
        .routes(routes!(logout))
        .routes(routes!(change_password))
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = api::auth::LoginRequest)]
pub struct LoginRequest {
    /// 用户名或邮箱。字段名保留 `username` 以兼容既有客户端。
    pub username: String,
    #[schema(write_only, format = Password)]
    pub password: String,
    /// 勾选「记住我」时会话保持 30 天，否则 7 天。旧客户端不带此字段视为未勾选。
    #[serde(default)]
    pub remember_me: bool,
    /// 两步验证码（或一次性恢复码）。账号未开启 TOTP 时忽略。
    #[serde(default)]
    pub totp_code: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::auth::LoginResponse)]
pub struct LoginResponse {
    /// 会话失效时间（RFC3339）。两步验证未通过时为空串。
    ///
    /// 不再返回会话 token：浏览器会话走 HttpOnly cookie，把同一凭据再放进 JSON 只会
    /// 让它暴露给前端脚本、客户端埋点和响应日志。需要 bearer token 的 API 客户端应
    /// 走单独的显式签发流程，而不是从登录响应里取。
    pub expires_at: String,
    /// 该账号已开启两步验证，但本次请求没有提供验证码：前端应提示输入。
    pub totp_required: bool,
    /// 验证码或恢复码错误（密码已通过）。
    pub totp_invalid: bool,
}

/// 会话保持时长：记住 30 天，默认 7 天。
///
/// 两个 cookie（session 与 csrf）必须使用同一 Max-Age，否则 CSRF cookie 先失效会让
/// 所有状态变更请求开始报 403，而用户看到的是「会话还有效」。
pub fn session_ttl(remember_me: bool) -> chrono::Duration {
    if remember_me {
        chrono::Duration::days(30)
    } else {
        chrono::Duration::days(7)
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::auth::AuthStatus)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub username: Option<String>,
    /// `owner` / `admin` / `user`，未登录时为 None。
    ///
    /// 前端靠它决定是否渲染管理入口。这只是界面收窄，不是权限边界：
    /// 真正的拦截是 `middleware::route_capability` 解析出的能力加 `CallerIdentity::has`，
    /// 改前端拿不到额外数据。
    pub role: Option<String>,
    /// 账号持有的能力标识（snake_case），前端据此渲染导航与路由守卫。
    ///
    /// 与 `role` 的分工：`role` 是粗粒度的「是不是员工」；能力列表是导航的
    /// 唯一判据 —— 前端每个入口登记一条必需能力，列表里没有就不渲染。
    /// 扩展能力合并到同一列表，供相应扩展的界面入口使用。
    pub capabilities: Vec<String>,
    /// 当前账号是否开启两步验证（设置页据此渲染「两步验证」面板状态）。
    pub totp_enabled: bool,
    /// 全站 AI 总开关状态（前端据此控制 AI 总控状态与二级导航解锁）。
    pub ai_enabled: bool,
}

impl AuthStatus {
    fn unauthenticated() -> Self {
        Self {
            authenticated: false,
            username: None,
            role: None,
            capabilities: Vec::new(),
            totp_enabled: false,
            ai_enabled: false,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::auth::ChangePasswordRequest)]
pub struct ChangePasswordRequest {
    #[schema(write_only, format = Password)]
    pub current_password: String,
    #[schema(write_only, format = Password)]
    pub new_password: String,
}

/// 由统一连接解析器得出的 HTTPS 状态决定 Cookie `Secure` 属性。
fn request_is_https(connection: Option<crate::middleware::ClientConnection>) -> bool {
    connection.is_some_and(|connection| connection.is_https)
}

/// 从请求的 Cookie 或 Authorization header 中提取 token
pub fn extract_token_from_request(req: &axum::extract::Request) -> Option<String> {
    extract_token_from_headers(req.headers())
}

pub fn extract_token_from_headers(headers: &HeaderMap) -> Option<String> {
    // 1. 优先从 Cookie 读取
    let cookie_token = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;

            if key == "session_token" {
                Some(value.to_string())
            } else {
                None
            }
        })
        .next();

    if cookie_token.is_some() {
        return cookie_token;
    }

    // 2. fallback 到 Authorization header（兼容旧客户端）
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.starts_with("Bearer "))
        .map(|s| s[7..].to_string())
}

/// 生成 256 bit 的凭据 token。
///
/// 会话与 CSRF token 都用它：显式使用 `SysRng`（rand 0.10 中由 `OsRng` 更名而来），
/// 让「这是密码学随机」在调用点可见，而不是依赖 `rand::random` 的默认实现细节。
fn new_credential_token(prefix: &str) -> String {
    use rand::TryRng;
    let mut bytes = [0u8; 32];
    // SysRng 直接取操作系统熵源。失败意味着系统熵不可用，此时不能退回弱随机来生成凭据。
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("操作系统熵源不可用，无法生成会话凭据");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{hex}")
}

fn session_key(token: &str) -> String {
    format!(
        "auth_session_{}",
        hex::encode(Sha256::digest(token.as_bytes()))
    )
}

async fn checked_session_account(
    db: &nuono_core::Database,
    token: &str,
) -> nuono_core::error::Result<Option<nuono_core::entity::users::Model>> {
    db.authenticate_session(token).await
}

async fn checked_session_token(
    db: &nuono_core::Database,
    token: &str,
) -> nuono_core::error::Result<Option<String>> {
    Ok(checked_session_account(db, token)
        .await?
        .map(|user| user.username))
}

/// 校验会话并解析出当前账号，供授权判断使用。
///
/// 角色不进会话载荷：会话是长期凭据，而 owner 随时可能降级某个账号的 role。若把角色缓存在
/// 会话里，降级要等到会话过期才生效。这里每次回查 `users` 表，让改权限即时起效。
///
/// 账号被停用（`status != active`）时一并拒绝，避免停用只挡登录、不挡已签发的会话。
pub async fn authenticated_user(
    db: &nuono_core::Database,
    token: &str,
) -> Option<nuono_core::entity::users::Model> {
    authenticated_user_checked(db, token).await.ok().flatten()
}
pub(crate) async fn authenticated_user_checked(
    db: &nuono_core::Database,
    token: &str,
) -> nuono_core::error::Result<Option<nuono_core::entity::users::Model>> {
    checked_session_account(db, token).await
}

/// 校验会话并要求某项能力，供 `/api` 之外的运维端点使用。
///
/// `/metrics` 和日志 WebSocket 不走 `security_middleware`（它只挂在 `/api` 上），所以这两处
/// 必须自己判权限。它们此前只调 `validate_session_token`，而那个函数只返回用户名 —— 任何
/// `role=user` 的会话都能读到全量服务器日志和 Prometheus 指标。
///
/// 判据是能力而非「是不是管理员」，与 `security_middleware` 的路由授权共用同一套词汇：
/// 将来把可观测数据部分开放给客户，只需要改 [`nuono_core::auth::Role::capabilities`] 一处。
pub async fn authenticated_with(
    db: &nuono_core::Database,
    token: &str,
    capability: Capability,
) -> Option<nuono_core::entity::users::Model> {
    let user = authenticated_user(db, token).await?;
    let caller = CallerIdentity::from_user(&user);
    if !caller.has(capability) {
        metrics::counter!(
            "nuono_authz_denied_total",
            "role" => caller.role.as_str(),
            "capability" => capability.as_str(),
        )
        .increment(1);
        tracing::info!(
            username = %caller.username,
            role = %caller.role,
            capability = %capability,
            "拒绝越权访问运维端点"
        );
        return None;
    }
    Some(user)
}

fn attempt_is_blocked<K: std::hash::Hash + Eq>(
    map: &dashmap::DashMap<K, LoginAttempt>,
    key: &K,
) -> bool {
    let now = Instant::now();
    if let Some(attempt) = map.get(key) {
        return attempt
            .banned_until
            .is_some_and(|banned_until| banned_until > now);
    }
    false
}

fn login_is_blocked(ip: IpAddr) -> bool {
    attempt_is_blocked(&LOGIN_ATTEMPTS, &ip)
}

fn account_login_is_blocked(account: &str) -> bool {
    attempt_is_blocked(&ACCOUNT_LOGIN_ATTEMPTS, &account.to_owned())
}

/// 账号锁定键：提交的登录名 trim + 小写，让 `Admin`/`admin`/邮箱大小写落到同一桶。
fn account_lockout_key(username: &str) -> String {
    username.trim().to_lowercase()
}

/// 登录失败表的容量兜底：优先淘汰未封禁且最旧的条目。
///
/// 不能整体 `clear()`：那会连同生效中的 ban 一起清掉，攻击者只要让失败记录
/// 来自足够多的 IP（IPv6 部署或代理追加式 XFF 下都不难），就能随时重置整张表
/// ——包括自己的锁定 —— 随后继续暴破。排序把未封禁条目排在前面（`false < true`），
/// 淘汰顺序因此是「未封禁的最旧 → 封禁中最旧」，刚生效的 ban 最后才会被触及。
fn evict_oldest_login_attempts<K: std::hash::Hash + Eq + Clone>(
    map: &dashmap::DashMap<K, LoginAttempt>,
) {
    let mut order: Vec<(K, (bool, Instant))> = map
        .iter()
        .map(|entry| {
            (
                entry.key().clone(),
                (
                    entry.value().banned_until.is_some(),
                    entry.value().first_failure,
                ),
            )
        })
        .collect();
    order.sort_by_key(|&(_, key)| key);
    let keep = MAX_LOGIN_ATTEMPT_ENTRIES - MAX_LOGIN_ATTEMPT_ENTRIES / 10;
    let evict = order.len().saturating_sub(keep);
    for (key, _) in order.into_iter().take(evict) {
        map.remove(&key);
    }
}

fn record_attempt_failure<K: std::hash::Hash + Eq + Clone>(
    map: &dashmap::DashMap<K, LoginAttempt>,
    key: K,
    max_failures: u32,
    ban_minutes: u32,
) {
    let now = Instant::now();
    map.retain(|_, attempt| {
        attempt.banned_until.is_some_and(|until| until > now)
            || now.duration_since(attempt.first_failure) < Duration::from_secs(3600)
    });
    if map.len() >= MAX_LOGIN_ATTEMPT_ENTRIES {
        evict_oldest_login_attempts(map);
    }

    let mut attempt = map.entry(key).or_insert(LoginAttempt {
        failures: 0,
        first_failure: now,
        banned_until: None,
    });
    if now.duration_since(attempt.first_failure) >= Duration::from_secs(3600) {
        attempt.failures = 0;
        attempt.first_failure = now;
        attempt.banned_until = None;
    }
    attempt.failures = attempt.failures.saturating_add(1);
    if attempt.failures >= max_failures.max(1) {
        attempt.banned_until =
            Some(now + Duration::from_secs(u64::from(ban_minutes.max(1)).saturating_mul(60)));
    }
}

fn record_login_failure(ip: IpAddr, max_failures: u32, ban_minutes: u32) {
    record_attempt_failure(&LOGIN_ATTEMPTS, ip, max_failures, ban_minutes);
}

/// 失败同时计入账号维度。不存在的账号名也照记（校验层无法区分「不存在」与
/// 「密码错」），垃圾名的量由表容量兜底；过长的不可能对应真实账号，直接跳过。
fn record_account_login_failure(account: &str, max_failures: u32, ban_minutes: u32) {
    if account.len() <= MAX_ACCOUNT_LOCKOUT_KEY_LEN {
        record_attempt_failure(
            &ACCOUNT_LOGIN_ATTEMPTS,
            account.to_owned(),
            max_failures,
            ban_minutes,
        );
    }
}

/// 审计载荷里的登录名按此截断：它来自未验证的请求体，不设上限等于允许攻击者
/// 借失败登录向 event_logs 灌任意长度的字符串。
fn audit_username(username: &str) -> String {
    username.chars().take(128).collect()
}

/// POST /auth/login
#[utoipa::path(
    post,
    path = "/auth/login",
    operation_id = "auth_login",
    tag = "auth",
    request_body = LoginRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<LoginResponse>))
)]
async fn login(
    State(state): State<AppState>,
    axum::Extension(connection): axum::Extension<crate::middleware::ClientConnection>,
    Json(req): Json<LoginRequest>,
) -> Response {
    // 两个维度独立判锁：IP 维度挡单点暴破，账号维度挡换 IP 的慢速暴破。
    // 响应保持同一文案，不泄露是被哪个维度拦下的。
    let account_key = account_lockout_key(&req.username);
    if login_is_blocked(connection.ip) || account_login_is_blocked(&account_key) {
        metrics::counter!("nuono_auth_login_rejected_total", "reason" => "banned").increment(1);
        let body = ApiResponse::<LoginResponse>::bad_request("登录失败次数过多，请稍后再试");
        let mut response = body.into_response();
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        return response;
    }

    // 凭据来自 users 表，标识可以是用户名或邮箱。账号不存在与密码错误返回同一响应，
    // 避免账号枚举。
    let verified = state
        .db
        .verify_user_password(&req.username, &req.password)
        .await;

    let user = match verified {
        Ok(Some(user)) => user,
        Ok(None) => {
            let cfg = state.config.get().await;
            record_login_failure(
                connection.ip,
                cfg.security.max_failed_attempts,
                cfg.security.ban_duration_minutes,
            );
            record_account_login_failure(
                &account_key,
                cfg.security.max_failed_attempts,
                cfg.security.ban_duration_minutes,
            );
            metrics::counter!("nuono_auth_login_rejected_total", "reason" => "credentials")
                .increment(1);
            // actor 留空：登录失败时没有任何已验证身份，提交的登录名只作为
            // （截断后的）数据字段记录，不冒充真实 actor。
            let _ = nuono_core::audit::record_event(
                &state.db,
                "auth.login_failed",
                None,
                None,
                serde_json::json!({
                    "username": audit_username(&account_key),
                    "ip": connection.ip.to_string(),
                    "reason": "credentials",
                }),
            )
            .await;
            return unauthorized_login();
        }
        Err(error) => {
            return password_operation_error::<LoginResponse>(
                &error,
                "登录时校验凭据失败",
                "暂时无法登录，请稍后再试",
            );
        }
    };

    // 两步验证：密码通过后、签发会话前。开启 TOTP 的账号缺码 → totp_required（不算
    // 失败次数，还没开始试码）；码错/恢复码错 → 计入同一 IP 失败桶（与密码错误共享
    // 锁定窗口），返回 totp_invalid。
    if user.totp_enabled {
        let now = chrono::Utc::now().timestamp() as u64;
        let Some(code) = req
            .totp_code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
        else {
            return totp_required_response();
        };

        // 验证码通过后立刻消费时间步（重放保护，RFC 6238 §5.2）：同一个码在
        // 窗口内第二次出现时 consume_totp_step 返回 false，按验证码错误处理，
        // 防止实时钓鱼代理拿到码后立刻重放登录。
        let code_ok = match nuono_core::auth::validate_code_counter(
            user.totp_secret.as_deref().unwrap_or_default(),
            code,
            now,
        ) {
            Some(step) => match state.db.consume_totp_step(user.id, step).await {
                Ok(consumed) => consumed,
                Err(e) => {
                    tracing::warn!(error = %e, "TOTP 时间步消费失败");
                    false
                }
            },
            None => false,
        };
        let recovery_ok = if code_ok {
            false
        } else {
            // 恢复码是一次性的：命中即从列表删除。
            let hash = nuono_core::auth::recovery_code_hash(code);
            match state.db.consume_recovery_code(user.id, &hash).await {
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    tracing::warn!(error = %e, "恢复码消费失败");
                    false
                }
            }
        };

        if !code_ok && !recovery_ok {
            let cfg = state.config.get().await;
            record_login_failure(
                connection.ip,
                cfg.security.max_failed_attempts,
                cfg.security.ban_duration_minutes,
            );
            record_account_login_failure(
                &account_key,
                cfg.security.max_failed_attempts,
                cfg.security.ban_duration_minutes,
            );
            metrics::counter!("nuono_auth_login_rejected_total", "reason" => "totp").increment(1);
            let _ = nuono_core::audit::record_event(
                &state.db,
                "auth.login_failed",
                None,
                None,
                serde_json::json!({
                    "username": audit_username(&account_key),
                    "ip": connection.ip.to_string(),
                    "reason": "totp",
                }),
            )
            .await;
            return totp_invalid_response();
        }

        if recovery_ok {
            let _ = nuono_core::audit::record_event(
                &state.db,
                "account.totp_recovery_used",
                None,
                Some(user.username.as_str()),
                serde_json::json!({"user_id": user.id.to_string()}),
            )
            .await;
        }
    }

    {
        let token = new_credential_token("nuono");
        let ttl = session_ttl(req.remember_me);
        let expires = chrono::Utc::now() + ttl;
        let expires_str = expires.to_rfc3339();
        let max_age = ttl.num_seconds();

        match state
            .db
            .create_session_for_verified_user(&user, &session_key(&token), expires)
            .await
        {
            Ok(true) => {}
            Ok(false) => return unauthorized_login(),
            Err(error) => {
                tracing::error!(error = %error, "登录时创建会话失败");
                let body = ApiResponse::<LoginResponse>::bad_request("登录未完成，请稍后再试");
                let mut response = body.into_response();
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                return response;
            }
        }
        LOGIN_ATTEMPTS.remove(&connection.ip);
        ACCOUNT_LOGIN_ATTEMPTS.remove(&account_key);
        let _ = nuono_core::audit::record_event(
            &state.db,
            "auth.login_succeeded",
            None,
            Some(user.username.as_str()),
            serde_json::json!({
                "user_id": user.id.to_string(),
                "ip": connection.ip.to_string(),
            }),
        )
        .await;

        let secure = if request_is_https(Some(connection)) {
            "; Secure"
        } else {
            ""
        };

        let session_cookie = format!(
            "session_token={}; HttpOnly; SameSite=Lax{}; Max-Age={}; Path=/",
            token, secure, max_age
        );
        // 设置 CSRF Token Cookie（非 HttpOnly，前端可读）
        let csrf_token = new_credential_token("csrf");
        let csrf_cookie = format!(
            "csrf_token={}; SameSite=Lax{}; Max-Age={}; Path=/",
            csrf_token, secure, max_age
        );

        let body = ApiResponse::ok(LoginResponse {
            expires_at: expires_str,
            totp_required: false,
            totp_invalid: false,
        });
        let mut res = body.into_response();
        res.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&session_cookie).unwrap(),
        );
        res.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&csrf_cookie).unwrap(),
        );
        res
    }
}

fn unauthorized_login() -> Response {
    let body = ApiResponse::<LoginResponse>::bad_request("用户名或密码错误");
    let mut res = body.into_response();
    *res.status_mut() = StatusCode::UNAUTHORIZED;
    res
}

fn password_operation_error<T: Serialize>(
    error: &nuono_core::NuonoError,
    context: &str,
    message: &str,
) -> Response {
    if matches!(error, nuono_core::NuonoError::Overloaded(_)) {
        metrics::counter!("nuono_auth_password_rejected_total", "reason" => "full").increment(1);
        return ApiResponse::<T>::from_err(error).into_response();
    }
    tracing::error!(%error, context, "密码操作失败");
    crate::api::ApiReply::<T>::error(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
}

/// 密码正确但需要两步验证码。401 状态码但 body 走 ok 载荷：前端按 `totp_required`
/// 决定是展示验证码输入还是跳登录失败提示。
fn totp_required_response() -> Response {
    let body = ApiResponse::ok(LoginResponse {
        expires_at: String::new(),
        totp_required: true,
        totp_invalid: false,
    });
    let mut res = body.into_response();
    *res.status_mut() = StatusCode::UNAUTHORIZED;
    res
}

/// 两步验证码或恢复码错误。
fn totp_invalid_response() -> Response {
    let body = ApiResponse::ok(LoginResponse {
        expires_at: String::new(),
        totp_required: false,
        totp_invalid: true,
    });
    let mut res = body.into_response();
    *res.status_mut() = StatusCode::UNAUTHORIZED;
    res
}

/// GET /auth/status
#[utoipa::path(
    get,
    path = "/auth/status",
    operation_id = "auth_status",
    tag = "auth",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<AuthStatus>))
)]
async fn status(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> crate::api::ApiReply<AuthStatus> {
    let Some(token) = extract_token_from_request(&req) else {
        return ApiResponse::ok(AuthStatus::unauthenticated());
    };

    // 走 authenticated_user 而非 validate_session_token：前者回查 users 表拿到 role，
    // 并且顺带拒掉已停用账号，让状态查询和授权中间件对「谁算已登录」保持同一判断。
    match authenticated_user_checked(&state.db, &token).await {
        Ok(Some(user)) => {
            let role = nuono_core::auth::Role::from_db_value(&user.role);
            let ai_enabled = state.config.snapshot().await.ai.enabled;
            ApiResponse::ok(AuthStatus {
                authenticated: true,
                username: Some(user.username),
                role: Some(user.role),
                capabilities: capabilities_for(role),
                totp_enabled: user.totp_enabled,
                ai_enabled,
            })
        }
        Ok(None) => ApiResponse::ok(AuthStatus::unauthenticated()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

/// 该角色本次会话持有的全部能力标识：框架能力在前，扩展能力在后。
///
/// 顺序固定下来是为了让前端和排查都能按位置比对 —— 扩展能力的注入不该改变框架
/// 那些条目的相对次序。
fn capabilities_for(role: nuono_core::auth::Role) -> Vec<String> {
    let mut capabilities: Vec<String> = role
        .capabilities()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    capabilities.extend(
        nuono_core::auth::extension_capabilities(role)
            .into_iter()
            .map(str::to_string),
    );
    capabilities
}

/// POST /auth/logout
#[utoipa::path(
    post,
    path = "/auth/logout",
    operation_id = "auth_logout",
    tag = "auth",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn logout(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    // 从 Cookie 或 Header 读取 token，删除对应的 session
    if let Some(token) = extract_token_from_request(&req)
        && let Err(error) = state.db.revoke_session_token(&token).await
    {
        tracing::error!(%error, "撤销登录会话失败");
        let mut response =
            ApiResponse::<()>::bad_request("暂时无法退出登录，请稍后再试").into_response();
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        return response;
    }

    // 清除浏览器 Cookie（session + csrf）
    let clear_session = "session_token=; HttpOnly; SameSite=Lax; Max-Age=0; Path=/";
    let clear_csrf = "csrf_token=; SameSite=Lax; Max-Age=0; Path=/";
    let body = ApiResponse::ok(());
    let mut res = body.into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(clear_session).unwrap(),
    );
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(clear_csrf).unwrap(),
    );
    res
}

/// POST /auth/password
#[utoipa::path(
    post,
    path = "/auth/password",
    operation_id = "auth_change_password",
    tag = "auth",
    request_body = ChangePasswordRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChangePasswordRequest>,
) -> Response {
    let token = extract_token_from_headers(&headers);

    if let Err(message) = crate::api::setup::validate_password_strength(&payload.new_password) {
        return bad_password_request(message);
    }
    if payload.new_password == payload.current_password {
        return bad_password_request("新密码不能与当前密码相同");
    }

    // 当前会话决定改谁的密码，不允许通过请求体指定用户。
    let Some(ref session_token) = token else {
        return ApiResponse::<()>::unauthenticated("请先登录再修改密码").into_response();
    };
    let username = match checked_session_token(&state.db, session_token).await {
        Ok(Some(username)) => username,
        Ok(None) => {
            return ApiResponse::<()>::unauthenticated("登录状态已过期，请重新登录")
                .into_response();
        }
        Err(error) => return ApiResponse::<()>::from_err(&error).into_response(),
    };

    let user = match state
        .db
        .verify_user_password(&username, &payload.current_password)
        .await
    {
        Ok(Some(user)) => user,
        Ok(None) => return bad_password_request("当前密码不正确"),
        Err(error) => {
            return password_operation_error::<()>(
                &error,
                "修改密码时校验当前密码失败",
                "暂时无法修改密码，请稍后再试",
            );
        }
    };

    match state
        .db
        .change_password_for_verified_user(
            &user,
            &session_key(session_token),
            &payload.new_password,
        )
        .await
    {
        Ok(Some(count)) => {
            tracing::info!(username = %username, count, "密码已修改，撤销该账号全部会话");
        }
        Ok(None) => {
            return bad_password_request("账号或登录状态已变化，请重新登录后再修改密码");
        }
        Err(error) => {
            return password_operation_error::<()>(
                &error,
                "修改密码事务失败",
                "暂时无法修改密码，请稍后再试",
            );
        }
    }

    let clear_session = "session_token=; HttpOnly; SameSite=Lax; Max-Age=0; Path=/";
    let clear_csrf = "csrf_token=; SameSite=Lax; Max-Age=0; Path=/";
    let body = ApiResponse::ok(());
    let mut res = body.into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(clear_session).unwrap(),
    );
    res.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(clear_csrf).unwrap(),
    );
    res
}

fn bad_password_request(message: impl Into<String>) -> Response {
    let body = ApiResponse::<()>::bad_request(message);
    let mut res = body.into_response();
    *res.status_mut() = StatusCode::BAD_REQUEST;
    res
}

#[cfg(test)]
mod tests {
    use super::{
        ACCOUNT_LOGIN_ATTEMPTS, MAX_ACCOUNT_LOCKOUT_KEY_LEN, account_lockout_key,
        account_login_is_blocked, audit_username, authenticated_user, authenticated_with,
        record_account_login_failure, session_key, session_ttl,
    };
    use nuono_core::Database;
    use nuono_core::auth::Capability;

    #[test]
    fn account_lockout_blocks_across_ips_and_ignores_oversized_names() {
        ACCOUNT_LOGIN_ATTEMPTS.clear();
        let key = account_lockout_key("  Victim@Example.COM ");
        assert_eq!(key, "victim@example.com", "登录名归一化到同一锁定桶");

        // 连续失败达到阈值即封账号——锁定挂在账号上，换源 IP 拦不住的慢速暴破
        // 也会在这里被拦。
        for _ in 0..5 {
            record_account_login_failure(&key, 5, 15);
        }
        assert!(account_login_is_blocked(&key));
        assert!(!account_login_is_blocked("bystander"));

        // 成功登录清零。
        ACCOUNT_LOGIN_ATTEMPTS.remove(&key);
        assert!(!account_login_is_blocked(&key));

        // 超长登录名不可能对应真实账号，不进表：防止攻击者借垃圾名灌内存。
        let oversized = "a".repeat(MAX_ACCOUNT_LOCKOUT_KEY_LEN + 1);
        for _ in 0..10 {
            record_account_login_failure(&oversized, 5, 15);
        }
        assert!(!account_login_is_blocked(&oversized.to_lowercase()));
        assert!(ACCOUNT_LOGIN_ATTEMPTS.is_empty());
    }

    #[test]
    fn audit_username_truncates_attacker_controlled_input() {
        assert_eq!(audit_username(&"x".repeat(300)).chars().count(), 128);
        assert_eq!(audit_username("正常名"), "正常名");
    }

    use nuono_core::db::{
        NewUser, ROLE_ADMIN, ROLE_OWNER, ROLE_USER, STATUS_ACTIVE, STATUS_DISABLED,
    };

    #[tokio::test]
    async fn password_work_overload_is_retryable_and_does_not_become_bad_credentials() {
        let error = nuono_core::NuonoError::Overloaded("internal-work-capacity-detail".into());
        for response in [
            super::password_operation_error::<super::LoginResponse>(
                &error,
                "login",
                "暂时无法登录，请稍后再试",
            ),
            super::password_operation_error::<()>(
                &error,
                "change_password",
                "暂时无法修改密码，请稍后再试",
            ),
        ] {
            assert_eq!(
                response.status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["success"], false);
            assert!(body["data"].is_null());
            assert!(!body.to_string().contains("internal-work-capacity-detail"));
            assert!(!body.to_string().contains("密码不正确"));
        }
        assert_eq!(
            super::unauthorized_login().status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn password_work_internal_failure_keeps_generic_server_error() {
        let response = super::password_operation_error::<super::LoginResponse>(
            &nuono_core::NuonoError::Internal("sensitive-worker-failure".into()),
            "login",
            "暂时无法登录，请稍后再试",
        );
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(
            !String::from_utf8(bytes.to_vec())
                .unwrap()
                .contains("sensitive-worker-failure")
        );
    }

    #[tokio::test]
    async fn totp_challenges_preserve_the_login_protocol() {
        for (response, flag) in [
            (super::totp_required_response(), "totp_required"),
            (super::totp_invalid_response(), "totp_invalid"),
        ] {
            assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["success"], true);
            assert_eq!(body["data"][flag], true);
        }
    }

    #[test]
    fn session_ttl_honours_remember_me() {
        assert_eq!(session_ttl(true), chrono::Duration::days(30));
        assert_eq!(session_ttl(false), chrono::Duration::days(7));
    }

    async fn test_db() -> Database {
        nuono_core::test_support::test_db(2).await
    }

    /// 建号并直接写一条有效会话，返回会话令牌。
    async fn user_with_session(db: &Database, username: &str, role: &str, status: &str) -> String {
        let user = db
            .create_user(NewUser {
                username: username.to_string(),
                email: Some(format!("{username}@example.com")),
                password: "correct-horse-battery".to_string(),
                role: role.to_string(),
                status: STATUS_ACTIVE.to_string(),
                email_verified: true,
            })
            .await
            .unwrap();

        let token = format!("nuono_test_{username}");
        assert!(
            db.create_session_for_verified_user(
                &user,
                &session_key(&token),
                chrono::Utc::now() + chrono::Duration::days(1)
            )
            .await
            .unwrap()
        );
        if status != STATUS_ACTIVE {
            use sea_orm::{EntityTrait, Set};
            nuono_core::entity::users::orm::Entity::update(
                nuono_core::entity::users::orm::ActiveModel {
                    id: Set(user.id),
                    status: Set(status.into()),
                    ..Default::default()
                },
            )
            .exec(db.orm())
            .await
            .unwrap();
        }
        token
    }

    /// `/metrics` 和日志 WebSocket 的授权闸门：普通用户必须被挡在外面。
    ///
    /// 这两处在 `/api` 之外，`security_middleware` 覆盖不到，只能靠这个函数把关。
    #[tokio::test]
    async fn admin_gate_admits_owner_and_admin_but_rejects_plain_user() {
        let db = test_db().await;
        let owner = user_with_session(&db, "gate_owner", ROLE_OWNER, STATUS_ACTIVE).await;
        let admin = user_with_session(&db, "gate_admin", ROLE_ADMIN, STATUS_ACTIVE).await;
        let plain = user_with_session(&db, "gate_user", ROLE_USER, STATUS_ACTIVE).await;

        assert!(
            authenticated_with(&db, &owner, Capability::ViewObservability)
                .await
                .is_some()
        );
        assert!(
            authenticated_with(&db, &admin, Capability::ViewObservability)
                .await
                .is_some()
        );
        assert!(
            authenticated_with(&db, &plain, Capability::ViewObservability)
                .await
                .is_none(),
            "role=user 不能读全量日志和指标"
        );

        // 普通用户仍然是合法登录用户，只是不具备运维端点的权限。
        assert!(authenticated_user(&db, &plain).await.is_some());
    }

    /// 停用账号即时失效，而不是等会话自然过期。
    #[tokio::test]
    async fn admin_gate_rejects_disabled_admin() {
        let db = test_db().await;
        let token = user_with_session(&db, "gate_off", ROLE_ADMIN, STATUS_DISABLED).await;

        assert!(
            authenticated_with(&db, &token, Capability::ViewObservability)
                .await
                .is_none()
        );
        assert!(authenticated_user(&db, &token).await.is_none());
    }

    #[tokio::test]
    async fn admin_gate_rejects_unknown_token() {
        let db = test_db().await;
        assert!(
            authenticated_with(&db, "nuono_not_a_session", Capability::ViewObservability)
                .await
                .is_none()
        );
    }
}
