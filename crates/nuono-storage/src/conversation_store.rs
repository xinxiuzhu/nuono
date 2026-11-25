//! 统一聊天会话容器（`nuono_conversations`）、多平台消息流水（`nuono_messages`）与 AI 推理轨迹（`nuono_ai_runs`）持久化。

use nuono_app::agent::{
    account_chat_owner,
    input::{HistoryMessage, USER_CONTEXT_ROLE},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, Statement,
};
use serde_json::Value;

use crate::db::Database;
use crate::entity::{ai_runs, conversations, messages, users};
use crate::error::{NuonoError, Result};

pub const PLATFORM_WEB: &str = "web";
pub const SCOPE_WEB: &str = "web";
pub const SCOPE_PRIVATE: &str = "private";
pub const SCOPE_GROUP: &str = "group";
pub const SCOPE_GROUP_USER: &str = "group_user";
pub const SCOPE_THREAD: &str = "thread";

pub const CONVERSATION_STATUS_ACTIVE: &str = "active";
pub const CONVERSATION_STATUS_ARCHIVED: &str = "archived";

pub const DIRECTION_INBOUND: &str = "inbound";
pub const DIRECTION_OUTBOUND: &str = "outbound";

pub const ROLE_USER: &str = "user";
pub const ROLE_ASSISTANT: &str = "assistant";
pub const ROLE_SYSTEM: &str = "system";

pub const AI_RUN_STATUS_RUNNING: &str = "running";
pub const AI_RUN_STATUS_SUCCEEDED: &str = "succeeded";
pub const AI_RUN_STATUS_FAILED: &str = "failed";
pub const AI_RUN_STATUS_CANCELLED: &str = "cancelled";

/// 供 AI 对话读取与历史接口复用的消息投影（底层来自 `nuono_conversations` 与 `nuono_messages`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMessageRow {
    pub id: i64,
    pub conversation_id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub timestamp: Option<crate::clock::UtcMillis>,
}

/// 创建或定位活跃会话的输入参数。
#[derive(Clone, Debug)]
pub struct EnsureConversation {
    pub platform: String,
    pub bot_instance_id: Option<String>,
    pub scope_type: String,
    pub group_id: Option<i64>,
    pub owner_user_id: Option<i64>,
    pub native_chat_id: String,
    pub native_thread_id: String,
    pub title: Option<String>,
}

/// 写入一条多平台或 Web 端消息的输入参数。
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub conversation_id: Option<String>,
    pub platform: String,
    pub bot_instance_id: Option<String>,
    pub native_message_id: Option<String>,
    pub reply_to_native_message_id: Option<String>,
    pub sender_user_id: Option<i64>,
    pub sender_identity_id: Option<i64>,
    pub native_sender_id: Option<String>,
    pub native_group_id: Option<String>,
    pub sender_nickname: Option<String>,
    pub direction: String,
    pub role: String,
    pub text_content: String,
    pub content_blocks_json: String,
    pub raw_payload_json: Option<String>,
    pub occurred_at_ms: i64,
}

/// 启动一条 AI 推理记录的输入参数。
#[derive(Clone, Debug)]
pub struct NewAiRun {
    pub id: Option<String>,
    pub conversation_id: String,
    pub trigger_message_id: Option<i64>,
    pub user_id: Option<i64>,
    pub provider: String,
    pub model: String,
}

/// 结束一条 AI 推理记录时的回填参数。
#[derive(Clone, Debug)]
pub struct FinishAiRun {
    pub reply_message_id: Option<i64>,
    pub status: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub latency_ms: i32,
    pub steps_json: String,
    pub error_message: Option<String>,
}

/// 直接写入一条已结算 AI 推理记录的输入参数。
#[derive(Clone, Debug)]
pub struct CompletedAiRun {
    pub id: Option<String>,
    pub conversation_id: String,
    pub trigger_message_id: Option<i64>,
    pub reply_message_id: Option<i64>,
    pub user_id: Option<i64>,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub latency_ms: i32,
    pub steps_json: String,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

/// 归一化消息方向，仅允许 `inbound` 与 `outbound`。
pub fn normalize_direction(direction: &str) -> &'static str {
    match direction.trim().to_ascii_lowercase().as_str() {
        "outbound" | "send" | "out" => DIRECTION_OUTBOUND,
        _ => DIRECTION_INBOUND,
    }
}

/// 解析运行时会话键，映射到统一的平台标识、范围类型与归属用户。
pub(crate) fn parse_session_key(session_id: &str) -> (String, String, Option<i64>) {
    if let Some(uid) = account_chat_owner(session_id) {
        return (PLATFORM_WEB.to_owned(), SCOPE_WEB.to_owned(), Some(uid));
    }
    let known_platforms = ["onebot", "qq_official", "feishu", "telegram", "discord"];
    for platform in known_platforms {
        if let Some(rest) = session_id.strip_prefix(&format!("{platform}:")) {
            let scope = if rest.contains(":group:") {
                SCOPE_GROUP
            } else if rest.contains(":thread:") || rest.contains(":channel:") {
                SCOPE_THREAD
            } else {
                SCOPE_PRIVATE
            };
            return (platform.to_owned(), scope.to_owned(), None);
        }
    }
    (PLATFORM_WEB.to_owned(), SCOPE_WEB.to_owned(), None)
}

/// 将上层对话角色与内容编码为 `nuono_messages` 的规范列（保持 `role` 落在允许枚举内，不重复存放纯文本）。
pub(crate) fn encode_message_payload(
    role: &str,
    content: &str,
) -> Result<(&'static str, String, String)> {
    if role == USER_CONTEXT_ROLE {
        let text_content = HistoryMessage::decode(role, content)
            .map(|msg| msg.content)
            .unwrap_or_else(|| content.to_owned());
        let blocks = serde_json::to_string(&serde_json::json!([
            {"type": "user_context", "raw": content}
        ]))?;
        return Ok((ROLE_USER, text_content, blocks));
    }
    let db_role = match role {
        ROLE_ASSISTANT => ROLE_ASSISTANT,
        ROLE_SYSTEM => ROLE_SYSTEM,
        _ => ROLE_USER,
    };
    Ok((db_role, content.to_owned(), "[]".to_owned()))
}

/// 将 `nuono_messages` 中的规范列还原为 AI 对话上下文使用的角色与正文。
pub(crate) fn decode_message_payload(row: &messages::Model) -> (String, String) {
    if row.content_blocks_json != "[]"
        && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&row.content_blocks_json)
        && let Some(raw) = items.first().and_then(|item| {
            (item.get("type").and_then(Value::as_str) == Some(USER_CONTEXT_ROLE))
                .then(|| item.get("raw").and_then(Value::as_str))
                .flatten()
        })
    {
        return (USER_CONTEXT_ROLE.to_owned(), raw.to_owned());
    }
    (row.role.clone(), row.text_content.clone())
}

/// 在调用方事务或连接上定位或创建活跃会话容器。
pub async fn ensure_active_conversation_on<C: ConnectionTrait>(
    conn: &C,
    input: EnsureConversation,
) -> Result<conversations::Model> {
    let platform = required_text("平台标识", &input.platform, 64)?;
    let scope_type = validate_scope_type(&input.scope_type)?;
    let native_chat_id = sanitize_text(&input.native_chat_id, 1024)?;
    let native_thread_id = sanitize_text(&input.native_thread_id, 256)?;
    let bot_instance_id = input
        .bot_instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| required_text("Bot 实例 ID", value, 256))
        .transpose()?;
    let title = input
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_utf8_bytes(value.replace('\0', "").as_str(), 256));

    let mut query = conversations::Entity::find()
        .filter(conversations::Column::Platform.eq(&platform))
        .filter(conversations::Column::ScopeType.eq(scope_type))
        .filter(conversations::Column::NativeChatId.eq(&native_chat_id))
        .filter(conversations::Column::NativeThreadId.eq(&native_thread_id))
        .filter(conversations::Column::Status.eq(CONVERSATION_STATUS_ACTIVE));
    query = match &bot_instance_id {
        Some(bot_id) => query.filter(conversations::Column::BotInstanceId.eq(bot_id)),
        None => query.filter(conversations::Column::BotInstanceId.is_null()),
    };
    if let Some(existing) = query
        .order_by_desc(conversations::Column::CreatedAtMs)
        .one(conn)
        .await?
    {
        return Ok(existing);
    }

    let owner_user_id = match input.owner_user_id {
        Some(uid)
            if users::orm::Entity::find_by_id(uid)
                .one(conn)
                .await?
                .is_some() =>
        {
            Some(uid)
        }
        _ => None,
    };

    let now_ms = crate::clock::db_now().timestamp_millis();
    let id = uuid::Uuid::now_v7().to_string();
    let model = conversations::ActiveModel {
        id: Set(id),
        platform: Set(platform),
        bot_instance_id: Set(bot_instance_id),
        scope_type: Set(scope_type.to_owned()),
        group_id: Set(input.group_id),
        owner_user_id: Set(owner_user_id),
        native_chat_id: Set(native_chat_id),
        native_thread_id: Set(native_thread_id),
        title: Set(title),
        status: Set(CONVERSATION_STATUS_ACTIVE.to_owned()),
        summary_text: Set(None),
        last_message_at_ms: Set(None),
        created_at_ms: Set(now_ms),
        updated_at_ms: Set(now_ms),
    }
    .insert(conn)
    .await?;
    Ok(model)
}

/// 根据运行时会话键定位或创建对应的活跃 `nuono_conversations` 记录。
pub async fn ensure_session_conversation_on<C: ConnectionTrait>(
    conn: &C,
    session_id: &str,
) -> Result<conversations::Model> {
    let (platform, scope_type, owner_user_id) = parse_session_key(session_id);
    ensure_active_conversation_on(
        conn,
        EnsureConversation {
            platform,
            bot_instance_id: None,
            scope_type,
            group_id: None,
            owner_user_id,
            native_chat_id: session_id.to_owned(),
            native_thread_id: String::new(),
            title: None,
        },
    )
    .await
}

/// 按运行时会话键查找已有活跃会话容器（不触发创建）。
pub async fn find_session_conversation_on<C: ConnectionTrait>(
    conn: &C,
    session_id: &str,
) -> Result<Option<conversations::Model>> {
    let (platform, scope_type, _) = parse_session_key(session_id);
    Ok(conversations::Entity::find()
        .filter(conversations::Column::Platform.eq(platform))
        .filter(conversations::Column::BotInstanceId.is_null())
        .filter(conversations::Column::ScopeType.eq(scope_type))
        .filter(conversations::Column::NativeChatId.eq(session_id))
        .filter(conversations::Column::NativeThreadId.eq(""))
        .filter(conversations::Column::Status.eq(CONVERSATION_STATUS_ACTIVE))
        .order_by_desc(conversations::Column::CreatedAtMs)
        .one(conn)
        .await?)
}

/// 归档指定会话（用于 `/new` 重置上下文而不丢失历史记录）。
pub async fn archive_conversation(db: &Database, conversation_id: &str) -> Result<bool> {
    let now_ms = crate::clock::db_now().timestamp_millis();
    let result = conversations::Entity::update_many()
        .set(conversations::ActiveModel {
            status: Set(CONVERSATION_STATUS_ARCHIVED.to_owned()),
            updated_at_ms: Set(now_ms),
            ..Default::default()
        })
        .filter(conversations::Column::Id.eq(conversation_id))
        .filter(conversations::Column::Status.eq(CONVERSATION_STATUS_ACTIVE))
        .exec(db.orm())
        .await?;
    Ok(result.rows_affected > 0)
}

/// 按归属用户与平台列出活跃会话（用于 Web 端侧边栏历史对话列表）。
pub async fn list_user_conversations(
    db: &Database,
    owner_user_id: i64,
    platform: &str,
    limit: u64,
) -> Result<Vec<conversations::Model>> {
    Ok(conversations::Entity::find()
        .filter(conversations::Column::OwnerUserId.eq(owner_user_id))
        .filter(conversations::Column::Platform.eq(platform))
        .filter(conversations::Column::Status.eq(CONVERSATION_STATUS_ACTIVE))
        .order_by_desc(conversations::Column::LastMessageAtMs)
        .order_by_desc(conversations::Column::CreatedAtMs)
        .limit(limit.clamp(1, 200))
        .all(db.orm())
        .await?)
}

/// 追加一条消息到 `nuono_messages`；若携带 `(bot_instance_id, native_message_id)` 则按部分唯一索引幂等去重。
/// 返回 `Ok(Some(id))` 表示已新写入，`Ok(None)` 表示命中平台重复事件去重并跳过。
pub async fn append_message_on<C: ConnectionTrait>(
    conn: &C,
    input: NewMessage,
) -> Result<Option<i64>> {
    let platform = required_text("平台标识", &input.platform, 64)?;
    let direction = normalize_direction(&input.direction);
    let role = validate_role(&input.role)?;
    let blocks = validate_json_array(&input.content_blocks_json)?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let occurred_at_ms = input.occurred_at_ms;
    let sender_nickname = input
        .sender_nickname
        .map(|value| {
            value
                .replace('\0', "")
                .chars()
                .take(100)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty());
    let text_content = input.text_content.replace('\0', "");
    let raw_payload_json = input.raw_payload_json.map(|value| value.replace('\0', ""));

    let inserted_id = if input.bot_instance_id.is_some() && input.native_message_id.is_some() {
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                conn.get_database_backend(),
                "INSERT INTO nuono_messages ( \
                    conversation_id, platform, bot_instance_id, native_message_id, \
                    reply_to_native_message_id, sender_user_id, sender_identity_id, \
                    native_sender_id, native_group_id, sender_nickname, direction, role, \
                    text_content, content_blocks_json, raw_payload_json, occurred_at_ms, created_at_ms \
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
                 ON CONFLICT (bot_instance_id, native_message_id) \
                 WHERE bot_instance_id IS NOT NULL AND native_message_id IS NOT NULL \
                 DO NOTHING \
                 RETURNING id",
                [
                    input.conversation_id.clone().into(),
                    platform.into(),
                    input.bot_instance_id.into(),
                    input.native_message_id.into(),
                    input.reply_to_native_message_id.into(),
                    input.sender_user_id.into(),
                    input.sender_identity_id.into(),
                    input.native_sender_id.into(),
                    input.native_group_id.into(),
                    sender_nickname.into(),
                    direction.into(),
                    role.into(),
                    text_content.into(),
                    blocks.into(),
                    raw_payload_json.into(),
                    occurred_at_ms.into(),
                    now_ms.into(),
                ],
            ))
            .await?;
        match row {
            Some(row) => Some(row.try_get::<i64>("", "id")?),
            None => None,
        }
    } else {
        let model = messages::ActiveModel {
            conversation_id: Set(input.conversation_id.clone()),
            platform: Set(platform),
            bot_instance_id: Set(input.bot_instance_id),
            native_message_id: Set(input.native_message_id),
            reply_to_native_message_id: Set(input.reply_to_native_message_id),
            sender_user_id: Set(input.sender_user_id),
            sender_identity_id: Set(input.sender_identity_id),
            native_sender_id: Set(input.native_sender_id),
            native_group_id: Set(input.native_group_id),
            sender_nickname: Set(sender_nickname),
            direction: Set(direction.to_owned()),
            role: Set(role.to_owned()),
            text_content: Set(text_content),
            content_blocks_json: Set(blocks),
            raw_payload_json: Set(raw_payload_json),
            occurred_at_ms: Set(occurred_at_ms),
            created_at_ms: Set(now_ms),
            ..Default::default()
        }
        .insert(conn)
        .await?;
        Some(model.id)
    };

    if inserted_id.is_some()
        && let Some(conversation_id) = input.conversation_id.as_deref()
    {
        conn.execute_raw(Statement::from_sql_and_values(
            conn.get_database_backend(),
            "UPDATE nuono_conversations \
             SET last_message_at_ms = CASE \
                     WHEN last_message_at_ms IS NULL OR last_message_at_ms < $1 THEN $1 \
                     ELSE last_message_at_ms \
                 END, \
                 updated_at_ms = $2 \
             WHERE id = $3",
            [occurred_at_ms.into(), now_ms.into(), conversation_id.into()],
        ))
        .await?;
    }

    Ok(inserted_id)
}

/// 按会话 ID 拉取最近 `limit` 条消息，按主键升序返回。
pub async fn recent_messages(
    db: &Database,
    conversation_id: &str,
    limit: u64,
) -> Result<Vec<messages::Model>> {
    let mut rows = messages::Entity::find()
        .filter(messages::Column::ConversationId.eq(conversation_id))
        .order_by_desc(messages::Column::Id)
        .limit(limit)
        .all(db.orm())
        .await?;
    rows.reverse();
    Ok(rows)
}

/// 启动一条 AI 推理运行记录。
pub async fn start_ai_run(db: &Database, input: NewAiRun) -> Result<ai_runs::Model> {
    let now_ms = crate::clock::db_now().timestamp_millis();
    let id = input.id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let model = ai_runs::ActiveModel {
        id: Set(id),
        conversation_id: Set(input.conversation_id),
        trigger_message_id: Set(input.trigger_message_id),
        reply_message_id: Set(None),
        user_id: Set(input.user_id),
        provider: Set(truncate_utf8_bytes(&input.provider.replace('\0', ""), 128)),
        model: Set(truncate_utf8_bytes(&input.model.replace('\0', ""), 256)),
        status: Set(AI_RUN_STATUS_RUNNING.to_owned()),
        prompt_tokens: Set(0),
        completion_tokens: Set(0),
        latency_ms: Set(0),
        steps_json: Set("[]".to_owned()),
        error_message: Set(None),
        created_at_ms: Set(now_ms),
        finished_at_ms: Set(None),
    }
    .insert(db.orm())
    .await?;
    Ok(model)
}

/// 回填 AI 推理结束状态、消耗 Token、耗时与步骤轨迹。
pub async fn finish_ai_run(
    db: &Database,
    run_id: &str,
    input: FinishAiRun,
) -> Result<Option<ai_runs::Model>> {
    let status = validate_ai_run_status(&input.status)?;
    let steps_json = validate_json_array(&input.steps_json)?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let Some(existing) = ai_runs::Entity::find_by_id(run_id).one(db.orm()).await? else {
        return Ok(None);
    };
    let updated = ai_runs::ActiveModel {
        id: Set(existing.id),
        reply_message_id: Set(input.reply_message_id),
        status: Set(status.to_owned()),
        prompt_tokens: Set(input.prompt_tokens.max(0)),
        completion_tokens: Set(input.completion_tokens.max(0)),
        latency_ms: Set(input.latency_ms.max(0)),
        steps_json: Set(steps_json),
        error_message: Set(input.error_message.map(|v| v.replace('\0', ""))),
        finished_at_ms: Set(Some(now_ms)),
        ..Default::default()
    }
    .update(db.orm())
    .await?;
    Ok(Some(updated))
}

/// 在调用方事务内直接写入一条已完成的 AI 推理记录。
pub async fn record_completed_ai_run_on<C: ConnectionTrait>(
    conn: &C,
    input: CompletedAiRun,
) -> Result<ai_runs::Model> {
    let status = validate_ai_run_status(&input.status)?;
    let steps_json = validate_json_array(&input.steps_json)?;
    let id = input.id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let model = ai_runs::ActiveModel {
        id: Set(id),
        conversation_id: Set(input.conversation_id),
        trigger_message_id: Set(input.trigger_message_id),
        reply_message_id: Set(input.reply_message_id),
        user_id: Set(input.user_id),
        provider: Set(truncate_utf8_bytes(&input.provider.replace('\0', ""), 128)),
        model: Set(truncate_utf8_bytes(&input.model.replace('\0', ""), 256)),
        status: Set(status.to_owned()),
        prompt_tokens: Set(input.prompt_tokens.max(0)),
        completion_tokens: Set(input.completion_tokens.max(0)),
        latency_ms: Set(input.latency_ms.max(0)),
        steps_json: Set(steps_json),
        error_message: Set(input.error_message.map(|v| v.replace('\0', ""))),
        created_at_ms: Set(input.created_at_ms),
        finished_at_ms: Set(input.finished_at_ms),
    }
    .insert(conn)
    .await?;
    Ok(model)
}

/// 按会话查询关联的 AI 推理运行记录，按开始时间升序返回。
pub async fn list_ai_runs_for_conversation(
    db: &Database,
    conversation_id: &str,
    limit: u64,
) -> Result<Vec<ai_runs::Model>> {
    Ok(ai_runs::Entity::find()
        .filter(ai_runs::Column::ConversationId.eq(conversation_id))
        .order_by_asc(ai_runs::Column::CreatedAtMs)
        .limit(limit.clamp(1, 200))
        .all(db.orm())
        .await?)
}

/// 查询匹配指定作用域及其 `context:` 子会话的全部会话 ID（供隐私删除与历史查询使用）。
pub(crate) async fn matching_scope_conversation_ids_on<C: ConnectionTrait>(
    conn: &C,
    scope: &str,
) -> Result<Vec<String>> {
    Ok(conversations::Entity::find()
        .select_only()
        .column(conversations::Column::Id)
        .filter(
            Condition::any()
                .add(conversations::Column::NativeChatId.eq(scope))
                .add(conversations::Column::NativeChatId.starts_with(format!("{scope}:context:"))),
        )
        .into_tuple::<String>()
        .all(conn)
        .await?)
}

fn required_text(label: &str, value: &str, max_bytes: usize) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > max_bytes || trimmed.contains('\0') {
        return Err(NuonoError::Validation(format!(
            "{label}须为 1 至 {max_bytes} 字节且不能包含空字符"
        )));
    }
    Ok(trimmed.to_owned())
}

fn sanitize_text(value: &str, max_bytes: usize) -> Result<String> {
    if value.len() > max_bytes || value.contains('\0') {
        return Err(NuonoError::Validation(format!(
            "标识字段超出 {max_bytes} 字节上限或包含空字符"
        )));
    }
    Ok(value.to_owned())
}

fn validate_scope_type(scope: &str) -> Result<&'static str> {
    match scope.trim() {
        SCOPE_WEB => Ok(SCOPE_WEB),
        SCOPE_PRIVATE => Ok(SCOPE_PRIVATE),
        SCOPE_GROUP => Ok(SCOPE_GROUP),
        SCOPE_GROUP_USER => Ok(SCOPE_GROUP_USER),
        SCOPE_THREAD => Ok(SCOPE_THREAD),
        other => Err(NuonoError::Validation(format!(
            "无效的会话范围类型: {other}"
        ))),
    }
}

fn validate_role(role: &str) -> Result<&'static str> {
    match role.trim() {
        ROLE_USER => Ok(ROLE_USER),
        ROLE_ASSISTANT => Ok(ROLE_ASSISTANT),
        ROLE_SYSTEM => Ok(ROLE_SYSTEM),
        other => Err(NuonoError::Validation(format!("无效的消息角色: {other}"))),
    }
}

fn validate_ai_run_status(status: &str) -> Result<&'static str> {
    match status.trim() {
        AI_RUN_STATUS_RUNNING => Ok(AI_RUN_STATUS_RUNNING),
        AI_RUN_STATUS_SUCCEEDED => Ok(AI_RUN_STATUS_SUCCEEDED),
        AI_RUN_STATUS_FAILED => Ok(AI_RUN_STATUS_FAILED),
        AI_RUN_STATUS_CANCELLED => Ok(AI_RUN_STATUS_CANCELLED),
        other => Err(NuonoError::Validation(format!("无效的推理状态: {other}"))),
    }
}

fn validate_json_array(raw: &str) -> Result<String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|err| NuonoError::Validation(format!("JSON 数组格式无效: {err}")))?;
    if !value.is_array() {
        return Err(NuonoError::Validation("字段必须为 JSON 数组".into()));
    }
    let encoded = serde_json::to_string(&value)?;
    if encoded.contains('\0') {
        return Err(NuonoError::Validation("JSON 字段不能包含空字符".into()));
    }
    Ok(encoded)
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewUser;

    #[tokio::test]
    async fn conversations_messages_dedupe_and_ai_runs_round_trip() {
        let db = crate::test_support::test_db(2).await;
        let user = db
            .create_user(NewUser {
                username: "conv_owner".into(),
                password: "test-password-with-length".into(),
                role: crate::db::ROLE_USER.into(),
                status: crate::db::STATUS_ACTIVE.into(),
                email: None,
                email_verified: false,
            })
            .await
            .unwrap();

        let web_conv = ensure_active_conversation_on(
            db.orm(),
            EnsureConversation {
                platform: PLATFORM_WEB.into(),
                bot_instance_id: None,
                scope_type: SCOPE_WEB.into(),
                group_id: None,
                owner_user_id: Some(user.id),
                native_chat_id: "web-chat-1".into(),
                native_thread_id: String::new(),
                title: Some("架构设计讨论".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(web_conv.status, CONVERSATION_STATUS_ACTIVE);

        let now_ms = crate::clock::db_now().timestamp_millis();
        let trigger_id = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: Some(web_conv.id.clone()),
                platform: PLATFORM_WEB.into(),
                bot_instance_id: None,
                native_message_id: None,
                reply_to_native_message_id: None,
                sender_user_id: Some(user.id),
                sender_identity_id: None,
                native_sender_id: None,
                native_group_id: None,
                sender_nickname: Some("测试用户".into()),
                direction: DIRECTION_INBOUND.into(),
                role: ROLE_USER.into(),
                text_content: "你好，帮我查一下状态".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: None,
                occurred_at_ms: now_ms,
            },
        )
        .await
        .unwrap()
        .expect("Web 消息应写入成功");

        let run = start_ai_run(
            &db,
            NewAiRun {
                id: None,
                conversation_id: web_conv.id.clone(),
                trigger_message_id: Some(trigger_id),
                user_id: Some(user.id),
                provider: "openai".into(),
                model: "gpt-5".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(run.status, AI_RUN_STATUS_RUNNING);

        let reply_id = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: Some(web_conv.id.clone()),
                platform: PLATFORM_WEB.into(),
                bot_instance_id: None,
                native_message_id: None,
                reply_to_native_message_id: None,
                sender_user_id: None,
                sender_identity_id: None,
                native_sender_id: None,
                native_group_id: None,
                sender_nickname: None,
                direction: DIRECTION_OUTBOUND.into(),
                role: ROLE_ASSISTANT.into(),
                text_content: "当前运行正常".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: None,
                occurred_at_ms: now_ms + 25,
            },
        )
        .await
        .unwrap()
        .expect("助手回复应写入成功");

        let finished = finish_ai_run(
            &db,
            &run.id,
            FinishAiRun {
                reply_message_id: Some(reply_id),
                status: AI_RUN_STATUS_SUCCEEDED.into(),
                prompt_tokens: 120,
                completion_tokens: 45,
                latency_ms: 380,
                steps_json: serde_json::json!([{"type":"tool_call","name":"status"}]).to_string(),
                error_message: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(finished.status, AI_RUN_STATUS_SUCCEEDED);
        assert_eq!(finished.reply_message_id, Some(reply_id));
        assert_eq!(finished.prompt_tokens, 120);
        assert_eq!(finished.completion_tokens, 45);
        assert!(finished.finished_at_ms.is_some());

        // 验证 Web 端侧边栏按用户和平台查到该会话且活跃时间已更新。
        let sidebar = list_user_conversations(&db, user.id, PLATFORM_WEB, 20)
            .await
            .unwrap();
        assert_eq!(sidebar.len(), 1);
        assert_eq!(sidebar[0].last_message_at_ms, Some(now_ms + 25));

        // 模拟 /new 归档后，重新定位会分配全新会话 ID 且保留历史。
        assert!(archive_conversation(&db, &web_conv.id).await.unwrap());
        let next_conv = ensure_active_conversation_on(
            db.orm(),
            EnsureConversation {
                platform: PLATFORM_WEB.into(),
                bot_instance_id: None,
                scope_type: SCOPE_WEB.into(),
                group_id: None,
                owner_user_id: Some(user.id),
                native_chat_id: "web-chat-1".into(),
                native_thread_id: String::new(),
                title: None,
            },
        )
        .await
        .unwrap();
        assert_ne!(next_conv.id, web_conv.id);
        assert_eq!(
            recent_messages(&db, &web_conv.id, 10).await.unwrap().len(),
            2
        );

        // 验证 IM 平台重推同一 (bot_instance_id, native_message_id) 时命中部分唯一索引并幂等跳过，
        // 同时允许 native_message_id 为 NULL 的多条消息共存。
        let first_im = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: None,
                platform: "onebot".into(),
                bot_instance_id: Some("bot-uuid-1".into()),
                native_message_id: Some("msg-1001".into()),
                reply_to_native_message_id: None,
                sender_user_id: None,
                sender_identity_id: None,
                native_sender_id: Some("10001".into()),
                native_group_id: Some("20002".into()),
                sender_nickname: Some("群友".into()),
                direction: DIRECTION_INBOUND.into(),
                role: ROLE_USER.into(),
                text_content: "重推测试".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: Some("{\"raw\":true}".into()),
                occurred_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
        assert!(first_im.is_some());

        let dup_im = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: None,
                platform: "onebot".into(),
                bot_instance_id: Some("bot-uuid-1".into()),
                native_message_id: Some("msg-1001".into()),
                reply_to_native_message_id: None,
                sender_user_id: None,
                sender_identity_id: None,
                native_sender_id: Some("10001".into()),
                native_group_id: Some("20002".into()),
                sender_nickname: Some("群友".into()),
                direction: DIRECTION_INBOUND.into(),
                role: ROLE_USER.into(),
                text_content: "重推测试".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: None,
                occurred_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
        assert!(
            dup_im.is_none(),
            "同一 Bot 与原生消息 ID 重复入库必须被幂等忽略"
        );

        let null_a = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: None,
                platform: "onebot".into(),
                bot_instance_id: Some("bot-uuid-1".into()),
                native_message_id: None,
                reply_to_native_message_id: None,
                sender_user_id: None,
                sender_identity_id: None,
                native_sender_id: None,
                native_group_id: None,
                sender_nickname: None,
                direction: DIRECTION_OUTBOUND.into(),
                role: ROLE_ASSISTANT.into(),
                text_content: "无消息 ID 回复 A".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: None,
                occurred_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
        let null_b = append_message_on(
            db.orm(),
            NewMessage {
                conversation_id: None,
                platform: "onebot".into(),
                bot_instance_id: Some("bot-uuid-1".into()),
                native_message_id: None,
                reply_to_native_message_id: None,
                sender_user_id: None,
                sender_identity_id: None,
                native_sender_id: None,
                native_group_id: None,
                sender_nickname: None,
                direction: DIRECTION_OUTBOUND.into(),
                role: ROLE_ASSISTANT.into(),
                text_content: "无消息 ID 回复 B".into(),
                content_blocks_json: "[]".into(),
                raw_payload_json: None,
                occurred_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
        assert!(null_a.is_some() && null_b.is_some());
        assert_eq!(
            list_ai_runs_for_conversation(&db, &web_conv.id, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        db.shutdown().await.unwrap();
    }
}
