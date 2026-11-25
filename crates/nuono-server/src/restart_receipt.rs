//! 重启完成回执：在新进程启动就绪且对应机器人连接上线后，向触发重启的聊天会话发送一次「重启完成」通知。
use chrono::{DateTime, Utc};
use nuono_core::{
    core::RuntimeActionRouter,
    core_commands::RestartReplyTarget,
    db::Database,
    domain::{DeliveryTarget, MessageSegment, OutboundAction},
    entity::global_configs,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const KEY: &str = "runtime.restart_receipt.v1";
const TTL_SECS: i64 = 30 * 60;

#[derive(Serialize, Deserialize)]
struct Receipt {
    id: uuid::Uuid,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    requested_at: DateTime<Utc>,
    reply: RestartReplyTarget,
}

pub(crate) struct PendingReceipt {
    stored: String,
    receipt: Receipt,
}

pub(crate) async fn record(db: &Database, reply: RestartReplyTarget) -> anyhow::Result<()> {
    db.set_global_config(
        KEY,
        serde_json::to_value(Receipt {
            id: uuid::Uuid::new_v4(),
            requested_at: Utc::now(),
            reply,
        })?,
    )
    .await?;
    Ok(())
}

// 在平台消息接入层启动前先从数据库读出上次遗留的重启回执，避免当前进程误消费自己刚写入的新重启请求。
pub(crate) async fn load(db: &Database) -> Option<PendingReceipt> {
    match read(db).await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "无法读取上次重启的完成通知");
            None
        }
    }
}

async fn read(db: &Database) -> anyhow::Result<Option<PendingReceipt>> {
    let Some(row) = global_configs::Entity::find_by_id(KEY)
        .one(db.orm())
        .await?
    else {
        return Ok(None);
    };
    let decoded = serde_json::from_str::<Receipt>(&row.value);
    if let Ok(receipt) = decoded {
        let age = Utc::now()
            .signed_duration_since(receipt.requested_at)
            .num_seconds();
        let reply = &receipt.reply;
        if (0..TTL_SECS).contains(&age)
            && nuono_plugin::core::runtime_transport::route_identity(&reply.route)
                .is_ok_and(|bot| bot == reply.bot)
            && reply.conversation.platform() == reply.bot.platform()
        {
            return Ok(Some(PendingReceipt {
                stored: row.value,
                receipt,
            }));
        }
    }
    claim(db, &row.value).await?;
    tracing::warn!("丢弃过期或无效的重启完成通知");
    Ok(None)
}

// 按数据库中保存的原始 JSON 字符串精确匹配删除（CAS），防止旧的等待任务误删在这期间新写入的重启回执。
async fn claim(db: &Database, stored: &str) -> anyhow::Result<bool> {
    Ok(global_configs::Entity::delete_many()
        .filter(global_configs::Column::Key.eq(KEY))
        .filter(global_configs::Column::Value.eq(stored))
        .exec(db.orm())
        .await?
        .rows_affected
        == 1)
}

pub(crate) async fn run(
    pending: Option<PendingReceipt>,
    db: Database,
    router: Arc<RuntimeActionRouter>,
    ready: Arc<AtomicBool>,
    stop: CancellationToken,
) {
    let Some(pending) = pending else { return };
    loop {
        if stop.is_cancelled() {
            return;
        }
        let age = Utc::now()
            .signed_duration_since(pending.receipt.requested_at)
            .num_seconds();
        if !(0..TTL_SECS).contains(&age) {
            if let Err(error) = claim(&db, &pending.stored).await {
                tracing::warn!(%error, "清理过期重启通知失败");
            }
            return;
        }
        if ready.load(Ordering::Acquire) && router.contains(&pending.receipt.reply.route) {
            if let Err(error) = deliver(&db, &router, pending).await {
                tracing::warn!(%error, "发送重启完成通知失败，不重复发送");
            }
            return;
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
        }
    }
}

async fn deliver(
    db: &Database,
    router: &RuntimeActionRouter,
    pending: PendingReceipt,
) -> anyhow::Result<()> {
    // 在调用平台发送消息前先核销回执：即使后续发送超时，下一次也可能实际已送达，宁可漏报也不重复刷屏。
    if !claim(db, &pending.stored).await? {
        return Ok(());
    }
    let receipt = pending.receipt;
    let elapsed = Utc::now()
        .signed_duration_since(receipt.requested_at)
        .num_milliseconds()
        .max(0) as f64
        / 1000.0;
    let action = OutboundAction::SendMessage {
        target: DeliveryTarget::conversation(receipt.reply.conversation)?,
        segments: vec![MessageSegment::text(format!(
            "Nuono 重启完成（耗时 {elapsed:.1} 秒）。"
        ))],
        reply_to: None,
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        router.notify_typed_routed(
            &receipt.reply.route,
            nuono_app::transport::TransportAction::new(receipt.reply.bot, action),
        ),
    )
    .await??;
    tracing::info!(elapsed_seconds = elapsed, "已发送 Nuono 重启完成通知");
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/restart_receipt/tests.rs"]
mod tests;
