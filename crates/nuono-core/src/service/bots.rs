use crate::db::Database;
use crate::error::Result;
use crate::service::BotStatus;

/// 在未传入运行时在线注册表时，数据库中残留的历史 online 状态不能直接采信，必须回退标记为离线（offline）。
pub async fn get_bot_list(db: &Database) -> Result<Vec<BotStatus>> {
    get_bot_list_with_runtime(db, &crate::core::bot_state::BotStateRegistry::new()).await
}

pub async fn get_bot_list_with_runtime(
    db: &Database,
    runtime: &crate::core::bot_state::BotStateRegistry,
) -> Result<Vec<BotStatus>> {
    let rows = nuono_storage::connections::get_registered_bot_connections(db).await?;
    let mut bots: std::collections::BTreeMap<String, BotStatus> = rows
        .into_iter()
        .map(|r| {
            let mut status = crate::core::bot_state::disconnected(r.bot_id);
            status.platform = r.platform;
            status.nickname = r.nickname;
            (status.bot_id.clone(), status)
        })
        .collect();
    for mut live in runtime.snapshots() {
        if let Some(history) = bots.get(&live.bot_id) {
            live.nickname = live.nickname.or_else(|| history.nickname.clone());
        }
        bots.insert(live.bot_id.clone(), live);
    }
    Ok(bots.into_values().collect())
}

pub async fn delete_bot(db: &Database, bot_id: &str) -> Result<()> {
    db.delete_bot(bot_id).await
}

#[cfg(test)]
#[path = "../../tests/unit/service/bots.rs"]
mod tests;
