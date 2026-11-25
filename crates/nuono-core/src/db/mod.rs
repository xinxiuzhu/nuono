//! 数据库访问层重导出门面，具体连接池、实体与仓储实现位于 `nuono-storage`。
pub use nuono_storage::db::*;
pub use nuono_storage::{group_authorizations, portable};
pub use nuono_storage::{groups, plugin_global_settings, plugin_overrides};
pub(crate) mod runtime_config;
mod secrets;
pub(crate) use runtime_config::runtime_config_diff_patch;
pub use runtime_config::{ConfigRuntimePersistence, RuntimeConfigPatch};
pub use secrets::ConfigSecretPersistence;
