//! 面向应用用例的服务层。
//!
//! 服务在核心模型与数据库/平台能力之上组织审计、管理 API 和订阅等流程；具体
//! HTTP 路由和进程生命周期由 `nuono-server` 的 composition root 负责。

pub mod ai_models;
pub mod assistant;
pub mod bot_instances;
pub mod bots;
pub mod discovery;
pub mod group_plugins;
pub mod groups;
pub mod log_files;
pub mod logs;
pub mod mail;
pub mod managed_messages;
pub mod messages;
pub mod plugin_configuration;
pub mod plugin_debug;
pub mod plugins;
pub mod privacy_journal;
pub mod pubsub;
pub mod scheduler;
pub mod security;
pub mod stats;
pub mod tasks;
pub mod types;
pub mod users;

pub use types::*;

pub mod account_mail;
