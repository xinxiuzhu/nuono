use crate::db::Database;
use crate::error::Result;
use crate::service::DashboardStats;

pub use nuono_storage::activity::{ActivityFilter, ActivityReport};
pub use nuono_storage::dashboard_stats::{CommandStats, get_command_stats};

pub async fn get_dashboard_stats(db: &Database) -> Result<DashboardStats> {
    get_dashboard_stats_with_runtime(db, &crate::core::bot_state::BotStateRegistry::new()).await
}

pub async fn get_dashboard_stats_with_runtime(
    db: &Database,
    runtime: &crate::core::bot_state::BotStateRegistry,
) -> Result<DashboardStats> {
    let bots = super::bots::get_bot_list_with_runtime(db, runtime).await?;
    let online_bots = bots.iter().filter(|b| b.status == "online").count();

    let counts = nuono_storage::dashboard_stats::get_dashboard_counts(db).await?;

    // bot_id 是主键；统计接入过的真实连接，排除旧版本写入的 system 内部状态。
    let bot_count = bots.len() as i64;

    Ok(DashboardStats {
        online_bots: online_bots as i64,
        total_messages: counts.total_messages,
        user_count: counts.user_count,
        group_count: counts.group_count,
        bot_count,
    })
}
