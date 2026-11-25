//! Nuono 应用层核心业务逻辑与抽象接口（与底层数据库存储及具体聊天平台实现解耦）。
//!
//! 上层宿主负责注入事件处理阶段与协议传输层；本 crate 负责事件管线编排、消费状态与故障分类、Agent 运行时以及出站动作的校验与鉴权分发。

pub mod action;
pub mod business_runtime;
pub mod media;
pub mod pipeline;
pub mod rich_message;
pub mod transport;

pub mod auth;
pub mod permission;

pub mod settings;

pub mod entitlement;
pub mod plugin_extensions;

pub mod agent;
pub mod ai;
pub mod model_catalog;
pub mod persona;

pub mod clock;

pub mod adapter;
pub mod bot_instances;
pub mod config_schema;
pub mod config_validation;
pub mod configuration;
pub mod configuration_resources;
pub mod platform;

pub mod legacy_action;

pub mod accounts;
pub mod assistant;
pub mod companion;
pub mod companion_data;
pub mod personal_profile;

pub mod image_output;
pub mod localization;
pub mod message_interactions;
pub mod preferences;

pub mod plugin_data;
pub mod plugin_manifest;
pub mod plugin_role;

pub mod account_security;

pub mod account_registration;

pub mod filter;

pub mod group_authorization;
pub mod groups;

pub mod account_provisioning;

pub mod web_settings;

pub mod account_merge;
pub mod managed_messages;
pub mod plugin_identity;
pub mod scheduled_tasks;
