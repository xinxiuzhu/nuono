use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 消息趋势统计窗口（按调用方本地时区的自然日边界划分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageTrendRange {
    /// 今天（本地时间今日 00:00 至当前整点，按小时分桶）
    #[default]
    Today,
    /// 昨天（本地时间昨日 00:00 至 23:00，共 24 个整点小时桶）
    Yesterday,
    /// 一周（含今天在内近 7 个自然日，按天分桶）
    Week,
    /// 一个月（含今天在内近 30 个自然日，按天分桶）
    Month,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MessageTrend {
    pub time: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub received: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub sent: i64,
}
