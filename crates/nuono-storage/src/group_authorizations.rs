//! 群组插件配置管理权限的独立授权来源管理（含管理员手动授权与受委派插件签发，全程记录审计）。
use crate::db::{AuditEvent, append_audit_on};
use crate::{
    Database,
    error::{NuonoError, Result},
};
pub use nuono_app::group_authorization::{
    GroupAuthorization, GroupGrantRequest as ManualGroupGrant, PluginGrantDelegation,
};
use sea_orm::{ConnectionTrait, DatabaseTransaction, DbBackend, Statement};

fn from_row(row: sea_orm::QueryResult) -> Result<GroupAuthorization> {
    Ok(GroupAuthorization {
        id: row.try_get("", "id")?,
        user_id: row.try_get("", "user_id")?,
        group_id: row.try_get("", "group_id")?,
        plugin_id: row.try_get("", "plugin_id")?,
        issuer_kind: row.try_get("", "issuer_kind")?,
        issuer_user_id: row.try_get("", "issuer_user_id")?,
        issuer_plugin_id: row.try_get("", "issuer_plugin_id")?,
        source_key: row.try_get("", "source_key")?,
        valid_from_ms: row.try_get("", "valid_from_ms")?,
        expires_at_ms: row.try_get("", "expires_at_ms")?,
        revoked_at_ms: row.try_get("", "revoked_at_ms")?,
        revision: row.try_get("", "revision")?,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Issuer<'a> {
    Manual(i64),
    Plugin(&'a str),
}
impl Issuer<'_> {
    fn user(self) -> Option<i64> {
        if let Self::Manual(id) = self {
            Some(id)
        } else {
            None
        }
    }
    fn plugin(self) -> Option<String> {
        if let Self::Plugin(id) = self {
            Some(id.to_owned())
        } else {
            None
        }
    }
    fn kind(self) -> &'static str {
        if matches!(self, Self::Manual(_)) {
            "manual"
        } else {
            "plugin"
        }
    }
}

impl Database {
    pub async fn has_group_configuration_grant(
        &self,
        user_id: i64,
        group_id: i64,
        plugin: Option<&str>,
    ) -> Result<bool> {
        let now = crate::clock::db_now().timestamp_millis();
        Ok(self.orm().query_one_raw(Statement::from_sql_and_values(self.backend(),
            "SELECT a.id FROM nuono_group_authorizations a JOIN nuono_users u ON u.id = a.user_id WHERE a.user_id = $1 AND a.group_id = $2 AND u.status = 'active' AND a.capability = 'manage_plugin_config' AND a.revoked_at_ms IS NULL AND a.valid_from_ms <= $3 AND (a.expires_at_ms IS NULL OR a.expires_at_ms > $3) AND (CAST($4 AS TEXT) IS NULL OR a.plugin_id IS NULL OR a.plugin_id = $4) LIMIT 1",
            [user_id.into(),group_id.into(),now.into(),plugin.map(str::to_owned).into()])).await?.is_some())
    }

    pub async fn group_configuration_grants(
        &self,
        group_id: i64,
    ) -> Result<Vec<GroupAuthorization>> {
        self.orm()
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT * FROM nuono_group_authorizations WHERE group_id = $1 ORDER BY id",
                [group_id.into()],
            ))
            .await?
            .into_iter()
            .map(from_row)
            .collect()
    }

    /// 列出指定站点账号名下的全部群组插件配置授权记录（供后台用户详情面板统一查看与撤销）。
    pub async fn user_configuration_grants(&self, user_id: i64) -> Result<Vec<GroupAuthorization>> {
        self.orm()
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT * FROM nuono_group_authorizations WHERE user_id = $1 ORDER BY id",
                [user_id.into()],
            ))
            .await?
            .into_iter()
            .map(from_row)
            .collect()
    }

    pub async fn grant_group_configuration(
        &self,
        actor_user_id: i64,
        grant: &ManualGroupGrant,
    ) -> Result<GroupAuthorization> {
        let tx = self.begin_write().await?;
        let result = issue_on(&tx, Issuer::Manual(actor_user_id), grant).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn renew_group_configuration(
        &self,
        actor_user_id: i64,
        id: i64,
        revision: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<GroupAuthorization> {
        let tx = self.begin_write().await?;
        let result = change_on(
            &tx,
            Issuer::Manual(actor_user_id),
            id,
            Some(revision),
            Change::Renew(expires_at_ms),
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn revoke_group_configuration(
        &self,
        actor_user_id: i64,
        group_id: i64,
        id: i64,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        let saved = source_on(&tx, id).await?;
        if saved.group_id != group_id {
            return Err(NuonoError::NotFound("授权记录不存在".into()));
        }
        change_on(&tx, Issuer::Manual(actor_user_id), id, None, Change::Revoke).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 配置插件代签发群组权限的委派范围（仅限活跃的 `owner` 或 `admin` 操作，委派配置持久化落盘并记录审计）。
    pub async fn set_plugin_grant_delegation(
        &self,
        actor: i64,
        plugin_id: &str,
        delegation: &PluginGrantDelegation,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        lock_accounts(&tx, &[actor]).await?;
        require_administrator(&tx, actor).await?;
        require_plugin(&tx, plugin_id).await?;
        for target in &delegation.plugin_ids {
            require_feature_plugin(&tx, target).await?;
        }
        let key = format!("authorization.issuer.{plugin_id}");
        let now = crate::clock::db_now().timestamp_millis();
        tx.execute_raw(Statement::from_sql_and_values(tx.get_database_backend(),
            "INSERT INTO nuono_global_configs (key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,1,1,$3,$3) ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, revision = nuono_global_configs.revision + 1, updated_at_ms = excluded.updated_at_ms",
            [key.into(),serde_json::to_string(delegation)?.into(),now.into()])).await?;
        append_audit_on(&tx, AuditEvent { actor_user_id:Some(actor),actor_plugin_id:None, action:"authorization.delegate",result:"success",subject_user_id:None,group_id:None,operation_id:None,
            details:serde_json::json!({"plugin_id":plugin_id,"all_plugins":delegation.all_plugins,"plugin_ids":delegation.plugin_ids}) }).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn require_plugin(tx: &DatabaseTransaction, id: &str) -> Result<()> {
    if tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "SELECT plugin_id FROM nuono_plugins WHERE plugin_id = $1",
            [id.into()],
        ))
        .await?
        .is_none()
    {
        return Err(NuonoError::Validation("插件尚未登记".into()));
    }
    Ok(())
}

async fn require_feature_plugin(tx: &DatabaseTransaction, id: &str) -> Result<()> {
    let row = tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "SELECT plugin_role FROM nuono_plugins WHERE plugin_id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| NuonoError::Validation("插件尚未登记".into()))?;
    let role: String = row.try_get("", "plugin_role")?;
    if role == nuono_app::plugin_role::PluginRole::Service.as_str() {
        return Err(NuonoError::PermissionDenied(
            "底层服务插件不支持群级授权委派".into(),
        ));
    }
    Ok(())
}

async fn require_issuer(
    tx: &DatabaseTransaction,
    issuer: Issuer<'_>,
    scope: Option<&str>,
) -> Result<()> {
    match issuer {
        Issuer::Manual(id) => require_administrator(tx, id).await,
        Issuer::Plugin(id) => {
            // 锁定该插件的委派配置行，使管理员并发撤销委派的操作必须等待当前签发事务结束。
            let row = tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),
                locked_sql(tx,"SELECT c.value_json FROM nuono_global_configs c JOIN nuono_plugins p ON p.plugin_id = $2 WHERE c.key = $1 AND p.enabled = TRUE"),
                [format!("authorization.issuer.{id}").into(),id.into()])).await?;
            let delegation: PluginGrantDelegation = row
                .map(|row| row.try_get::<String>("", "value_json"))
                .transpose()?
                .map(|json| serde_json::from_str(&json))
                .transpose()?
                .unwrap_or_default();
            if !delegation.allows(scope) {
                return Err(NuonoError::PermissionDenied(
                    "插件未获该范围的授权签发能力".into(),
                ));
            }
            Ok(())
        }
    }
}

pub(crate) async fn issue_on(
    tx: &DatabaseTransaction,
    issuer: Issuer<'_>,
    grant: &ManualGroupGrant,
) -> Result<GroupAuthorization> {
    if grant.user_id <= 0
        || grant.group_id <= 0
        || grant.source_key.is_empty()
        || grant.source_key.len() > 256
        || grant.source_key.contains('\0')
        || grant
            .expires_at_ms
            .is_some_and(|end| end <= grant.valid_from_ms)
    {
        return Err(NuonoError::Validation("授权来源、对象或有效期无效".into()));
    }
    let mut ids = vec![grant.user_id];
    if let Some(actor) = issuer.user() {
        ids.push(actor);
    }
    lock_accounts(tx, &ids).await?;
    require_issuer(tx, issuer, grant.plugin_id.as_deref()).await?;
    if let Some(plugin_id) = grant.plugin_id.as_deref() {
        require_feature_plugin(tx, plugin_id).await?;
    }
    let existing=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        "SELECT * FROM nuono_group_authorizations WHERE issuer_kind=$1 AND (issuer_user_id=$2 OR issuer_plugin_id=$3) AND source_key=$4",
        [issuer.kind().into(),issuer.user().into(),issuer.plugin().into(),grant.source_key.clone().into()])).await?;
    if let Some(row) = existing {
        let saved = from_row(row)?;
        verify_issuance_receipt(tx, &saved, grant).await?;
        return Ok(saved);
    }
    require_active(tx, grant.user_id).await?;
    let now = crate::clock::db_now().timestamp_millis();
    let inserted = tx.execute_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        "INSERT INTO nuono_group_authorizations (user_id,group_id,capability,scope_kind,plugin_id,issuer_kind,issuer_user_id,issuer_plugin_id,source_key,valid_from_ms,expires_at_ms,revision,created_at_ms,updated_at_ms) VALUES ($1,$2,'manage_plugin_config',$3,$4,$5,$6,$7,$8,$9,$10,1,$11,$11) ON CONFLICT DO NOTHING",
        [grant.user_id.into(),grant.group_id.into(),if grant.plugin_id.is_some(){"one_plugin"}else{"all_plugins"}.into(),grant.plugin_id.clone().into(),issuer.kind().into(),issuer.user().into(),issuer.plugin().into(),grant.source_key.clone().into(),grant.valid_from_ms.into(),grant.expires_at_ms.into(),now.into()])).await?.rows_affected();
    let row = tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        "SELECT * FROM nuono_group_authorizations WHERE issuer_kind = $1 AND (issuer_user_id = $2 OR issuer_plugin_id = $3) AND source_key = $4",
        [issuer.kind().into(),issuer.user().into(),issuer.plugin().into(),grant.source_key.clone().into()])).await?
        .ok_or_else(|| NuonoError::Conflict("授权来源冲突".into()))?;
    let saved = from_row(row)?;
    // 首次签发的原始请求快照不可变地保存在审计回执中，即便后续发生续期或账号合并，幂等重放仍按原始签发参数校验。
    if inserted == 0 {
        verify_issuance_receipt(tx, &saved, grant).await?;
    } else {
        audit(
            tx,
            issuer,
            "authorization.issue",
            &saved,
            serde_json::json!({"request":grant}),
        )
        .await?;
    }
    Ok(saved)
}

async fn verify_issuance_receipt(
    tx: &DatabaseTransaction,
    saved: &GroupAuthorization,
    grant: &ManualGroupGrant,
) -> Result<()> {
    let receipt=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),
            "SELECT details_json FROM nuono_audit_events WHERE action = 'authorization.issue' AND operation_id = $1 ORDER BY id LIMIT 1",[format!("grant:{}",saved.id).into()])).await?;
    let original = receipt
        .map(|row| row.try_get::<String>("", "details_json"))
        .transpose()?
        .ok_or_else(|| NuonoError::Conflict("授权缺少原始签发凭据".into()))?;
    let details = nuono_domain::scalar::parse_json(&original)?;
    if details.get("request") != Some(&serde_json::to_value(grant)?) {
        return Err(NuonoError::Conflict("授权来源编号已用于其他内容".into()));
    }
    Ok(())
}

pub(crate) async fn plugin_sources_on(
    tx: &DatabaseTransaction,
    plugin_id: &str,
    group_id: i64,
) -> Result<Vec<GroupAuthorization>> {
    // 每个签发插件只能查看自己签发的授权记录（含已撤销和已过期的历史记录）。
    tx.query_all_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        "SELECT * FROM nuono_group_authorizations WHERE issuer_plugin_id = $1 AND group_id = $2 ORDER BY id",[plugin_id.into(),group_id.into()]))
        .await?.into_iter().map(from_row).collect()
}

pub(crate) enum Change {
    Renew(Option<i64>),
    Revoke,
}

pub(crate) async fn change_on(
    tx: &DatabaseTransaction,
    issuer: Issuer<'_>,
    id: i64,
    revision: Option<i64>,
    change: Change,
) -> Result<GroupAuthorization> {
    let before = source_on(tx, id).await?;
    let mut ids = vec![before.user_id];
    if let Some(actor) = issuer.user() {
        ids.push(actor);
    }
    lock_accounts(tx, &ids).await?;
    require_issuer(tx, issuer, before.plugin_id.as_deref()).await?;
    let row = tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            locked_sql(tx, "SELECT * FROM nuono_group_authorizations WHERE id = $1"),
            [id.into()],
        ))
        .await?
        .ok_or_else(|| NuonoError::NotFound("授权不存在".into()))?;
    let saved = from_row(row)?;
    if saved.user_id != before.user_id {
        return Err(NuonoError::Conflict("授权受益账号已合并，请重试".into()));
    }
    if let Issuer::Plugin(plugin) = issuer
        && saved.issuer_plugin_id.as_deref() != Some(plugin)
    {
        return Err(NuonoError::PermissionDenied(
            "插件只能修改自己签发的授权".into(),
        ));
    }
    if revision.is_some_and(|revision| revision != saved.revision) {
        return Err(NuonoError::Conflict("授权版本已变更".into()));
    }
    let now = crate::clock::db_now().timestamp_millis();
    let action = match change {
        Change::Renew(expiry) => {
            require_active(tx, saved.user_id).await?;
            if saved.revoked_at_ms.is_some()
                || expiry.is_some_and(|end| end <= saved.valid_from_ms)
                || (saved.expires_at_ms.is_none() && expiry.is_some())
                || saved
                    .expires_at_ms
                    .zip(expiry)
                    .is_some_and(|(old, new)| new < old)
            {
                return Err(NuonoError::Validation(
                    "续期不能缩短、复活已撤销来源或设置无效期限".into(),
                ));
            }
            tx.execute_raw(Statement::from_sql_and_values(tx.get_database_backend(),
                "UPDATE nuono_group_authorizations SET expires_at_ms = $1, revision = revision + 1, updated_at_ms = $2 WHERE id = $3",[expiry.into(),now.into(),id.into()])).await?;
            "authorization.renew"
        }
        Change::Revoke => {
            if saved.revoked_at_ms.is_some() {
                return Ok(saved);
            }
            tx.execute_raw(Statement::from_sql_and_values(tx.get_database_backend(),
                "UPDATE nuono_group_authorizations SET revoked_at_ms = $1, revision = revision + 1, updated_at_ms = $1 WHERE id = $2",[now.into(),id.into()])).await?;
            "authorization.revoke"
        }
    };
    let result = source_on(tx, id).await?;
    audit(tx,issuer,action,&result,serde_json::json!({"previous_revision":saved.revision.to_string(),"expires_at_ms":result.expires_at_ms.map(|v|v.to_string())})).await?;
    Ok(result)
}

async fn source_on(tx: &DatabaseTransaction, id: i64) -> Result<GroupAuthorization> {
    from_row(
        tx.query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "SELECT * FROM nuono_group_authorizations WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| NuonoError::NotFound("授权不存在".into()))?,
    )
}

async fn audit(
    tx: &DatabaseTransaction,
    issuer: Issuer<'_>,
    action: &str,
    grant: &GroupAuthorization,
    details: serde_json::Value,
) -> Result<()> {
    append_audit_on(
        tx,
        AuditEvent {
            actor_user_id: issuer.user(),
            actor_plugin_id: issuer.plugin().as_deref(),
            action,
            result: "success",
            subject_user_id: Some(grant.user_id),
            group_id: Some(grant.group_id),
            operation_id: Some(&format!("grant:{}", grant.id)),
            details,
        },
    )
    .await
}

async fn require_administrator(tx: &DatabaseTransaction, user_id: i64) -> Result<()> {
    if tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        "SELECT id FROM nuono_users WHERE id = $1 AND status = 'active' AND role IN ('owner','admin')",[user_id.into()])).await?.is_none(){
        return Err(NuonoError::PermissionDenied("需要框架管理员权限".into()));
    }
    Ok(())
}
async fn require_active(tx: &DatabaseTransaction, user_id: i64) -> Result<()> {
    if tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "SELECT id FROM nuono_users WHERE id = $1 AND status = 'active'",
            [user_id.into()],
        ))
        .await?
        .is_none()
    {
        return Err(NuonoError::PermissionDenied("授权账号不可用".into()));
    }
    Ok(())
}
fn locked_sql(tx: &DatabaseTransaction, sql: &str) -> String {
    if tx.get_database_backend() == DbBackend::Postgres {
        format!("{sql} FOR UPDATE")
    } else {
        sql.into()
    }
}

/// 在加锁具体业务行之前调用：按统一的账号 ID 升序锁定相关账号行，与所有账号生命周期写操作保持一致的加锁顺序。
pub(crate) async fn lock_accounts(tx: &DatabaseTransaction, ids: &[i64]) -> Result<()> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        tx.query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            locked_sql(tx, "SELECT id FROM nuono_users WHERE id = $1"),
            [id.into()],
        ))
        .await?;
    }
    Ok(())
}

pub(crate) async fn lock_group_configuration_grant(
    tx: &DatabaseTransaction,
    user_id: i64,
    group_id: i64,
    plugin_id: &str,
) -> Result<()> {
    lock_accounts(tx, &[user_id]).await?;
    require_active(tx, user_id).await?;
    let sources=tx.query_all_raw(Statement::from_sql_and_values(tx.get_database_backend(),
        locked_sql(tx,"SELECT * FROM nuono_group_authorizations WHERE user_id = $1 AND group_id = $2 AND capability = 'manage_plugin_config' AND (plugin_id IS NULL OR plugin_id = $3) ORDER BY id"),
        [user_id.into(),group_id.into(),plugin_id.into()])).await?;
    let now = crate::clock::db_now().timestamp_millis();
    for row in sources {
        let grant = from_row(row)?;
        if grant.revoked_at_ms.is_none()
            && grant.valid_from_ms <= now
            && grant.expires_at_ms.is_none_or(|end| end > now)
        {
            return Ok(());
        }
    }
    Err(NuonoError::PermissionDenied("群插件配置授权已失效".into()))
}

#[cfg(test)]
#[path = "group_authorizations_tests.rs"]
mod tests;
