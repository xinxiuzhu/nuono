//! 机器人实例配置与持久化查询重导出。
//!
//! 应用记录存储在 `nuono-app`；数据库查询与密钥收敛存储在 `nuono-storage`。

pub use nuono_app::bot_instances::{
    BotInstance, NewBotInstance, ONEBOT_WS_REVERSE_ADAPTER, RUNTIME_STATUS_ERROR,
    RUNTIME_STATUS_OFFLINE, RUNTIME_STATUS_ONLINE, UpdateBotInstance,
};
pub use nuono_storage::bot_instance_store::{
    bind_native_bot_id, create, delete, find_by_native_id, get, list, list_all,
    update_config_and_secrets, update_status,
};
