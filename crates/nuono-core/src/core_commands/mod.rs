//! Nuono 自身的聊天维护入口，与源码插件的注册和生命周期分开。
//!
//! 系统指令支持自定义前缀、每条指令的独立启停、自定义快捷别名与可上调的权限门槛。
//! 只有精确命中已注册的系统前缀子指令或已启用指令的自定义别名时才会认领消息，
//! 普通聊天即使恰好以系统前缀开头也不会被拦截或回复报错。

mod auth;
mod help;
mod restart;
mod status;

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use nuono_app::localization::Locale;

use crate::config::{ConfigManager, CoreCommandPolicy, CoreCommandsConfig};
use crate::core::event_kernel::RuntimeEventContext;
use crate::core::permission::{PermissionLevel, PermissionManager};
use crate::core::system_monitor::SystemMonitor;
use crate::error::{NuonoError, Result};

/// 系统核心指令的稳定标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreCommandId {
    Help,
    Status,
    Restart,
}

impl CoreCommandId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Status => "status",
            Self::Restart => "restart",
        }
    }
}

/// 系统核心指令的权限等级；顺序保证 `Everyone < GroupAdmin < BotAdmin`，用于强制校验安全下限。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum CoreCommandPermission {
    Everyone,
    GroupAdmin,
    /// 机器人管理员及以上（`BotAdmin` / `BotOwner`，含已绑定的站点 Owner/Admin 账号）。
    BotAdmin,
}

impl CoreCommandPermission {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Everyone => "Everyone",
            Self::GroupAdmin => "GroupAdmin",
            Self::BotAdmin => "BotAdmin",
        }
    }
}

impl fmt::Display for CoreCommandPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CoreCommandPermission {
    type Err = NuonoError;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "Everyone" => Ok(Self::Everyone),
            "GroupAdmin" => Ok(Self::GroupAdmin),
            "BotAdmin" => Ok(Self::BotAdmin),
            other => Err(NuonoError::Validation(format!(
                "无效的系统指令权限: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CoreCommandDefinition {
    pub id: CoreCommandId,
    /// 对外稳定的唯一标识（`help` / `status` / `restart`）。
    pub key: &'static str,
    /// 默认中文子指令名（`帮助` / `状态` / `重启`）。
    pub name: &'static str,
    /// 默认英文子指令名（`help` / `status` / `restart`）。
    pub english: &'static str,
    /// 控制台与帮助面板展示名称。
    pub display_name: &'static str,
    pub description: &'static str,
    /// 可选参数占位说明。
    pub arg_hint: Option<&'static str>,
    pub default_permission: CoreCommandPermission,
    /// 不可下调的最低安全权限下限。
    pub min_permission: CoreCommandPermission,
    pub accepts_args: bool,
}

/// 结合运行配置解析后的单条系统指令生效状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCoreCommand {
    pub definition: &'static CoreCommandDefinition,
    pub enabled: bool,
    pub permission: CoreCommandPermission,
    pub aliases: Vec<String>,
    pub builtin_triggers: Vec<String>,
    pub usage: String,
}

/// 单次消息匹配命中的系统核心指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedCoreCommand<'a> {
    pub resolved: ResolvedCoreCommand,
    pub args: &'a str,
    pub via_prefix: bool,
}

const DEFINITIONS: [CoreCommandDefinition; 3] = [
    CoreCommandDefinition {
        id: CoreCommandId::Help,
        key: "help",
        name: "帮助",
        english: "help",
        display_name: "系统帮助",
        description: "查看系统核心指令与已启用插件概览",
        arg_hint: Some("[插件名]"),
        default_permission: CoreCommandPermission::Everyone,
        min_permission: CoreCommandPermission::Everyone,
        accepts_args: true,
    },
    CoreCommandDefinition {
        id: CoreCommandId::Status,
        key: "status",
        name: "状态",
        english: "status",
        display_name: "运行状态",
        description: "查看 Nuono 图片状态卡与插件公开统计",
        arg_hint: None,
        default_permission: CoreCommandPermission::Everyone,
        min_permission: CoreCommandPermission::Everyone,
        accepts_args: false,
    },
    CoreCommandDefinition {
        id: CoreCommandId::Restart,
        key: "restart",
        name: "重启",
        english: "restart",
        display_name: "重启服务",
        description: "安全重启 Nuono 服务进程",
        arg_hint: None,
        default_permission: CoreCommandPermission::BotAdmin,
        min_permission: CoreCommandPermission::BotAdmin,
        accepts_args: false,
    },
];

pub fn definitions() -> &'static [CoreCommandDefinition] {
    &DEFINITIONS
}

pub fn definition_by_key(key: &str) -> Option<&'static CoreCommandDefinition> {
    DEFINITIONS.iter().find(|command| command.key == key)
}

/// 按当前运行配置解析全部系统指令的生效触发词、用法、权限与启用状态。
pub fn resolve_commands(config: &CoreCommandsConfig) -> Vec<ResolvedCoreCommand> {
    let prefix = config.effective_prefix();
    DEFINITIONS
        .iter()
        .map(|definition| {
            let policy = config.policy_for(definition.key);
            let enabled = policy.is_none_or(|item| item.enabled);
            let permission = policy
                .and_then(|item| item.permission)
                .map(|perm| perm.max(definition.min_permission))
                .unwrap_or(definition.default_permission);
            let aliases = policy
                .map(|item| {
                    item.aliases
                        .iter()
                        .map(|alias| alias.trim().to_owned())
                        .filter(|alias| !alias.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let builtin_triggers = builtin_triggers_for(prefix, definition);
            let usage = format_usage(prefix, definition);
            ResolvedCoreCommand {
                definition,
                enabled,
                permission,
                aliases,
                builtin_triggers,
                usage,
            }
        })
        .collect()
}

fn builtin_triggers_for(prefix: &str, definition: &CoreCommandDefinition) -> Vec<String> {
    let compact_zh = format!("{prefix}{}", definition.name);
    let spaced_zh = format!("{prefix} {}", definition.name);
    let spaced_en = format!("{prefix} {}", definition.english);
    let mut triggers = vec![spaced_zh];
    if !triggers.contains(&compact_zh) {
        triggers.push(compact_zh);
    }
    if !triggers.contains(&spaced_en) {
        triggers.push(spaced_en);
    }
    triggers
}

fn format_usage(prefix: &str, definition: &CoreCommandDefinition) -> String {
    match definition.arg_hint {
        Some(arg_hint) => format!(
            "{prefix} {} {arg_hint} / {prefix} {} {arg_hint}",
            definition.name, definition.english
        ),
        None => format!(
            "{prefix} {} / {prefix} {}",
            definition.name, definition.english
        ),
    }
}

/// 严格匹配系统指令：支持 `<前缀> <子指令>`、`<前缀><子指令>`、可选前导 `/` 及已启用指令的自定义别名。
/// 不符合任何已注册系统指令的普通消息返回 `None`，绝不抢占日常聊天。
pub fn match_command<'a>(
    text: &'a str,
    config: &CoreCommandsConfig,
) -> Option<MatchedCoreCommand<'a>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let resolved = resolve_commands(config);
    let prefix = config.effective_prefix();

    // 路径一：系统前缀 + 中/英子指令（若前缀本身不带 `/` 则同时允许前导 `/`）。
    let candidate = if !prefix.starts_with('/') {
        trimmed.strip_prefix('/').unwrap_or(trimmed)
    } else {
        trimmed
    };
    if let Some(after_prefix) = strip_prefix_case_insensitive(candidate, prefix) {
        let rest = after_prefix.trim_start();
        if !rest.is_empty() {
            for item in &resolved {
                if let Some(args) = match_keyword(
                    rest,
                    item.definition.name,
                    item.definition.accepts_args,
                )
                .or_else(|| {
                    match_keyword(rest, item.definition.english, item.definition.accepts_args)
                }) {
                    return Some(MatchedCoreCommand {
                        resolved: item.clone(),
                        args,
                        via_prefix: true,
                    });
                }
            }
        }
    }

    // 路径二：已启用指令的自定义快捷别名（禁用后的指令不再借别名抢占插件消息）。
    for item in resolved {
        if !item.enabled {
            continue;
        }
        for alias in &item.aliases {
            if let Some(args) = match_keyword(trimmed, alias, item.definition.accepts_args) {
                return Some(MatchedCoreCommand {
                    resolved: item,
                    args,
                    via_prefix: false,
                });
            }
        }
    }

    None
}

fn strip_prefix_case_insensitive<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_ascii() {
        let head = input.get(..prefix.len())?;
        head.eq_ignore_ascii_case(prefix)
            .then(|| &input[prefix.len()..])
    } else {
        input.strip_prefix(prefix)
    }
}

fn match_keyword<'a>(input: &'a str, keyword: &str, accepts_args: bool) -> Option<&'a str> {
    let rest = strip_prefix_case_insensitive(input, keyword)?;
    if rest.is_empty() {
        return Some("");
    }
    if accepts_args && rest.starts_with(char::is_whitespace) {
        return Some(rest.trim());
    }
    None
}

/// 默认配置下的快速认领检查（用于无配置上下文的测试与兜底等待拦截）。
pub fn is_core_command(text: &str) -> bool {
    is_core_command_with_config(text, &CoreCommandsConfig::default())
}

/// 结合运行时配置判断消息是否由系统核心指令或账号绑定令牌认领。
pub fn is_core_command_with_config(text: &str, config: &CoreCommandsConfig) -> bool {
    binding_code(text).is_some() || match_command(text, config).is_some()
}

/// 绑定令牌由框架处理，不能被插件的等待会话消费。
pub(crate) fn binding_code(text: &str) -> Option<&str> {
    let code = text.trim().strip_prefix("绑定 ")?;
    (code.len() == 32 && code.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(code)
}

pub(super) fn localized(locale: Locale, zh: &str, en: &str, ja: &str, ko: &str) -> String {
    match locale {
        Locale::ZhCn => zh,
        Locale::En => en,
        Locale::Ja => ja,
        Locale::Ko => ko,
    }
    .into()
}

fn record_outcome(ctx: &RuntimeEventContext<'_>, command: &str, outcome: &str) {
    tracing::info!(
        target: "core_commands",
        command,
        outcome,
        bot_id = ctx.bot_id,
        user_id = ?ctx.event.get("user_id"),
        group_id = ?ctx.event.get("group_id"),
        platform = ?ctx.event.get("platform"),
        "Nuono 核心指令处理"
    );
}

/// 进程监督和源码构建约束由 server 实现，核心指令不自行拉起或退出进程。
#[async_trait::async_trait]
pub trait RestartService: Send + Sync {
    async fn prepare_restart(
        &self,
        reason: &str,
        reply: Option<RestartReplyTarget>,
    ) -> std::result::Result<PreparedRestart, String>;
}

/// 仅持久化原生平台目标会话标识以跨越进程重启，不携带任何旧连接套接字或会话令牌。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RestartReplyTarget {
    pub route: crate::core::RuntimeBotRoute,
    pub bot: crate::domain::BotIdentity,
    pub conversation: crate::domain::ConversationRef,
}

type RestartCommit =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<(), String>> + Send>>;

/// 持有一次重启预约凭证（期间调用方先向聊天端发送确认提示）；
/// 若直接丢弃则自动取消预约，只有显式调用 commit 才会真正触发停机重启。
pub struct PreparedRestart(Box<dyn FnOnce() -> RestartCommit + Send>);

impl fmt::Debug for PreparedRestart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedRestart(..)")
    }
}

impl PreparedRestart {
    pub fn new<F, Fut>(commit: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        Self(Box::new(move || Box::pin(commit())))
    }

    pub async fn commit(self) -> std::result::Result<(), String> {
        (self.0)().await
    }
}

pub struct CoreCommands {
    config: Arc<ConfigManager>,
    permission_manager: Arc<PermissionManager>,
    system_monitor: Arc<SystemMonitor>,
    restart_service: RwLock<Option<Arc<dyn RestartService>>>,
    command_updates: tokio::sync::Mutex<()>,
    action_router: Option<Arc<crate::core::RuntimeActionRouter>>,
    status_source: Option<status::StatusSource>,
    status_started: std::time::Instant,
    status_requests: Arc<tokio::sync::Semaphore>,
    status_command_slots: Arc<tokio::sync::Semaphore>,
    status_render_scope: nuono_plugin::PluginTaskScope,
}

impl CoreCommands {
    pub fn new(
        config: Arc<ConfigManager>,
        permission_manager: Arc<PermissionManager>,
        system_monitor: Arc<SystemMonitor>,
    ) -> Self {
        Self {
            status_source: None,
            status_started: std::time::Instant::now(),
            status_requests: Arc::new(tokio::sync::Semaphore::new(2)),
            status_command_slots: Arc::new(tokio::sync::Semaphore::new(64)),
            status_render_scope: nuono_plugin::PluginTaskScope::new("NuonoStatus"),
            action_router: None,
            config,
            permission_manager,
            system_monitor,
            restart_service: RwLock::new(None),
            command_updates: tokio::sync::Mutex::new(()),
        }
    }

    pub fn with_action_router(mut self, router: Arc<crate::core::RuntimeActionRouter>) -> Self {
        self.action_router = Some(router);
        self
    }

    pub fn with_status_source(
        mut self,
        db: crate::db::Database,
        host: Arc<crate::core::plugin_host::PluginHost>,
        data_dir: std::path::PathBuf,
    ) -> Self {
        self.status_source = Some(status::StatusSource { db, host, data_dir });
        self
    }

    pub fn set_restart_service(&self, service: Arc<dyn RestartService>) {
        *self
            .restart_service
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(service);
    }

    /// 读取当前生效的系统指令前缀与解析后的指令列表。
    pub async fn resolved(&self) -> (String, Vec<ResolvedCoreCommand>) {
        let snapshot = self.config.snapshot().await;
        let prefix = snapshot.core_commands.effective_prefix().to_owned();
        let commands = resolve_commands(&snapshot.core_commands);
        (prefix, commands)
    }

    /// 判断消息是否会被当前配置下的系统核心指令或账号绑定令牌认领（供等待会话避让使用）。
    pub async fn claims_message(&self, text: &str) -> bool {
        if binding_code(text).is_some() {
            return true;
        }
        let snapshot = self.config.snapshot().await;
        match_command(text, &snapshot.core_commands).is_some()
    }

    /// 单条核心指令开关即时持久化，保留该指令已有的权限与自定义别名配置。
    pub async fn set_enabled(&self, key: &str, enabled: bool) -> Result<()> {
        if definition_by_key(key).is_none() {
            return Err(NuonoError::Validation("核心指令不存在".into()));
        }
        let _guard = self.command_updates.lock().await;
        let snapshot = self.config.snapshot().await;
        let resolved = resolve_commands(&snapshot.core_commands);
        let policies: Vec<CoreCommandPolicy> = resolved
            .into_iter()
            .map(|item| CoreCommandPolicy {
                id: item.definition.key.to_owned(),
                enabled: if item.definition.key == key {
                    enabled
                } else {
                    item.enabled
                },
                permission: Some(item.permission),
                aliases: item.aliases,
            })
            .collect();
        let next = CoreCommandsConfig {
            prefix: snapshot.core_commands.effective_prefix().to_owned(),
            policies,
        };
        self.persist_config_locked(next).await?;
        Ok(())
    }

    /// 原子更新系统指令前缀及各条指令策略，校验通过并持久化后才发布新快照。
    pub async fn update_config(&self, next: CoreCommandsConfig) -> Result<CoreCommandsConfig> {
        let _guard = self.command_updates.lock().await;
        let normalized = next.normalized();
        if let Err(errors) = normalized.validate() {
            return Err(NuonoError::Validation(errors.join("；")));
        }
        self.persist_config_locked(normalized).await
    }

    async fn persist_config_locked(&self, next: CoreCommandsConfig) -> Result<CoreCommandsConfig> {
        let value = serde_json::to_value(&next)
            .map_err(|error| NuonoError::Serialization(error.to_string()))?;
        self.config
            .update_patch(crate::db::RuntimeConfigPatch {
                settings: serde_json::json!({ "core_commands": value }),
                secrets: vec![],
            })
            .await?;
        Ok(self.config.snapshot().await.core_commands.clone())
    }

    pub async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        if !ctx.is_message || ctx.blocked {
            return Ok(());
        }
        // raw_text 是平台原始报文；指令文本来自已归一化事件，避免把协议 JSON 误当成指令输入。
        let text = ctx
            .event
            .get("raw_message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let config = self.config.snapshot().await;
        let Some(matched) = match_command(text, &config.core_commands) else {
            return Ok(());
        };
        let command = matched.resolved.definition;
        ctx.blocked = true;

        let actor = match self.permission_manager.try_evaluate(&ctx.event).await {
            Ok(actor) => actor,
            Err(error) => {
                record_outcome(ctx, command.key, "permission_unavailable");
                ctx.send_reply_text(
                    localized(
                        ctx.locale.locale,
                        "暂时无法验证权限，请稍后再试。",
                        "Permissions could not be verified. Please try again later.",
                        "権限を確認できません。しばらくしてから再試行してください。",
                        "권한을 확인할 수 없습니다. 잠시 후 다시 시도하세요.",
                    ),
                    "CoreCommands",
                )
                .await;
                return Err(error);
            }
        };
        if actor.is_banned() {
            record_outcome(ctx, command.key, "denied");
            ctx.send_reply_text(
                localized(
                    ctx.locale.locale,
                    "当前账号无权使用系统核心指令。",
                    "This account cannot use system core commands.",
                    "このアカウントはシステムコアコマンドを使用できません。",
                    "이 계정은 시스템 핵심 명령을 사용할 수 없습니다.",
                ),
                "CoreCommands",
            )
            .await;
            return Ok(());
        }

        if !matched.resolved.enabled {
            record_outcome(ctx, command.key, "disabled");
            let label = format!(
                "{} {}",
                config.core_commands.effective_prefix(),
                command.name
            );
            let message = localized(
                ctx.locale.locale,
                &format!("{label} 已禁用。"),
                &format!(
                    "{} {} is disabled.",
                    config.core_commands.effective_prefix(),
                    command.english
                ),
                &format!(
                    "{} {} は無効です。",
                    config.core_commands.effective_prefix(),
                    command.english
                ),
                &format!(
                    "{} {} 명령이 비활성화되어 있습니다.",
                    config.core_commands.effective_prefix(),
                    command.english
                ),
            );
            ctx.send_reply_text(message, "CoreCommands").await;
            return Ok(());
        }

        match matched.resolved.permission {
            CoreCommandPermission::Everyone => {}
            CoreCommandPermission::BotAdmin => {
                if !actor.has_permission(PermissionLevel::BotAdmin) {
                    record_outcome(ctx, command.key, "denied");
                    ctx.send_reply_text(
                        localized(
                            ctx.locale.locale,
                            "该指令仅限机器人管理员及以上使用。",
                            "This command is restricted to bot administrators.",
                            "このコマンドはボット管理者以上のみ使用できます。",
                            "이 명령은 봇 관리자 이상만 사용할 수 있습니다.",
                        ),
                        "CoreCommands",
                    )
                    .await;
                    return Ok(());
                }
            }
            CoreCommandPermission::GroupAdmin => {
                if let Err(error) = auth::authorize_group_admin(self, ctx).await {
                    record_outcome(ctx, command.key, "denied");
                    ctx.send_reply_text(error.render(ctx.locale.locale), "CoreCommands")
                        .await;
                    return Ok(());
                }
            }
        }

        status::record_command(self, ctx, &format!("core:{}", command.key));
        match command.id {
            CoreCommandId::Help => {
                let text =
                    help::render(self, &config.core_commands, ctx.locale.locale, matched.args)
                        .await;
                ctx.send_reply_text(text, "CoreCommands").await;
            }
            CoreCommandId::Status => {
                status::handle(self, ctx).await;
            }
            CoreCommandId::Restart => {
                let service = self
                    .restart_service
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                restart::handle(service.as_deref(), ctx).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core_commands/tests.rs"]
mod tests;
