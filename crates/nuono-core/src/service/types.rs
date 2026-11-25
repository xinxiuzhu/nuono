use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BotStatus {
    pub bot_id: String,
    pub nickname: Option<String>,
    /// online：有新鲜的账号在线证据；offline：无传输层或平台明确离线；
    /// unknown：已连接但无账号证据；degraded：在线证据过期或探测失败。
    pub status: String,
    pub ip_address: String,
    /// 最近一次收到心跳的时间（已断开连接则为历史最后心跳时间）。
    pub last_heartbeat: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    /// 客户端自报的模块健康信息可以作为在线状态的诊断附注。
    #[serde(default)]
    pub status_reason: Option<String>,
    #[serde(default)]
    pub transport_connected: Option<bool>,
    #[serde(default)]
    pub last_seen: Option<String>,
}

fn default_platform() -> String {
    "onebot".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DashboardStats {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub online_bots: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub total_messages: i64,
    /// 账号数（`users` 表）。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_count: i64,
    /// 群组数（`nuono_groups` 表）。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub group_count: i64,
    /// 接入过的 Bot 数（`bot_connections` 按 bot_id 去重）。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_count: i64,
}

pub use nuono_storage::statistics::MessageTrend;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FilterRule {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub filter_type: String,
    pub pattern: String,
    pub enabled: bool,
}
