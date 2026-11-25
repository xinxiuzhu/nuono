//! 应用层与存储层共享的多平台机器人实例数据记录。
//!
//! 保留配置与密钥的原始 JSON 结构；敏感密钥统一落盘至 `nuono_runtime_secrets`，
//! 对外 HTTP 响应层负责脱敏过滤。

use serde_json::Value;

/// 反向 WebSocket OneBot 连接自动登记时使用的适配器类型标识。
pub const ONEBOT_WS_REVERSE_ADAPTER: &str = "onebot_ws_reverse";

/// 机器人实例持久化的粗粒度运行状态集合。
pub const RUNTIME_STATUS_OFFLINE: &str = "offline";
pub const RUNTIME_STATUS_ONLINE: &str = "online";
pub const RUNTIME_STATUS_ERROR: &str = "error";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BotInstance {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub namespace: String,
    pub adapter_type: String,
    pub native_bot_id: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub secrets: Value,
    pub runtime_status: String,
    pub last_error: Option<String>,
    pub last_connected_at_ms: Option<i64>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NewBotInstance {
    pub name: String,
    pub platform: String,
    pub namespace: String,
    pub adapter_type: String,
    pub native_bot_id: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub secrets: Value,
    pub runtime_status: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateBotInstance {
    pub name: String,
    pub platform: String,
    pub namespace: String,
    pub adapter_type: String,
    pub native_bot_id: Option<String>,
    pub enabled: bool,
    pub config: Value,
    pub secrets: Value,
    pub expected_revision: Option<i64>,
}
