//! 外部聊天平台原生身份解析与乐观锁版本复核。
//!
//! 任意交互过的平台用户均已通过 `IdentityResolver` 获得确定的非空内部 `user_id`（`shadow` 或 `registered`）。
use crate::{
    Database,
    db::platform_identities::identities,
    entity::users,
    error::{NuonoError, Result},
    groups::GroupIdentity,
    identity_resolver::resolve_canonical_user_id_on,
};
use nuono_app::plugin_identity::{CanonicalGroup, IdentityAccess, IdentityAccountState};
use nuono_domain::{ConversationRef, IdentityKind, IdentityRef, Platform};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use sha2::{Digest, Sha256};

fn native(row: &identities::Model) -> Result<IdentityRef> {
    // 数据库 platform 列存储的是 Platform::as_str() 纯字符串，而非带 serde 标签的 JSON 表示。
    let platform = match row.platform.as_str() {
        "onebot" => Platform::OneBot,
        "telegram" => Platform::Telegram,
        "discord" => Platform::Discord,
        "feishu" => Platform::Feishu,
        "qq_official" => Platform::QqOfficial,
        value => Platform::Other(value.to_owned()),
    };
    IdentityRef::new(
        platform,
        IdentityKind::User,
        serde_json::from_str(&row.identity_kind)?,
        row.native_user_id.clone(),
        (!row.namespace.is_empty()).then(|| row.namespace.clone()),
    )
    .map_err(|e| NuonoError::Validation(e.to_string()))
}

async fn access_on<C: ConnectionTrait>(conn: &C, row: identities::Model) -> Result<IdentityAccess> {
    let canonical_user_id = resolve_canonical_user_id_on(conn, row.user_id).await?;
    let account_row = users::orm::Entity::find_by_id(canonical_user_id)
        .one(conn)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("身份所绑定的账号不存在".into()))?;
    let (account, bound_identities, bindings_revision) =
        if account_row.account_kind == users::ACCOUNT_KIND_SHADOW {
            (
                IdentityAccountState::Shadow {
                    user_id: canonical_user_id,
                    status: account_row.status,
                    auth_epoch: account_row.auth_epoch,
                },
                vec![native(&row)?],
                String::new(),
            )
        } else {
            let rows = identities::Entity::find()
                .filter(identities::Column::UserId.eq(canonical_user_id))
                .filter(identities::Column::VerifiedAtMs.is_not_null())
                .order_by_asc(identities::Column::Id)
                .all(conn)
                .await?;
            let stamp = rows.iter().map(|r| (r.id, r.revision)).collect::<Vec<_>>();
            let refs = rows.iter().map(native).collect::<Result<Vec<_>>>()?;
            (
                IdentityAccountState::Account {
                    user_id: canonical_user_id,
                    status: account_row.status,
                    auth_epoch: account_row.auth_epoch,
                },
                refs,
                hex::encode(Sha256::digest(serde_json::to_vec(&stamp)?)),
            )
        };
    Ok(IdentityAccess {
        identity_id: row.id,
        revision: row.revision,
        identity: native(&row)?,
        account,
        bound_identities,
        bindings_revision,
    })
}

impl Database {
    /// 解析或幂等懒创建外部平台用户身份与占位账号（仅可传入受信任的平台适配器事件建立的完整平台身份）。
    pub async fn resolve_plugin_identity(&self, identity: &IdentityRef) -> Result<IdentityAccess> {
        // 通过一次完整的 JSON 往返反序列化，校验直接通过枚举构造的 IdentityRef 同样满足反序列化约束。
        let _: IdentityRef = serde_json::from_value(serde_json::to_value(identity)?)?;
        let resolved = self.resolve_or_provision_identity(identity, None).await?;
        identity_by_id_on(self.orm(), resolved.identity_id).await
    }

    /// 根据服务端已保存的平台身份主键 ID 刷新权限快照（绝不可直接信任外部公开请求自报的操作者 ID）。
    pub async fn plugin_identity_by_id(&self, id: i64) -> Result<IdentityAccess> {
        identity_by_id_on(self.orm(), id).await
    }

    pub async fn require_plugin_identity(&self, access: &IdentityAccess) -> Result<()> {
        let tx = self.begin_write().await?;
        require_identity_on(&tx, access).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn find_plugin_group(
        &self,
        conversation: &ConversationRef,
    ) -> Result<Option<CanonicalGroup>> {
        Ok(self
            .group_for_conversation(conversation)
            .await?
            .map(|group| CanonicalGroup {
                id: group.id,
                revision: group.revision,
                enabled: group.enabled,
                identity: conversation.clone(),
            }))
    }

    pub async fn resolve_plugin_group(
        &self,
        conversation: &ConversationRef,
    ) -> Result<Option<CanonicalGroup>> {
        let Some(identity) = GroupIdentity::from_conversation(conversation)? else {
            return Ok(None);
        };
        let group = self.ensure_group(&identity, "").await?;
        Ok(Some(CanonicalGroup {
            id: group.id,
            revision: group.revision,
            enabled: group.enabled,
            identity: conversation.clone(),
        }))
    }
}

/// 在事务内锁定并复核平台身份及其关联账号状态未发生变化（遵循「先锁账号、后锁平台身份」的统一锁序）。
pub async fn require_identity_on(
    tx: &DatabaseTransaction,
    expected: &IdentityAccess,
) -> Result<()> {
    let expected_user_id = expected.active_user_id()?;
    crate::group_authorizations::lock_accounts(tx, &[expected_user_id]).await?;
    let mut query = identities::Entity::find_by_id(expected.identity_id);
    if tx.get_database_backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    let row = query
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("平台身份已失效".into()))?;
    let current = access_on(tx, row).await?;
    if &current != expected {
        return Err(NuonoError::PermissionDenied(
            "平台身份、账号或绑定列表已变化，请重新操作".into(),
        ));
    }
    current.active_user_id()?;
    Ok(())
}

pub async fn require_group_on(tx: &DatabaseTransaction, expected: &CanonicalGroup) -> Result<()> {
    let identity = GroupIdentity::from_conversation(&expected.identity)?
        .ok_or_else(|| NuonoError::Validation("需要完整群会话".into()))?;
    let mut query = crate::entity::groups::Entity::find_by_id(expected.id);
    if tx.get_database_backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    let row = query
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::NotFound("群对象不存在".into()))?;
    if !expected.enabled
        || !row.enabled
        || row.revision != expected.revision
        || row.platform != identity.platform
        || row.namespace != identity.namespace
        || row.conversation_kind != identity.conversation_kind
        || row.native_parent_id != identity.native_parent_id
        || row.native_group_id != identity.native_group_id
    {
        return Err(NuonoError::PermissionDenied("群对象已变化或停用".into()));
    }
    Ok(())
}

pub async fn identity_by_id_on<C: ConnectionTrait>(conn: &C, id: i64) -> Result<IdentityAccess> {
    let row = identities::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| NuonoError::NotFound("平台身份不存在".into()))?;
    access_on(conn, row).await
}

#[cfg(test)]
#[path = "plugin_identity_tests.rs"]
mod tests;
