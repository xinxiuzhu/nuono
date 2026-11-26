//! Nuono 进程级配置的加载、校验与持久化。
//!
//! 框架配置分两层：`nuono.toml` 是 **bootstrap 层**，只管「连上数据库之前就固定」的启动
//! 参数（host/port、数据库连接、备份、管理员认证开关等），缺失时生成最小模板；其余
//! 运行设置以数据库 `runtime.settings.v1` 为权威来源，Dashboard 普通设置只写数据库；
//! 数据库连接专用编辑只修改启动文件，重启生效。
//!
//! secret（token、API key）存在数据库 `nuono_secrets` 并由 Dashboard 维护，永不回写
//! `nuono.toml`；TOML 中的 secret 仅作为首次启动的种子。所有层合并后必须通过
//! [`NuonoConfig::validate`]，不能把结构化 schema 的前端约束当作安全边界。

//! 插件配置另由 [`PluginConfigManager`] 管理，以数据库全局配置及个人／群差异为权威；
//! `data/<插件名>/config.toml` 仅作为显式审核导入的来源。

mod ai_web;
mod database_settings;
mod message_interactions;
mod model_catalog;
pub use ai_web::web_settings_revision;
pub use database_settings::DatabaseConnectionSettings;
pub use message_interactions::message_interactions_revision;
pub mod plugin_config;
mod plugin_files;
pub use plugin_files::{PluginConfigHandle, PluginConfigMigrationSource, PluginConfigStatus};
pub mod schema;

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use validator::Validate;

use crate::error::{NuonoError, Result};
pub use nuono_app::image_output::ImageOutputSettings;
pub use nuono_app::message_interactions::MessageInteractionSettings;
use nuono_app::settings::{RuntimeSettingsService, RuntimeSettingsStore};

pub use plugin_config::{PluginConfigManager, plugin_fields_to_schema};
pub use schema::{
    ConfigEntry, ConfigField, ConfigSchema, ConfigValue, StructuredConfig, StructuredConfigField,
};

/// Nuono 全局运行期配置
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct NuonoConfig {
    pub host: String,
    pub port: u16,

    pub log: LogConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub build: BuildConfig,

    #[serde(default)]
    pub ai: AIConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub feishu: FeishuConfig,
    #[serde(default)]
    pub qq_official: QqOfficialConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,

    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub mail: MailConfig,
    #[serde(default)]
    pub backup: BackupConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub plugin_store: PluginStoreConfig,
    #[serde(default)]
    pub core_commands: CoreCommandsConfig,
    #[serde(default)]
    pub localization: LocalizationConfig,
    #[serde(default)]
    pub image_output: ImageOutputSettings,
    #[serde(default)]
    pub message_interactions: MessageInteractionSettings,
}

/// 聊天语言的系统兜底；浏览器界面语言和平台账号偏好独立管理。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalizationConfig {
    pub default_locale: nuono_app::localization::Locale,
}

/// 框架系统指令默认触发前缀。
pub const DEFAULT_CORE_COMMAND_PREFIX: &str = "nuono";
/// 系统指令前缀最大字符数。
pub const MAX_CORE_COMMAND_PREFIX_CHARS: usize = 32;
/// 单条系统指令别名最大字符数。
pub const MAX_CORE_COMMAND_ALIAS_CHARS: usize = 32;
/// 单条系统指令允许配置的最大自定义别名数量。
pub const MAX_CORE_COMMAND_ALIASES: usize = 10;

const fn default_core_command_enabled() -> bool {
    true
}

/// 单个系统核心指令的运行策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreCommandPolicy {
    /// 核心指令稳定标识（`help` / `status` / `restart`）。
    pub id: String,
    /// 是否启用该指令。
    #[serde(default = "default_core_command_enabled")]
    pub enabled: bool,
    /// 生效权限级别；未指定时沿用该指令默认权限，且不可低于安全下限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<crate::core_commands::CoreCommandPermission>,
    /// 无需系统前缀即可直接触发的自定义快捷别名。
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// 框架聊天指令的运行设置，随其他运行设置保存在数据库。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoreCommandsConfig {
    /// 系统指令统一触发前缀（默认 `nuono`），可按机器人品牌自由修改。
    pub prefix: String,
    /// 各核心指令的策略覆盖（数组在配置补丁合并时整体替换，不残留旧键）。
    pub policies: Vec<CoreCommandPolicy>,
}

impl Default for CoreCommandsConfig {
    fn default() -> Self {
        Self {
            prefix: DEFAULT_CORE_COMMAND_PREFIX.to_owned(),
            policies: Vec::new(),
        }
    }
}

impl CoreCommandsConfig {
    /// 返回生效的系统指令前缀（去除首尾空白，空串回退为默认值）。
    pub fn effective_prefix(&self) -> &str {
        let trimmed = self.prefix.trim();
        if trimmed.is_empty() {
            DEFAULT_CORE_COMMAND_PREFIX
        } else {
            trimmed
        }
    }

    /// 查找指定稳定标识对应的策略覆盖。
    pub fn policy_for(&self, id: &str) -> Option<&CoreCommandPolicy> {
        self.policies.iter().find(|policy| policy.id == id)
    }

    /// 归一化前缀、去重别名并按固定定义顺序整理策略项。
    pub fn normalized(&self) -> Self {
        let mut policies: Vec<CoreCommandPolicy> = self
            .policies
            .iter()
            .map(|policy| {
                let mut seen = std::collections::HashSet::new();
                let mut aliases = Vec::new();
                for alias in &policy.aliases {
                    let trimmed = alias.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let key = trimmed.to_lowercase();
                    if seen.insert(key) {
                        aliases.push(trimmed.to_owned());
                    }
                }
                CoreCommandPolicy {
                    id: policy.id.trim().to_owned(),
                    enabled: policy.enabled,
                    permission: policy.permission,
                    aliases,
                }
            })
            .collect();
        policies.sort_by_key(|policy| {
            crate::core_commands::definitions()
                .iter()
                .position(|def| def.key == policy.id)
                .unwrap_or(usize::MAX)
        });
        Self {
            prefix: self.prefix.trim().to_owned(),
            policies,
        }
    }

    /// 校验前缀、策略标识、权限安全下限与别名唯一性。
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let prefix = self.prefix.trim();
        if prefix.is_empty() {
            errors.push("系统指令前缀不能为空".to_string());
        } else if prefix.chars().count() > MAX_CORE_COMMAND_PREFIX_CHARS {
            errors.push(format!(
                "系统指令前缀不能超过 {MAX_CORE_COMMAND_PREFIX_CHARS} 个字符"
            ));
        } else if prefix
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            errors.push("系统指令前缀不能包含空白或控制字符".to_string());
        }

        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_aliases: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for policy in &self.policies {
            let Some(def) = crate::core_commands::definition_by_key(&policy.id) else {
                errors.push(format!("未知核心指令: {}", policy.id));
                continue;
            };
            if !seen_ids.insert(policy.id.as_str()) {
                errors.push(format!("核心指令策略重复: {}", policy.id));
            }
            if let Some(permission) = policy.permission
                && permission < def.min_permission
            {
                errors.push(format!(
                    "核心指令 {} 的权限不能低于 {}",
                    def.display_name,
                    def.min_permission.as_str()
                ));
            }
            if policy.aliases.len() > MAX_CORE_COMMAND_ALIASES {
                errors.push(format!(
                    "核心指令 {} 的自定义别名不能超过 {MAX_CORE_COMMAND_ALIASES} 个",
                    def.display_name
                ));
            }
            for alias in &policy.aliases {
                let trimmed = alias.trim();
                if trimmed.is_empty() {
                    errors.push(format!("核心指令 {} 存在空白别名", def.display_name));
                    continue;
                }
                if trimmed.chars().count() > MAX_CORE_COMMAND_ALIAS_CHARS {
                    errors.push(format!(
                        "核心指令 {} 的别名「{trimmed}」不能超过 {MAX_CORE_COMMAND_ALIAS_CHARS} 个字符",
                        def.display_name
                    ));
                }
                if trimmed.chars().any(char::is_control) {
                    errors.push(format!(
                        "核心指令 {} 的别名不能包含控制字符",
                        def.display_name
                    ));
                }
                let normalized_key = trimmed.to_lowercase();
                if let Some(other) = seen_aliases.insert(normalized_key, def.display_name.into()) {
                    errors.push(format!(
                        "自定义别名「{trimmed}」同时分配给了 {other} 与 {}",
                        def.display_name
                    ));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// 源码构建参数；只从 bootstrap 文件读取，修改后在下一次构建时生效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildConfig {
    pub incremental: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            incremental: nuono_domain::deployment::DEFAULT_BUILD_INCREMENTAL,
        }
    }
}

impl BuildConfig {
    /// 在启动源码构建前同步读取 `[build]`，不连接数据库或校验运行模块。
    ///
    /// 缺失或空文件使用默认值，不生成或改写文件；完整 bootstrap 模板由宿主首次
    /// 启动生成。未知的其他配置段不参与构建参数解析。
    pub fn load(path: &Path) -> Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(NuonoError::ConfigLoad(format!(
                    "读取 {} 失败: {error}",
                    path.display()
                )));
            }
        };
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        #[derive(Default, Deserialize)]
        struct BootstrapBuild {
            #[serde(default)]
            build: BuildConfig,
        }
        toml::from_str::<BootstrapBuild>(&content)
            .map(|config| config.build)
            .map_err(|error| {
                NuonoError::ConfigLoad(format!("解析 {} 失败: {error}", path.display()))
            })
    }
}

/// 自动备份策略。
///
/// 自动备份写入 `data/backup/`（与手工备份的数据库同目录策略分开），每天至多一份，
/// 超出保留天数的自动备份会被清理。手工备份不受保留策略影响。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_backup_retention_days")]
    pub retention_days: u64,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: default_backup_retention_days(),
        }
    }
}

fn default_backup_retention_days() -> u64 {
    7
}

/// 审计日志留存策略。
///
/// `retention_days` 控制命令日志与事件日志的保留天数，由 main.rs 的每日 sweep 清理；
/// 设为 0 表示不清理。审计是安全审计的关键证据，默认值刻意给得宽。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            retention_days: default_audit_retention_days(),
        }
    }
}

fn default_audit_retention_days() -> u64 {
    180
}

/// 插件商店的供应链签名策略。
///
/// - `signing_public_key`：运营者的 ed25519 公钥（hex，`nuono-server sign keygen` 生成）。
/// - `require_signature`：true 时未签名的清单一律拒绝安装/更新；false（默认）时
///   未签名照常安装，但**带签名而验错仍然拒绝** —— 有签名却对不上是篡改信号，
///   渐进模式只放宽「没有签名」这一种情况。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginStoreConfig {
    /// 缺省使用官方索引；显式 null 保留为仅使用本地清单。
    #[serde(default = "default_plugin_store_index_url")]
    pub index_url: Option<String>,
    #[serde(default)]
    pub signing_public_key: Option<String>,
    #[serde(default)]
    pub require_signature: bool,
}

pub const OFFICIAL_PLUGIN_STORE_INDEX_URL: &str =
    "https://raw.githubusercontent.com/xinxiuzhu/nuono/catalog/extensions-v2.json";

fn default_plugin_store_index_url() -> Option<String> {
    Some(OFFICIAL_PLUGIN_STORE_INDEX_URL.to_string())
}

impl Default for PluginStoreConfig {
    fn default() -> Self {
        Self {
            index_url: default_plugin_store_index_url(),
            signing_public_key: None,
            require_signature: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub level: String,
    pub output: Vec<String>,
    pub module: bool,
    #[serde(default = "default_true")]
    pub show_receive: bool,
    #[serde(default = "default_true")]
    pub show_send: bool,
    #[serde(
        default = "default_message_preview_chars",
        deserialize_with = "deserialize_message_preview_chars"
    )]
    pub message_preview_chars: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "INFO".into(),
            output: vec!["stdout".into(), "file".into()],
            module: false,
            show_receive: true,
            show_send: true,
            message_preview_chars: default_message_preview_chars(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_message_preview_chars() -> usize {
    400
}

fn deserialize_message_preview_chars<'de, D>(
    deserializer: D,
) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    usize::deserialize(deserializer).map(|value| value.clamp(20, 4_000))
}

pub use nuono_app::settings::DatabaseConfig;

pub use nuono_app::settings::AIConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct SecurityConfig {
    #[serde(default)]
    pub ws_token: String,
    #[serde(default = "default_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
    /// 平台 webhook（QQ/飞书官方网关回调）的独立限流上限。
    ///
    /// 默认远高于管理面限流：官方网关的单 IP 事件量随 bot 体量增长，不能按登录面
    /// 的低默认值一刀切；但完全不限流会让未认证请求白嫖 JSON 解析做 CPU 消耗。
    /// 设为 0 表示关闭该层限流（仅建议在部署层已有 webhook 限流时使用）。
    #[serde(default = "default_webhook_rate_limit_per_minute")]
    pub webhook_rate_limit_per_minute: u32,
    #[serde(default = "default_max_failed_attempts")]
    pub max_failed_attempts: u32,
    #[serde(default = "default_ban_duration_minutes")]
    pub ban_duration_minutes: u32,
    #[serde(default = "default_admin_auth_enabled")]
    pub admin_auth_enabled: bool,
    /// 受信任的反向代理网段（CIDR 列表）。仅当直连对端 IP 命中该列表时，才会信任 X-Forwarded-For 等转发头。
    #[serde(default)]
    pub trusted_proxy_cidrs: Vec<String>,
    /// 注册验证码的 HMAC pepper。仅由 runtime_secrets 管理，不回写配置文件。
    #[serde(default)]
    pub registration_otp_pepper: String,
}

fn default_rate_limit_per_minute() -> u32 {
    600
}

fn default_webhook_rate_limit_per_minute() -> u32 {
    3000
}

fn default_max_failed_attempts() -> u32 {
    5
}

fn default_ban_duration_minutes() -> u32 {
    15
}

fn default_admin_auth_enabled() -> bool {
    true
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            ws_token: String::new(),
            rate_limit_per_minute: default_rate_limit_per_minute(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit_per_minute(),
            max_failed_attempts: default_max_failed_attempts(),
            ban_duration_minutes: default_ban_duration_minutes(),
            admin_auth_enabled: default_admin_auth_enabled(),
            trusted_proxy_cidrs: Vec::new(),
            registration_otp_pepper: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub token: Option<String>,
    pub api_base_url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
pub struct DiscordConfig {
    pub enabled: bool,
    pub token: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
pub struct FeishuConfig {
    pub enabled: bool,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub verification_token: Option<String>,
    /// 飞书事件订阅的 Encrypt Key。配置后 webhook 强制要求
    /// `X-Lark-Signature` 签名校验（timestamp+nonce+encrypt_key+body 的 SHA-256）。
    #[serde(default)]
    pub encrypt_key: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct QqOfficialConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
    #[serde(default = "default_qq_official_api_base_url")]
    pub api_base_url: String,
    #[serde(default = "default_qq_official_token_url")]
    pub token_url: String,
    #[serde(default = "default_qq_official_intents")]
    pub intents: u64,
    #[serde(default = "default_qq_official_connection_mode")]
    pub connection_mode: String,
    #[serde(default = "default_qq_official_environment")]
    pub environment: String,
}

fn default_qq_official_connection_mode() -> String {
    "websocket".to_string()
}

fn default_qq_official_environment() -> String {
    "production".to_string()
}

fn default_qq_official_api_base_url() -> String {
    "https://api.sgroup.qq.com".to_string()
}

fn default_qq_official_token_url() -> String {
    "https://bots.qq.com/app/getAppAccessToken".to_string()
}

fn default_qq_official_intents() -> u64 {
    // GROUP_AND_C2C_EVENT 和 INTERACTION_CREATE。
    (1 << 25) | (1 << 26)
}

impl Default for QqOfficialConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: None,
            app_secret: None,
            api_base_url: default_qq_official_api_base_url(),
            token_url: default_qq_official_token_url(),
            intents: default_qq_official_intents(),
            connection_mode: default_qq_official_connection_mode(),
            environment: default_qq_official_environment(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
pub struct UpstreamConfig {
    pub default_url: String,
    pub reconnect_interval_secs: u64,
    pub heartbeat_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
    #[serde(default = "default_metrics_path")]
    pub path: String,
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            path: default_metrics_path(),
            token: None,
        }
    }
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
pub struct SubscriptionConfig {
    pub enabled: bool,
}

/// SMTP 发信与邮件注册配置。
///
/// `enabled` 为 false 时所有依赖邮件的流程（注册、找回密码、改邮箱）都会拒绝服务，
/// 而不是静默失败——否则用户会一直等一封永远不会到达的邮件。
///
/// `registration_enabled` 单独控制开放注册：关闭时只有管理员能在管理面建号，
/// 开启时任何人都可以自助注册。两个开关独立，因为关闭自助注册的部署同样需要找回密码与改邮箱等邮件功能。
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct MailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_mail_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_username: String,
    #[serde(default)]
    pub smtp_password: Option<String>,
    /// 发信地址。必须是收信方能接受的地址，通常需要与 SMTP 账号同域。
    #[serde(default)]
    pub from_address: String,
    #[serde(default = "default_mail_from_name")]
    pub from_name: String,
    /// 传输加密方式：`starttls`（587 端口惯例）、`tls`（465 直连）、`none`（仅限本机测试）。
    #[serde(default = "default_mail_encryption")]
    pub encryption: String,
    /// 是否允许自助注册。false 时仅管理员可手动建号。
    #[serde(default)]
    pub registration_enabled: bool,
    #[serde(default = "default_mail_verify_token_ttl_minutes")]
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub verify_token_ttl_minutes: i64,
    #[serde(default = "default_mail_reset_token_ttl_minutes")]
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub reset_token_ttl_minutes: i64,
    #[serde(default = "default_mail_timeout_secs")]
    pub timeout_secs: u64,
}

// 从 20000 起：避开 32768 起的 Linux 临时端口区，否则新容器可能抢到内核已经分给
// 某条出站连接的端口，表现为随机的「端口已被占用」。
fn default_mail_smtp_port() -> u16 {
    587
}

fn default_mail_from_name() -> String {
    "Nuono".to_string()
}

fn default_mail_encryption() -> String {
    "starttls".to_string()
}

/// 验证链接与重置链接都取 10 分钟。
///
/// 这两类链接都躺在收件箱里，谁读到邮件谁就能用，所以有效期直接等于暴露窗口。
/// 10 分钟够走完「收信 → 点开」，又短到邮箱事后被翻出来也用不上。过期不是死路：
/// 两个流程都有重发入口，代价只是再收一封信。
fn default_mail_verify_token_ttl_minutes() -> i64 {
    10
}

fn default_mail_reset_token_ttl_minutes() -> i64 {
    10
}

fn default_mail_timeout_secs() -> u64 {
    10
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            smtp_port: default_mail_smtp_port(),
            smtp_username: String::new(),
            smtp_password: None,
            from_address: String::new(),
            from_name: default_mail_from_name(),
            encryption: default_mail_encryption(),
            registration_enabled: false,
            verify_token_ttl_minutes: default_mail_verify_token_ttl_minutes(),
            reset_token_ttl_minutes: default_mail_reset_token_ttl_minutes(),
            timeout_secs: default_mail_timeout_secs(),
        }
    }
}

impl Default for NuonoConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 9923,
            log: LogConfig::default(),
            database: DatabaseConfig::default(),
            build: BuildConfig::default(),
            ai: AIConfig::default(),
            security: SecurityConfig::default(),
            telegram: TelegramConfig::default(),
            discord: DiscordConfig::default(),
            feishu: FeishuConfig::default(),
            qq_official: QqOfficialConfig::default(),
            subscription: SubscriptionConfig::default(),
            upstream: UpstreamConfig {
                default_url: "ws://127.0.0.1:9923/ws".into(),
                reconnect_interval_secs: 5,
                heartbeat_timeout_secs: 30,
            },
            metrics: MetricsConfig::default(),
            mail: MailConfig::default(),
            backup: BackupConfig::default(),
            audit: AuditConfig::default(),
            plugin_store: PluginStoreConfig::default(),
            core_commands: CoreCommandsConfig::default(),
            localization: LocalizationConfig::default(),
            image_output: ImageOutputSettings::default(),
            message_interactions: MessageInteractionSettings::default(),
        }
    }
}

impl NuonoConfig {
    /// 从 `nuono.toml` 加载配置并做完整校验。
    ///
    /// 文件不存在或为空时写入默认配置。完整校验要求「启用中的模块必须已有密钥」，
    /// 因此依赖数据库 secret 的服务启动请改用 [`Self::load_bootstrap`]，这里保留原语义供测试与嵌入方使用。
    pub async fn load(path: Option<PathBuf>) -> Result<Self> {
        let config = Self::read_toml(path, false).await?;
        config
            .validate()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        Ok(config)
    }

    /// 启动引导加载：读取 TOML，仅执行 [`Self::validate_bootstrap`] 校验。
    ///
    /// 服务启动专用。此时数据库里的运行设置与 secret 还没有读回，启用中的模块可能
    /// 尚无密钥，完整校验必须推迟到 `ConfigManager::attach_database` 读取数据库之后；
    /// 若在这里就做完整校验，合法的部署（密钥只在数据库中）会被误判为配置错误。
    pub async fn load_bootstrap(path: Option<PathBuf>) -> Result<Self> {
        let config = Self::read_toml(path, true).await?;
        config
            .validate_bootstrap()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        Ok(config)
    }

    /// 读取 TOML，不做校验。
    ///
    /// 文件不存在时生成默认文件：bootstrap 模式写入只含启动参数的最小模板，
    /// 兼容模式（`bootstrap_only = false`）写入完整默认配置。已有用户文件原样解析，
    /// bootstrap 模式绝不改写；已有空文件使用默认值。
    async fn read_toml(path: Option<PathBuf>, bootstrap_only: bool) -> Result<Self> {
        let config_path = path.unwrap_or_else(default_config_path);
        let config = if config_path.exists() {
            let content = tokio::fs::read_to_string(&config_path)
                .await
                .map_err(|e| NuonoError::ConfigLoad(e.to_string()))?;
            if content.trim().is_empty() {
                let default = Self::default();
                if bootstrap_only {
                    write_bootstrap_template(&config_path).await?;
                } else {
                    write_toml_atomic(&config_path, &default).await?;
                }
                default
            } else {
                toml::from_str(&content).map_err(|e| {
                    NuonoError::ConfigLoad(format!(
                        "解析 {} 失败: {e}",
                        config_path.to_string_lossy()
                    ))
                })?
            }
        } else {
            let default = Self::default();
            if bootstrap_only {
                write_bootstrap_template(&config_path).await?;
            } else {
                write_toml_atomic(&config_path, &default).await?;
            }
            default
        };

        Ok(config)
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        self.validate()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        write_toml_atomic(path, self).await
    }

    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Validate::validate(self)
            .err()
            .map(|e| vec![e.to_string()])
            .unwrap_or_default();

        if let Err(error) = self.image_output.validate() {
            errors.push(error.to_string());
        }
        if let Err(error) = self.message_interactions.validate() {
            errors.push(error.to_string());
        }

        if let Err(error) = self.ai.web.validate() {
            errors.push(error.to_string());
        }
        if let Err(error) = self.ai.output.validate() {
            errors.push(error.to_string());
        }

        if let Err(core_errors) = self.core_commands.validate() {
            errors.extend(core_errors);
        }

        if self.host.parse::<IpAddr>().is_err() {
            errors.push("host 必须是有效的 IPv4 或 IPv6 地址".to_string());
        }
        if self.port == 0 {
            errors.push("port 必须在 1..=65535 范围内".to_string());
        }
        if let Some(index_url) = self.plugin_store.index_url.as_deref() {
            match url::Url::parse(index_url) {
                Ok(url)
                    if !index_url
                        .bytes()
                        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
                        && index_url.len() <= 2048
                        && url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none() => {}
                _ => errors.push("plugin_store.index_url 必须是无凭据的 HTTPS 地址".into()),
            }
        }
        if self.database.max_connections == 0 || self.database.max_connections > 1024 {
            errors.push("database.max_connections 必须在 1..=1024 范围内".to_string());
        }
        validate_database_url(&self.database.url, &mut errors);
        validate_url_scheme(
            "upstream.default_url",
            &self.upstream.default_url,
            &["ws", "wss"],
            &mut errors,
        );
        if self.upstream.reconnect_interval_secs == 0 {
            errors.push("upstream.reconnect_interval_secs 必须大于 0".to_string());
        }
        if self.upstream.heartbeat_timeout_secs == 0 {
            errors.push("upstream.heartbeat_timeout_secs 必须大于 0".to_string());
        }

        let log_level = self.log.level.to_ascii_uppercase();
        if !matches!(
            log_level.as_str(),
            "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR" | "OFF"
        ) {
            errors.push("log.level 必须是 TRACE/DEBUG/INFO/WARN/ERROR/OFF 之一".to_string());
        }
        if self.security.rate_limit_per_minute > 1_000_000 {
            errors.push("security.rate_limit_per_minute 不能超过 1000000".to_string());
        }
        if !self.security.admin_auth_enabled
            && self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| !address.is_loopback())
        {
            errors.push("非 loopback 监听时必须启用 security.admin_auth_enabled".to_string());
        }
        if self.security.admin_auth_enabled {
            if self.security.max_failed_attempts == 0 || self.security.max_failed_attempts > 10_000
            {
                errors.push(
                    "启用管理员认证时 security.max_failed_attempts 必须在 1..=10000 范围内"
                        .to_string(),
                );
            }
            if self.security.ban_duration_minutes == 0
                || self.security.ban_duration_minutes > 43_200
            {
                errors.push(
                    "启用管理员认证时 security.ban_duration_minutes 必须在 1..=43200 范围内"
                        .to_string(),
                );
            }
        }
        for cidr in &self.security.trusted_proxy_cidrs {
            if cidr.parse::<ipnet::IpNet>().is_err() {
                errors.push(format!(
                    "security.trusted_proxy_cidrs 包含无效 CIDR: {cidr}"
                ));
            }
        }

        if self.telegram.enabled {
            require_non_empty(
                "telegram.token",
                self.telegram.token.as_deref(),
                &mut errors,
            );
        }
        if !self.telegram.api_base_url.trim().is_empty() {
            validate_url_scheme(
                "telegram.api_base_url",
                &self.telegram.api_base_url,
                &["http", "https"],
                &mut errors,
            );
        }
        if self.discord.enabled {
            require_non_empty("discord.token", self.discord.token.as_deref(), &mut errors);
        }
        if self.feishu.enabled {
            require_non_empty("feishu.app_id", self.feishu.app_id.as_deref(), &mut errors);
            require_non_empty(
                "feishu.app_secret",
                self.feishu.app_secret.as_deref(),
                &mut errors,
            );
        }
        if self.qq_official.enabled {
            require_non_empty(
                "qq_official.app_id",
                self.qq_official.app_id.as_deref(),
                &mut errors,
            );
            require_non_empty(
                "qq_official.app_secret",
                self.qq_official.app_secret.as_deref(),
                &mut errors,
            );
        }
        validate_url_scheme(
            "qq_official.api_base_url",
            &self.qq_official.api_base_url,
            &["http", "https"],
            &mut errors,
        );
        validate_url_scheme(
            "qq_official.token_url",
            &self.qq_official.token_url,
            &["http", "https"],
            &mut errors,
        );
        if !matches!(
            self.qq_official.connection_mode.as_str(),
            "websocket" | "webhook"
        ) {
            errors.push("qq_official.connection_mode 必须是 websocket 或 webhook".to_string());
        }
        if !matches!(
            self.qq_official.environment.as_str(),
            "production" | "sandbox"
        ) {
            errors.push("qq_official.environment 必须是 production 或 sandbox".to_string());
        }

        if self.ai.any_live_channel_active() && !self.ai.provider_router_enabled {
            require_non_empty("ai.provider", Some(&self.ai.provider), &mut errors);
            require_non_empty("ai.model", Some(&self.ai.model), &mut errors);
        }
        if let Some(endpoint) = self.ai.endpoint.as_deref().filter(|v| !v.trim().is_empty()) {
            validate_url_scheme("ai.endpoint", endpoint, &["http", "https"], &mut errors);
        }
        if !(0.0..=2.0).contains(&self.ai.temperature) {
            errors.push("ai.temperature 必须在 0.0..=2.0 范围内".to_string());
        }
        if !(0.0..=1.0).contains(&self.ai.provider_health_max_failure_rate) {
            errors.push("ai.provider_health_max_failure_rate 必须在 0.0..=1.0 范围内".to_string());
        }
        if self.ai.provider_timeout_secs == 0 {
            errors.push("ai.provider_timeout_secs 必须大于 0".to_string());
        }
        if let Err(error) = self.ai.agent_memory_policy.validate() {
            errors.push(format!("ai.agent_memory_policy: {error}"));
        }
        if let Err(error) = self.ai.assistant.validate() {
            errors.push(format!("ai.assistant: {error}"));
        }
        if self.metrics.path.is_empty()
            || !self.metrics.path.starts_with('/')
            || self.metrics.path.chars().any(char::is_whitespace)
        {
            errors.push("metrics.path 必须是以 / 开头且不含空白的路径".to_string());
        }

        // 加密方式与端口无论是否启用都要校验：写错了应该立刻报错，而不是等到开启发信才暴露。
        if !matches!(self.mail.encryption.as_str(), "starttls" | "tls" | "none") {
            errors.push("mail.encryption 必须是 starttls/tls/none 之一".to_string());
        }
        if self.mail.smtp_port == 0 {
            errors.push("mail.smtp_port 必须在 1..=65535 范围内".to_string());
        }
        if self.mail.verify_token_ttl_minutes < 1 || self.mail.verify_token_ttl_minutes > 43_200 {
            errors.push("mail.verify_token_ttl_minutes 必须在 1..=43200 范围内".to_string());
        }
        if self.mail.reset_token_ttl_minutes < 1 || self.mail.reset_token_ttl_minutes > 43_200 {
            errors.push("mail.reset_token_ttl_minutes 必须在 1..=43200 范围内".to_string());
        }
        if self.mail.timeout_secs == 0 || self.mail.timeout_secs > 300 {
            errors.push("mail.timeout_secs 必须在 1..=300 范围内".to_string());
        }
        if self.mail.enabled {
            require_non_empty("mail.smtp_host", Some(&self.mail.smtp_host), &mut errors);
            require_non_empty(
                "mail.smtp_username",
                Some(&self.mail.smtp_username),
                &mut errors,
            );
            require_non_empty(
                "mail.from_address",
                Some(&self.mail.from_address),
                &mut errors,
            );
            // 发信地址必须能被 SMTP 服务器接受，结构错了发信必然失败。
            if !self.mail.from_address.trim().is_empty()
                && !is_structurally_valid_email(self.mail.from_address.trim())
            {
                errors.push("mail.from_address 不是有效的邮箱地址".to_string());
            }
        }
        // 开放注册依赖发信能力：关闭发信时无法投递验证邮件，账号会永远停在 pending。
        if self.mail.registration_enabled && !self.mail.enabled {
            errors.push("mail.registration_enabled 需要同时启用 mail.enabled".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 启动引导校验：只检查「连上数据库之前就固定」的字段。
    ///
    /// 覆盖 host/port（bind 需要）、数据库连接（读运行设置需要）、日志级别（telemetry
    /// 初始化需要）和管理认证开关（非 loopback 监听的 fail-closed 检查）。其余字段的
    /// 值可能仍躺在数据库里，完整校验由 `ConfigManager` 在读取数据库后执行。
    pub fn validate_bootstrap(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if self.host.parse::<IpAddr>().is_err() {
            errors.push("host 必须是有效的 IPv4 或 IPv6 地址".to_string());
        }
        if self.port == 0 {
            errors.push("port 必须在 1..=65535 范围内".to_string());
        }
        validate_database_url(&self.database.url, &mut errors);
        if self.database.max_connections == 0 || self.database.max_connections > 1024 {
            errors.push("database.max_connections 必须在 1..=1024 范围内".to_string());
        }
        let log_level = self.log.level.to_ascii_uppercase();
        if !matches!(
            log_level.as_str(),
            "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR" | "OFF"
        ) {
            errors.push("log.level 必须是 TRACE/DEBUG/INFO/WARN/ERROR/OFF 之一".to_string());
        }
        if !self.security.admin_auth_enabled
            && self
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| !address.is_loopback())
        {
            errors.push("非 loopback 监听时必须启用 security.admin_auth_enabled".to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn telemetry_config(&self) -> crate::telemetry::LogConfig {
        let outputs: std::collections::HashSet<_> = self
            .log
            .output
            .iter()
            .map(|output| output.trim().to_ascii_lowercase())
            .collect();
        crate::telemetry::LogConfig {
            level: crate::telemetry::normalize_filter(&self.log.level),
            console_output: outputs.contains("stdout") || outputs.contains("console"),
            console_format: crate::telemetry::LogFormat::Compact,
            file_output: outputs.contains("file"),
            file_dir: PathBuf::from("logs"),
            file_format: crate::telemetry::LogFormat::Json,
            include_location: false,
            include_target: self.log.module,
            retention_days: 7,
        }
    }
}

fn validate_database_url(value: &str, errors: &mut Vec<String>) {
    if value.starts_with("sqlite:") || value == ":memory:" {
        if let Err(error) = nuono_storage::db::sqlite_database_path(value) {
            errors.push(format!("database.url 无效: {error}"));
        }
    } else {
        validate_url_scheme("database.url", value, &["postgres", "postgresql"], errors);
    }
}

fn validate_url_scheme(field: &str, value: &str, schemes: &[&str], errors: &mut Vec<String>) {
    match url::Url::parse(value) {
        Ok(parsed) if schemes.contains(&parsed.scheme()) => {}
        Ok(parsed) => errors.push(format!(
            "{field} 仅允许协议 {}，实际为 {}",
            schemes.join("/"),
            parsed.scheme()
        )),
        Err(e) => errors.push(format!("{field} 不是有效 URL: {e}")),
    }
}

fn require_non_empty(field: &str, value: Option<&str>, errors: &mut Vec<String>) {
    if value.is_none_or(|v| v.trim().is_empty()) {
        errors.push(format!("启用相关功能时 {field} 不能为空"));
    }
}

/// 结构性邮箱校验：只挡明显错误的输入，不保证地址真实可投递。
///
/// 真正的有效性由邮件验证流程确认（能收到链接才算有效），因此这里刻意保持宽松，
/// 不实现 RFC 5322 全集。发信地址配置与账号注册共用同一套规则，避免两处判断不一致。
pub fn is_structurally_valid_email(email: &str) -> bool {
    if email.chars().count() > 320 || email.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = email.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return false;
    }
    domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// 主配置文件的默认位置：仓库根目录的 `nuono.toml`。
pub fn default_config_path() -> PathBuf {
    PathBuf::from("nuono.toml")
}

/// 首次启动生成的 bootstrap 最小模板。
///
/// 只包含「连接数据库之前就固定」的字段：其余运行设置以数据库为权威来源，写进
/// 模板反而会诱导用户直接编辑文件、与 Dashboard 的保存互相冲突。模板必须能反序
/// 化出与 [`NuonoConfig::default`] 等价的有效配置——凡是 serde 缺省值不合法的段
/// （如 `upstream` 的派生默认值全为 0）都必须显式列出。
const BOOTSTRAP_TEMPLATE: &str = r#"# Nuono bootstrap 配置
#
# 这里只保留「连接数据库之前就固定」的启动参数。其余运行设置（AI、平台接入、
# 邮件、限流等）以数据库为权威来源，由 Dashboard 的设置页维护，保存后立即生效
# 且重启不丢失，不要写进本文件。

# 监听地址。改为 0.0.0.0 属于知情动作：公网部署前应先完成初始化。
host = "127.0.0.1"
port = 9923

[log]
level = "INFO"
output = ["stdout", "file"]
module = false

[database]
url = "sqlite://data/nuono.db?mode=rwc"
max_connections = 5

[build]
# 默认开启增量编译；改为 false 可关闭后续源码构建的增量缓存。
incremental = true

[upstream]
default_url = "ws://127.0.0.1:9923/ws"
reconnect_interval_secs = 5
heartbeat_timeout_secs = 30
"#;

/// 源码启动器在构建或初始化运行时前生成缺失的 bootstrap 文件。
///
/// 只使用 `create_new` 创建文件；已有文件（包括空文件）和并发创建的文件原样保留。
/// 返回 `true` 表示本次已创建。模板与普通宿主引导共用，不连接数据库或加载运行设置。
pub fn ensure_bootstrap_config(path: &Path) -> Result<bool> {
    use std::io::Write;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| NuonoError::FileWrite {
            path: parent.display().to_string(),
            reason: error.to_string(),
        })?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(NuonoError::FileWrite {
                path: path.display().to_string(),
                reason: error.to_string(),
            });
        }
    };
    file.write_all(BOOTSTRAP_TEMPLATE.as_bytes())
        .map_err(|error| NuonoError::FileWrite {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    Ok(true)
}

async fn write_file_atomic(path: &Path, content: String) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| NuonoError::FileWrite {
                path: parent.to_string_lossy().to_string(),
                reason: e.to_string(),
            })?;
    }

    let tmp_path = path.with_extension("toml.tmp");

    tokio::fs::write(&tmp_path, content)
        .await
        .map_err(|e| NuonoError::FileWrite {
            path: tmp_path.to_string_lossy().to_string(),
            reason: e.to_string(),
        })?;
    set_private_file_permissions(&tmp_path).await?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| NuonoError::FileWrite {
            path: path.to_string_lossy().to_string(),
            reason: e.to_string(),
        })?;

    set_private_file_permissions(path).await?;
    Ok(())
}

async fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let content =
        toml::to_string_pretty(value).map_err(|e| NuonoError::Serialization(e.to_string()))?;
    write_file_atomic(path, content).await
}

/// 首次启动生成 bootstrap 最小模板；只写缺失的文件，绝不改写已有用户文件。
async fn write_bootstrap_template(path: &Path) -> Result<()> {
    ensure_bootstrap_config(path)?;
    Ok(())
}

async fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| NuonoError::FileWrite {
                path: path.to_string_lossy().to_string(),
                reason: e.to_string(),
            })?;
    }
    // Windows 由 NTFS ACL 默认继承控制权限，不额外收紧；显式标记参数已在其他平台使用。
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl TelegramConfig {
    pub fn resolve_token(&self) -> Option<String> {
        self.token.clone()
    }
}

/// 进程内配置快照及其 bootstrap 文件路径。
///
/// 分层模型：[`Self::load`] 读取 `nuono.toml` + 环境变量作为初始内存快照，同时保留纯
/// TOML 基线（`startup` 字段）；`attach_database` 之后，内存快照始终是「数据库运行设
/// 置 + secret」的有效配置，普通设置的写入只进数据库（[`Self::update`] 兼容
/// 整快照 diff、[`Self::update_patch`] 是显式补丁入口），普通设置不改写 `nuono.toml`。
/// 数据库地址的专用编辑只保存文件供下次启动读取，不修改当前快照。发布
/// 新快照前都会执行完整校验；启动引导期的部分校验见 [`NuonoConfig::validate_bootstrap`]。
pub struct ConfigManager {
    // 仅在该同步锁内克隆或替换不可变的 Arc 指针，绝不在 I/O 期间持锁；
    // 数据库事务提交成功后可立即同步发布新快照，中间无需跨越 .await。
    config: std::sync::Arc<std::sync::RwLock<std::sync::Arc<NuonoConfig>>>,
    /// 启动配置文件基线：数据库首次初始化种子与重建有效配置时的底座。
    startup: NuonoConfig,
    path: PathBuf,
    runtime: tokio::sync::RwLock<Option<std::sync::Arc<RuntimeSettingsService>>>,
    /// 配置写路径互斥锁：attach、update、update_patch、update_secret 共用，
    /// 保证「数据库提交 → 内存发布」的读改写不与并发写交错。
    writes: tokio::sync::Mutex<()>,
}

impl ConfigManager {
    /// 实际加载的启动配置路径；完整备份将其保存为部署参考。
    pub fn config_path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn for_test(config: NuonoConfig, path: PathBuf) -> Self {
        Self {
            config: std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(
                config.clone(),
            ))),
            startup: config,
            path,
            runtime: tokio::sync::RwLock::new(None),
            writes: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(default_config_path);
        let boot = NuonoConfig::load_bootstrap(Some(path.clone())).await?;
        Ok(Self {
            config: std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(boot.clone()))),
            startup: boot,
            path,
            runtime: tokio::sync::RwLock::new(None),
            writes: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn attach_database(&self, db: crate::db::Database) -> Result<()> {
        self.attach_store(std::sync::Arc::new(db)).await
    }

    pub async fn attach_store(
        &self,
        store: std::sync::Arc<dyn RuntimeSettingsStore>,
    ) -> Result<()> {
        let _guard = self.writes.lock().await;
        let runtime = std::sync::Arc::new(RuntimeSettingsService::new(
            store,
            std::sync::Arc::new(crate::db::runtime_config::EffectiveConfigValidator(
                self.startup.clone(),
            )),
        ));
        let effective = runtime
            .initialize(crate::db::runtime_config::runtime_seed(&self.startup)?)
            .await?;
        self.publish(serde_json::from_value(effective)?).await?;
        *self.runtime.write().await = Some(runtime);
        Ok(())
    }

    /// 深拷贝当前配置。需要修改配置副本时使用；只读热路径请用 [`Self::snapshot`]。
    pub async fn get(&self) -> NuonoConfig {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .clone()
    }

    /// 零拷贝配置快照。每条消息的热路径应使用此方法，避免 clone 整棵配置树。
    pub async fn snapshot(&self) -> std::sync::Arc<NuonoConfig> {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 校验并替换内存快照，不持久化。
    ///
    /// 所有会改写运行配置的常规入口（update/update_patch/update_secret）都应在 `writes` 锁内走 `publish`。
    pub async fn replace_runtime(&self, new_config: NuonoConfig) -> Result<()> {
        let _guard = self.writes.lock().await;
        self.publish(new_config).await
    }

    /// 兼容入口：整快照更新。普通设置与 secret 按「生效快照 diff」落库。
    ///
    /// 服务端所有写入口（系统设置、AI 设置、日志等级、密钥增删）都已迁移到
    /// [`Self::update_patch`] / [`Self::update_secret`] 显式补丁；本方法仅供未挂接
    /// 数据库的嵌入/测试场景使用。
    pub async fn update(&self, new_config: NuonoConfig) -> Result<()> {
        let _guard = self.writes.lock().await;
        new_config
            .validate()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        if let Some(runtime) = self.runtime.read().await.as_ref() {
            let previous = self.get().await;
            reject_startup_field_changes(&previous, &new_config)?;
            let patch = crate::db::runtime_config_diff_patch(&previous, &new_config)?;
            let effective = serde_json::from_value(runtime.update(&patch).await?)?;
            return self.publish(effective).await;
        }
        let persisted_config = self.config_for_persistence(new_config.clone());
        write_toml_atomic(&self.path, &persisted_config).await?;
        let mut config = self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *config = std::sync::Arc::new(new_config);
        Ok(())
    }

    /// 显式补丁入口：普通设置与 secret 一次事务写入数据库，提交前用「存储值 + 补丁」的
    /// 候选配置做完整校验，失败则拒绝且数据库不变。
    ///
    /// 需要挂接数据库；启动字段（host/port/database 等）不在补丁范围内，
    /// 数据库连接用 [`Self::save_database_settings`] 保存，其余启动字段手改文件后重启。
    pub async fn update_patch(&self, patch: crate::db::RuntimeConfigPatch) -> Result<()> {
        let _guard = self.writes.lock().await;
        let runtime = self
            .runtime
            .read()
            .await
            .clone()
            .ok_or_else(|| NuonoError::ConfigLoad("配置管理器尚未挂接数据库".into()))?;
        let effective = serde_json::from_value(runtime.update(&patch).await?)?;
        self.publish(effective).await
    }

    /// 显式设置或删除单个框架 secret（`value = None` 表示删除）。
    ///
    /// 与 [`Self::update_patch`] 走同一条事务路径：候选配置在提交前于事务内完整校验，
    /// 删除会让启用中的模块失去必填密钥（如已启用的 Telegram 的 token）时拒绝请求并回滚；
    /// 提交成功后直接发布事务返回的有效配置，不再提交后重读数据库，避免「已落库但内存刷新失败」
    /// 的中间态。空串等价于删除。
    pub async fn update_secret(&self, key: &str, value: Option<&str>) -> Result<()> {
        let _guard = self.writes.lock().await;
        if !crate::db::SECRET_KEYS.contains(&key) {
            return Err(NuonoError::ConfigValidation(format!(
                "未知 secret 键: {key}"
            )));
        }
        let runtime = self
            .runtime
            .read()
            .await
            .clone()
            .ok_or_else(|| NuonoError::ConfigLoad("配置管理器尚未挂接数据库".into()))?;
        let patch = crate::db::RuntimeConfigPatch {
            settings: serde_json::json!({}),
            secrets: vec![(
                key.to_string(),
                value.filter(|v| !v.trim().is_empty()).map(str::to_string),
            )],
        };
        let effective = serde_json::from_value(runtime.update(&patch).await?)?;
        self.publish(effective).await
    }

    /// 校验并发布内存快照。调用方应已持有 `writes` 锁。
    async fn publish(&self, new_config: NuonoConfig) -> Result<()> {
        new_config
            .validate()
            .map_err(|errors| NuonoError::ConfigValidation(errors.join("; ")))?;
        let mut config = self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *config = std::sync::Arc::new(new_config);
        Ok(())
    }

    /// 返回可安全落盘的配置副本：清空所有 secret 字段。
    ///
    /// secret 以数据库为唯一可信来源，`nuono.toml` 只在首次启动时作为种子被读取一次。
    /// 因此这里无条件清空，避免 Dashboard 保存运行配置时把内存中的 secret 写回文件。
    /// 方法对 [`NuonoConfig`] 无借用依赖，供嵌入方在序列化自己的扩展配置前复用同一份 secret 清单。
    pub fn config_for_persistence(&self, mut config: NuonoConfig) -> NuonoConfig {
        config.clear_secrets();
        config
    }
}

/// 拒绝通过运行期写路径修改启动字段。
///
/// host/port、数据库连接、构建参数、备份策略和管理认证开关只由 `nuono.toml` 决定，
/// Dashboard 提交里携带这些字段时，只有与当前值完全一致才被接受（视为未改动）。
fn reject_startup_field_changes(
    previous: &NuonoConfig,
    next: &NuonoConfig,
) -> std::result::Result<(), NuonoError> {
    if previous.log.output != next.log.output
        || previous.host != next.host
        || previous.port != next.port
        || previous.build != next.build
        || previous.database.url != next.database.url
        || previous.database.max_connections != next.database.max_connections
        || previous.security.admin_auth_enabled != next.security.admin_auth_enabled
        || serde_json::to_value(&previous.backup).ok() != serde_json::to_value(&next.backup).ok()
    {
        return Err(NuonoError::ConfigValidation(
            "启动配置请修改 nuono.toml 后重启".into(),
        ));
    }
    Ok(())
}

impl NuonoConfig {
    /// 清空所有 secret 字段，返回可安全落盘的配置。
    ///
    /// secret 以数据库 `runtime_secrets` 为唯一可信来源；这份清空清单必须在
    /// [`ConfigManager::update`] 与嵌入方的落盘路径之间保持一致，任何一方漏掉
    /// 一个 secret 字段都会把内存里的明文写回 `nuono.toml`。
    pub fn clear_secrets(&mut self) {
        self.security.ws_token = String::new();
        self.security.registration_otp_pepper = String::new();
        self.telegram.token = None;
        self.discord.token = None;
        self.feishu.app_secret = None;
        self.feishu.verification_token = None;
        self.feishu.encrypt_key = None;
        self.qq_official.app_secret = None;
        self.ai.api_key = None;
        self.ai.web.credentials = Default::default();
        self.metrics.token = None;
        self.mail.smtp_password = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_output_defaults_for_legacy_config_and_is_a_runtime_setting() {
        use nuono_app::image_output::ImageOutputFormat;
        let legacy: NuonoConfig = toml::from_str(BOOTSTRAP_TEMPLATE).unwrap();
        assert_eq!(legacy.image_output, ImageOutputSettings::default());
        let mut old_json = serde_json::to_value(&legacy).unwrap();
        old_json.as_object_mut().unwrap().remove("image_output");
        let restored: NuonoConfig = serde_json::from_value(old_json).unwrap();
        assert_eq!(restored.image_output, ImageOutputSettings::default());

        let mut next = legacy.clone();
        next.image_output.format = ImageOutputFormat::Jpeg;
        next.image_output.jpeg_quality = 95;
        assert!(next.validate().is_ok());
        assert!(reject_startup_field_changes(&legacy, &next).is_ok());
    }

    #[test]
    fn full_config_validates_image_output_quality_for_both_formats() {
        use nuono_app::image_output::ImageOutputFormat;
        let mut config = NuonoConfig::default();
        for format in [ImageOutputFormat::Png, ImageOutputFormat::Jpeg] {
            config.image_output.format = format;
            for quality in [1, 80, 100] {
                config.image_output.jpeg_quality = quality;
                assert!(config.validate().is_ok());
            }
            for quality in [0, 101, u8::MAX] {
                config.image_output.jpeg_quality = quality;
                let errors = config.validate().unwrap_err();
                assert!(errors.iter().any(|error| error.contains("JPEG 质量")));
                // 启动引导阶段跳过运行期配置校验，推迟到挂载数据库后再统一校验。
                assert!(config.validate_bootstrap().is_ok());
            }
        }
    }

    #[test]
    fn localization_defaults_to_chinese_and_accepts_only_supported_settings() {
        use nuono_app::localization::Locale;
        let legacy: NuonoConfig = toml::from_str(BOOTSTRAP_TEMPLATE).unwrap();
        assert_eq!(legacy.localization.default_locale, Locale::ZhCn);
        for locale in Locale::ALL {
            let setting: LocalizationConfig = serde_json::from_value(serde_json::json!({
                "default_locale": locale.as_str()
            }))
            .unwrap();
            assert_eq!(setting.default_locale, locale);
            let mut next = legacy.clone();
            next.localization = setting;
            assert!(reject_startup_field_changes(&legacy, &next).is_ok());
        }
        for value in ["", "fr", "en-US", "default", "EN"] {
            assert!(
                serde_json::from_value::<LocalizationConfig>(serde_json::json!({
                    "default_locale": value
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn build_config_defaults_and_rejects_invalid_incremental_value() {
        let default: NuonoConfig = toml::from_str(BOOTSTRAP_TEMPLATE).unwrap();
        assert!(default.build.incremental);
        let mut legacy: toml::Value = toml::from_str(BOOTSTRAP_TEMPLATE).unwrap();
        legacy.as_table_mut().unwrap().remove("build");
        let legacy: NuonoConfig = legacy.try_into().unwrap();
        assert!(legacy.build.incremental);
        assert!(toml::from_str::<BuildConfig>("").unwrap().incremental);
        let disabled: NuonoConfig = toml::from_str(
            &BOOTSTRAP_TEMPLATE.replace("incremental = true", "incremental = false"),
        )
        .unwrap();
        assert!(!disabled.build.incremental);
        assert!(
            toml::from_str::<NuonoConfig>(
                &BOOTSTRAP_TEMPLATE.replace("incremental = true", "incremental = \"yes\"")
            )
            .is_err()
        );
    }

    #[test]
    fn build_config_load_reads_only_build_and_never_rewrites_user_file() {
        let dir = std::env::temp_dir().join(format!("nuono-build-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nuono.toml");
        assert_eq!(BuildConfig::load(&path).unwrap(), BuildConfig::default());
        assert!(!path.exists());

        let content = "# user configuration\n[database]\nurl = 123\n[telegram]\nenabled = true\n[build]\nincremental = false\n";
        std::fs::write(&path, content).unwrap();
        assert!(!BuildConfig::load(&path).unwrap().incremental);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);

        std::fs::write(&path, "[build]\nincremental = \"yes\"\n").unwrap();
        assert!(
            BuildConfig::load(&path)
                .unwrap_err()
                .to_string()
                .contains("nuono.toml")
        );
        std::fs::write(&path, " \n").unwrap();
        assert_eq!(BuildConfig::load(&path).unwrap(), BuildConfig::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runtime_snapshot_update_cannot_change_build_settings() {
        let previous = NuonoConfig::default();
        let mut next = previous.clone();
        next.build.incremental = false;
        assert!(reject_startup_field_changes(&previous, &next).is_err());
    }

    #[test]
    fn bootstrap_ensure_creates_private_template_and_keeps_existing_files() {
        let dir = std::env::temp_dir().join(format!("nuono-bootstrap-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested/nuono.toml");
        assert!(ensure_bootstrap_config(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BOOTSTRAP_TEMPLATE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for content in [
            "",
            "# user configuration\n[build]\nincremental = true\n",
            "# user configuration\n[build]\nincremental = false\n",
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(!ensure_bootstrap_config(&path).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn empty_bootstrap_file_uses_defaults_without_rewriting_it() {
        let dir =
            std::env::temp_dir().join(format!("nuono-bootstrap-empty-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("nuono.toml");
        tokio::fs::write(&path, " \n").await.unwrap();
        let config = NuonoConfig::load_bootstrap(Some(path.clone()))
            .await
            .unwrap();
        assert!(config.build.incremental);
        assert_eq!(config.database.url, "sqlite://data/nuono.db?mode=rwc");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), " \n");
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[test]
    fn database_backends_validate_with_sqlite_default_and_optional_postgres() {
        let mut config = NuonoConfig::default();
        assert_eq!(config.database.url, "sqlite://data/nuono.db?mode=rwc");
        for url in [
            "postgres://nuono:nuono@127.0.0.1:5432/nuono",
            "postgresql://nuono:nuono@127.0.0.1:5432/nuono",
            "sqlite://data/nuono.db?mode=rwc",
            "sqlite::memory:",
            ":memory:",
        ] {
            config.database.url = url.into();
            assert!(config.validate_bootstrap().is_ok(), "{url}");
            assert!(config.validate().is_ok(), "{url}");
        }
        config.database.url = "mysql://localhost/nuono".into();
        assert!(config.validate_bootstrap().is_err());
        assert!(config.validate().is_err());
        for url in ["sqlite://file:nuono.db", "sqlite://nuono.db?vfs=custom"] {
            config.database.url = url.into();
            assert!(config.validate_bootstrap().is_err(), "{url}");
            assert!(config.validate().is_err(), "{url}");
        }
    }

    #[test]
    fn bootstrap_validation_skips_module_checks_that_full_validation_keeps() {
        let mut config = NuonoConfig::default();
        config.telegram.enabled = true;
        // 启动引导期数据库里的 token 还没读回，启用中的模块不能在这里报错。
        assert!(config.validate_bootstrap().is_ok());
        // 完整校验仍然要求启用中的模块提供密钥。
        assert!(config.validate().is_err());

        config.host = "not an ip".to_string();
        assert!(config.validate_bootstrap().is_err());
    }

    #[tokio::test]
    async fn missing_config_generates_bootstrap_template_and_keeps_user_files() {
        let dir = std::env::temp_dir().join(format!("nuono-config-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("nuono.toml");

        // 首次启动生成 bootstrap 最小模板：只含启动参数，且解析回有效配置。
        let config = NuonoConfig::load_bootstrap(Some(path.clone()))
            .await
            .unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9923);
        assert_eq!(config.database.url, "sqlite://data/nuono.db?mode=rwc");
        assert!(config.build.incremental);
        assert_eq!(config.upstream.reconnect_interval_secs, 5);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("bootstrap 配置"));
        assert!(content.contains("[build]"));
        let parsed: NuonoConfig = toml::from_str(&content).unwrap();
        assert!(parsed.validate_bootstrap().is_ok());
        assert_eq!(parsed.host, config.host);
        assert_eq!(parsed.port, config.port);
        assert_eq!(parsed.database.url, config.database.url);
        assert_eq!(parsed.build, config.build);
        assert_eq!(parsed.upstream.reconnect_interval_secs, 5);

        // 已有用户文件原样解析、绝不改写。
        let user = NuonoConfig {
            port: 8080,
            database: DatabaseConfig {
                url: "postgres://configured-user:configured-password@127.0.0.1:5432/existing"
                    .into(),
                max_connections: 7,
            },
            build: BuildConfig { incremental: false },
            ..NuonoConfig::default()
        };
        user.save(&path).await.unwrap();
        let before = tokio::fs::read_to_string(&path).await.unwrap();
        let reloaded = NuonoConfig::load_bootstrap(Some(path.clone()))
            .await
            .unwrap();
        assert_eq!(reloaded.port, 8080);
        assert_eq!(reloaded.database.url, user.database.url);
        assert_eq!(reloaded.database.max_connections, 7);
        assert!(!reloaded.build.incremental);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            before,
            "bootstrap 加载不得改写已有用户文件"
        );

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[test]
    fn qq_official_defaults_are_stable_and_apply_when_deserializing() {
        let default = QqOfficialConfig::default();
        assert!(!default.enabled);
        assert_eq!(default.api_base_url, "https://api.sgroup.qq.com");
        assert_eq!(
            default.token_url,
            "https://bots.qq.com/app/getAppAccessToken"
        );
        assert_eq!(default.intents, 100_663_296);
        assert_eq!(default.connection_mode, "websocket");
        assert_eq!(default.environment, "production");

        let config: NuonoConfig = serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1",
            "port": 9923,
            "log": { "level": "INFO", "output": ["stdout"], "module": false },
            "database": { "url": "postgres://nuono:nuono@127.0.0.1:5432/test", "max_connections": 1 }
        }))
        .unwrap();
        assert_eq!(config.qq_official.intents, 100_663_296);
        assert_eq!(config.qq_official.connection_mode, "websocket");
        assert_eq!(config.qq_official.environment, "production");
        assert!(config.log.show_receive);
        assert!(config.log.show_send);
        assert_eq!(config.log.message_preview_chars, 400);

        let explicit: QqOfficialConfig =
            serde_json::from_value(serde_json::json!({ "intents": 33_554_432 })).unwrap();
        assert_eq!(explicit.intents, 33_554_432);
    }

    #[test]
    fn telemetry_config_honors_configured_outputs() {
        let mut config = NuonoConfig::default();
        config.log.output = vec!["stdout".to_string()];
        let telemetry = config.telemetry_config();
        assert!(telemetry.console_output);
        assert!(!telemetry.file_output);

        config.log.output = vec!["file".to_string()];
        let telemetry = config.telemetry_config();
        assert!(!telemetry.console_output);
        assert!(telemetry.file_output);
    }

    #[test]
    fn message_preview_chars_is_clamped_during_deserialization() {
        let mut value = serde_json::to_value(NuonoConfig::default()).unwrap();
        value["log"]["message_preview_chars"] = serde_json::json!(1);
        let config: NuonoConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(config.log.message_preview_chars, 20);

        value["log"]["message_preview_chars"] = serde_json::json!(10_000);
        let config: NuonoConfig = serde_json::from_value(value).unwrap();
        assert_eq!(config.log.message_preview_chars, 4_000);
    }

    #[test]
    fn plugin_store_defaults_to_official_without_overriding_explicit_sources() {
        let default = PluginStoreConfig::default();
        let missing: PluginStoreConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            default.index_url.as_deref(),
            Some(OFFICIAL_PLUGIN_STORE_INDEX_URL)
        );
        assert_eq!(missing.index_url, default.index_url);

        let disabled: PluginStoreConfig = serde_json::from_str(r#"{"index_url":null}"#).unwrap();
        assert_eq!(disabled.index_url, None);
        let round_trip: PluginStoreConfig =
            serde_json::from_value(serde_json::to_value(disabled).unwrap()).unwrap();
        assert_eq!(round_trip.index_url, None);

        let custom: PluginStoreConfig =
            serde_json::from_str(r#"{"index_url":"https://example.com/catalog.json"}"#).unwrap();
        assert_eq!(
            custom.index_url.as_deref(),
            Some("https://example.com/catalog.json")
        );
    }

    #[test]
    fn plugin_store_index_accepts_https_without_credentials() {
        let mut config = NuonoConfig::default();
        for address in [
            "http://example.com/catalog.json",
            "https://user:pass@example.com/catalog.json",
            "https://example.com/catalog.json#fragment",
            "https://example.com/ca\ttalog.json",
        ] {
            config.plugin_store.index_url = Some(address.into());
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .iter()
                    .any(|error| error.contains("plugin_store.index_url"))
            );
        }
        config.plugin_store.index_url = Some("https://example.com/catalog.json".into());
        assert!(config.validate().is_ok());
        let serialized = serde_json::to_value(&config).unwrap();
        let round_trip: NuonoConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            round_trip.plugin_store.index_url,
            config.plugin_store.index_url
        );
    }

    #[test]
    fn validation_reports_concrete_security_and_network_errors() {
        let mut cfg = NuonoConfig {
            host: "not an ip".to_string(),
            ..NuonoConfig::default()
        };
        cfg.security.max_failed_attempts = 0;
        cfg.security.trusted_proxy_cidrs = vec!["bad-cidr".to_string()];
        cfg.upstream.default_url = "http://example.com/ws".to_string();
        cfg.qq_official.connection_mode = "polling".to_string();
        cfg.qq_official.environment = "staging".to_string();
        let errors = cfg.validate().unwrap_err().join(" | ");
        assert!(errors.contains("host"));
        assert!(errors.contains("max_failed_attempts"));
        assert!(errors.contains("trusted_proxy_cidrs"));
        assert!(errors.contains("upstream.default_url"));
        assert!(errors.contains("qq_official.connection_mode"));
        assert!(errors.contains("qq_official.environment"));
    }

    #[tokio::test]
    async fn update_never_persists_secrets_to_toml() {
        let dir = std::env::temp_dir().join(format!("nuono-config-{}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let config_path = dir.join("runtime.toml");
        NuonoConfig::default().save(&config_path).await.unwrap();

        let manager = ConfigManager::load(Some(config_path.clone()))
            .await
            .unwrap();
        let mut updated = manager.get().await;
        updated.security.ws_token = "ws-secret".to_string();
        updated.telegram.token = Some("tg-secret".to_string());
        updated.ai.api_key = Some("sk-secret".to_string());
        updated.qq_official.app_secret = Some("qq-secret".to_string());
        updated.metrics.token = Some("metrics-secret".to_string());
        manager.update(updated).await.unwrap();

        let raw = tokio::fs::read_to_string(&config_path).await.unwrap();
        for leaked in [
            "ws-secret",
            "tg-secret",
            "sk-secret",
            "qq-secret",
            "metrics-secret",
        ] {
            assert!(!raw.contains(leaked), "secret {leaked} 泄露到了 nuono.toml");
        }

        // 内存快照仍保留 secret，运行期功能不受影响。
        let live = manager.get().await;
        assert_eq!(live.security.ws_token, "ws-secret");
        assert_eq!(live.ai.api_key.as_deref(), Some("sk-secret"));

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn mail_validation_requires_fields_when_enabled() {
        let mut config = NuonoConfig::default();
        config.mail.enabled = true;
        config.mail.smtp_host = String::new();
        config.mail.smtp_username = String::new();
        config.mail.from_address = String::new();

        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err().join("; ");
        assert!(errors.contains("mail.smtp_host"));
        assert!(errors.contains("mail.smtp_username"));
        assert!(errors.contains("mail.from_address"));
    }

    #[test]
    fn mail_validation_accepts_complete_smtp_configuration() {
        let mut config = NuonoConfig::default();
        config.mail.enabled = true;
        config.mail.smtp_host = "smtp.example.com".into();
        config.mail.smtp_username = "user".into();
        config.mail.from_address = "from@example.com".into();
        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn mail_validation_accepts_disabled_with_empty_fields() {
        let mut config = NuonoConfig::default();
        config.mail.enabled = false;
        config.mail.smtp_host = String::new();
        config.mail.smtp_username = String::new();
        config.mail.from_address = String::new();

        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn mail_validation_rejects_invalid_port_and_encryption() {
        let mut config = NuonoConfig::default();
        config.mail.enabled = true;
        config.mail.smtp_host = "smtp.example.com".to_string();
        config.mail.smtp_port = 0;
        config.mail.smtp_username = "user".to_string();
        config.mail.from_address = "from@example.com".to_string();
        config.mail.encryption = "invalid".to_string();

        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err().join("; ");
        assert!(errors.contains("mail.smtp_port"));
        assert!(errors.contains("mail.encryption"));
    }

    #[tokio::test]
    async fn mail_validation_rejects_malformed_from_address() {
        let mut config = NuonoConfig::default();
        config.mail.enabled = true;
        config.mail.smtp_host = "smtp.example.com".to_string();
        config.mail.smtp_username = "user".to_string();
        config.mail.from_address = "not-an-email".to_string();

        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err().join("; ");
        assert!(errors.contains("mail.from_address"));
    }

    #[tokio::test]
    async fn mail_smtp_password_is_not_persisted() {
        let dir = std::env::temp_dir().join(format!("nuono_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let config_path = dir.join("nuono.toml");

        let mut config = NuonoConfig::default();
        config.mail.smtp_password = Some("secret123".to_string());

        let manager = ConfigManager::for_test(config.clone(), config_path.clone());
        manager.update(config.clone()).await.unwrap();

        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(!content.contains("secret123"));

        let reloaded: NuonoConfig = toml::from_str(&content).unwrap();
        assert!(reloaded.mail.smtp_password.is_none());

        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
}
