//! 消息历史异步尽力落盘（best-effort）：控制入队准入容量、内存载荷总字节上限与后台写入协程生命周期，
//! 落盘至统一多平台消息表 `nuono_messages`（并按需绑定活跃 `nuono_conversations` 容器）。

use crate::{
    conversation_store::{
        DIRECTION_OUTBOUND, EnsureConversation, NewMessage, ROLE_ASSISTANT, ROLE_USER, SCOPE_GROUP,
        SCOPE_PRIVATE, SCOPE_THREAD, append_message_on, ensure_active_conversation_on,
        normalize_direction,
    },
    db::platform_identities::identities as platform_identities,
    entity::{bot_instances, groups},
    error::{NuonoError, Result},
};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::Value;
use std::{
    future::Future,
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const QUEUE_CAPACITY: usize = 2048;
const MAX_RECORD_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_RETAINED_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const FAILURE_BACKOFF: Duration = Duration::from_secs(1);

/// 消息历史异步队列的运行指标（涵盖准入、落盘与各类丢弃计数；注意 `accepted` 仅代表已进入内存队列，不代表已持久化落盘）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageHistoryStats {
    pub accepted: u64,
    pub written: u64,
    pub dropped_full: u64,
    pub dropped_byte_budget: u64,
    pub dropped_oversized: u64,
    pub dropped_closed: u64,
    pub dropped_write_failed: u64,
    pub dropped_timeout: u64,
    pub dropped_backoff: u64,
    /// 当前排队及正在写入数据库的记录所占用的 UTF-8 字符串总字节数。
    /// 该值为消息载荷大小估算，不包含结构体、内存分配器或 ORM 临时对象的额外开销。
    pub retained_payload_bytes: usize,
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: [AtomicU64; 7],
}

#[derive(Clone, Copy)]
enum DropReason {
    Full,
    ByteBudget,
    Oversized,
    Closed,
    WriteFailed,
    Timeout,
    Backoff,
}
impl DropReason {
    fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ByteBudget => "byte_budget",
            Self::Oversized => "oversized",
            Self::Closed => "closed",
            Self::WriteFailed => "write_failed",
            Self::Timeout => "timeout",
            Self::Backoff => "backoff",
        }
    }
}
impl Counters {
    fn drop_record(&self, reason: DropReason) {
        self.dropped[reason as usize].fetch_add(1, Ordering::Relaxed);
        metrics::counter!("nuono_message_history_dropped_total", "reason" => reason.label())
            .increment(1);
    }
}

#[derive(Debug)]
struct Record {
    platform: String,
    bot_id: String,
    bot_instance_hint: Option<String>,
    message_id: Option<String>,
    reply_to_message_id: Option<String>,
    message_type: String,
    user_id: Option<String>,
    group_id: Option<String>,
    sender_nickname: Option<String>,
    content: String,
    raw_message: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    direction: String,
}
impl Record {
    fn payload_bytes(&self) -> usize {
        self.bot_id.len()
            + self.message_type.len()
            + self.direction.len()
            + self.content.len()
            + self.raw_message.len()
            + [
                &self.message_id,
                &self.reply_to_message_id,
                &self.user_id,
                &self.group_id,
                &self.sender_nickname,
                &self.bot_instance_hint,
            ]
            .into_iter()
            .map(|v| v.as_ref().map_or(0, String::len))
            .sum::<usize>()
    }
}

#[derive(Debug)]
struct Pending {
    record: Record,
    // 信号量许可在数据库写入 await 期间一直持有，直到单条写入结束才释放，防止通道被迅速取空后绕过内存总字节上限。
    _payload: OwnedSemaphorePermit,
}

#[derive(Clone, Copy)]
struct Limits {
    queue: usize,
    record_bytes: usize,
    retained_bytes: usize,
    write_timeout: Duration,
    failure_backoff: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            queue: QUEUE_CAPACITY,
            record_bytes: MAX_RECORD_PAYLOAD_BYTES,
            retained_bytes: MAX_RETAINED_PAYLOAD_BYTES,
            write_timeout: WRITE_TIMEOUT,
            failure_backoff: FAILURE_BACKOFF,
        }
    }
}

#[derive(Debug)]
pub(super) struct HistoryWriter {
    tx: Mutex<Option<mpsc::Sender<Pending>>>,
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    payload: Arc<Semaphore>,
    counters: Arc<Counters>,
    max_record_bytes: usize,
    max_retained_bytes: usize,
}
impl HistoryWriter {
    pub(super) fn start(conn: DatabaseConnection) -> Arc<Self> {
        Self::start_with(Limits::default(), move |record| {
            let conn = conn.clone();
            async move { insert(&conn, record).await }
        })
    }

    fn start_with<F, Fut>(limits: Limits, mut write: F) -> Arc<Self>
    where
        F: FnMut(Record) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Pending>(limits.queue);
        let counters = Arc::new(Counters::default());
        let worker_counters = counters.clone();
        let handle = tokio::spawn(async move {
            let mut retry_after = None;
            while let Some(Pending { record, _payload }) = rx.recv().await {
                if retry_after.is_some_and(|at| tokio::time::Instant::now() < at) {
                    worker_counters.drop_record(DropReason::Backoff);
                    continue;
                }
                // 超时或失败后绝不重试可能已在服务端提交的 INSERT。
                // 写入失败后进入短暂退避冷却期，期间直接丢弃并排空积压队列，避免在关闭时为每条排队记录都卡满一次数据库超时。
                match tokio::time::timeout(limits.write_timeout, write(record)).await {
                    Ok(Ok(())) => {
                        worker_counters.written.fetch_add(1, Ordering::Relaxed);
                        metrics::counter!("nuono_message_history_written_total").increment(1);
                    }
                    result => {
                        worker_counters.drop_record(if result.is_err() {
                            DropReason::Timeout
                        } else {
                            DropReason::WriteFailed
                        });
                        retry_after = Some(tokio::time::Instant::now() + limits.failure_backoff);
                    }
                }
            }
        });
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            handle: tokio::sync::Mutex::new(Some(handle)),
            payload: Arc::new(Semaphore::new(limits.retained_bytes)),
            counters,
            max_record_bytes: limits.record_bytes,
            max_retained_bytes: limits.retained_bytes,
        })
    }

    pub(super) fn try_record(&self, bot: &str, msg: &Value, direction: &str) -> Result<()> {
        // 发送端句柄不向锁外克隆：确保关闭（close）与入队准入之间有严格一致的先后顺序，
        // 一旦关闭就不再有新生产者入队，也不会因为生产者持有所克隆的 Sender 导致后台通道无法关闭。
        // 仅当队列仍有空闲槽位时才执行带字节上限的字段拷贝与 JSON 序列化。
        let guard = self
            .tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(tx) = guard.as_ref().filter(|tx| !tx.is_closed()) else {
            return self.reject(DropReason::Closed);
        };
        if tx.capacity() == 0 {
            return self.reject(DropReason::Full);
        }
        let Some(record) = build_record(bot, msg, direction, self.max_record_bytes) else {
            return self.reject(DropReason::Oversized);
        };
        let Ok(payload) = self
            .payload
            .clone()
            .try_acquire_many_owned(record.payload_bytes() as u32)
        else {
            return self.reject(DropReason::ByteBudget);
        };
        match tx.try_send(Pending {
            record,
            _payload: payload,
        }) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("nuono_message_history_accepted_total").increment(1);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => self.reject(DropReason::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => self.reject(DropReason::Closed),
        }
    }

    fn reject(&self, reason: DropReason) -> Result<()> {
        self.counters.drop_record(reason);
        Err(NuonoError::DatabaseQuery(format!(
            "消息历史未进入尽力写入队列: {}",
            reason.label()
        )))
    }

    pub(super) fn close(&self) {
        self.tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    pub(super) async fn shutdown(&self) -> Result<()> {
        self.close();
        let mut handle = self.handle.lock().await;
        if let Some(worker) = handle.as_mut() {
            let result = worker.await;
            *handle = None;
            result.map_err(|error| {
                NuonoError::DatabaseQuery(format!("等待消息历史写入任务失败: {error}"))
            })?;
        }
        Ok(())
    }

    pub(super) fn stats(&self) -> MessageHistoryStats {
        let dropped =
            |reason: DropReason| self.counters.dropped[reason as usize].load(Ordering::Relaxed);
        MessageHistoryStats {
            accepted: self.counters.accepted.load(Ordering::Relaxed),
            written: self.counters.written.load(Ordering::Relaxed),
            dropped_full: dropped(DropReason::Full),
            dropped_byte_budget: dropped(DropReason::ByteBudget),
            dropped_oversized: dropped(DropReason::Oversized),
            dropped_closed: dropped(DropReason::Closed),
            dropped_write_failed: dropped(DropReason::WriteFailed),
            dropped_timeout: dropped(DropReason::Timeout),
            dropped_backoff: dropped(DropReason::Backoff),
            retained_payload_bytes: self.max_retained_bytes - self.payload.available_permits(),
        }
    }
}

/// 在申请内存配额前预检所有拷贝字段与 JSON 回退文本的总字节数：
/// 避免在检查大小前就调用 `to_string()` 分配超大临时字符串，且仅在缺少文本字段时才按剩余配额序列化 JSON 回退内容。
fn build_record(bot: &str, msg: &Value, direction: &str, limit: usize) -> Option<Record> {
    let mut remaining = limit;
    let mut copy = |value: &str| {
        remaining = remaining.checked_sub(value.len())?;
        Some(value.to_owned())
    };
    let bot_id = copy(bot)?;
    let normalized_direction = normalize_direction(direction);
    let direction = copy(normalized_direction)?;
    let platform = msg
        .get("platform")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .unwrap_or("onebot")
        .to_owned();
    let message_type = copy(
        msg.get("message_type")
            .and_then(Value::as_str)
            .unwrap_or("raw"),
    )?;
    let mut identity = |key| -> Option<Option<String>> {
        let value = match msg.get(key) {
            Some(Value::String(value)) => {
                // 先检查原始字节长度，避免在持有准入锁期间为了判断是不是纯空白而全量扫描超大字符串。
                if value.len() > limit {
                    return None;
                }
                if value.trim().is_empty() {
                    None
                } else {
                    Some(copy(value)?)
                }
            }
            Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
                Some(copy(&value.to_string())?)
            }
            _ => None,
        };
        Some(value)
    };
    let bot_instance_hint = identity("bot_instance_id")?;
    let message_id = identity("message_id")?;
    let reply_to_message_id = identity("reply_to_message_id")?;
    let user_id = identity("user_id")?;
    let group_id = identity("group_id")?;
    let sender_nickname = match msg
        .get("sender")
        .and_then(|v| v.get("nickname"))
        .and_then(Value::as_str)
    {
        Some(value) => Some(copy(value)?),
        None => None,
    };
    let content = msg
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| msg.get("raw_message").and_then(Value::as_str));
    let raw = msg.get("raw_message").and_then(Value::as_str);
    let content = match content {
        Some(value) => Some(copy(value)?),
        None => None,
    };
    let raw = match raw {
        Some(value) => Some(copy(value)?),
        None => None,
    };
    let missing = usize::from(content.is_none()) + usize::from(raw.is_none());
    let fallback = if let Some(limit) = remaining.checked_div(missing) {
        let mut out = LimitedJson {
            bytes: Vec::new(),
            limit,
        };
        serde_json::to_writer(&mut out, msg).ok()?;
        // serde_json 总是输出 UTF-8。
        String::from_utf8(out.bytes).ok()?
    } else {
        String::new()
    };
    Some(Record {
        platform,
        bot_id,
        bot_instance_hint,
        message_id,
        reply_to_message_id,
        message_type,
        user_id,
        group_id,
        sender_nickname,
        content: content.unwrap_or_else(|| fallback.clone()),
        raw_message: raw.unwrap_or(fallback),
        timestamp: crate::clock::db_now().into(),
        direction,
    })
}

struct LimitedJson {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("history payload exceeds limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn resolve_bot_instance_id(
    conn: &DatabaseConnection,
    platform: &str,
    hint: Option<&str>,
    bot_id: &str,
) -> Result<Option<String>> {
    let hint = hint.map(str::trim).filter(|value| !value.is_empty());
    let bot_id = {
        let trimmed = bot_id.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    if let Some(hint_id) = hint
        && let Some(row) = bot_instances::Entity::find_by_id(hint_id).one(conn).await?
    {
        return Ok(Some(row.id));
    }
    if let Some(account_id) = bot_id
        && let Some(row) = bot_instances::Entity::find()
            .filter(
                Condition::any()
                    .add(bot_instances::Column::Id.eq(account_id))
                    .add(
                        Condition::all()
                            .add(bot_instances::Column::Platform.eq(platform))
                            .add(bot_instances::Column::NativeBotId.eq(account_id)),
                    ),
            )
            .one(conn)
            .await?
    {
        return Ok(Some(row.id));
    }
    if platform != "onebot"
        && let Some(hint_id) = hint
    {
        return Ok(Some(hint_id.to_owned()));
    }
    Ok(bot_id.or(hint).map(str::to_owned))
}

async fn resolve_sender_binding(
    conn: &DatabaseConnection,
    platform: &str,
    native_sender_id: &str,
) -> Result<(Option<i64>, Option<i64>)> {
    let found = platform_identities::Entity::find()
        .filter(platform_identities::Column::Platform.eq(platform))
        .filter(platform_identities::Column::Namespace.eq(""))
        .filter(platform_identities::Column::NativeUserId.eq(native_sender_id))
        .one(conn)
        .await?;
    Ok(match found {
        Some(row) => (Some(row.id), Some(row.user_id)),
        None => (None, None),
    })
}

async fn insert(conn: &DatabaseConnection, r: Record) -> Result<()> {
    let bot_instance_id =
        resolve_bot_instance_id(conn, &r.platform, r.bot_instance_hint.as_deref(), &r.bot_id)
            .await?;

    let (sender_identity_id, sender_user_id) = match r.user_id.as_deref() {
        Some(native_uid) if r.direction != DIRECTION_OUTBOUND => {
            resolve_sender_binding(conn, &r.platform, native_uid).await?
        }
        _ => (None, None),
    };

    let (scope_type, native_chat_id, group_row_id) = match r.message_type.as_str() {
        "group" => {
            let group_row_id = match r.group_id.as_deref() {
                Some(gid) => groups::Entity::find()
                    .filter(groups::Column::Platform.eq(&r.platform))
                    .filter(groups::Column::Namespace.eq(""))
                    .filter(groups::Column::NativeGroupId.eq(gid))
                    .one(conn)
                    .await?
                    .map(|row| row.id),
                None => None,
            };
            (SCOPE_GROUP, r.group_id.clone(), group_row_id)
        }
        "channel" | "thread" => (SCOPE_THREAD, r.group_id.clone(), None),
        "private" => (SCOPE_PRIVATE, r.user_id.clone(), None),
        _ => (SCOPE_PRIVATE, None, None),
    };

    let conversation_id = if let Some(chat_id) = native_chat_id {
        let conv = ensure_active_conversation_on(
            conn,
            EnsureConversation {
                platform: r.platform.clone(),
                bot_instance_id: bot_instance_id.clone(),
                scope_type: scope_type.to_owned(),
                group_id: group_row_id,
                owner_user_id: (scope_type == SCOPE_PRIVATE)
                    .then_some(sender_user_id)
                    .flatten(),
                native_chat_id: chat_id,
                native_thread_id: String::new(),
                title: None,
            },
        )
        .await?;
        Some(conv.id)
    } else {
        None
    };

    let role = if r.direction == DIRECTION_OUTBOUND {
        ROLE_ASSISTANT
    } else {
        ROLE_USER
    };
    let raw_payload_json = (!r.raw_message.is_empty()).then_some(r.raw_message);

    append_message_on(
        conn,
        NewMessage {
            conversation_id,
            platform: r.platform,
            bot_instance_id,
            native_message_id: r.message_id,
            reply_to_native_message_id: r.reply_to_message_id,
            sender_user_id,
            sender_identity_id,
            native_sender_id: r.user_id,
            native_group_id: r.group_id,
            sender_nickname: r.sender_nickname,
            direction: r.direction,
            role: role.to_owned(),
            text_content: r.content,
            content_blocks_json: "[]".to_owned(),
            raw_payload_json,
            occurred_at_ms: r.timestamp.timestamp_millis(),
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/db/message_history/tests.rs"]
mod tests;
