//! Nuono 插件 SDK
//!
//! 插件作者依赖此 crate，实现 `Plugin` trait，将源码放入 `plugins/` 目录。
//! `plugins/*` 是独立 Cargo workspace；它们不会被根 workspace 的默认检查自动覆盖。
//!
//! 插件作为普通 Rust 库与宿主统一编译、链接，通过 Rust 类型直接调用宿主能力。
//!
//! ## 快速示例
//!
//! ```rust,standalone_crate
//! use std::sync::Arc;
//! use nuono_plugin::prelude::*;
//!
//! #[derive(Default)]
//! struct MyState {
//!     count: usize,
//! }
//!
//! // 快速开始用 struct 形态：生成 name/version/description 与装饰器收集器，
//! // Arc<T> 字段自动成为状态注入候选。
//! #[derive(Default)]
//! #[nuono_plugin(name = "MyPlugin", version = "1.0.0", description = "一个示例插件")]
//! struct MyPlugin {
//!     state: Arc<MyState>,
//! }
//!
//! // 第 4 个参数声明状态注入：按类型从插件 struct 的 Arc 字段领取
//! #[nuono_command(pattern = "ping")]
//! async fn ping(bot: &Bot, _msg: &Message, _data: MatchData, state: &MyState) -> Result<()> {
//!     bot.send(format!("pong! count={}", state.count)).await?;
//!     Ok(())
//! }
//!
//! // 需要生命周期/配置/事件定制时改用 impl 块形态：宏把缺失的方法合并进你的块，
//! // 手写方法原样保留（Rust 不允许把一个 trait 实现拆到多个 impl 块）：
//! //
//! // #[nuono_plugin(name = "MyPlugin", state_fields = "state")]
//! // #[async_trait]
//! // impl Plugin for MyPlugin {
//! //     fn event_filter(&self) -> PluginEventFilter {
//! //         PluginEventFilter::MESSAGES // 只收消息事件
//! //     }
//! //
//! //     async fn on_load(&mut self, ctx: &PluginContext) -> Result<()> {
//! //         // 后台任务交给宿主管理（禁用/卸载前统一 abort）：
//! //         // ctx.spawn("polling", async { /* 轮询逻辑 */ });
//! //         Ok(())
//! //     }
//! // }
//!
//! register_plugin!(MyPlugin);
//!
//! # let plugin = nuono_create_plugin();
//! # assert_eq!(plugin.name(), "MyPlugin");
//! # let commands = plugin.commands();
//! # assert_eq!(commands.len(), 1);
//! # assert_eq!(commands[0].name, "ping");
//! ```

#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::exhaustive_enums,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::allow_attributes_without_reason
)]

pub mod api;
pub mod assets;
pub mod browser;
pub mod core;
pub mod db;
pub mod embedding;
pub mod extract;
pub mod factory;
pub mod image_output;
#[cfg(feature = "mcp")]
pub mod mcp;
/// 与宿主无关的语言作用域、偏好与不可变消息快照。
pub mod localization {
    pub use nuono_app::localization::{
        Locale, LocaleContext, LocalePreferenceStore, LocaleScope, LocaleService, LocaleSource,
        MemoryLocalePreferenceStore, SystemLocaleProvider,
    };
}
/// 共享的 AI 接口定义与可复用的知识检索算法。
pub use nuono_ai as ai;
pub use nuono_rag as rag;
pub mod media;
pub mod media_link;
pub mod onebot;
pub mod permission;
pub mod plugin;
pub use nuono_app::business_runtime::BusinessGate;
/// 插件卸载后仍保留在数据库中的持久化存储清单声明。
pub mod storage_manifest {
    pub use nuono_app::plugin_manifest::{
        AccountMergePolicy, AccountReference, PersistentAsset, PluginStorageDeclaration,
        PluginStorageVersion, PortableManifest, migration_checksum, normalize_table_prefix,
        validate_table_prefix,
    };
}
pub mod render;
pub mod runtime;
pub mod error {
    pub use nuono_error::{ErrorCause, NuonoError, Result, to_api_error};
}
pub mod agent;
/// 个人助手授权的数据结构定义；实际对话与额度扣减始终由框架宿主执行。
pub mod assistant {
    pub use nuono_app::assistant::{AssistantGrant, MAX_DAILY_LIMIT};
}

/// 由宿主或插件测试桩提供的出站消息动作发送接口。
pub use nuono_app::transport::RuntimeActionSink;

pub mod schedule;
pub mod status;
pub mod subscriptions;
pub mod web;
pub use nuono_app::plugin_extensions::PluginExtensions;
/// 宿主在开源构建里留空的判定切点。
///
/// 默认开放群服务并遵循助手设置。部署扩展在
/// 通过 `Plugin::extensions` 声明实现，由宿主统一管理启停。`set_*` 仅供需要固定到进程结束的嵌入部署使用。
pub mod core_seams {
    pub use nuono_app::entitlement::{
        AllActive, EntitlementDecision, EntitlementRecord, EntitlementUpsert,
        FEATURE_GROUP_BOT_ACCESS, FEATURE_USER_PRIVATE_BOT, FEATURE_USER_WEB_AI,
        GroupEntitlementQuery, SOURCE_BILLING, SOURCE_MANUAL, SOURCE_TRIAL, STATUS_ACTIVE,
        STATUS_REVOKED, SUBJECT_GROUP, SUBJECT_USER, ServiceEntitlement,
        ServiceEntitlementAlreadyRegistered, check_group_entitlement, is_group_active,
        requires_assistant_grant, set_service_entitlement,
    };
}
/// 账号角色与能力：框架闭集 + 扩展点注册。
///
/// 插件通过 `Plugin::extensions` 声明 [`auth::CapabilityProvider`]，
/// 前端再按能力字符串显示。未注册时前端拿不到任何扩展能力，入口自然不出现。
pub mod auth {
    pub use nuono_app::auth::Role;
    pub use nuono_app::auth::{
        CapabilityProvider, CapabilityProviderError, extension_capabilities,
        has_extension_capability, set_capability_provider,
    };
}
pub use api::UserAvatar;
pub use assets::{AssetLimits, PluginAssets};
pub use browser::{BrowserContent, BrowserPage, BrowserRequest, BrowserWaitUntil, PluginBrowser};
pub use embedding::{
    PluginEmbeddingBatch, PluginEmbeddingModel, PluginEmbeddingPort, PluginEmbeddings,
};
pub use image_output::{
    ImageOutputFormat, ImageOutputSettings, ImageOutputSettingsSource, PluginImageOutput,
};
pub use localization::{Locale, LocaleContext, LocaleScope, LocaleService, LocaleSource};
pub use media::ImageSource;
pub use nuono_app::media::UserAvatarRequest;
pub use nuono_app::media::{FileSendResponse, NativeFileSendRequest};
pub use plugin::PluginRuntime;
pub use render::{RenderLimits, RenderPool};
pub use schedule::{Schedule, ScheduledTask};
pub use subscriptions::{Subscription, Subscriptions};

pub mod config;
pub mod config_validation;
pub use config::{
    ConfigSchema, LiveConfig, PluginConfigSnapshot, PluginConfigUpdatePort, PluginConfigUpdates,
};

pub use crate::core::command::{ActorPermission, Command, PermissionLevel};
pub use crate::core::config_schema::{
    ConfigFieldOptionsSource, ConfigFieldType, ConfigFieldUi, ConfigFieldVisibility,
};
pub use crate::core::framework_control::{FrameworkControl, FrameworkControlSignal};
pub use crate::core::plugin_service::{PluginArea, Plugins, SV};
pub use crate::core::{HostLogLevel, HostLogRecord, RuntimeActionReceipt, RuntimeBotRoute};
#[doc = include_str!("nuono_plugin_example.md")]
pub use nuono_plugin_macros::nuono_plugin;
pub use nuono_plugin_macros::{
    ConfigSchema, nuono_command, nuono_file, nuono_fullmatch, nuono_keyword, nuono_message,
    nuono_prefix, nuono_regex, nuono_suffix, nuono_tool,
};

/// 供宿主集成与结构化跨平台动作使用的统一领域模型。
pub mod domain {
    pub use nuono_domain::{
        ActionError, ActionErrorKind, ActionId, ActionReceipt, ActionReceiptStatus, BotIdentity,
        ButtonAction, ButtonOne, ButtonPermission, ButtonStyle, Capability, CapabilitySet,
        CapabilitySupport, ConversationRef, CorrelationId, DeliveryTarget, DomainEvent,
        DomainEventKind, DomainMessage, DomainModelError, FileSource, IdentityKind, IdentityRef,
        IdentityScheme, MessageSegment, MessageTarget, ModerationOperation, NativeId,
        OutboundAction, Platform, QueryOperation, SessionKey, SessionScope, UserType, action,
        button, database, deployment, event, identity, message, scalar,
    };
    pub mod projection;
}
pub use crate::agent::{
    AgentTool, AgentToolContext, AgentToolImage, AgentToolOutput, ToolContext, ToolExecution,
    ToolExecutionStatus, ToolParameter, ToolParameterType, ToolResource, ToolResourceMode,
    ToolRiskLevel, ToolScheduling,
};
pub use crate::api::bot::{MessageTarget, SendableMessage};
pub use crate::api::routed::{RuntimeMessageTarget, RuntimeTargetKind};
pub use crate::api::segment::{MessageBuilder, MessageSegment};
pub use crate::api::waiter::ReplyWaitMode;
pub use crate::api::{Bot, NativeMessage};
pub use crate::core::command::CommandHandler;
pub use crate::core::config_schema::ConfigFieldOption;
pub use crate::core::config_schema::PluginConfigField;
pub use crate::core::event_bus::{Event as BusEvent, EventBus, EventType};
pub use crate::core::trigger::Trigger;
pub use crate::core::trigger::{MatchData, TriggerHandler, TriggerType};
pub use crate::core::trigger_registry::TriggerRegistry;
pub use crate::db::{
    Database, PluginDatabaseBackend, PluginMigration, PluginMigrationForBackend, PluginSqlValue,
};
pub use crate::extract::{
    Args, CmdState, CommandContext, CommandGroup, CommandGuard, CommandRejection, EventSnapshot,
    FromCommandContext, GroupId, IntoCommandOutcome, OptionalGroupId, ParsedArg, RawArgs, SenderId,
    TypedCommandHandler, guard_fn,
};
pub use crate::factory::{
    FactoryPlugin, MountedLifecycle, MountedPlugin, PluginDescriptor, PluginFactory,
};
pub use crate::onebot::Message;
pub use crate::plugin::plugin_secret_canonical_key;
pub use crate::plugin::{
    Event, Plugin, PluginActivationReason, PluginAgentSkill, PluginConfigHandle, PluginContext,
    PluginEventFilter, PluginHttpClient, PluginMessageListener, PluginResponse, PluginRole,
    PluginSecretField, PluginSecretSchema, PluginSecretStore, PluginServiceError,
    PluginServiceHandler, PluginServiceRegistry, PluginServiceResult, PluginStateScope,
    PluginStateStore, PluginStateValue, PluginTaskScope, PluginWebAccessPolicy, PluginWebAsset,
    PluginWebPage, PluginWebPagePlacement, PluginWebRequest, PluginWebResponse, PluginWebRoute,
};
pub use crate::status::{PluginStatusMetric, PluginStatusRequest};
pub use crate::web::{
    ApiJson, ErasedWebRouter, FromWebRequest, Html, IntoWebResponse, Json, MethodRouter,
    OptionalWebUser, Path, PluginWebRouter, Query, RawQuery, State, WebBody, WebContext,
    WebHeaders, WebUser, api_error_response, delete, error_to_response, get, patch, post, put,
};
pub use nuono_error::{NuonoError, Result as NuonoResult};
// 注意: Plugin trait 使用 anyhow::Result，不是 crate::error::Result；
// 两者之间的 `?` 转换由 `impl From<anyhow::Error> for NuonoError` 打通。
pub use anyhow::{Result, anyhow};
pub use async_trait::async_trait;
pub use inventory;
pub mod identity {
    pub use nuono_app::plugin_identity::{CanonicalGroup, IdentityAccess, IdentityAccountState};
}
pub use nuono_app::account_merge::{
    AccountMergeContext, AccountMergeParticipant, AccountMergeRead, AccountMergeTransaction,
    MergeStateRecord, MergeStepSummary, UserMergeParticipant, register_user_merge_participant,
    unregister_user_merge_participant,
};
pub use nuono_app::group_authorization::{
    GroupAuthorization, GroupGrantRequest, PluginGrantDelegation,
};
pub use schemars;
pub use schemars::JsonSchema;
pub use serde;
pub use serde_json;
pub use serde_json::Value;
pub use tracing;

/// 插件 SDK 包版本；源码兼容性通过 Cargo 和 Rust 类型检查验证。
pub const NUONO_PLUGIN_SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 插件 HTTP 调用常用别名。
pub type HttpClient = PluginHttpClient;

#[doc(hidden)]
pub mod __decorators {
    use crate::{AgentTool, Command, CommandHandler, Trigger, TriggerHandler, TriggerType};
    use std::sync::Arc;

    /// 装饰器 handler 可注入的共享状态形态：插件 struct 的 `Arc<T>` 字段。
    pub type SharedState = Arc<dyn std::any::Any + Send + Sync>;

    pub struct DecoratedTrigger {
        pub owner: &'static str,
        pub trigger_type: TriggerType,
        pub pattern: &'static str,
        pub display_name: Option<&'static str>,
        pub sv_name: Option<&'static str>,
        pub aliases: &'static [&'static str],
        pub description: &'static str,
        pub usage: &'static str,
        pub permission: crate::PermissionLevel,
        pub prefix: &'static str,
        pub priority: i32,
        pub block: bool,
        pub to_me: bool,
        /// 有状态 handler 期望的状态类型；None 表示无状态。
        pub state_type: Option<fn() -> std::any::TypeId>,
        pub handler: fn(Option<SharedState>) -> Arc<dyn TriggerHandler>,
    }

    pub struct DecoratedCommand {
        pub owner: &'static str,
        pub pattern: &'static str,
        pub display_name: Option<&'static str>,
        pub sv_name: Option<&'static str>,
        pub aliases: &'static [&'static str],
        pub description: &'static str,
        pub usage: &'static str,
        pub permission: crate::PermissionLevel,
        pub priority: i32,
        pub block: bool,
        pub to_me: bool,
        pub state_type: Option<fn() -> std::any::TypeId>,
        pub handler: fn(Option<SharedState>) -> Arc<dyn CommandHandler>,
    }

    pub struct DecoratedEventTrigger {
        pub owner: &'static str,
        pub trigger_type: TriggerType,
        pub pattern: &'static str,
        pub sv_name: Option<&'static str>,
        pub prefix: &'static str,
        pub priority: i32,
        pub block: bool,
        pub to_me: bool,
        pub state_type: Option<fn() -> std::any::TypeId>,
        pub handler: fn(Option<SharedState>) -> Arc<dyn CommandHandler>,
    }

    pub struct DecoratedTool {
        pub owner: &'static str,
        pub handler: fn() -> Arc<dyn AgentTool>,
    }

    impl std::fmt::Debug for DecoratedTrigger {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DecoratedTrigger")
                .field("owner", &self.owner)
                .field("trigger_type", &self.trigger_type)
                .field("pattern", &self.pattern)
                .finish_non_exhaustive()
        }
    }

    impl std::fmt::Debug for DecoratedCommand {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DecoratedCommand")
                .field("owner", &self.owner)
                .field("pattern", &self.pattern)
                .finish_non_exhaustive()
        }
    }

    impl std::fmt::Debug for DecoratedEventTrigger {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DecoratedEventTrigger")
                .field("owner", &self.owner)
                .field("trigger_type", &self.trigger_type)
                .field("pattern", &self.pattern)
                .finish_non_exhaustive()
        }
    }

    impl std::fmt::Debug for DecoratedTool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DecoratedTool")
                .field("owner", &self.owner)
                .finish_non_exhaustive()
        }
    }

    /// 按 TypeId 为装饰器项挑选状态；无状态项直接拿 None。
    fn pick_state(item: &DecoratedTrigger, states: &[SharedState]) -> Option<SharedState> {
        let want = item.state_type?();
        states
            .iter()
            .find(|s| s.as_ref().type_id() == want)
            .cloned()
    }

    /// 显式状态版：`#[nuono_plugin]` 生成的 `commands()` 把插件 struct 的全部
    /// `Arc<T>` 字段传进来；手写 `impl Plugin` 时也可自己调用。
    pub fn commands_with(states: &[SharedState], owner: &str) -> Vec<Command> {
        let mut commands: Vec<Command> = inventory::iter::<DecoratedTrigger>
            .into_iter()
            .filter(|item| item.owner == owner)
            .filter(|item| item.trigger_type == TriggerType::Command)
            .map(|item| {
                let mut command =
                    Command::from_message(item.pattern, (item.handler)(pick_state(item, states)))
                        .with_priority(item.priority)
                        .with_block(item.block)
                        .with_to_me(item.to_me)
                        .with_description(item.description)
                        .with_permission(item.permission);
                command.aliases = item.aliases.iter().map(|a| a.to_string()).collect();
                command.usage = item.usage.to_string();
                command.display_name = item.display_name.map(str::to_string);
                command.sv_name = item.sv_name.map(str::to_string);
                command
            })
            .collect();
        commands.extend(event_commands_with(states, owner));
        commands
    }

    /// 显式状态版，见 [`commands_with`]。
    pub fn triggers_with(states: &[SharedState], owner: &str) -> Vec<Trigger> {
        let mut triggers: Vec<Trigger> = inventory::iter::<DecoratedTrigger>
            .into_iter()
            .filter(|item| item.owner == owner)
            .filter(|item| item.trigger_type != TriggerType::Command)
            .map(|item| {
                let mut trigger = Trigger::new(
                    item.trigger_type,
                    item.pattern,
                    item.prefix,
                    item.block,
                    item.to_me,
                    (item.handler)(pick_state(item, states)),
                )
                .with_priority(item.priority);
                trigger.sv_name = item.sv_name.map(str::to_string);
                trigger
            })
            .collect();
        triggers.extend(
            inventory::iter::<DecoratedEventTrigger>
                .into_iter()
                .filter(|item| item.owner == owner)
                .map(|item| {
                    let state = item.state_type.and_then(|want| {
                        let want = want();
                        states
                            .iter()
                            .find(|s| s.as_ref().type_id() == want)
                            .cloned()
                    });
                    let mut trigger = Trigger::new_event(
                        item.trigger_type,
                        item.pattern,
                        item.block,
                        item.to_me,
                        (item.handler)(state),
                    )
                    .with_prefix(item.prefix)
                    .with_priority(item.priority);
                    trigger.sv_name = item.sv_name.map(str::to_string);
                    trigger
                }),
        );
        triggers
    }

    pub fn commands(owner: &str) -> Vec<Command> {
        commands_with(&[], owner)
    }

    pub fn triggers(owner: &str) -> Vec<Trigger> {
        triggers_with(&[], owner)
    }

    fn event_commands_with(states: &[SharedState], owner: &str) -> Vec<Command> {
        inventory::iter::<DecoratedCommand>
            .into_iter()
            .filter(|item| item.owner == owner)
            .map(|item| {
                let mut command = Command::new(
                    item.pattern,
                    (item.handler)(pick_command_state(item, states)),
                )
                .with_aliases(item.aliases)
                .with_description(item.description)
                .with_usage(item.usage)
                .with_permission(item.permission)
                .with_priority(item.priority)
                .with_block(item.block)
                .with_to_me(item.to_me);
                command.display_name = item.display_name.map(str::to_string);
                command.sv_name = item.sv_name.map(str::to_string);
                command
            })
            .collect()
    }

    fn pick_command_state(item: &DecoratedCommand, states: &[SharedState]) -> Option<SharedState> {
        let want = item.state_type?();
        states
            .iter()
            .find(|s| s.as_ref().type_id() == want)
            .cloned()
    }

    pub fn tools(owner: &str) -> Vec<Arc<dyn AgentTool>> {
        inventory::iter::<DecoratedTool>
            .into_iter()
            .filter(|item| item.owner == owner)
            .map(|item| (item.handler)())
            .collect()
    }

    inventory::collect!(DecoratedTrigger);
    inventory::collect!(DecoratedEventTrigger);
    inventory::collect!(DecoratedCommand);
    inventory::collect!(DecoratedTool);
}

/// 插件作者主入口。
///
/// 推荐插件里使用 `use nuono_plugin::prelude::*;`，避免直接依赖框架内部模块路径。
pub mod prelude {
    pub use crate::media_link::{
        DEFAULT_MEDIA_LINK_TTL, MediaLink, MediaLinkService, PluginMediaLinks,
    };
    pub use crate::{
        ActorPermission, AgentTool, AgentToolContext, AgentToolImage, AgentToolOutput,
        ConfigFieldOption, ConfigFieldOptionsSource, ConfigFieldType, ConfigFieldUi,
        ConfigFieldVisibility, ConfigSchema, Event, HostLogLevel, HostLogRecord, HttpClient,
        ImageSource, JsonSchema, LiveConfig, MatchData, Message, MessageBuilder, MessageSegment,
        MessageTarget, NuonoError, NuonoResult, PermissionLevel, Plugin, PluginActivationReason,
        PluginAgentSkill, PluginArea, PluginConfigField, PluginConfigHandle, PluginConfigSnapshot,
        PluginConfigUpdatePort, PluginConfigUpdates, PluginContext, PluginDatabaseBackend,
        PluginEventFilter, PluginMessageListener, PluginMigration, PluginMigrationForBackend,
        PluginResponse, PluginRole, PluginSecretField, PluginSecretSchema, PluginSecretStore,
        PluginServiceError, PluginServiceHandler, PluginServiceRegistry, PluginServiceResult,
        PluginSqlValue, PluginStateScope, PluginStateStore, PluginStateValue, PluginStatusMetric,
        PluginStatusRequest, PluginTaskScope, PluginWebAccessPolicy, PluginWebAsset, PluginWebPage,
        PluginWebPagePlacement, PluginWebRequest, PluginWebResponse, PluginWebRoute, Plugins,
        ReplyWaitMode, Result, RuntimeActionReceipt, RuntimeBotRoute, RuntimeMessageTarget,
        RuntimeTargetKind, SV, SendableMessage, ToolContext, ToolExecution, ToolExecutionStatus,
        ToolParameter, ToolParameterType, ToolResource, ToolResourceMode, ToolRiskLevel,
        ToolScheduling, Trigger, TriggerHandler, TriggerType, Value, anyhow, async_trait,
        plugin_secret_canonical_key, register_plugin,
    };
    pub use crate::{AssetLimits, PluginAssets, RenderLimits, RenderPool};
    pub use crate::{Bot, Command, CommandHandler};
    pub use crate::{BrowserContent, BrowserPage, BrowserRequest, BrowserWaitUntil, PluginBrowser};
    pub use crate::{
        CommandContext, CommandGroup, CommandRejection, FactoryPlugin, FromCommandContext,
        MountedLifecycle, MountedPlugin, PluginDescriptor, PluginFactory, PluginWebRouter,
        register_plugin_factory,
    };
    pub use crate::{
        ImageOutputFormat, ImageOutputSettings, ImageOutputSettingsSource, PluginImageOutput,
    };
    pub use crate::{Locale, LocaleContext, LocaleScope, LocaleService, LocaleSource};
    pub use crate::{PluginRuntime, Schedule, ScheduledTask, Subscription, Subscriptions};
    pub use crate::{nuono_command, nuono_file, nuono_fullmatch, nuono_keyword, nuono_message};
    pub use crate::{nuono_plugin, nuono_prefix, nuono_regex, nuono_suffix, nuono_tool};
}

/// 导出普通 Rust 插件工厂，由本地应用在统一构建时注册。
#[macro_export]
macro_rules! register_plugin {
    ($plugin_type:ty) => {
        pub fn nuono_create_plugin() -> Box<dyn $crate::Plugin> {
            Box::new(<$plugin_type as Default>::default())
        }
    };
}

/// 导出两段式 [`PluginFactory`] 插件入口（静态描述 + `mount(ctx)` 运行期挂载）。
#[macro_export]
macro_rules! register_plugin_factory {
    ($factory_type:ty) => {
        pub fn nuono_create_plugin() -> Box<dyn $crate::Plugin> {
            Box::new($crate::FactoryPlugin::new(
                <$factory_type as Default>::default(),
            ))
        }
    };
}

/// SeaORM 实体与查询构建 API（底层数据库连接与 SQL 驱动由宿主统一提供）。
pub use sea_orm as orm;

/// 由宿主托管的插件配置快照与带前置条件的配置补丁。
pub mod configuration {
    pub use nuono_app::configuration::{
        GroupConfigurationCandidates, OverridePatch, PlatformRoleProof, PluginConfigurationAccess,
        PluginConfigurationSnapshot,
    };
    pub use nuono_app::configuration_resources::{
        ConfigurationResource, ConfigurationResourceDefinition,
    };
}
