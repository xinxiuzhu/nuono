//! 反向 WebSocket OneBot 机器人实例的按需上下线落库与展示昵称同步。
//!
//! 每台接入过的 OneBot 机器人直接以 UUIDv7 主键、`platform = 'onebot'`、`adapter_type = 'onebot_ws_reverse'`
//! 与 `native_bot_id` 持久化在 `nuono_bot_instances` 中；实时心跳与来源 IP 仅保留在宿主内存态。

use nuono_app::bot_instances::{
    ONEBOT_WS_REVERSE_ADAPTER, RUNTIME_STATUS_ERROR, RUNTIME_STATUS_OFFLINE, RUNTIME_STATUS_ONLINE,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Statement};
use serde_json::Value;

use crate::db::Database;
use crate::entity::{bot_instances, runtime_secrets};
use crate::error::{NuonoError, Result};

const ONEBOT_PLATFORM: &str = "onebot";
const DEFAULT_NAMESPACE: &str = "";

/// 已登记的反向 WS 机器人连接记录投影（仅含持久化元数据，在线态由运行时注册表覆盖）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotConnectionRecord {
    pub instance_id: String,
    pub bot_id: String,
    pub platform: String,
    pub namespace: String,
    pub nickname: Option<String>,
    pub runtime_status: String,
    pub last_connected_at_ms: Option<i64>,
}

/// 读取全部已登记的反向 WS OneBot 实例；实时在线状态、IP 与心跳由宿主运行时合并。
pub async fn get_registered_bot_connections(db: &Database) -> Result<Vec<BotConnectionRecord>> {
    let rows = bot_instances::Entity::find()
        .filter(bot_instances::Column::AdapterType.eq(ONEBOT_WS_REVERSE_ADAPTER))
        .filter(bot_instances::Column::NativeBotId.is_not_null())
        .order_by_asc(bot_instances::Column::CreatedAtMs)
        .order_by_asc(bot_instances::Column::Id)
        .all(db.orm())
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let bot_id = row.native_bot_id?;
            if bot_id == "system" {
                return None;
            }
            let nickname = parse_nickname(&row.config_json);
            Some(BotConnectionRecord {
                instance_id: row.id,
                bot_id,
                platform: row.platform,
                namespace: row.namespace,
                nickname,
                runtime_status: row.runtime_status,
                last_connected_at_ms: row.last_connected_at_ms,
            })
        })
        .collect())
}

/// 仅在反向 WS 机器人上下线状态变化时按需落库，并在首次握手时生成 UUIDv7 实例记录。
pub async fn upsert_reverse_ws_status(
    db: &Database,
    bot_id: &str,
    runtime_status: &str,
) -> Result<()> {
    let bot_id = bot_id.trim();
    if bot_id.is_empty() || bot_id == "system" {
        return Ok(());
    }
    let runtime_status = match runtime_status.trim() {
        RUNTIME_STATUS_ONLINE => RUNTIME_STATUS_ONLINE,
        RUNTIME_STATUS_OFFLINE => RUNTIME_STATUS_OFFLINE,
        RUNTIME_STATUS_ERROR => RUNTIME_STATUS_ERROR,
        other => {
            return Err(NuonoError::Validation(format!(
                "无效的机器人运行状态: {other}"
            )));
        }
    };
    let now_ms = crate::clock::db_now().timestamp_millis();
    let connected_at_ms = (runtime_status == RUNTIME_STATUS_ONLINE).then_some(now_ms);
    let tx = db.begin_write().await?;

    if let Some(existing) = find_reverse_ws_row(&tx, bot_id).await? {
        if existing.runtime_status == runtime_status && runtime_status != RUNTIME_STATUS_ONLINE {
            tx.commit().await?;
            return Ok(());
        }
        let next_connected = connected_at_ms.or(existing.last_connected_at_ms);
        tx.execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE nuono_bot_instances \
             SET runtime_status = $1, last_connected_at_ms = $2, \
                 revision = revision + 1, updated_at_ms = $3 \
             WHERE id = $4",
            [
                runtime_status.into(),
                next_connected.into(),
                now_ms.into(),
                existing.id.into(),
            ],
        ))
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let id = uuid::Uuid::now_v7().to_string();
    let name = allocate_reverse_ws_name(&tx, bot_id, &id).await?;
    tx.execute_raw(Statement::from_sql_and_values(
        tx.get_database_backend(),
        "INSERT INTO nuono_bot_instances ( \
            id, name, platform, namespace, adapter_type, native_bot_id, \
            enabled, config_json, runtime_status, last_error, \
            last_connected_at_ms, revision, created_at_ms, updated_at_ms \
         ) VALUES ($1, $2, $3, $4, $5, $6, TRUE, '{}', $7, NULL, $8, 1, $9, $9) \
         ON CONFLICT (platform, namespace, native_bot_id) WHERE native_bot_id IS NOT NULL \
         DO UPDATE SET \
            runtime_status = excluded.runtime_status, \
            last_connected_at_ms = COALESCE(excluded.last_connected_at_ms, nuono_bot_instances.last_connected_at_ms), \
            revision = nuono_bot_instances.revision + 1, \
            updated_at_ms = excluded.updated_at_ms",
        [
            id.into(),
            name.into(),
            ONEBOT_PLATFORM.into(),
            DEFAULT_NAMESPACE.into(),
            ONEBOT_WS_REVERSE_ADAPTER.into(),
            bot_id.into(),
            runtime_status.into(),
            connected_at_ms.into(),
            now_ms.into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 仅在昵称实际变化时更新反向 WS 机器人的展示昵称（存于 `config_json.nickname`），避免周期探测空写库。
///
/// # Errors
/// 若昵称写入数据库失败则返回存储错误。
pub async fn update_bot_nickname(db: &Database, bot_id: &str, nickname: &str) -> Result<()> {
    let bot_id = bot_id.trim();
    let nickname = nickname.trim();
    if bot_id.is_empty() || bot_id == "system" || nickname.is_empty() || nickname.contains('\0') {
        return Ok(());
    }
    let truncated: String = nickname.chars().take(100).collect();

    // 事务外先做只读快速检查：昵称未变时零写事务、零 revision 递增。
    if let Some(existing) = find_reverse_ws_row(db.orm(), bot_id).await?
        && parse_nickname(&existing.config_json).as_deref() == Some(truncated.as_str())
    {
        return Ok(());
    }

    let now_ms = crate::clock::db_now().timestamp_millis();
    let tx = db.begin_write().await?;
    if let Some(existing) = find_reverse_ws_row(&tx, bot_id).await? {
        if parse_nickname(&existing.config_json).as_deref() == Some(truncated.as_str()) {
            tx.commit().await?;
            return Ok(());
        }
        let next_config = with_nickname(&existing.config_json, &truncated)?;
        tx.execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE nuono_bot_instances \
             SET config_json = $1, revision = revision + 1, updated_at_ms = $2 \
             WHERE id = $3",
            [next_config.into(), now_ms.into(), existing.id.into()],
        ))
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let id = uuid::Uuid::now_v7().to_string();
    let name = allocate_reverse_ws_name(&tx, bot_id, &id).await?;
    let config_json = with_nickname("{}", &truncated)?;
    tx.execute_raw(Statement::from_sql_and_values(
        tx.get_database_backend(),
        "INSERT INTO nuono_bot_instances ( \
            id, name, platform, namespace, adapter_type, native_bot_id, \
            enabled, config_json, runtime_status, last_error, \
            last_connected_at_ms, revision, created_at_ms, updated_at_ms \
         ) VALUES ($1, $2, $3, $4, $5, $6, TRUE, $7, $8, NULL, NULL, 1, $9, $9) \
         ON CONFLICT (platform, namespace, native_bot_id) WHERE native_bot_id IS NOT NULL \
         DO UPDATE SET \
            config_json = excluded.config_json, \
            revision = nuono_bot_instances.revision + 1, \
            updated_at_ms = excluded.updated_at_ms",
        [
            id.into(),
            name.into(),
            ONEBOT_PLATFORM.into(),
            DEFAULT_NAMESPACE.into(),
            ONEBOT_WS_REVERSE_ADAPTER.into(),
            bot_id.into(),
            config_json.into(),
            RUNTIME_STATUS_OFFLINE.into(),
            now_ms.into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 删除指定反向 WS 机器人的历史持久化实例记录及其关联密钥。
pub async fn delete_reverse_ws_bot(db: &Database, bot_id: &str) -> Result<()> {
    let bot_id = bot_id.trim();
    if bot_id.is_empty() {
        return Ok(());
    }
    let tx = db.begin_write().await?;
    if let Some(existing) = find_reverse_ws_row(&tx, bot_id).await? {
        bot_instances::Entity::delete_by_id(&existing.id)
            .exec(&tx)
            .await?;
        runtime_secrets::Entity::delete_by_id(crate::bot_instance_store::secret_key(&existing.id))
            .exec(&tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn find_reverse_ws_row<C: ConnectionTrait>(
    conn: &C,
    bot_id: &str,
) -> Result<Option<bot_instances::Model>> {
    Ok(bot_instances::Entity::find()
        .filter(bot_instances::Column::Platform.eq(ONEBOT_PLATFORM))
        .filter(bot_instances::Column::Namespace.eq(DEFAULT_NAMESPACE))
        .filter(bot_instances::Column::NativeBotId.eq(bot_id))
        .one(conn)
        .await?)
}

async fn allocate_reverse_ws_name<C: ConnectionTrait>(
    conn: &C,
    bot_id: &str,
    instance_id: &str,
) -> Result<String> {
    let preferred = format!("OneBot {bot_id}");
    let taken = bot_instances::Entity::find()
        .filter(bot_instances::Column::Name.eq(&preferred))
        .one(conn)
        .await?
        .is_some();
    if !taken {
        Ok(preferred)
    } else {
        let suffix: String = instance_id.chars().take(8).collect();
        Ok(format!("OneBot {bot_id} ({suffix})"))
    }
}

fn parse_nickname(config_json: &str) -> Option<String> {
    serde_json::from_str::<Value>(config_json)
        .ok()
        .and_then(|value| {
            value
                .get("nickname")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
}

fn with_nickname(config_json: &str, nickname: &str) -> Result<String> {
    let mut map = serde_json::from_str::<Value>(config_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    map.insert("nickname".to_owned(), Value::String(nickname.to_owned()));
    serde_json::to_string(&Value::Object(map))
        .map_err(|err| NuonoError::Serialization(format!("序列化 Bot 配置失败: {err}")))
}

#[cfg(test)]
#[path = "../tests/unit/connections.rs"]
mod tests;
