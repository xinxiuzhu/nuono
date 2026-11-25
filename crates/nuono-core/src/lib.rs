//! Nuono 的运行时内核与基础设施装配。
//!
//! 该 crate 按依赖方向拆分为：协议/实体与配置、核心事件与插件运行时、面向用例的
//! `service`，以及对外部平台的 `adapter`。可执行程序负责选择具体实现并把这些部件装配
//! 起来；库层不读取命令行参数，也不自行启动监听器。

pub mod adapter;
pub mod agent;
pub mod ai;
pub mod api;
pub mod audit;
pub mod auth;
pub mod clock;
pub mod config;
pub mod core;
pub mod core_commands;
pub mod db;
pub mod domain;
pub mod entity;
pub mod error;
pub mod localization;
pub mod media;
pub mod plugin;
pub mod service;
pub mod telemetry;

/// 测试基建（PG 独立测试库），供本 crate 与下游 crate 的集成测试复用。
pub mod test_support;

pub use auth::{CallerIdentity, Capability, Role, Scope};
pub use config::ConfigManager;
pub use core::event_bus::EventBus;
pub use core::system_monitor::{SystemMonitor, SystemStatus};
pub use db::Database;
pub use error::{NuonoError, Result};
pub use nuono_storage::conversation_store;

pub use ai::{
    AiConfig, AiError, AiService, ChatMessage, ChatRequest, ChatResponse, ChatTool, ChatToolCall,
    ConversationManager, OpenAiCompatibleProvider, ProviderType, parse_provider_type,
};

pub use agent::{
    AgentContext, AgentPolicy, AgentResponse, AgentRuntime, AgentTool, AgentToolContext,
    AutonomyLevel, BuiltinToolRegistry, ToolExecution, ToolExecutionStatus, ToolParameter,
    ToolParameterType, ToolRegistry, ToolRiskLevel,
};

pub mod onebot;
