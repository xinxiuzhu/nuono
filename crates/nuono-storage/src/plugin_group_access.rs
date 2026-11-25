//! 在插件业务写事务内校验用户与群组的实时框架授权状态。
use crate::{
    db::platform_identities::identities,
    entity::users,
    error::{NuonoError, Result},
};
use nuono_app::{
    configuration::{GroupConfigurationCandidates, PlatformRoleProof, PlatformRoleSubject},
    plugin_identity::{CanonicalGroup, IdentityAccess},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait, QueryFilter,
    QueryOrder, Statement,
};

pub(crate) async fn require_administrator_on(
    tx: &DatabaseTransaction,
    access: &IdentityAccess,
) -> Result<()> {
    crate::plugin_identity::require_identity_on(tx, access).await?;
    let user_id = access
        .active_registered_user_id()?
        .ok_or_else(|| NuonoError::PermissionDenied("需要框架管理员账号".into()))?;
    let user = users::orm::Entity::find_by_id(user_id)
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
    if !matches!(user.role.as_str(), "owner" | "admin") {
        return Err(NuonoError::PermissionDenied("需要框架管理员权限".into()));
    }
    Ok(())
}
async fn platform_roles_enabled(tx: &DatabaseTransaction) -> Result<bool> {
    let suffix = if tx.get_database_backend() == DbBackend::Postgres {
        " FOR UPDATE"
    } else {
        ""
    };
    let row=tx.query_one_raw(Statement::from_string(tx.get_database_backend(),format!("SELECT value_json FROM nuono_global_configs WHERE key='configuration.platform_roles_enabled'{suffix}"))).await?;
    Ok(row
        .map(|r| r.try_get::<String>("", "value_json"))
        .transpose()?
        .as_deref()
        == Some("true"))
}
pub(crate) async fn prepare_on(
    tx: &DatabaseTransaction,
    access: &IdentityAccess,
    group: &CanonicalGroup,
) -> Result<GroupConfigurationCandidates> {
    crate::plugin_identity::require_identity_on(tx, access).await?;
    crate::plugin_identity::require_group_on(tx, group).await?;
    let user_id = access
        .active_registered_user_id()?
        .ok_or_else(|| NuonoError::PermissionDenied("请先绑定框架账号".into()))?;
    let enabled = platform_roles_enabled(tx).await?;
    let mut subjects = Vec::new();
    if enabled {
        let group_identity = crate::groups::GroupIdentity::from_conversation(&group.identity)?
            .ok_or_else(|| NuonoError::Validation("需要完整群会话".into()))?;
        let rows = identities::Entity::find()
            .filter(identities::Column::UserId.eq(user_id))
            .filter(identities::Column::VerifiedAtMs.is_not_null())
            .filter(identities::Column::Platform.eq(&group_identity.platform))
            .filter(identities::Column::Namespace.eq(&group_identity.namespace))
            .order_by_asc(identities::Column::Id)
            .all(tx)
            .await?;
        for row in rows {
            let scheme = nuono_domain::scalar::parse_json(&row.identity_kind)?;
            let scheme = scheme
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("json:{scheme}"));
            if group_identity
                .conversation_kind
                .ends_with(&format!(":{scheme}"))
            {
                subjects.push(PlatformRoleSubject {
                    identity_id: row.id,
                    identity_revision: row.revision,
                    group_id: group.id,
                    platform: row.platform,
                    namespace: row.namespace,
                    native_user_id: row.native_user_id,
                    conversation_kind: group_identity.conversation_kind.clone(),
                    native_parent_id: group_identity.native_parent_id.clone(),
                    native_group_id: group_identity.native_group_id.clone(),
                });
            }
        }
    }
    Ok(GroupConfigurationCandidates {
        platform_roles_enabled: enabled,
        subjects,
    })
}
pub(crate) async fn require_platform_role_on(
    tx: &DatabaseTransaction,
    user_id: i64,
    group_id: i64,
    proof: &PlatformRoleProof,
) -> Result<()> {
    proof.validate_current()?;
    if proof.group_id() != group_id || !platform_roles_enabled(tx).await? {
        return Err(NuonoError::PermissionDenied(
            "平台角色授权已关闭或不属于该群".into(),
        ));
    }
    let subject = proof.subject();
    let suffix = if tx.get_database_backend() == DbBackend::Postgres {
        " FOR UPDATE"
    } else {
        ""
    };
    let row=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),format!("SELECT identity.id FROM nuono_platform_identities identity JOIN nuono_groups grp ON grp.id=$1 WHERE identity.id=$2 AND identity.user_id=$3 AND identity.revision=$4 AND identity.verified_at_ms IS NOT NULL AND identity.platform=$5 AND identity.namespace=$6 AND identity.native_user_id=$7 AND grp.platform=$5 AND grp.namespace=$6 AND grp.conversation_kind=$8 AND grp.native_parent_id=$9 AND grp.native_group_id=$10{suffix}"),[group_id.into(),proof.identity_id().into(),user_id.into(),proof.identity_revision().into(),subject.platform.clone().into(),subject.namespace.clone().into(),subject.native_user_id.clone().into(),subject.conversation_kind.clone().into(),subject.native_parent_id.clone().into(),subject.native_group_id.clone().into()])).await?;
    if row.is_none() {
        return Err(NuonoError::PermissionDenied("平台身份绑定已改变".into()));
    }
    Ok(())
}
pub(crate) async fn require_on(
    tx: &DatabaseTransaction,
    plugin: &str,
    access: &IdentityAccess,
    group: &CanonicalGroup,
    proof: Option<&PlatformRoleProof>,
) -> Result<()> {
    crate::plugin_identity::require_identity_on(tx, access).await?;
    crate::plugin_identity::require_group_on(tx, group).await?;
    let user_id = access
        .active_registered_user_id()?
        .ok_or_else(|| NuonoError::PermissionDenied("请先绑定框架账号".into()))?;
    let user = users::orm::Entity::find_by_id(user_id)
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
    if !matches!(user.role.as_str(), "owner" | "admin") {
        if let Some(proof) = proof {
            require_platform_role_on(tx, user_id, group.id, proof).await?;
        } else {
            crate::group_authorizations::lock_group_configuration_grant(
                tx, user_id, group.id, plugin,
            )
            .await?;
        }
    }
    let suffix = if tx.get_database_backend() == DbBackend::Postgres {
        " FOR UPDATE"
    } else {
        ""
    };
    let row=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),format!("SELECT plugin_id FROM nuono_plugins WHERE plugin_id=$1 AND plugin_role='feature' AND allow_group_overrides=$2{suffix}"),[plugin.into(),if tx.get_database_backend()==DbBackend::Postgres{true.into()}else{1_i64.into()}])).await?;
    if row.is_none() {
        return Err(NuonoError::PermissionDenied(
            "部署者未开放该插件群配置".into(),
        ));
    }
    Ok(())
}
