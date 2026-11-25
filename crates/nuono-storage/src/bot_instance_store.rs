//! 多平台 Bot 实例持久化：低频配置、粗粒度在线状态、乐观锁版本，以及收敛至 `nuono_runtime_secrets` 的密钥管理。

use nuono_app::bot_instances::{
    BotInstance, NewBotInstance, ONEBOT_WS_REVERSE_ADAPTER, RUNTIME_STATUS_ERROR,
    RUNTIME_STATUS_OFFLINE, RUNTIME_STATUS_ONLINE, UpdateBotInstance,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set, Statement,
};
use serde_json::{Map, Value};

use crate::db::Database;
use crate::entity::{bot_instances, runtime_secrets};
use crate::error::{NuonoError, Result};

const MAX_CONFIG_JSON_BYTES: usize = 64 * 1024;

pub(crate) fn secret_key(instance_id: &str) -> String {
    format!("bot_instance.{instance_id}.secrets")
}

pub async fn create(db: &Database, input: NewBotInstance) -> Result<BotInstance> {
    let name = required_text("实例名称", input.name, 128)?;
    let platform = required_text("平台标识", input.platform, 64)?;
    let namespace = normalize_namespace(input.namespace)?;
    let adapter_type = required_text("适配器类型", input.adapter_type, 64)?;
    let native_bot_id = normalize_native_bot_id(input.native_bot_id)?;
    let runtime_status = validate_runtime_status(&input.runtime_status)?;
    validate_composite_key_budget(&platform, &namespace, native_bot_id.as_deref())?;
    let config_json = serialize_object_json("实例配置", &input.config)?;
    let secrets = validate_secret_object(input.secrets)?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let id = uuid::Uuid::now_v7().to_string();
    let last_connected_at_ms = (runtime_status == RUNTIME_STATUS_ONLINE).then_some(now_ms);

    let tx = db.begin_write().await?;
    let am = bot_instances::ActiveModel {
        id: Set(id.clone()),
        name: Set(name.clone()),
        platform: Set(platform),
        namespace: Set(namespace),
        adapter_type: Set(adapter_type),
        native_bot_id: Set(native_bot_id),
        enabled: Set(input.enabled),
        config_json: Set(config_json),
        runtime_status: Set(runtime_status.to_owned()),
        last_error: Set(None),
        last_connected_at_ms: Set(last_connected_at_ms),
        revision: Set(1),
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
    };
    bot_instances::Entity::insert(am)
        .exec(&tx)
        .await
        .map_err(|err| map_write_error(err, &name))?;
    write_secrets_on(&tx, &id, &secrets).await?;
    let model = find_required_on(&tx, &id).await?;
    tx.commit().await?;

    model_to_dto(model, Value::Object(secrets))
}

/// 列出由宿主托管生命周期的适配器实例（不含反向 WS 自动接入的 OneBot 连接实例）。
pub async fn list(db: &Database) -> Result<Vec<BotInstance>> {
    let rows = bot_instances::Entity::find()
        .filter(bot_instances::Column::AdapterType.ne(ONEBOT_WS_REVERSE_ADAPTER))
        .order_by_asc(bot_instances::Column::CreatedAtMs)
        .order_by_asc(bot_instances::Column::Id)
        .all(db.orm())
        .await?;
    hydrate_list(db.orm(), rows).await
}

/// 列出库内全部多平台 Bot 实例（含反向 WS 接入的实例）。
pub async fn list_all(db: &Database) -> Result<Vec<BotInstance>> {
    let rows = bot_instances::Entity::find()
        .order_by_asc(bot_instances::Column::CreatedAtMs)
        .order_by_asc(bot_instances::Column::Id)
        .all(db.orm())
        .await?;
    hydrate_list(db.orm(), rows).await
}

pub async fn get(db: &Database, id: &str) -> Result<Option<BotInstance>> {
    let Some(row) = bot_instances::Entity::find_by_id(id).one(db.orm()).await? else {
        return Ok(None);
    };
    let secrets = read_secrets_on(db.orm(), &row.id).await?;
    Ok(Some(model_to_dto(row, secrets)?))
}

pub async fn find_by_native_id(
    db: &Database,
    platform: &str,
    namespace: &str,
    native_bot_id: &str,
) -> Result<Option<BotInstance>> {
    let Some(row) = bot_instances::Entity::find()
        .filter(bot_instances::Column::Platform.eq(platform))
        .filter(bot_instances::Column::Namespace.eq(namespace))
        .filter(bot_instances::Column::NativeBotId.eq(native_bot_id))
        .one(db.orm())
        .await?
    else {
        return Ok(None);
    };
    let secrets = read_secrets_on(db.orm(), &row.id).await?;
    Ok(Some(model_to_dto(row, secrets)?))
}

pub async fn update_config_and_secrets(
    db: &Database,
    id: &str,
    input: UpdateBotInstance,
) -> Result<BotInstance> {
    let name = required_text("实例名称", input.name, 128)?;
    let platform = required_text("平台标识", input.platform, 64)?;
    let namespace = normalize_namespace(input.namespace)?;
    let adapter_type = required_text("适配器类型", input.adapter_type, 64)?;
    let native_bot_id = normalize_native_bot_id(input.native_bot_id)?;
    validate_composite_key_budget(&platform, &namespace, native_bot_id.as_deref())?;
    let config_json = serialize_object_json("实例配置", &input.config)?;

    let tx = db.begin_write().await?;
    let model = lock_required_on(&tx, id).await?;
    if let Some(expected) = input.expected_revision
        && model.revision != expected
    {
        return Err(NuonoError::Conflict(
            "Bot 实例配置已被其他操作更新，请刷新后重试".into(),
        ));
    }
    let existing_secrets = read_secrets_on(&tx, id).await?;
    let merged_secrets = merge_secrets(existing_secrets, input.secrets)?;
    let next_revision = model
        .revision
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("Bot 实例版本号已达上限".into()))?;
    let now_ms = crate::clock::db_now().timestamp_millis();

    let result = tx
        .execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE nuono_bot_instances \
             SET name = $1, platform = $2, namespace = $3, adapter_type = $4, \
                 native_bot_id = $5, enabled = $6, config_json = $7, \
                 revision = $8, updated_at_ms = $9 \
             WHERE id = $10 AND revision = $11",
            [
                name.clone().into(),
                platform.into(),
                namespace.into(),
                adapter_type.into(),
                native_bot_id.into(),
                input.enabled.into(),
                config_json.into(),
                next_revision.into(),
                now_ms.into(),
                id.into(),
                model.revision.into(),
            ],
        ))
        .await
        .map_err(|err| map_write_error(err, &name))?;
    if result.rows_affected() != 1 {
        return Err(NuonoError::Conflict(
            "Bot 实例配置已被其他操作更新，请刷新后重试".into(),
        ));
    }
    write_secrets_on(&tx, id, &merged_secrets).await?;
    let updated = find_required_on(&tx, id).await?;
    tx.commit().await?;

    model_to_dto(updated, Value::Object(merged_secrets))
}

/// 更新实例粗粒度运行态；仅在切换到 `online` 时刷新 `last_connected_at_ms`，不记录高频心跳。
pub async fn update_status(
    db: &Database,
    id: &str,
    runtime_status: &str,
    last_error: Option<&str>,
) -> Result<BotInstance> {
    let runtime_status = validate_runtime_status(runtime_status)?;
    let last_error = last_error.map(|value| value.replace('\0', ""));

    let tx = db.begin_write().await?;
    let model = lock_required_on(&tx, id).await?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let next_connected = if runtime_status == RUNTIME_STATUS_ONLINE {
        Some(now_ms)
    } else {
        model.last_connected_at_ms
    };
    let next_revision = model
        .revision
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("Bot 实例版本号已达上限".into()))?;

    let result = tx
        .execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE nuono_bot_instances \
             SET runtime_status = $1, last_error = $2, last_connected_at_ms = $3, \
                 revision = $4, updated_at_ms = $5 \
             WHERE id = $6 AND revision = $7",
            [
                runtime_status.into(),
                last_error.into(),
                next_connected.into(),
                next_revision.into(),
                now_ms.into(),
                id.into(),
                model.revision.into(),
            ],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Err(NuonoError::Conflict(
            "Bot 实例状态已被其他操作更新，请重试".into(),
        ));
    }
    let updated = find_required_on(&tx, id).await?;
    let secrets = read_secrets_on(&tx, id).await?;
    tx.commit().await?;

    model_to_dto(updated, secrets)
}

/// 握手或登录确认后回填平台侧原生 Bot ID，受 `(platform, namespace, native_bot_id)` 部分唯一索引保护。
pub async fn bind_native_bot_id(
    db: &Database,
    id: &str,
    namespace: &str,
    native_bot_id: &str,
) -> Result<BotInstance> {
    let namespace = normalize_namespace(namespace.to_owned())?;
    let native_bot_id = required_text("平台原生 Bot ID", native_bot_id.to_owned(), 256)?;

    let tx = db.begin_write().await?;
    let model = lock_required_on(&tx, id).await?;
    validate_composite_key_budget(&model.platform, &namespace, Some(&native_bot_id))?;
    if model.namespace == namespace
        && model.native_bot_id.as_deref() == Some(native_bot_id.as_str())
    {
        let secrets = read_secrets_on(&tx, id).await?;
        tx.commit().await?;
        return model_to_dto(model, secrets);
    }
    let next_revision = model
        .revision
        .checked_add(1)
        .ok_or_else(|| NuonoError::Conflict("Bot 实例版本号已达上限".into()))?;
    let now_ms = crate::clock::db_now().timestamp_millis();

    let result = tx
        .execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE nuono_bot_instances \
             SET namespace = $1, native_bot_id = $2, revision = $3, updated_at_ms = $4 \
             WHERE id = $5 AND revision = $6",
            [
                namespace.into(),
                Some(native_bot_id).into(),
                next_revision.into(),
                now_ms.into(),
                id.into(),
                model.revision.into(),
            ],
        ))
        .await
        .map_err(|err| map_write_error(err, &model.name))?;
    if result.rows_affected() != 1 {
        return Err(NuonoError::Conflict(
            "Bot 实例已被其他操作更新，请重试".into(),
        ));
    }
    let updated = find_required_on(&tx, id).await?;
    let secrets = read_secrets_on(&tx, id).await?;
    tx.commit().await?;

    model_to_dto(updated, secrets)
}

pub async fn delete(db: &Database, id: &str) -> Result<bool> {
    let tx = db.begin_write().await?;
    let result = bot_instances::Entity::delete_many()
        .filter(bot_instances::Column::Id.eq(id))
        .exec(&tx)
        .await?;
    runtime_secrets::Entity::delete_by_id(secret_key(id))
        .exec(&tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected > 0)
}

async fn hydrate_list<C: ConnectionTrait>(
    conn: &C,
    rows: Vec<bot_instances::Model>,
) -> Result<Vec<BotInstance>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<String> = rows.iter().map(|row| secret_key(&row.id)).collect();
    let secret_rows = runtime_secrets::Entity::find()
        .filter(runtime_secrets::Column::Key.is_in(keys))
        .all(conn)
        .await?;
    let mut secrets_by_key = std::collections::HashMap::with_capacity(secret_rows.len());
    for row in secret_rows {
        let raw = crate::db::secrets::decode_secret(row.value)?;
        let parsed = parse_json("实例密钥", &raw)?;
        secrets_by_key.insert(row.key, parsed);
    }
    rows.into_iter()
        .map(|row| {
            let secrets = secrets_by_key
                .remove(&secret_key(&row.id))
                .unwrap_or_else(|| Value::Object(Map::new()));
            model_to_dto(row, secrets)
        })
        .collect()
}

async fn read_secrets_on<C: ConnectionTrait>(conn: &C, id: &str) -> Result<Value> {
    let Some(row) = runtime_secrets::Entity::find_by_id(secret_key(id))
        .one(conn)
        .await?
    else {
        return Ok(Value::Object(Map::new()));
    };
    let raw = crate::db::secrets::decode_secret(row.value)?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    parse_json("实例密钥", &raw)
}

async fn write_secrets_on<C: ConnectionTrait>(
    conn: &C,
    id: &str,
    secrets: &Map<String, Value>,
) -> Result<()> {
    let key = secret_key(id);
    if secrets.is_empty() {
        runtime_secrets::Entity::delete_by_id(key)
            .exec(conn)
            .await?;
    } else {
        let encoded = serde_json::to_string(secrets)
            .map_err(|err| NuonoError::Serialization(format!("实例密钥 JSON 序列化失败: {err}")))?;
        crate::db::secrets::set_secret_on(conn, &key, &encoded).await?;
    }
    Ok(())
}

async fn find_required_on<C: ConnectionTrait>(conn: &C, id: &str) -> Result<bot_instances::Model> {
    bot_instances::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| NuonoError::NotFound(format!("Bot 实例 {id}")))
}

async fn lock_required_on<C: ConnectionTrait>(conn: &C, id: &str) -> Result<bot_instances::Model> {
    let mut query = bot_instances::Entity::find_by_id(id);
    if conn.get_database_backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    query
        .one(conn)
        .await?
        .ok_or_else(|| NuonoError::NotFound(format!("Bot 实例 {id}")))
}

fn model_to_dto(model: bot_instances::Model, secrets: Value) -> Result<BotInstance> {
    Ok(BotInstance {
        id: model.id,
        name: model.name,
        platform: model.platform,
        namespace: model.namespace,
        adapter_type: model.adapter_type,
        native_bot_id: model.native_bot_id,
        enabled: model.enabled,
        config: parse_json("实例配置", &model.config_json)?,
        secrets,
        runtime_status: model.runtime_status,
        last_error: model.last_error,
        last_connected_at_ms: model.last_connected_at_ms,
        revision: model.revision,
        created_at_ms: model.created_at_ms,
        updated_at_ms: model.updated_at_ms,
    })
}

fn required_text(field: &str, value: String, max_bytes: usize) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(NuonoError::Validation(format!("{field}不能为空")));
    }
    if value.contains('\0') {
        return Err(NuonoError::Validation(format!("{field}不能包含空字符")));
    }
    if value.len() > max_bytes {
        return Err(NuonoError::Validation(format!(
            "{field}不能超过 {max_bytes} 字节"
        )));
    }
    Ok(value.to_owned())
}

fn normalize_namespace(value: String) -> Result<String> {
    let value = value.trim();
    if value.contains('\0') {
        return Err(NuonoError::Validation("命名空间不能包含空字符".into()));
    }
    if value.len() > 256 {
        return Err(NuonoError::Validation("命名空间不能超过 256 字节".into()));
    }
    Ok(value.to_owned())
}

fn normalize_native_bot_id(value: Option<String>) -> Result<Option<String>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.contains('\0') {
        return Err(NuonoError::Validation(
            "平台原生 Bot ID 不能包含空字符".into(),
        ));
    }
    if trimmed.len() > 256 {
        return Err(NuonoError::Validation(
            "平台原生 Bot ID 不能超过 256 字节".into(),
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

fn validate_composite_key_budget(
    platform: &str,
    namespace: &str,
    native_bot_id: Option<&str>,
) -> Result<()> {
    let total = platform.len() + namespace.len() + native_bot_id.map_or(0, str::len);
    if total > 1024 {
        return Err(NuonoError::Validation(
            "平台、命名空间与原生 Bot ID 组合键不能超过 1024 字节".into(),
        ));
    }
    Ok(())
}

fn validate_runtime_status(value: &str) -> Result<&'static str> {
    match value.trim() {
        RUNTIME_STATUS_OFFLINE => Ok(RUNTIME_STATUS_OFFLINE),
        RUNTIME_STATUS_ONLINE => Ok(RUNTIME_STATUS_ONLINE),
        RUNTIME_STATUS_ERROR => Ok(RUNTIME_STATUS_ERROR),
        other => Err(NuonoError::Validation(format!(
            "无效的 Bot 实例运行状态: {other}"
        ))),
    }
}

fn parse_json(field: &str, value: &str) -> Result<Value> {
    serde_json::from_str(value)
        .map_err(|err| NuonoError::Deserialization(format!("{field} JSON 无效: {err}")))
}

fn serialize_object_json(field: &str, value: &Value) -> Result<String> {
    if !value.is_object() {
        return Err(NuonoError::Validation(format!("{field}必须是 JSON 对象")));
    }
    let encoded = serde_json::to_string(value)
        .map_err(|err| NuonoError::Serialization(format!("{field} JSON 序列化失败: {err}")))?;
    if encoded.contains('\0') {
        return Err(NuonoError::Validation(format!("{field}不能包含空字符")));
    }
    if encoded.len() > MAX_CONFIG_JSON_BYTES {
        return Err(NuonoError::Validation(format!(
            "{field}不能超过 {MAX_CONFIG_JSON_BYTES} 字节"
        )));
    }
    Ok(encoded)
}

fn validate_secret_object(value: Value) -> Result<Map<String, Value>> {
    if is_empty_secret_update(&value) {
        return Ok(Map::new());
    }
    let Value::Object(map) = value else {
        return Err(NuonoError::Validation(
            "实例密钥更新必须是 JSON 对象".to_string(),
        ));
    };
    let mut cleaned = Map::new();
    merge_secret_objects(&mut cleaned, map);
    Ok(cleaned)
}

fn merge_secrets(existing: Value, updates: Value) -> Result<Map<String, Value>> {
    let Value::Object(mut existing) = existing else {
        return Err(NuonoError::Deserialization(
            "数据库中的实例密钥必须是 JSON 对象".to_string(),
        ));
    };
    if is_empty_secret_update(&updates) {
        return Ok(existing);
    }
    let Value::Object(updates) = updates else {
        return Err(NuonoError::Validation(
            "实例密钥更新必须是 JSON 对象".to_string(),
        ));
    };

    merge_secret_objects(&mut existing, updates);
    Ok(existing)
}

fn merge_secret_objects(existing: &mut Map<String, Value>, updates: Map<String, Value>) {
    for (key, update) in updates {
        if is_empty_secret_update(&update) {
            continue;
        }

        match (existing.get_mut(&key), update) {
            (Some(Value::Object(current)), Value::Object(update)) => {
                merge_secret_objects(current, update);
            }
            (_, update) => {
                existing.insert(key, update);
            }
        }
    }
}

fn is_empty_secret_update(value: &Value) -> bool {
    matches!(value, Value::Null)
        || matches!(value, Value::String(value) if value.trim().is_empty())
        || matches!(value, Value::Object(value) if value.is_empty())
}

fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    matches!(
        err.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    )
}

fn map_write_error(err: sea_orm::DbErr, name: &str) -> NuonoError {
    if is_unique_violation(&err) {
        let message = err.to_string();
        if message.contains("idx_nuono_bot_instances_native") || message.contains("native_bot_id") {
            NuonoError::Conflict("同一平台与命名空间下该机器人原生 ID 已绑定其他实例".into())
        } else {
            NuonoError::Conflict(format!("Bot 实例名称“{name}”已存在"))
        }
    } else {
        err.into()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    async fn test_db() -> Database {
        crate::test_support::test_db(1).await
    }

    fn new_instance(name: &str) -> NewBotInstance {
        NewBotInstance {
            name: name.to_string(),
            platform: "qq_official".to_string(),
            namespace: String::new(),
            adapter_type: "qq_official".to_string(),
            native_bot_id: None,
            enabled: true,
            config: json!({"app_id": "123", "intents": 33554432}),
            secrets: json!({"app_secret": "secret", "nested": {"token": "old"}}),
            runtime_status: RUNTIME_STATUS_OFFLINE.to_string(),
        }
    }

    #[tokio::test]
    async fn crud_secrets_convergence_and_status_updates() {
        let db = test_db().await;
        let created = create(&db, new_instance("primary")).await.unwrap();

        let parsed_uuid = uuid::Uuid::parse_str(&created.id).expect("实例 ID 应为合法 UUID");
        assert_eq!(parsed_uuid.get_version_num(), 7, "新实例 ID 应为 UUIDv7");
        assert_eq!(created.name, "primary");
        assert_eq!(created.platform, "qq_official");
        assert_eq!(created.namespace, "");
        assert_eq!(created.revision, 1);
        assert_eq!(created.runtime_status, RUNTIME_STATUS_OFFLINE);
        assert_eq!(created.last_connected_at_ms, None);
        assert_eq!(created.config["app_id"], "123");
        assert_eq!(created.secrets["app_secret"], "secret");
        assert_eq!(list(&db).await.unwrap().len(), 1);
        assert_eq!(get(&db, &created.id).await.unwrap(), Some(created.clone()));

        // 验证密钥确实落盘在 nuono_runtime_secrets 中，而非 nuono_bot_instances。
        let stored_secret = db
            .get_secret(&secret_key(&created.id))
            .await
            .unwrap()
            .expect("密钥应写入 nuono_runtime_secrets");
        assert!(stored_secret.contains("secret"));

        let updated = update_config_and_secrets(
            &db,
            &created.id,
            UpdateBotInstance {
                name: "renamed".into(),
                platform: "qq_official".into(),
                namespace: String::new(),
                adapter_type: "qq_official".into(),
                native_bot_id: Some("10203040".into()),
                enabled: false,
                config: json!({"app_id": "456"}),
                secrets: json!({"app_secret": "", "nested": {"token": "new"}}),
                expected_revision: Some(created.revision),
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.name, "renamed");
        assert!(!updated.enabled);
        assert_eq!(updated.native_bot_id.as_deref(), Some("10203040"));
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.config["app_id"], "456");
        assert_eq!(updated.secrets["app_secret"], "secret");
        assert_eq!(updated.secrets["nested"]["token"], "new");

        // 过期 revision 必须触发乐观锁冲突。
        let stale_err = update_config_and_secrets(
            &db,
            &created.id,
            UpdateBotInstance {
                name: "stale".into(),
                platform: "qq_official".into(),
                namespace: String::new(),
                adapter_type: "qq_official".into(),
                native_bot_id: Some("10203040".into()),
                enabled: true,
                config: json!({"app_id": "789"}),
                secrets: json!({}),
                expected_revision: Some(1),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(stale_err, NuonoError::Conflict(_)));

        let online = update_status(&db, &created.id, RUNTIME_STATUS_ONLINE, None)
            .await
            .unwrap();
        assert_eq!(online.runtime_status, RUNTIME_STATUS_ONLINE);
        assert!(online.last_connected_at_ms.is_some());

        let failed = update_status(
            &db,
            &created.id,
            RUNTIME_STATUS_ERROR,
            Some("gateway failed"),
        )
        .await
        .unwrap();
        assert_eq!(failed.runtime_status, RUNTIME_STATUS_ERROR);
        assert_eq!(failed.last_error.as_deref(), Some("gateway failed"));
        assert_eq!(failed.last_connected_at_ms, online.last_connected_at_ms);

        assert!(delete(&db, &created.id).await.unwrap());
        assert!(!delete(&db, &created.id).await.unwrap());
        assert!(get(&db, &created.id).await.unwrap().is_none());
        assert!(
            db.get_secret(&secret_key(&created.id))
                .await
                .unwrap()
                .is_none(),
            "删除实例必须同步清理 nuono_runtime_secrets 中的对应密钥"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn partial_unique_index_prevents_duplicate_native_bot_id_but_allows_multiple_nulls() {
        let db = test_db().await;
        let first = create(&db, new_instance("bot-a")).await.unwrap();
        let second = create(&db, new_instance("bot-b")).await.unwrap();
        assert_ne!(first.id, second.id);

        let bound = bind_native_bot_id(&db, &first.id, "", "99887766")
            .await
            .unwrap();
        assert_eq!(bound.native_bot_id.as_deref(), Some("99887766"));

        let err = bind_native_bot_id(&db, &second.id, "", "99887766")
            .await
            .unwrap_err();
        assert!(matches!(err, NuonoError::Conflict(_)));

        // 不同命名空间允许相同原生 ID。
        let other_ns = bind_native_bot_id(&db, &second.id, "sandbox", "99887766")
            .await
            .unwrap();
        assert_eq!(other_ns.namespace, "sandbox");
        assert_eq!(other_ns.native_bot_id.as_deref(), Some("99887766"));

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn empty_secret_update_keeps_existing_values() {
        let db = test_db().await;
        let created = create(&db, new_instance("secret-merge")).await.unwrap();

        for empty in [Value::Null, json!({}), json!({"app_secret": "   "})] {
            let updated = update_config_and_secrets(
                &db,
                &created.id,
                UpdateBotInstance {
                    name: created.name.clone(),
                    platform: created.platform.clone(),
                    namespace: created.namespace.clone(),
                    adapter_type: created.adapter_type.clone(),
                    native_bot_id: created.native_bot_id.clone(),
                    enabled: created.enabled,
                    config: created.config.clone(),
                    secrets: empty,
                    expected_revision: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(updated.secrets["app_secret"], "secret");
            assert_eq!(updated.secrets["nested"]["token"], "old");
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_name_returns_clear_conflict_error() {
        let db = test_db().await;
        create(&db, new_instance("duplicate")).await.unwrap();

        let err = create(&db, new_instance("duplicate")).await.unwrap_err();
        assert!(matches!(err, NuonoError::Conflict(_)));
        assert!(err.to_string().contains("duplicate"));
        assert!(err.to_string().contains("已存在"));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn config_updates_preserve_runtime_state_and_status_recovery_clears_error() {
        let db = test_db().await;
        let created = create(&db, new_instance("runtime-state")).await.unwrap();
        let failed = update_status(
            &db,
            &created.id,
            RUNTIME_STATUS_ERROR,
            Some("gateway failed"),
        )
        .await
        .unwrap();

        let updated = update_config_and_secrets(
            &db,
            &created.id,
            UpdateBotInstance {
                name: "renamed".into(),
                platform: "qq_official".into(),
                namespace: String::new(),
                adapter_type: "qq_official".into(),
                native_bot_id: None,
                enabled: false,
                config: json!({"app_id": "456"}),
                secrets: json!({"nested": {"token": "new"}}),
                expected_revision: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(updated.id, created.id);
        assert_eq!(updated.created_at_ms, created.created_at_ms);
        assert_eq!(updated.runtime_status, failed.runtime_status);
        assert_eq!(updated.last_error, failed.last_error);

        let recovered = update_status(&db, &created.id, RUNTIME_STATUS_ONLINE, None)
            .await
            .unwrap();
        assert_eq!(recovered.runtime_status, RUNTIME_STATUS_ONLINE);
        assert!(recovered.last_error.is_none());
        assert!(recovered.last_connected_at_ms.is_some());
        assert_eq!(recovered.name, updated.name);
        assert_eq!(recovered.enabled, updated.enabled);
        assert_eq!(recovered.config, updated.config);
        assert_eq!(recovered.secrets, updated.secrets);
        assert_eq!(recovered.created_at_ms, created.created_at_ms);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_name_update_leaves_existing_record_unchanged() {
        let db = test_db().await;
        let created = create(&db, new_instance("primary")).await.unwrap();
        create(&db, new_instance("reserved")).await.unwrap();

        let err = update_config_and_secrets(
            &db,
            &created.id,
            UpdateBotInstance {
                name: "reserved".into(),
                platform: "qq_official".into(),
                namespace: String::new(),
                adapter_type: "qq_official".into(),
                native_bot_id: None,
                enabled: false,
                config: json!({"app_id": "456"}),
                secrets: json!({"app_secret": "replacement"}),
                expected_revision: None,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, NuonoError::Conflict(_)));
        assert!(err.to_string().contains("reserved"));
        assert!(err.to_string().contains("已存在"));
        assert_eq!(get(&db, &created.id).await.unwrap(), Some(created));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_secret_update_does_not_modify_configuration() {
        let db = test_db().await;
        let created = create(&db, new_instance("invalid-secrets")).await.unwrap();

        let err = update_config_and_secrets(
            &db,
            &created.id,
            UpdateBotInstance {
                name: "renamed".into(),
                platform: "qq_official".into(),
                namespace: String::new(),
                adapter_type: "qq_official".into(),
                native_bot_id: None,
                enabled: false,
                config: json!({"app_id": "456"}),
                secrets: json!(["replacement"]),
                expected_revision: None,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, NuonoError::Validation(_)));
        assert!(err.to_string().contains("实例密钥更新必须是 JSON 对象"));
        assert_eq!(get(&db, &created.id).await.unwrap(), Some(created));
        db.shutdown().await.unwrap();
    }
}
