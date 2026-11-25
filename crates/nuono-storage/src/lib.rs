//! 存储层具体持久化实现：封装 SeaORM 数据库查询、表结构定义、迁移与事务。
//! 上层应用接口约定（`nuono-app`）不直接暴露这些底层连接或事务类型。
pub mod activity;
pub mod ai_usage;
pub mod audit;
pub mod bot_instance_store;
pub mod clock;
mod configuration_resources;
pub mod connections;
pub mod conversation_store;
pub mod dashboard_stats;
pub mod db;
pub mod entitlements;
pub mod entity;
pub mod group_authorizations;
pub mod groups;
pub mod identity_resolver;
pub mod messages;
mod plugin_config_migration;
pub mod plugin_debug;
pub mod plugin_global_settings;
pub mod plugin_overrides;
pub mod plugins;
pub mod portable;
pub mod security;
mod service_policy;
pub mod statistics;
pub mod status_commands;
pub mod status_stats;
pub mod subscriptions;
pub mod task_runs;
pub mod tasks;
pub mod test_support;
pub mod config {
    pub use nuono_app::settings::DatabaseConfig;
}
pub mod error {
    pub use nuono_error::*;
}
pub use db::Database;

#[async_trait::async_trait]
impl nuono_app::permission::PermissionStore for Database {
    async fn permission_settings(
        &self,
    ) -> std::result::Result<nuono_app::permission::PermissionSettings, String> {
        Ok(nuono_app::permission::PermissionSettings {
            banned_users: self
                .get_global_config("banned_users")
                .await
                .map_err(|e| e.to_string())?,
        })
    }

    async fn bound_account_role(
        &self,
        platform: Option<&str>,
        namespace: Option<&str>,
        user_id: &str,
    ) -> std::result::Result<Option<nuono_app::auth::Role>, String> {
        use sea_orm::{ConnectionTrait, Statement};
        let Some(platform) = platform.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let Some(namespace) = namespace else {
            return Ok(None);
        };
        let sql = if self.backend() == sea_orm::DbBackend::Sqlite {
            "SELECT u.role FROM nuono_platform_identities pi \
             JOIN nuono_users u ON u.id = pi.user_id \
             WHERE pi.platform = ?1 AND pi.namespace = ?2 AND pi.native_user_id = ?3 \
               AND pi.verified_at_ms IS NOT NULL AND u.status = 'active' AND u.account_kind = 'registered'"
        } else {
            "SELECT u.role FROM nuono_platform_identities pi \
             JOIN nuono_users u ON u.id = pi.user_id \
             WHERE pi.platform = $1 AND pi.namespace = $2 AND pi.native_user_id = $3 \
               AND pi.verified_at_ms IS NOT NULL AND u.status = 'active' AND u.account_kind = 'registered'"
        };
        let rows = self
            .orm()
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                sql,
                [
                    platform.to_owned().into(),
                    namespace.to_owned().into(),
                    user_id.to_owned().into(),
                ],
            ))
            .await
            .map_err(|e| e.to_string())?;
        let mut best: Option<nuono_app::auth::Role> = None;
        for row in rows {
            let role_text: String = row.try_get("", "role").map_err(|e| e.to_string())?;
            if let Some(role) = nuono_app::auth::Role::parse(&role_text) {
                best =
                    match (best, role) {
                        (Some(nuono_app::auth::Role::Owner), _)
                        | (_, nuono_app::auth::Role::Owner) => Some(nuono_app::auth::Role::Owner),
                        (Some(nuono_app::auth::Role::Admin), _)
                        | (_, nuono_app::auth::Role::Admin) => Some(nuono_app::auth::Role::Admin),
                        _ => Some(nuono_app::auth::Role::User),
                    };
            }
        }
        Ok(best)
    }
}

mod agent_store;
mod memory_embeddings;
mod memory_store;
mod persona_store;

mod account_store;

mod localization;
mod preferences;

mod plugin_data;
pub mod plugin_identity;
pub mod plugin_kv;

mod registration_store;

mod filter_store;

mod group_store;
mod group_sync;

#[cfg(test)]
mod portable_business_tests;

mod plugin_group_access;
