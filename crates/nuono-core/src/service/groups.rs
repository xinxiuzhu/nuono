use crate::{
    auth::Scope,
    db::Database,
    error::{NuonoError, Result},
};
pub use nuono_app::groups::{GroupBotPresence, GroupRecord};
use serde_json::{Value, json};
pub async fn get_groups(db: &Database, scope: Scope) -> Result<Vec<GroupRecord>> {
    nuono_app::groups::GroupService::new(db).list(scope).await
}
pub async fn ensure_group_writable(db: &Database, id: i64, scope: Scope) -> Result<()> {
    nuono_app::groups::GroupService::new(db)
        .ensure_writable(id, scope)
        .await
}
pub async fn ensure_group_serviceable(db: &Database, id: i64, scope: Scope) -> Result<()> {
    nuono_app::groups::GroupService::new(db)
        .ensure_serviceable(id, scope)
        .await
}
pub async fn toggle_group(db: &Database, id: i64, enabled: bool) -> Result<()> {
    nuono_app::groups::GroupStore::toggle_group(db, id, enabled).await
}
pub async fn rename_group(db: &Database, id: i64, display_name: &str) -> Result<()> {
    nuono_app::groups::GroupStore::rename_group(db, id, display_name).await
}
pub async fn onebot_group_id(db: &Database, id: i64) -> Result<String> {
    let group = db
        .group_by_id(id)
        .await?
        .ok_or_else(|| NuonoError::NotFound("群不存在".into()))?;
    if group.platform != "onebot"
        || !group.namespace.is_empty()
        || group.conversation_kind != "group:native"
        || !group.native_parent_id.is_empty()
    {
        return Err(NuonoError::InvalidInput(
            "该群不支持 OneBot 平台管理".into(),
        ));
    }
    Ok(group.native_group_id)
}
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct GroupSyncSummary {
    pub reported: usize,
    pub created: usize,
    pub updated: usize,
}
pub async fn sync_groups_from_bot(
    db: &Database,
    self_id: i64,
    api_client: &std::sync::Arc<crate::core::onebot_client::ApiClient>,
    msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
) -> Result<GroupSyncSummary> {
    let resp = api_client
        .call("get_group_list", json!({}), msg_tx)
        .await
        .map_err(|e| NuonoError::OneBot(e.to_string()))?;
    let groups = resp
        .as_array()
        .ok_or_else(|| NuonoError::OneBot("get_group_list 返回格式错误".into()))?;
    let mut summary = GroupSyncSummary {
        reported: groups.len(),
        ..Default::default()
    };
    let mut synced_group_ids = Vec::with_capacity(groups.len());
    for group in groups {
        let (group_id, created) = db.upsert_synced_group(group).await?;
        synced_group_ids.push(group_id);
        if created {
            summary.created += 1;
        } else {
            summary.updated += 1;
        }
    }
    db.replace_bot_groups("onebot", &self_id.to_string(), &synced_group_ids)
        .await?;
    Ok(summary)
}
pub async fn get_group_members(
    api_client: &std::sync::Arc<crate::core::onebot_client::ApiClient>,
    msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    group_id: &str,
) -> Result<Vec<Value>> {
    let gid = group_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("群号格式错误".into()))?;
    let resp = api_client
        .call("get_group_member_list", json!({"group_id": gid}), msg_tx)
        .await
        .map_err(|e| NuonoError::OneBot(e.to_string()))?;
    Ok(resp.as_array().cloned().unwrap_or_default())
}

pub async fn send_group_message(
    api_client: &std::sync::Arc<crate::core::onebot_client::ApiClient>,
    msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    group_id: &str,
    content: &str,
) -> Result<()> {
    let gid = group_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("群号格式错误".into()))?;
    let _ = api_client
        .call(
            "send_group_msg",
            json!({
                "group_id": gid,
                "message": [{"type": "text", "data": {"text": content}}]
            }),
            msg_tx,
        )
        .await
        .map_err(|e| NuonoError::OneBot(e.to_string()))?;
    Ok(())
}

pub async fn set_group_ban(
    api_client: &std::sync::Arc<crate::core::onebot_client::ApiClient>,
    msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    group_id: &str,
    user_id: &str,
    duration_secs: u64,
) -> Result<()> {
    let gid = group_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("群号格式错误".into()))?;
    let uid = user_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("用户 ID 格式错误".into()))?;
    let _ = api_client
        .call(
            "set_group_ban",
            json!({
                "group_id": gid,
                "user_id": uid,
                "duration": duration_secs
            }),
            msg_tx,
        )
        .await
        .map_err(|e| NuonoError::OneBot(e.to_string()))?;
    Ok(())
}

pub async fn set_group_kick(
    api_client: &std::sync::Arc<crate::core::onebot_client::ApiClient>,
    msg_tx: &tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    group_id: &str,
    user_id: &str,
) -> Result<()> {
    let gid = group_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("群号格式错误".into()))?;
    let uid = user_id
        .parse::<i64>()
        .map_err(|_| NuonoError::InvalidInput("用户 ID 格式错误".into()))?;
    let _ = api_client
        .call(
            "set_group_kick",
            json!({
                "group_id": gid,
                "user_id": uid,
                "reject_add_request": false
            }),
            msg_tx,
        )
        .await
        .map_err(|e| NuonoError::OneBot(e.to_string()))?;
    Ok(())
}
