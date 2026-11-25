use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
};
use nuono_app::image_output::{ImageOutputFormat, ImageOutputSettings};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(super::database_settings::router())
        .merge(super::message_interactions_settings::router())
        .routes(routes!(get_config))
        .routes(routes!(update_config))
        .routes(routes!(list_secrets))
        .routes(routes!(set_secret))
        .routes(routes!(delete_secret))
        .routes(routes!(send_test_mail))
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::ConfigResponse)]
pub struct ConfigResponse {
    pub server_host: String,
    pub server_port: u16,
    pub db_url: String,
    pub upstream_url: String,
    pub log_level: String,
    pub ws_token_configured: bool,
    pub rate_limit_per_minute: u32,
    pub max_failed_attempts: u32,
    pub ban_duration_minutes: u32,
    pub admin_auth_enabled: bool,
    pub subscription_enabled: bool,
    /// 聊天语言系统默认值：zh-CN、en、ja、ko。与浏览器界面语言独立。
    pub default_locale: String,
    pub image_output: ImageOutputConfigResponse,
    #[schema(value_type = Object)]
    pub message_interactions: nuono_app::message_interactions::MessageInteractionSettings,
    pub message_interactions_revision: String,
    pub mail: MailConfigResponse,
}

/// 生成图片的编码设置；不影响原作品、头像或其他上传素材。
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::ImageOutputConfigResponse)]
pub struct ImageOutputConfigResponse {
    /// png 或 jpeg。
    #[schema(value_type = String, example = "png")]
    pub format: ImageOutputFormat,
    #[schema(minimum = 1, maximum = 100, example = 80)]
    pub jpeg_quality: u8,
}

impl From<ImageOutputSettings> for ImageOutputConfigResponse {
    fn from(settings: ImageOutputSettings) -> Self {
        Self {
            format: settings.format,
            jpeg_quality: settings.jpeg_quality,
        }
    }
}

/// 邮件配置的可回显部分。
///
/// `smtp_password` 是 secret，只回显 `smtp_password_configured` 表示当前是否有非空凭据，
/// 与 `ws_token_configured` 同一处理方式——管理面不应成为读取 secret 的旁路。
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::MailConfigResponse)]
pub struct MailConfigResponse {
    pub enabled: bool,
    pub registration_enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password_configured: bool,
    pub from_address: String,
    pub from_name: String,
    pub encryption: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub verify_token_ttl_minutes: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub reset_token_ttl_minutes: i64,
}

/// 打码数据库 URL 中的口令段。
///
/// `db_url` 通常内嵌口令（`postgres://user:pass@host/db`），与 `ws_token`/`smtp_password`
/// 普通运行设置响应保持打码；管理员专用数据库编辑端点回显完整连接地址。
/// `update_config` 跳过含 `****` 的回传值，避免把打码串存回去。
fn mask_db_url(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        if parsed.password().is_some_and(|p| !p.is_empty()) {
            let _ = parsed.set_password(Some("****"));
        }
        return parsed.to_string();
    }
    // url crate 解析不了的写法退化为手工打码 authority 中的口令段。
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let Some((user, _password)) = userinfo.split_once(':') else {
        return url.to_string();
    };
    format!("{scheme}://{user}:****@{host}{}", &rest[authority_end..])
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "system_config_get_config",
    tag = "system_config",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ConfigResponse>))
)]
async fn get_config(State(state): State<AppState>) -> crate::api::ApiReply<ConfigResponse> {
    let cfg = state.config.get().await;

    let message_interactions_revision =
        match nuono_core::config::message_interactions_revision(&cfg.message_interactions) {
            Ok(revision) => revision,
            Err(error) => return ApiResponse::from_err(&error),
        };

    ApiResponse::ok(ConfigResponse {
        server_host: cfg.host,
        server_port: cfg.port,
        db_url: mask_db_url(&cfg.database.url),
        upstream_url: cfg.upstream.default_url,
        log_level: cfg.log.level,
        ws_token_configured: !cfg.security.ws_token.is_empty(),
        rate_limit_per_minute: cfg.security.rate_limit_per_minute,
        max_failed_attempts: cfg.security.max_failed_attempts,
        ban_duration_minutes: cfg.security.ban_duration_minutes,
        admin_auth_enabled: cfg.security.admin_auth_enabled,
        subscription_enabled: cfg.subscription.enabled,
        default_locale: cfg.localization.default_locale.as_str().to_owned(),
        image_output: cfg.image_output.into(),
        message_interactions: cfg.message_interactions,
        message_interactions_revision,
        mail: MailConfigResponse {
            enabled: cfg.mail.enabled,
            registration_enabled: cfg.mail.registration_enabled,
            smtp_host: cfg.mail.smtp_host,
            smtp_port: cfg.mail.smtp_port,
            smtp_username: cfg.mail.smtp_username,
            smtp_password_configured: cfg
                .mail
                .smtp_password
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            from_address: cfg.mail.from_address,
            from_name: cfg.mail.from_name,
            encryption: cfg.mail.encryption,
            verify_token_ttl_minutes: cfg.mail.verify_token_ttl_minutes,
            reset_token_ttl_minutes: cfg.mail.reset_token_ttl_minutes,
        },
    })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::system_config::UpdateConfigPayload)]
pub struct UpdateConfigPayload {
    pub server_host: Option<String>,
    pub server_port: Option<u16>,
    pub db_url: Option<String>,
    pub upstream_url: Option<String>,
    pub log_level: Option<String>,
    pub ws_token: Option<String>,
    pub rate_limit_per_minute: Option<u32>,
    pub admin_auth_enabled: Option<bool>,
    pub subscription_enabled: Option<bool>,
    pub default_locale: Option<String>,
    pub image_output: Option<ImageOutputPatch>,
    pub message_interactions: Option<super::message_interactions_settings::MessageInteractionPatch>,
    pub mail: Option<UpdateMailPayload>,
}

/// 生成图片设置的显式补丁，省略的子字段保持原值。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::system_config::ImageOutputPatch)]
pub struct ImageOutputPatch {
    /// png 或 jpeg。
    #[schema(value_type = Option<String>, example = "jpeg")]
    pub format: Option<ImageOutputFormat>,
    #[schema(minimum = 1, maximum = 100, example = 80)]
    pub jpeg_quality: Option<u8>,
}

impl ImageOutputPatch {
    fn into_settings(
        self,
        current: ImageOutputSettings,
    ) -> std::result::Result<serde_json::Value, String> {
        let mut target = current;
        let mut settings = serde_json::Map::new();
        if let Some(format) = self.format {
            target.format = format;
            settings.insert("format".into(), serde_json::json!(format));
        }
        if let Some(quality) = self.jpeg_quality {
            target.jpeg_quality = quality;
            settings.insert("jpeg_quality".into(), serde_json::json!(quality));
        }
        target.validate().map_err(|error| error.to_string())?;
        Ok(serde_json::Value::Object(settings))
    }
}

/// 邮件配置的可写字段。全部可选，未提供的字段保持原值。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::UpdateMailPayload)]
pub struct UpdateMailPayload {
    pub enabled: Option<bool>,
    pub registration_enabled: Option<bool>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_username: Option<String>,
    /// 明文 SMTP 密码。走 `runtime_secrets`，不落盘 TOML。空串表示清除。
    #[schema(write_only, format = Password)]
    pub smtp_password: Option<String>,
    pub from_address: Option<String>,
    pub from_name: Option<String>,
    pub encryption: Option<String>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub verify_token_ttl_minutes: Option<i64>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub reset_token_ttl_minutes: Option<i64>,
}

/// 传输加密方式白名单。与 `service::mail` 的实现分支一致。
const MAIL_ENCRYPTIONS: &[&str] = &["starttls", "tls", "none"];

/// 校验邮件配置的自洽性。
///
/// 关键约束是 `enabled = true` 时必须有 host 和 from_address：缺了它们 `mail::send`
/// 每次都会失败，而失败点在用户注册时才暴露，排查成本远高于在这里拒绝保存。
fn validate_mail(mail: &nuono_core::config::MailConfig) -> std::result::Result<(), String> {
    if !MAIL_ENCRYPTIONS.contains(&mail.encryption.as_str()) {
        return Err("加密方式请选择 TLS、STARTTLS 或不加密".to_string());
    }
    if mail.smtp_port == 0 {
        return Err("发信服务器端口无效".to_string());
    }
    if !mail.enabled {
        return Ok(());
    }
    if mail.smtp_host.trim().is_empty() {
        return Err("开启邮件功能前请填写发信服务器地址".to_string());
    }
    if mail.from_address.trim().is_empty() {
        return Err("开启邮件功能前请填写发件邮箱地址".to_string());
    }
    crate::api::setup::validate_email(mail.from_address.trim())?;
    if mail.verify_token_ttl_minutes <= 0 || mail.reset_token_ttl_minutes <= 0 {
        return Err("链接有效期至少为 1 分钟".to_string());
    }
    Ok(())
}

fn config_error(message: impl Into<String>) -> super::ApiReply<()> {
    ApiResponse::bad_request(message)
}

fn message_revision_header(headers: &HeaderMap) -> Result<String, String> {
    let mut values = headers.get_all(axum::http::header::IF_MATCH).iter();
    let value = values
        .next()
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'))
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        });
    match value {
        Some(value) if values.next().is_none() => Ok(value.to_string()),
        _ => Err("消息与交互设置需要读取时的版本号（If-Match）；请先刷新该页后重试。".into()),
    }
}

#[utoipa::path(
    method(post, patch),
    path = "/",
    operation_id = "system_config_update_config",
    tag = "system_config",
    request_body = UpdateConfigPayload,
    params(("If-Match" = Option<String>, Header, description = "提交 message_interactions 时必填，值为 GET 返回的子树版本，使用双引号包围。普通运行设置不要求此版本。")),
    responses(
        (status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>),
        (status = 409, description = "消息与交互版本已变化，未覆盖服务器；重新读取后确认修改", body = ApiResponse<super::docs::Empty>)
    )
)]
async fn update_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    super::input::ApiJson(body): super::input::ApiJson<UpdateConfigPayload>,
) -> super::ApiReply<()> {
    let cfg = state.config.get().await;
    let new_log_level = body.log_level.clone();
    let rich_revision = if body.message_interactions.is_some() {
        match message_revision_header(&headers) {
            Ok(revision) => Some(revision),
            Err(error) => return config_error(error),
        }
    } else {
        None
    };

    // 启动字段不进入数据库运行设置；请求里携带时仅当
    // 与当前值一致（视为未改动）才放行。GET 回显的 db_url 带打码口令，原样回传即未改。
    if let Some(v) = body.server_host
        && v != cfg.host
    {
        return config_error(
            "监听地址属于启动配置，请修改服务器上的 nuono.toml 配置文件后重启 Nuono。",
        );
    }
    if let Some(v) = body.server_port
        && v != cfg.port
    {
        return config_error(
            "服务端口属于启动配置，请修改服务器上的 nuono.toml 配置文件后重启 Nuono。",
        );
    }
    if let Some(v) = body.db_url {
        // GET 回显的是打码值（口令段为 ****）；原样回传说明前端没改这一项。
        if !v.contains("****") && v != cfg.database.url {
            return config_error("请在数据库连接编辑卡保存启动配置，并重启 Nuono 生效。");
        }
    }
    if let Some(v) = body.admin_auth_enabled
        && v != cfg.security.admin_auth_enabled
    {
        return config_error(
            "管理员登录验证属于启动配置，请修改服务器上的 nuono.toml 配置文件后重启 Nuono。",
        );
    }

    // 显式补丁：settings 与 secrets 只包含请求明确给出的字段。密钥的提交语义由此
    // 与请求一一对应——显式提交的值即使与环境变量相同也会落库，显式清除总能删掉
    // 数据库里的旧值，不会被环境变量遮蔽成「无变化」。
    let mut settings = serde_json::Map::new();
    let mut secrets: Vec<(String, Option<String>)> = Vec::new();

    if let Some(v) = body.upstream_url {
        settings.insert("upstream".into(), serde_json::json!({ "default_url": v }));
    }
    if let Some(v) = body.log_level {
        settings.insert("log".into(), serde_json::json!({ "level": v }));
    }
    if let Some(v) = body.rate_limit_per_minute {
        settings.insert(
            "security".into(),
            serde_json::json!({ "rate_limit_per_minute": v }),
        );
    }
    if let Some(v) = body.subscription_enabled {
        settings.insert("subscription".into(), serde_json::json!({ "enabled": v }));
    }
    if let Some(value) = body.default_locale {
        let locale = match nuono_app::localization::Locale::parse_explicit(&value) {
            Ok(locale) => locale,
            Err(error) => return config_error(error.to_string()),
        };
        settings.insert(
            "localization".into(),
            serde_json::json!({ "default_locale": locale }),
        );
    }
    if let Some(image_output) = body.image_output {
        match image_output.into_settings(cfg.image_output) {
            Ok(value) => {
                settings.insert("image_output".into(), value);
            }
            Err(message) => return config_error(message),
        }
    }
    if let Some(interactions) = body.message_interactions {
        match interactions.into_settings(cfg.message_interactions.clone()) {
            Ok(value) => {
                settings.insert("message_interactions".into(), value);
            }
            Err(message) => return config_error(message),
        }
    }
    // ws_token 属于 secret：走 runtime_secrets，空值等价于删除。
    if let Some(v) = body.ws_token {
        secrets.push((
            "security.ws_token".into(),
            (!v.trim().is_empty()).then_some(v),
        ));
    }

    // 校验邮件字段的自洽性：先用「当前生效配置 + 请求字段」拼出目标值，缺哪个字段
    // 就能报哪个错，避免整棵配置提交后在数据库层才暴露。
    if let Some(mail) = body.mail {
        let UpdateMailPayload {
            enabled,
            registration_enabled,
            smtp_host,
            smtp_port,
            smtp_username,
            smtp_password,
            from_address,
            from_name,
            encryption,
            verify_token_ttl_minutes,
            reset_token_ttl_minutes,
        } = mail;

        let mut target = cfg.clone();
        if let Some(v) = enabled {
            target.mail.enabled = v;
        }
        if let Some(v) = registration_enabled {
            if v && !cfg.mail.registration_enabled
                && cfg.security.registration_otp_pepper.is_empty()
            {
                use rand::TryRng;
                let mut bytes = [0u8; 32];
                if let Err(e) = rand::rngs::SysRng.try_fill_bytes(&mut bytes) {
                    tracing::error!(error = %e, "生成注册验证码 pepper 失败");
                    return ApiResponse::from_err(&nuono_core::error::NuonoError::Internal(
                        "无法生成注册验证码密钥".into(),
                    ));
                }
                let pepper = hex::encode(bytes);
                // 与本轮 mail 变更同一事务落库，避免「注册已开而 pepper 缺失」的中间态。
                secrets.push(("security.registration_otp_pepper".into(), Some(pepper)));
            }
            target.mail.registration_enabled = v;
        }
        if let Some(v) = smtp_host.as_deref() {
            target.mail.smtp_host = v.trim().to_string();
        }
        if let Some(v) = smtp_port {
            target.mail.smtp_port = v;
        }
        if let Some(v) = smtp_username.as_deref() {
            target.mail.smtp_username = v.trim().to_string();
        }
        if let Some(v) = smtp_password.as_deref() {
            target.mail.smtp_password = (!v.is_empty()).then(|| v.to_string());
        }
        if let Some(v) = from_address.as_deref() {
            target.mail.from_address = v.trim().to_string();
        }
        if let Some(v) = from_name.as_deref() {
            target.mail.from_name = v.trim().to_string();
        }
        if let Some(v) = encryption.as_deref() {
            target.mail.encryption = v.trim().to_lowercase();
        }
        if let Some(v) = verify_token_ttl_minutes {
            target.mail.verify_token_ttl_minutes = v;
        }
        if let Some(v) = reset_token_ttl_minutes {
            target.mail.reset_token_ttl_minutes = v;
        }
        if let Err(message) = validate_mail(&target.mail) {
            return config_error(message);
        }

        let mut mail_settings = serde_json::Map::new();
        if let Some(v) = enabled {
            mail_settings.insert("enabled".into(), serde_json::json!(v));
        }
        if let Some(v) = registration_enabled {
            mail_settings.insert("registration_enabled".into(), serde_json::json!(v));
        }
        if let Some(v) = smtp_host.as_deref() {
            mail_settings.insert("smtp_host".into(), serde_json::json!(v.trim()));
        }
        if let Some(v) = smtp_port {
            mail_settings.insert("smtp_port".into(), serde_json::json!(v));
        }
        if let Some(v) = smtp_username.as_deref() {
            mail_settings.insert("smtp_username".into(), serde_json::json!(v.trim()));
        }
        if let Some(v) = smtp_password {
            // smtp_password 是 secret，不进设置树；空串表示清除。
            secrets.push((
                "mail.smtp_password".into(),
                (!v.trim().is_empty()).then_some(v),
            ));
        }
        if let Some(v) = from_address.as_deref() {
            mail_settings.insert("from_address".into(), serde_json::json!(v.trim()));
        }
        if let Some(v) = from_name.as_deref() {
            mail_settings.insert("from_name".into(), serde_json::json!(v.trim()));
        }
        if let Some(v) = encryption.as_deref() {
            mail_settings.insert(
                "encryption".into(),
                serde_json::json!(v.trim().to_lowercase()),
            );
        }
        if let Some(v) = verify_token_ttl_minutes {
            mail_settings.insert("verify_token_ttl_minutes".into(), serde_json::json!(v));
        }
        if let Some(v) = reset_token_ttl_minutes {
            mail_settings.insert("reset_token_ttl_minutes".into(), serde_json::json!(v));
        }
        settings.insert("mail".into(), serde_json::Value::Object(mail_settings));
    }

    let patch = nuono_core::db::RuntimeConfigPatch {
        settings: serde_json::Value::Object(settings),
        secrets,
    };

    let result = if let Some(revision) = rich_revision {
        state
            .config
            .update_message_interactions_patch(patch, &revision)
            .await
    } else {
        state.config.update_patch(patch).await
    };
    match result {
        Ok(()) => {
            if let Some(level) = new_log_level
                && let Err(e) = nuono_core::telemetry::reload_filter(&level)
            {
                tracing::error!(error = %e, "配置已保存，但日志级别即时应用失败");
                return super::ApiReply::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "配置已保存，但日志级别未能即时应用，请查看服务日志",
                );
            }
            ApiResponse::ok(())
        }
        Err(nuono_core::error::NuonoError::Conflict(message)) => {
            super::ApiReply::error(StatusCode::CONFLICT, message)
        }
        Err(e) => {
            tracing::error!(error = %e, "保存运行配置失败");
            ApiResponse::from_err(&e)
        }
    }
}

/// 单个 secret 的配置状态。只回显键名和是否已配置，不回显值。
///
/// 插件声明的 secret 额外携带 `title`/`description`/`required`（固定框架 secret 无这些字段，
/// `skip_serializing_if` 使旧客户端感知不到新增字段，保持向后兼容）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::system_config::SecretStatus)]
pub struct SecretStatus {
    pub key: String,
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// 一个 secret 键被管理 API 处理时的目标。
#[derive(Debug, PartialEq, Eq)]
enum SecretWriteTarget {
    /// 固定框架 `SECRET_KEYS`：Database 直写 + 同步内存配置。
    Framework,
    /// 插件声明的 canonical key（`plugin.*`）：必须走 PluginHost 原子方法，授权与写入
    /// 在宿主侧 schema 读锁内完成，避免重载夹在「校验 → 写入」之间。
    Plugin,
    /// 既非固定键也非 `plugin.*`：一律拒绝，避免客户端绕过插件声明设置键。
    Unknown,
}

/// 分类某个 secret 键应走哪条写入路径。
fn classify_secret_key(key: &str) -> SecretWriteTarget {
    if nuono_core::db::SECRET_KEYS.contains(&key) {
        SecretWriteTarget::Framework
    } else if key.starts_with("plugin.") {
        SecretWriteTarget::Plugin
    } else {
        SecretWriteTarget::Unknown
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::SetSecretPayload)]
pub struct SetSecretPayload {
    pub key: String,
    pub value: String,
}

/// GET /api/config/secrets
///
/// 列出所有受管 secret 及其配置状态。响应里没有 secret 值，避免通过管理面泄露。
#[utoipa::path(
    get,
    path = "/secrets",
    operation_id = "system_config_list_secrets",
    tag = "system_config",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<SecretStatus>>))
)]
async fn list_secrets(
    State(state): State<AppState>,
    super::input::ApiCaller(caller): super::input::ApiCaller,
) -> crate::api::ApiReply<Vec<SecretStatus>> {
    let configured = match state.db.list_configured_secret_keys().await {
        Ok(keys) => keys,
        Err(e) => {
            tracing::error!(error = %e, "读取已配置 secret 键失败");
            return ApiResponse::internal("无法读取密钥列表，请稍后重试");
        }
    };

    let statuses = nuono_core::db::SECRET_KEYS
        .iter()
        .map(|key| SecretStatus {
            key: (*key).to_string(),
            configured: configured.iter().any(|k| k == key),
            title: None,
            description: None,
            required: None,
        })
        .collect::<Vec<_>>();

    let mut statuses = statuses;
    for schema in state.manager.plugin_host.plugin_secret_schemas().await {
        if state
            .manager
            .plugin_host
            .plugin_role(&schema.plugin_name)
            .await
            .is_service()
            && !caller.is_owner()
        {
            continue;
        }
        let configured = configured.iter().any(|k| k == &schema.key);
        statuses.push(SecretStatus {
            key: schema.key,
            configured,
            title: Some(schema.title),
            description: Some(schema.description),
            required: Some(schema.required),
        });
    }
    // 固定键在前、插件键按 canonical 排序在后，输出稳定且不回显任何值。
    statuses.sort_by(|a, b| a.key.cmp(&b.key));
    ApiResponse::ok(statuses)
}

/// POST /api/config/secrets
///
/// 写入 secret 到数据库并立即同步到内存配置，无需重启。空值等价于删除。
/// 走 `ConfigManager::update_secret`：提交前先校验候选配置，若删除/清空会让启用中
/// 的模块失去必填密钥则拒绝，数据库保持原状。
#[utoipa::path(
    post,
    path = "/secrets",
    operation_id = "system_config_set_secret",
    tag = "system_config",
    request_body = SetSecretPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn set_secret(
    State(state): State<AppState>,
    super::input::ApiCaller(caller): super::input::ApiCaller,
    super::input::ApiJson(body): super::input::ApiJson<SetSecretPayload>,
) -> super::ApiReply<()> {
    match classify_secret_key(&body.key) {
        SecretWriteTarget::Unknown => ApiResponse::bad_request("不支持配置该密钥项"),
        SecretWriteTarget::Framework => {
            return match state
                .config
                .update_secret(&body.key, Some(&body.value))
                .await
            {
                Ok(()) => ApiResponse::ok(()),
                Err(e) => {
                    tracing::error!(error = %e, key = %body.key, "保存 secret 失败");
                    ApiResponse::from_err(&e)
                }
            };
        }
        SecretWriteTarget::Plugin => {
            if let Some(plugin_name) = body
                .key
                .strip_prefix("plugin.")
                .and_then(|rest| rest.split('.').next())
                && state
                    .manager
                    .plugin_host
                    .plugin_role(plugin_name)
                    .await
                    .is_service()
                && !caller.is_owner()
            {
                return ApiResponse::forbidden("仅部署所有者可管理底层服务插件密钥");
            }
            return match state
                .manager
                .plugin_host
                .set_plugin_secret(&body.key, &body.value)
                .await
            {
                Ok(()) => ApiResponse::ok(()),
                Err(e) => ApiResponse::from_err(&e),
            };
        }
    }
}

/// DELETE /api/config/secrets/{key}
///
/// 删除前先校验候选配置：会让启用中的模块失去必填密钥时拒绝请求，数据库不变。
#[utoipa::path(
    delete,
    path = "/secrets/{key}",
    operation_id = "system_config_delete_secret",
    tag = "system_config",
    params(("key" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn delete_secret(
    State(state): State<AppState>,
    super::input::ApiCaller(caller): super::input::ApiCaller,
    super::input::ApiPath(key): super::input::ApiPath<String>,
) -> super::ApiReply<()> {
    match classify_secret_key(&key) {
        SecretWriteTarget::Unknown => ApiResponse::bad_request("不支持配置该密钥项"),
        SecretWriteTarget::Framework => {
            return match state.config.update_secret(&key, None).await {
                Ok(()) => ApiResponse::ok(()),
                Err(e) => {
                    tracing::error!(error = %e, key = %key, "删除 secret 失败");
                    ApiResponse::from_err(&e)
                }
            };
        }
        SecretWriteTarget::Plugin => {
            if let Some(plugin_name) = key
                .strip_prefix("plugin.")
                .and_then(|rest| rest.split('.').next())
                && state
                    .manager
                    .plugin_host
                    .plugin_role(plugin_name)
                    .await
                    .is_service()
                && !caller.is_owner()
            {
                return ApiResponse::forbidden("仅部署所有者可管理底层服务插件密钥");
            }
            return match state.manager.plugin_host.delete_plugin_secret(&key).await {
                Ok(()) => ApiResponse::ok(()),
                Err(e) => ApiResponse::from_err(&e),
            };
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::system_config::TestMailPayload)]
pub struct TestMailPayload {
    /// 测试邮件的收件地址。
    pub to: String,
}

/// POST /api/config/mail/test
///
/// 用当前生效的邮件配置发一封测试邮件。SMTP 配错通常要等到真有用户注册才暴露，
/// 这里提供一条即时反馈路径。用的是运行期配置快照，因此需要先保存再测试。
#[utoipa::path(
    post,
    path = "/mail/test",
    operation_id = "system_config_send_test_mail",
    tag = "system_config",
    request_body = TestMailPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn send_test_mail(
    State(state): State<AppState>,
    caller: Option<axum::Extension<crate::middleware::CallerIdentity>>,
    Json(body): Json<TestMailPayload>,
) -> crate::api::ApiReply<()> {
    let actor = caller.map(|c| c.0.username);
    let to = body.to.trim().to_string();
    if to.is_empty() {
        return ApiResponse::bad_request("请填写测试收件地址");
    }
    if let Err(message) = crate::api::setup::validate_email(&to) {
        return ApiResponse::bad_request(message);
    }

    let cfg = state.config.get().await;
    if !cfg.mail.enabled {
        return ApiResponse::conflict("邮件功能未启用，请先开启并保存配置");
    }

    let mail = nuono_core::service::mail::ComposedMail {
        subject: format!("[{}] SMTP 配置测试", cfg.mail.from_name),
        text_body: "这是一封来自 Nuono 的测试邮件。\n\n收到它说明 SMTP 配置可用，注册验证和密码重置邮件可以正常送达。\n".to_string(),
        html_body: "<html><body style=\"font-family:system-ui,-apple-system,Segoe UI,sans-serif;line-height:1.6;color:#1f2328;\"><p>这是一封来自 Nuono 的测试邮件。</p><p>收到它说明 SMTP 配置可用，注册验证和密码重置邮件可以正常送达。</p></body></html>".to_string(),
    };

    match nuono_core::service::mail::send(&cfg.mail, &to, &mail).await {
        Ok(()) => {
            // 只记录收件地址，不记录 SMTP 凭据。
            let _ = nuono_core::audit::record_event(
                &state.db,
                "config.mail_test_sent",
                None,
                actor.as_deref(),
                serde_json::json!({"to": to}),
            )
            .await;
            ApiResponse::ok(())
        }
        // 这是「发送测试邮件」按钮，原始报错就是它的诊断价值所在，保留细节。
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_settings_require_one_strong_subtree_revision_without_changing_legacy_payloads() {
        assert!(message_revision_header(&HeaderMap::new()).is_err());
        for value in [
            "*".into(),
            format!("W/\"{}\"", "a".repeat(64)),
            "a".repeat(64),
            format!("\"{}\"", "A".repeat(64)),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::IF_MATCH, value.parse().unwrap());
            assert!(message_revision_header(&headers).is_err());
        }
        let mut headers = HeaderMap::new();
        let header = format!("\"{}\"", "a".repeat(64));
        headers.insert(axum::http::header::IF_MATCH, header.parse().unwrap());
        assert_eq!(message_revision_header(&headers).unwrap(), "a".repeat(64));
        headers.append(axum::http::header::IF_MATCH, header.parse().unwrap());
        assert!(message_revision_header(&headers).is_err());
        let old: UpdateConfigPayload =
            serde_json::from_value(serde_json::json!({"log_level":"INFO"})).unwrap();
        assert!(old.message_interactions.is_none());
    }

    #[test]
    fn image_output_response_contains_the_current_encoding_settings() {
        assert_eq!(
            serde_json::to_value(ImageOutputConfigResponse::from(
                ImageOutputSettings::default()
            ))
            .unwrap(),
            serde_json::json!({"format": "png", "jpeg_quality": 80})
        );
        assert_eq!(
            serde_json::to_value(ImageOutputConfigResponse::from(ImageOutputSettings {
                format: ImageOutputFormat::Jpeg,
                jpeg_quality: 95,
            }))
            .unwrap(),
            serde_json::json!({"format": "jpeg", "jpeg_quality": 95})
        );
    }

    #[test]
    fn image_output_payload_is_optional_partial_and_rejects_unknown_fields() {
        let omitted: UpdateConfigPayload = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(omitted.image_output.is_none());
        let current = ImageOutputSettings {
            format: ImageOutputFormat::Jpeg,
            jpeg_quality: 92,
        };
        for (body, expected) in [
            (serde_json::json!({}), serde_json::json!({})),
            (
                serde_json::json!({"format": "png"}),
                serde_json::json!({"format": "png"}),
            ),
            (
                serde_json::json!({"jpeg_quality": 85}),
                serde_json::json!({"jpeg_quality": 85}),
            ),
        ] {
            let payload: UpdateConfigPayload = serde_json::from_value(serde_json::json!({
                "image_output": body
            }))
            .unwrap();
            assert!(payload.mail.is_none());
            assert!(payload.default_locale.is_none());
            assert!(payload.log_level.is_none());
            assert_eq!(
                payload
                    .image_output
                    .unwrap()
                    .into_settings(current)
                    .unwrap(),
                expected
            );
        }
        for invalid in [
            serde_json::json!({"quality": 80}),
            serde_json::json!({"jpeg_quality": 80, "unknown": true}),
            serde_json::json!({"format": "webp"}),
            serde_json::json!({"format": "jpg"}),
            serde_json::json!({"format": "PNG"}),
            serde_json::json!({"jpeg_quality": -1}),
            serde_json::json!({"jpeg_quality": 256}),
            serde_json::json!({"jpeg_quality": 80.5}),
            serde_json::json!({"jpeg_quality": "80"}),
        ] {
            assert!(
                serde_json::from_value::<UpdateConfigPayload>(serde_json::json!({
                    "image_output": invalid
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn image_output_api_validates_quality_for_png_and_jpeg() {
        for format in ["png", "jpeg"] {
            for quality in [1, 80, 100] {
                let patch: ImageOutputPatch = serde_json::from_value(serde_json::json!({
                    "format": format, "jpeg_quality": quality
                }))
                .unwrap();
                assert!(patch.into_settings(ImageOutputSettings::default()).is_ok());
            }
            for quality in [0, 101, 255] {
                let patch: ImageOutputPatch = serde_json::from_value(serde_json::json!({
                    "format": format, "jpeg_quality": quality
                }))
                .unwrap();
                assert!(patch.into_settings(ImageOutputSettings::default()).is_err());
            }
        }
    }

    #[test]
    fn db_url_password_is_masked_in_response_shape() {
        // 带口令的标准 URL：口令段替换为 ****，其余部分（query 含 @）原样保留。
        assert_eq!(
            mask_db_url("postgres://nuono:secret@localhost:5432/nuono?sslmode=require"),
            "postgres://nuono:****@localhost:5432/nuono?sslmode=require"
        );

        // 没有口令、没有 userinfo 的 URL 不动。
        assert_eq!(
            mask_db_url("postgres://localhost:5432/nuono"),
            "postgres://localhost:5432/nuono"
        );
        assert_eq!(
            mask_db_url("postgres://user@localhost/nuono"),
            "postgres://user@localhost/nuono"
        );
    }

    #[test]
    fn db_url_response_never_carries_the_password() {
        // 契约测试：无论配置里的 db_url 长什么样，打码结果都不包含原口令。
        for (url, secret) in [
            ("postgres://u:p@h/db", "p@h"),
            (
                "postgresql://user:hunter2@db.internal:5432/x?sslmode=require",
                "hunter2",
            ),
        ] {
            assert!(!mask_db_url(url).contains(secret), "{url} 的口令被泄露");
        }
        // 非 URL 形态的值原样返回，不因打码而损坏。
        assert_eq!(mask_db_url("not a url at all"), "not a url at all");
    }

    #[test]
    fn config_patch_rejects_removed_and_unknown_settings() {
        for value in [
            serde_json::json!({"public_url": "https://bot.example.com"}),
            serde_json::json!({"unknown_setting": true}),
        ] {
            assert!(serde_json::from_value::<UpdateConfigPayload>(value).is_err());
        }
    }

    #[test]
    fn mail_patch_is_optional_and_partial() {
        // 不带 mail 的旧客户端请求必须仍然可解析，否则会打断现有保存流程。
        let omitted: UpdateConfigPayload = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(omitted.mail.is_none());
        assert!(omitted.default_locale.is_none());

        // 只改一个字段时，其余字段应为 None 而不是被默认值覆盖。
        let partial: UpdateConfigPayload = serde_json::from_value(serde_json::json!({
            "mail": { "enabled": true }
        }))
        .unwrap();
        let mail = partial.mail.unwrap();
        assert_eq!(mail.enabled, Some(true));
        assert!(mail.smtp_host.is_none());
        assert!(mail.smtp_password.is_none());
    }

    #[test]
    fn language_setting_is_a_partial_patch_independent_of_browser_language() {
        let body: UpdateConfigPayload = serde_json::from_value(serde_json::json!({
            "default_locale": "ko"
        }))
        .unwrap();
        assert_eq!(body.default_locale.as_deref(), Some("ko"));
        assert!(body.subscription_enabled.is_none());
        assert!(body.mail.is_none());
        assert!(body.ws_token.is_none());
    }

    #[test]
    fn mail_response_never_carries_the_password() {
        // 契约测试：响应结构体里不存在可回显密码的字段，只有布尔标记。
        for configured in [false, true] {
            let json = serde_json::to_value(MailConfigResponse {
                enabled: true,
                registration_enabled: true,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                smtp_username: "no-reply@example.com".to_string(),
                smtp_password_configured: configured,
                from_address: "no-reply@example.com".to_string(),
                from_name: "Nuono".to_string(),
                encryption: "starttls".to_string(),
                verify_token_ttl_minutes: 1440,
                reset_token_ttl_minutes: 60,
            })
            .unwrap();
            assert_eq!(json["smtp_password_configured"], configured);
            for key in [
                "smtp_password",
                "smtp_password_saved",
                "smtp_password_source",
            ] {
                assert!(json.get(key).is_none());
            }
        }
    }

    #[test]
    fn secret_key_routes_to_framework_plugin_or_rejected() {
        // 固定框架 SECRET_KEYS 走 Database 直写。
        assert!(matches!(
            classify_secret_key("ai.api_key"),
            SecretWriteTarget::Framework
        ));
        // `plugin.*` 一律走 host 原子方法（是否已声明由 host 授权层判定）。
        assert!(matches!(
            classify_secret_key("plugin.weather.captcha_api_key"),
            SecretWriteTarget::Plugin
        ));
        // 非 framework 非 plugin 的任意键直接拒绝。
        assert!(matches!(
            classify_secret_key("totally.made.up.key"),
            SecretWriteTarget::Unknown
        ));
        assert!(matches!(
            classify_secret_key(""),
            SecretWriteTarget::Unknown
        ));
    }

    #[test]
    fn secret_status_never_carries_a_value() {
        // 契约测试：可扩展的 SecretStatus 只有 key/configured/title/desc/required，
        // 永远不回显 value —— 管理面不能成为读取 secret 的旁路。
        let status = SecretStatus {
            key: "plugin.weather.captcha_api_key".into(),
            configured: true,
            title: Some("验证码 API Key".into()),
            description: Some("用于验证码服务".into()),
            required: Some(true),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"key\""));
        assert!(json.contains("\"configured\""));
        assert!(json.contains("\"title\""));
        assert!(json.contains("\"description\""));
        assert!(json.contains("\"required\""));
        assert!(!json.contains("value"), "secret 值绝不能出现在响应里");
    }
}
