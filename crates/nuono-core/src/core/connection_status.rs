//! 账号健康状态与身份轮询探测，生命周期与底层 WebSocket 连接绑定，不依赖是否有业务事件流入。
use super::{ApiClient, Database};
use crate::core::bot_state::BotStateRegistry;
use nuono_app::transport::TransportFrame;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

/// 轮询刷新间隔（15 秒），确保在默认 30 秒健康状态过期前为静默连接续期。
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
/// 状态探测单次查询超时（5 秒），避免协议端元数据接口卡死时让管理端干等 30 秒。
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const IDENTITY_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// 群列表后台自动同步间隔（5 分钟），避免频繁全量拉取，同时及时跟进新入群与群名变更。
const GROUP_SYNC_INTERVAL: Duration = Duration::from_secs(300);
/// 群列表同步单次查询超时（10 秒），群数量较多时比状态探测耗时略长。
const GROUP_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn refresh_status(
    client: &ApiClient,
    states: &BotStateRegistry,
    bot_id: i64,
    generation: u64,
    sender: &mpsc::Sender<TransportFrame>,
) {
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        QUERY_TIMEOUT,
        client.call("get_status", serde_json::json!({}), sender),
    )
    .await;
    let result = match &response {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            tracing::debug!(bot_id, %error, "OneBot 状态查询失败");
            Err("probe_failed".to_owned())
        }
        Err(_) => Err("probe_timeout".to_owned()),
    };
    states.probe_result_since(bot_id, generation, started, result);
}

pub(super) async fn refresh_identity(
    client: &ApiClient,
    states: &BotStateRegistry,
    db: &Database,
    bot_id: i64,
    generation: u64,
    sender: &mpsc::Sender<TransportFrame>,
) {
    let Ok(Ok(value)) = tokio::time::timeout(
        QUERY_TIMEOUT,
        client.call("get_login_info", serde_json::json!({}), sender),
    )
    .await
    else {
        return;
    };
    if let Some(nickname) = states.login_info_result(bot_id, generation, &value)
        && let Err(error) =
            nuono_storage::connections::update_bot_nickname(db, &bot_id.to_string(), &nickname)
                .await
    {
        tracing::warn!(bot_id, %error, "保存 OneBot 昵称失败");
    }
}

pub(super) async fn sync_groups(
    client: &Arc<ApiClient>,
    db: &Database,
    bot_id: i64,
    sender: &mpsc::Sender<TransportFrame>,
) {
    match tokio::time::timeout(
        GROUP_SYNC_TIMEOUT,
        crate::service::groups::sync_groups_from_bot(db, bot_id, client, sender),
    )
    .await
    {
        Ok(Ok(summary)) => {
            tracing::debug!(
                bot_id,
                reported = summary.reported,
                created = summary.created,
                updated = summary.updated,
                "已自动同步 OneBot 群列表"
            );
        }
        Ok(Err(error)) => {
            tracing::debug!(bot_id, %error, "自动同步 OneBot 群列表失败");
        }
        Err(_) => {
            tracing::debug!(bot_id, "自动同步 OneBot 群列表超时");
        }
    }
}

pub(super) async fn monitor(
    client: Arc<ApiClient>,
    states: Arc<BotStateRegistry>,
    db: Database,
    bot_id: i64,
    generation: u64,
    sender: mpsc::Sender<TransportFrame>,
    evidence_timeout: Duration,
) {
    let interval = STATUS_REFRESH_INTERVAL
        .min(evidence_timeout / 2)
        .max(Duration::from_millis(100));
    let mut identity_updated = None::<std::time::Instant>;
    let mut groups_synced = None::<std::time::Instant>;
    loop {
        if states
            .connection(bot_id)
            .is_none_or(|(current, _)| current != generation)
        {
            return;
        }
        let identity = async {
            if identity_updated.is_none_or(|at| at.elapsed() >= IDENTITY_REFRESH_INTERVAL)
                || states
                    .snapshot(bot_id)
                    .is_some_and(|bot| bot.nickname.is_none())
            {
                refresh_identity(&client, &states, &db, bot_id, generation, &sender).await;
                identity_updated = Some(std::time::Instant::now());
            }
        };
        tokio::join!(
            refresh_status(&client, &states, bot_id, generation, &sender),
            identity,
        );
        // 账号确认在线后自动同步一次群列表，之后按冷却间隔定期同步；失败也记录时间，避免对不支持该接口的协议端高频重试。
        if groups_synced.is_none_or(|at| at.elapsed() >= GROUP_SYNC_INTERVAL)
            && states
                .snapshot(bot_id)
                .is_some_and(|bot| bot.status == "online")
        {
            sync_groups(&client, &db, bot_id, &sender).await;
            groups_synced = Some(std::time::Instant::now());
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/connection_status.rs"]
mod tests;
