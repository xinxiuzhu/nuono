//! 消息与插件运行时的核心机制。
//!
//! 本模块包含事件管线、触发器、权限、会话、调度和插件宿主等领域能力。网络监听、
//! 数据库实例化及平台适配器选择属于 composition root，不应反向进入这些核心模块。

pub mod bot_session;
pub mod bot_state;
pub mod config_change;
pub mod connection;
pub mod event_kernel;
pub mod filter;
pub mod handler;
pub(crate) mod interaction;
pub mod matcher;
pub mod media_links;
pub(crate) mod message_log;
pub mod paths;
pub mod permission;
pub mod platform;
pub(crate) mod platform_role_verifier;
pub mod plugin_host;
pub(crate) mod runtime_outbound;
pub mod runtime_projection;
pub mod runtime_stages;
pub(crate) mod runtime_transport;
pub mod subscribe;
pub mod system_monitor;
pub mod task_executor;
pub mod template;
pub mod trigger_engine;
pub mod upstream;

pub use nuono_app::transport;
pub use nuono_plugin::core::{
    command, config_schema, cooldown, event_bus, framework_control, host_logging, model,
    onebot_client, plugin_service, runtime_action_router, stats, trigger, trigger_registry,
};

pub use crate::service::scheduler::Scheduler;
pub use bot_session::{BotSessionManager, SendActor, SendPriority, SendRequest, SessionStatus};
pub use command::{Command, CommandHandler};
pub use config_change::{
    AuditEvent, AuditEventType, AuditResult, ConfigChangeEvent, ConfigChangeManager,
    ConfigChangeType,
};
pub use connection::ConnectionManager;
pub use cooldown::CooldownTracker;
pub use event_bus::EventBus;
pub use event_kernel::{
    EventKernel, EventKernelPipeline, RuntimeActionSink, RuntimeEventContext, RuntimeEventPipeline,
    RuntimeEventProcessResult, RuntimeEventStage, RuntimeEventStageHandler,
};
pub use filter::{FilterHandler, FilterResponse};
pub use framework_control::{FrameworkControl, FrameworkControlSignal};
pub use handler::{CommandDispatchServices, EventHandler};
pub use host_logging::{HostLogBridge, HostLogLevel, HostLogRecord};
pub use matcher::{
    EventContext, HandlerResult, Matcher, MatcherRegistry, MatcherRule, Session, SessionBuilder,
    SessionManager,
};
pub use model::{
    Event as BusinessEvent, FilePayloadType, FileSource, MessageReceive,
    MessageSegment as BusinessMessageSegment, MessageSend, MessageTarget,
    TaskContext as BusinessTaskContext, UserType,
};
pub use onebot_client::ApiClient;
pub use paths::Paths;
pub use permission::{ActorPermission, PermissionLevel, PermissionManager};
pub use plugin_service::{PluginArea, Plugins, SV, SVList};
pub use runtime_action_router::{RuntimeActionReceipt, RuntimeActionRouter, RuntimeBotRoute};
pub use runtime_stages::{
    AgentRuntimeStage, CoreCommandsRuntimeStage, EventPublishRuntimeStage, FilterRuntimeStage,
    MatcherRuntimeStage, PluginHostRuntimeStage, TriggerDispatchRuntimeStage,
    WaitSessionRuntimeStage,
};
pub use stats::CommandStatsManager;
pub use subscribe::SubscribeManager;
pub use system_monitor::{SystemMonitor, SystemStatus};
pub use task_executor::{TaskExecutor, TaskFn, TaskMetadata, TaskResult, TaskStats, TaskStatus};
pub use template::TemplateManager;
pub use transport::{
    ActionTransport, BotInstanceKey, ConnectionId, ConnectionRegistry, ProtocolDescriptor,
    RegisteredConnection, TransportAction,
};
pub use trigger::{MatchData, Trigger, TriggerHandler, TriggerType};
pub use trigger_engine::{BusinessTrigger, BusinessTriggerType, TriggerEngine, TriggerMatch};
pub use trigger_registry::TriggerRegistry;

pub use config_schema::{ConfigFieldOption, ConfigFieldType, PluginConfigField};

use serde_json::Value;

/// Handler 统一响应类型
pub enum HandlerResponse {
    Continue,
    Stop,
    Reply(Value),
}

mod wait_delivery;
