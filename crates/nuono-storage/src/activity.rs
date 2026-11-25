//! 活动统计计数器：带容量上限、不保存消息正文。
//! 完整保留带平台命名空间的身份标识（重启后不丢失），不采用旧兼容层有损的数字 ID 转换。
mod report;
pub use report::{ActivityFilter, ActivityReport};

use crate::{
    Database,
    entity::activity_daily as rows,
    error::{NuonoError, Result},
};
use chrono::{DateTime, Duration, Utc};
use nuono_domain::{ConversationRef, DomainEvent};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
    sea_query::{Alias, Expr, ExprTrait, OnConflict},
};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug)]
pub enum ActivityKind {
    Message,
    Command,
    Rule,
    Listener,
    Event,
    Reply,
}
impl ActivityKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Command => "command",
            Self::Rule => "rule",
            Self::Listener => "listener",
            Self::Event => "event",
            Self::Reply => "reply",
        }
    }
}

#[derive(Debug)]
struct Record(rows::ActiveModel);
fn fingerprint(value: &impl serde::Serialize) -> Option<String> {
    serde_json::to_vec(value)
        .ok()
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
}
impl Record {
    fn new(
        event: &DomainEvent,
        plugin: &str,
        kind: ActivityKind,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        let message = event.message()?;
        if event.bot.platform().as_str().len() > 64
            || plugin.len() > 128
            || plugin.chars().any(char::is_control)
            || (!matches!(kind, ActivityKind::Message) && plugin.is_empty())
        {
            return None;
        }
        let actor = &message.sender;
        let user_key = fingerprint(actor)?;
        let bot_key = fingerprint(&event.bot)?;
        let (scene, scope_id) = match event.conversation.as_ref() {
            Some(ConversationRef::Group { group }) => ("group", group.value().as_str()),
            Some(ConversationRef::Channel { channel, .. }) => ("channel", channel.value().as_str()),
            _ => ("private", ""),
        };
        let scope_key = fingerprint(&event.conversation)?;
        let day = now.format("%Y-%m-%d").to_string();
        let bucket_key =
            fingerprint(&(&day, &bot_key, &user_key, &scope_key, plugin, kind.as_str()))?;
        Some(Self(rows::ActiveModel {
            bucket_key: Set(bucket_key),
            day: Set(day),
            platform: Set(event.bot.platform().as_str().into()),
            bot_id: Set(event.bot.instance_id.as_str().into()),
            user_key: Set(user_key),
            user_id: Set(actor.value().as_str().into()),
            namespace: Set(actor
                .namespace()
                .as_ref()
                .map_or("", |id| id.as_str())
                .into()),
            scope_key: Set(scope_key),
            scope_id: Set(scope_id.into()),
            scene: Set(scene.into()),
            plugin: Set(plugin.into()),
            kind: Set(kind.as_str().into()),
            count: Set(1),
        }))
    }
}
enum Command {
    Record(Box<Record>),
    Flush(oneshot::Sender<()>),
}
#[derive(Debug)]
pub(crate) struct ActivityWriter {
    tx: Mutex<Option<mpsc::Sender<Command>>>,
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    lost: Arc<AtomicU64>,
}
impl ActivityWriter {
    pub(crate) fn start(conn: DatabaseConnection) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel(2048);
        let lost = Arc::new(AtomicU64::new(0));
        let dropped = lost.clone();
        let handle = tokio::spawn(async move {
            let mut cleaned_day = String::new();
            let mut retry_after = None;
            while let Some(command) = rx.recv().await {
                let Command::Record(record) = command else {
                    if let Command::Flush(done) = command {
                        let _ = done.send(());
                    }
                    continue;
                };
                let mut batch = vec![*record];
                let mut barrier = None;
                while batch.len() < 128 {
                    match rx.try_recv() {
                        Ok(Command::Record(record)) => batch.push(*record),
                        Ok(Command::Flush(done)) => {
                            barrier = Some(done);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if retry_after.is_some_and(|at| std::time::Instant::now() < at) {
                    dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
                } else {
                    let now = crate::clock::db_now();
                    let day = now.format("%Y-%m-%d").to_string();
                    let clean = day != cleaned_day;
                    let count = batch.len() as u64;
                    let result =
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            let tx = conn.begin().await?;
                            if clean {
                                rows::Entity::delete_many()
                                    .filter(rows::Column::Day.lt(
                                        (now - Duration::days(89)).format("%Y-%m-%d").to_string(),
                                    ))
                                    .exec(&tx)
                                    .await?;
                            }
                            for record in batch {
                                rows::Entity::insert(record.0)
                                    .on_conflict(
                                        OnConflict::column(rows::Column::BucketKey)
                                            .value(
                                                rows::Column::Count,
                                                Expr::col((
                                                    Alias::new("nuono_activity_daily"),
                                                    rows::Column::Count,
                                                ))
                                                .add(1),
                                            )
                                            .to_owned(),
                                    )
                                    .exec_without_returning(&tx)
                                    .await?;
                            }
                            tx.commit().await
                        })
                        .await;
                    if matches!(result, Ok(Ok(()))) {
                        cleaned_day = day;
                    } else {
                        dropped.fetch_add(count, Ordering::Relaxed);
                        retry_after =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                        tracing::warn!("活动统计写入失败，部分记录未计入；稍后重试新记录");
                    }
                }
                if let Some(done) = barrier {
                    let _ = done.send(());
                }
            }
        });
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            handle: tokio::sync::Mutex::new(Some(handle)),
            lost,
        })
    }
    fn observe(&self, record: Record) {
        let accepted = self
            .tx
            .lock()
            .ok()
            .and_then(|tx| {
                tx.as_ref()
                    .map(|tx| tx.try_send(Command::Record(Box::new(record))).is_ok())
            })
            .unwrap_or(false);
        if !accepted {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
    async fn flush(&self) -> Result<()> {
        let sender = self.tx.lock().ok().and_then(|tx| tx.clone());
        if let Some(sender) = sender {
            let (done, completed) = oneshot::channel();
            tokio::time::timeout(std::time::Duration::from_secs(6), async {
                sender
                    .send(Command::Flush(done))
                    .await
                    .map_err(|_| NuonoError::DatabaseQuery("统计队列已关闭".into()))?;
                completed
                    .await
                    .map_err(|_| NuonoError::DatabaseQuery("统计任务已退出".into()))
            })
            .await
            .map_err(|_| NuonoError::Timeout("统计记录刷新超时".into()))??;
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        let mut handle = self.handle.lock().await;
        if let Some(worker) = handle.as_mut() {
            let result = worker.await;
            *handle = None;
            result.map_err(|_| NuonoError::DatabaseQuery("等待统计记录任务失败".into()))?;
        }
        Ok(())
    }
}
impl Database {
    /// 仅尝试写入内存有界队列，不阻塞等待数据库落盘，也不影响正常的消息分发流程。
    pub fn record_activity(&self, event: &DomainEvent, plugin: &str, kind: ActivityKind) {
        if let Some(record) = Record::new(event, plugin, kind, crate::clock::db_now().into()) {
            self.activity.observe(record);
        } else if event.message().is_some() {
            self.activity.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/activity/tests.rs"]
mod tests;
