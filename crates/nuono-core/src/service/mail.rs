//! 事务邮件发送。
//!
//! 只负责账号类事务邮件：邮箱验证、密码重置、更换邮箱确认。不做营销邮件、
//! 不做队列、不做重试——这些是低频操作，失败时由用户重新触发（「重新发送验证邮件」）
//! 比框架内自动重试更可控，也避免把 SMTP 故障放大成后台任务堆积。
//!
//! transport 每次发信按当前配置快照构建，不缓存连接：注册与重置不是热路径，而缓存
//! transport 就要处理 Dashboard 改配置后的失效问题，复杂度不划算。
//!
//! 消息组装（[`compose`]）与实际投递（[`send`]）分离，前者是纯函数因此可单测；后者需要
//! 真实 SMTP 服务器，只能手工验证。

use lettre::message::{Mailbox, MultiPart, SinglePart, header};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::MailConfig;
use crate::error::{NuonoError, Result};

/// 邮件类型。决定标题、正文文案与验证码输入页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailKind {
    /// 已签发的旧注册验证链接。仅在兼容窗口内使用，新注册改走 [`Self::RegistrationOtp`]。
    VerifyEmail,
    /// 注册邮箱验证码。正文只包含可手工输入的 code，不携带 bearer URL。
    RegistrationOtp,
    /// 忘记密码的一次性重置码。
    ResetPassword,
    /// 更换邮箱时发往新地址的确认码。
    ConfirmNewEmail,
}

impl MailKind {
    /// 在已知的管理界面中输入验证码的页面，不包含域名或令牌。
    fn page_path(self) -> &'static str {
        match self {
            Self::VerifyEmail | Self::RegistrationOtp => "/auth/verify",
            Self::ResetPassword => "/auth/reset",
            Self::ConfirmNewEmail => "/auth/confirm",
        }
    }

    fn subject(self, from_name: &str) -> String {
        match self {
            Self::VerifyEmail => format!("[{from_name}] 验证你的邮箱"),
            Self::RegistrationOtp => format!("[{from_name}] 你的邮箱验证码"),
            Self::ResetPassword => format!("[{from_name}] 重置密码"),
            Self::ConfirmNewEmail => format!("[{from_name}] 确认新邮箱"),
        }
    }

    fn intro(self) -> &'static str {
        match self {
            Self::VerifyEmail => {
                "请返回 Nuono 控制台，在邮箱验证页输入以下验证码，验证后即可登录。"
            }
            Self::RegistrationOtp => {
                "请回到刚才的注册页面，输入以下验证码完成邮箱验证，验证后即可登录。"
            }
            Self::ResetPassword => {
                "收到密码重置请求。请返回 Nuono 控制台，在重置密码页输入以下验证码并设置新密码。"
            }
            Self::ConfirmNewEmail => {
                "请返回 Nuono 控制台，在确认新邮箱页输入以下验证码，将该地址设为账号的新邮箱。"
            }
        }
    }

    /// 未预期收到此邮件时的提示。重置密码需要更强的提示，因为它可能意味着账号被盯上了。
    fn footer(self) -> &'static str {
        match self {
            Self::VerifyEmail | Self::RegistrationOtp | Self::ConfirmNewEmail => {
                "若非本人操作，请忽略此邮件，账号不会发生任何变更。"
            }
            Self::ResetPassword => {
                "若非本人操作，请忽略此邮件，密码不会被修改。\
                 若频繁收到此类邮件，建议检查账号安全。"
            }
        }
    }
}

/// 组装好的邮件内容。正文只包含可手工输入的验证码，不生成访问地址或令牌 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedMail {
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
}

/// 组装一次性验证码邮件。
///
/// `token` 是明文令牌（数据库里只有它的摘要），用户在原管理界面粘贴使用。
/// 不从请求 Host 或代理头构造邮件 URL，避免找回密码的令牌被引到伪造站点。
/// 同时给出纯文本与 HTML 两个版本：部分邮件客户端或安全网关会剥掉 HTML。
pub fn compose(kind: MailKind, token: &str, from_name: &str, ttl_minutes: i64) -> ComposedMail {
    let expiry = format_ttl(ttl_minutes);
    let page = kind.page_path();

    let text_body = format!(
        "{intro}\n\n一次性验证码：{token}\n\n页面路径：{page}\n\n验证码将在 {expiry}后失效，且只能使用一次。请勿将验证码告诉任何人。\n\n{footer}\n",
        intro = kind.intro(),
        token = token,
        page = page,
        expiry = expiry,
        footer = kind.footer(),
    );

    let html_body = format!(
        "<html><body style=\"font-family:system-ui,-apple-system,Segoe UI,sans-serif;\
         line-height:1.6;color:#1f2328;\">\
         <p>{intro}</p>\
         <p>一次性验证码：</p>\
         <p style=\"font-family:monospace;font-size:20px;font-weight:700;word-break:break-all;\">{token}</p>\
         <p style=\"font-size:13px;color:#57606a;\">页面路径：<code>{page}</code></p>\
         <p style=\"font-size:13px;color:#57606a;\">验证码将在 {expiry}后失效，且只能使用一次。请勿将验证码告诉任何人。</p>\
         <hr style=\"border:none;border-top:1px solid #d0d7de;margin:20px 0;\">\
         <p style=\"font-size:12px;color:#57606a;\">{footer}</p>\
         </body></html>",
        intro = escape_html(kind.intro()),
        token = escape_html(token),
        page = escape_html(page),
        expiry = escape_html(&expiry),
        footer = escape_html(kind.footer()),
    );

    ComposedMail {
        subject: kind.subject(from_name),
        text_body,
        html_body,
    }
}

/// 组装注册邮箱验证码邮件。
///
/// 不复用 [`compose`]：注册使用短数字验证码，code 只出现在正文中。
pub fn compose_registration_otp(code: &str, from_name: &str, ttl_minutes: i64) -> ComposedMail {
    let kind = MailKind::RegistrationOtp;
    let expiry = format_ttl(ttl_minutes);
    let text_body = format!(
        "{intro}\n\n验证码：{code}\n\n验证码将在 {expiry}后失效，且只能使用一次。请勿将验证码告诉任何人。\n\n{footer}\n",
        intro = kind.intro(),
        code = code,
        expiry = expiry,
        footer = kind.footer(),
    );
    let html_body = format!(
        "<html><body style=\"font-family:system-ui,-apple-system,Segoe UI,sans-serif;line-height:1.6;color:#1f2328;\">\
         <p>{intro}</p>\
         <p style=\"font-size:28px;font-weight:700;letter-spacing:0.2em;color:#111827;\">{code}</p>\
         <p>验证码将在 {expiry}后失效，且只能使用一次。请勿将验证码告诉任何人。</p>\
         <hr style=\"border:none;border-top:1px solid #d0d7de;margin:20px 0;\">\
         <p style=\"font-size:12px;color:#57606a;\">{footer}</p>\
         </body></html>",
        intro = escape_html(kind.intro()),
        code = escape_html(code),
        expiry = escape_html(&expiry),
        footer = escape_html(kind.footer()),
    );
    ComposedMail {
        subject: kind.subject(from_name),
        text_body,
        html_body,
    }
}

/// 邮件是否真的能发出去。
///
/// 与 `cfg.enabled` 区别开：`enabled` 只是用户的意图开关，而 secret 以数据库
/// `runtime_secrets` 为唯一可信来源、落盘时会从 `nuono.toml` 清空，所以「开关开着但密码
/// 还没录入」是正常的中间状态。前端拿 `enabled` 当就绪信号会显示出注册和找回密码入口，
/// 用户点进去只会撞上 SMTP 报错。
///
/// 这里做的是静态自检，不连 SMTP：主机、发信地址必须有值；配了用户名就必须配密码
/// （半套凭据一定认证失败）。无认证的本机测试 SMTP（`encryption = "none"`）不要求凭据。
pub fn is_ready(cfg: &MailConfig) -> bool {
    if !cfg.enabled {
        return false;
    }
    if cfg.smtp_host.trim().is_empty() || cfg.from_address.trim().is_empty() {
        return false;
    }
    let username = cfg.smtp_username.trim();
    let password = cfg.smtp_password.as_deref().unwrap_or("").trim();
    username.is_empty() || !password.is_empty()
}

/// 发送一封事务邮件。
///
/// 未启用 `[mail]` 时返回错误而不是静默成功：调用方需要知道邮件没发出去，否则用户会
/// 一直等一封永远不会到的信。
pub async fn send(cfg: &MailConfig, to: &str, mail: &ComposedMail) -> Result<()> {
    if !cfg.enabled {
        return Err(NuonoError::MessageSend(
            "邮件功能尚未开启，请先在系统设置中开启并保存".to_string(),
        ));
    }

    let from: Mailbox = format!("{} <{}>", cfg.from_name, cfg.from_address.trim())
        .parse()
        .map_err(|e| {
            tracing::warn!(error = %e, "发信地址解析失败");
            NuonoError::MessageSend("系统设置中的发件邮箱地址格式无效".to_string())
        })?;
    let recipient = to.trim().to_string();
    let to: Mailbox = recipient.parse().map_err(|e| {
        tracing::warn!(error = %e, "收信地址解析失败");
        NuonoError::MessageSend("收件邮箱地址格式无效".to_string())
    })?;

    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(&mail.subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(header::ContentType::TEXT_PLAIN)
                        .body(mail.text_body.clone()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(header::ContentType::TEXT_HTML)
                        .body(mail.html_body.clone()),
                ),
        )
        .map_err(|e| {
            tracing::error!(error = %e, "构造邮件正文失败");
            NuonoError::MessageSend("邮件内容生成失败，请稍后重试".to_string())
        })?;

    let transport = build_transport(cfg)?;
    // 投递结果必须留痕：注册/重发接口对外一律返回成功（不泄露邮箱是否存在），
    // 日志是唯一能区分「已投递」与「静默失败」的地方。
    match transport.send(message).await {
        Ok(_) => {
            tracing::info!(
                to = %recipient,
                subject = %mail.subject,
                smtp_host = %cfg.smtp_host,
                "📧 邮件已投递"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                to = %recipient,
                smtp_host = %cfg.smtp_host,
                smtp_port = cfg.smtp_port,
                error = %e,
                "邮件投递失败"
            );
            Err(NuonoError::MessageSend(
                "邮件发送失败，请检查发信服务器配置与网络连接".to_string(),
            ))
        }
    }
}

/// 按配置构建 SMTP transport。
///
/// `none` 模式仅用于本机测试用的无认证 SMTP（如 MailHog）：明文传输不应用于公网。
fn build_transport(cfg: &MailConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let host = cfg.smtp_host.trim();
    if host.is_empty() {
        return Err(NuonoError::MessageSend(
            "尚未配置发信服务器地址".to_string(),
        ));
    }

    let builder = match cfg.encryption.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| {
            tracing::error!(error = %e, host, "构建 TLS SMTP transport 失败");
            NuonoError::MessageSend("无法连接发信服务器，请检查服务器地址与加密方式".to_string())
        })?,
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| {
            tracing::error!(error = %e, host, "构建 STARTTLS SMTP transport 失败");
            NuonoError::MessageSend("无法连接发信服务器，请检查服务器地址与加密方式".to_string())
        })?,
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        other => {
            tracing::error!(encryption = other, "未知的邮件加密方式");
            return Err(NuonoError::MessageSend(
                "邮件加密方式无效，请选择 TLS、STARTTLS 或不加密".to_string(),
            ));
        }
    };

    let mut builder = builder
        .port(cfg.smtp_port)
        .timeout(Some(std::time::Duration::from_secs(cfg.timeout_secs)));

    // 用户名或密码任一为空就不带认证：本机测试的 SMTP 常常不需要登录。
    let username = cfg.smtp_username.trim();
    let password = cfg.smtp_password.as_deref().unwrap_or("").trim();
    if !username.is_empty() && !password.is_empty() {
        builder = builder.credentials(Credentials::new(username.to_string(), password.to_string()));
    }

    Ok(builder.build())
}

/// 把分钟数说成人话，避免正文出现「1440 分钟」这种要用户自己算的表达。
fn format_ttl(minutes: i64) -> String {
    let minutes = minutes.max(1);
    if minutes < 60 {
        format!("{minutes} 分钟")
    } else if minutes % (60 * 24) == 0 {
        format!("{} 天", minutes / (60 * 24))
    } else if minutes % 60 == 0 {
        format!("{} 小时", minutes / 60)
    } else {
        format!("{} 小时 {} 分钟", minutes / 60, minutes % 60)
    }
}

/// 转义 HTML 正文里的用户可控内容。
///
/// 验证码内容不能被当作 HTML 标签或实体。
fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_otp_mail_has_code_without_a_url() {
        let mail = compose_registration_otp("00123456", "Nuono", 10);
        assert!(mail.subject.contains("验证码"));
        assert!(mail.text_body.contains("00123456"));
        assert!(mail.html_body.contains("00123456"));
        assert!(mail.text_body.contains("10 分钟"));
        assert!(mail.text_body.contains("刚才的注册页面"));
        assert_no_mail_url(&mail);
    }

    #[test]
    fn compose_provides_manual_code_and_page_without_a_url() {
        let mail = compose(MailKind::VerifyEmail, "abc123", "Nuono", 1440);
        assert!(mail.subject.contains("验证"));
        for body in [&mail.text_body, &mail.html_body] {
            assert!(body.contains("abc123"));
            assert!(body.contains("/auth/verify"));
            assert!(body.contains("1 天"));
            assert!(body.contains("只能使用一次"));
        }
        assert_no_mail_url(&mail);
    }

    #[test]
    fn each_kind_targets_its_own_frontend_route() {
        for (kind, path) in [
            (MailKind::VerifyEmail, "/auth/verify"),
            (MailKind::ResetPassword, "/auth/reset"),
            (MailKind::ConfirmNewEmail, "/auth/confirm"),
        ] {
            let mail = compose(kind, "tok", "Nuono", 60);
            assert!(mail.text_body.contains(path), "{kind:?} 应提示 {path}");
            assert!(mail.html_body.contains(path), "{kind:?} 应提示 {path}");
            assert_no_mail_url(&mail);
        }
    }

    #[test]
    fn reset_password_warns_about_account_safety() {
        let reset = compose(MailKind::ResetPassword, "t", "Nuono", 60);
        let verify = compose(MailKind::VerifyEmail, "t", "Nuono", 60);
        // 重置密码是账号被盯上的信号，提示必须比验证邮件更强。
        assert!(reset.text_body.contains("账号安全"));
        assert!(!verify.text_body.contains("账号安全"));
    }

    #[test]
    fn ttl_is_rendered_in_human_units() {
        assert_eq!(format_ttl(30), "30 分钟");
        assert_eq!(format_ttl(60), "1 小时");
        assert_eq!(format_ttl(90), "1 小时 30 分钟");
        assert_eq!(format_ttl(1440), "1 天");
        // 0 或负数会让正文出现「0 分钟后失效」，统一抬到 1。
        assert_eq!(format_ttl(0), "1 分钟");
    }

    #[test]
    fn html_body_escapes_code_contents() {
        let code = "tok<&\"'";
        let mail = compose(MailKind::VerifyEmail, code, "Nuono", 60);
        assert!(mail.html_body.contains("tok&lt;&amp;&quot;&#39;"));
        assert!(!mail.html_body.contains(code));
        // 纯文本版本保留原样，供用户复制。
        assert!(mail.text_body.contains(code));
    }

    fn assert_no_mail_url(mail: &ComposedMail) {
        for body in [&mail.text_body, &mail.html_body] {
            assert!(!body.contains("http://"));
            assert!(!body.contains("https://"));
            assert!(!body.contains("token="));
            assert!(!body.contains("href="));
        }
    }

    #[tokio::test]
    async fn send_refuses_when_mail_is_disabled() {
        let cfg = MailConfig::default();
        assert!(!cfg.enabled);
        let mail = compose(MailKind::VerifyEmail, "t", "Nuono", 60);
        let err = send(&cfg, "user@example.com", &mail).await.unwrap_err();
        // 必须报错而不是静默成功，否则用户会一直等一封不会到的信。
        // 只断言「说明了邮件功能没开」，不锁定具体措辞，避免文案调整误伤测试。
        assert!(matches!(err, NuonoError::MessageSend(_)));
        assert!(err.to_string().contains("开启"));
    }

    #[test]
    fn transport_requires_a_host() {
        let mut cfg = MailConfig {
            enabled: true,
            ..MailConfig::default()
        };
        assert!(build_transport(&cfg).is_err(), "空 host 应拒绝");

        cfg.smtp_host = "smtp.example.com".to_string();
        assert!(build_transport(&cfg).is_ok());
    }

    #[test]
    fn unsupported_encryption_is_rejected_by_transport() {
        let cfg = MailConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            encryption: "quantum".to_string(),
            ..MailConfig::default()
        };
        assert!(build_transport(&cfg).is_err());
    }
}
