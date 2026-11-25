//! 表行的普通结构体（`Model`），字段与迁移里的列一一对应。
//!
//! 各 `Model` 已带 SeaORM `DeriveEntityModel`：同模块生成 `Entity` / `Column` /
//! `Relation` / `PrimaryKey` / `ActiveModel`，DAO 可直接走 ORM。`users` 的公开 `Model`
//! 为认证服务提供便利视图，完整列实体在 `users::orm` 子模块里。

pub mod account_assistant;
pub mod activity_daily;
pub mod agent_memories;
pub mod agent_memory_embeddings;
pub mod agent_memory_graphs;
pub mod agent_policy_rules;
pub mod agent_runs;
pub mod agent_tool_approvals;
pub mod agent_tool_receipts;
pub mod ai_models;
pub mod ai_persona_bindings;
pub mod ai_personas;
pub mod ai_runs;
pub mod ai_usage_events;
pub mod assistant_turns;
pub mod audit_events;
pub mod bot_instances;
pub mod companion_memory_sources;
pub mod conversations;
pub mod entitlements;
pub mod filter_rules;
pub mod global_configs;
pub mod groups;
pub mod messages;
pub mod notifications;
pub mod plugin_kv;
pub mod plugin_overrides;
pub mod plugins;
pub mod runtime_secrets;
pub mod scheduled_task_runs;
pub mod scheduled_tasks;
pub mod subscriptions;
pub mod users;
