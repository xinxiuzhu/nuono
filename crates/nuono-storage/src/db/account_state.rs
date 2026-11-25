//! 账号公共状态持久化：统一管理登录会话、验证挑战票据（challenges）与同事务原子审计。

use super::Database;
use crate::{
    entity::users,
    error::{NuonoError, Result},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, Statement,
    sea_query::{Expr, ExprTrait},
};
use sha2::{Digest, Sha256};

pub(crate) mod sessions {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "nuono_sessions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub user_id: i64,
        pub token_hash: Vec<u8>,
        pub auth_epoch: i64,
        pub created_at_ms: i64,
        pub expires_at_ms: i64,
        pub last_seen_at_ms: Option<i64>,
        pub revoked_at_ms: Option<i64>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod challenges {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "nuono_account_challenges")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub purpose: String,
        pub requester_user_id: Option<i64>,
        pub subject_user_id: Option<i64>,
        pub subject_auth_epoch: Option<i64>,
        pub session_id: Option<String>,
        pub token_hash: Vec<u8>,
        pub context_json: String,
        pub candidate_identity_id: Option<i64>,
        pub state: String,
        pub attempts: i64,
        pub expires_at_ms: i64,
        pub consumed_at_ms: Option<i64>,
        pub revision: i64,
        pub created_at_ms: i64,
        pub updated_at_ms: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 精简的不可变审计事件元数据；`details` 中不得包含任何密码凭据或私聊正文。
pub struct AuditEvent<'a> {
    pub actor_user_id: Option<i64>,
    pub actor_plugin_id: Option<&'a str>,
    pub action: &'a str,
    pub result: &'a str,
    pub subject_user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub operation_id: Option<&'a str>,
    pub details: serde_json::Value,
}

pub async fn append_audit_on(conn: &impl ConnectionTrait, event: AuditEvent<'_>) -> Result<()> {
    if !event.details.is_object() {
        return Err(NuonoError::Validation("审计详情必须是对象".into()));
    }
    conn.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "INSERT INTO nuono_audit_events (occurred_at_ms,actor_user_id,actor_plugin_id,action,result,subject_user_id,group_id,operation_id,details_json) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        [crate::clock::db_now().timestamp_millis().into(), event.actor_user_id.into(), event.actor_plugin_id.into(),
         event.action.into(),event.result.into(),event.subject_user_id.into(),event.group_id.into(),event.operation_id.into(),event.details.to_string().into()])).await?;
    Ok(())
}

pub(crate) fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// 兼容过渡期已持有 `auth_session_<hex>` 摘要键的调用方；所有会话仍统一归属同一张会话表。
pub(crate) fn session_hash_from_key(key: &str) -> Result<Vec<u8>> {
    let hash = key
        .strip_prefix("auth_session_")
        .ok_or_else(|| NuonoError::Validation("会话参数无效".into()))?;
    let value = hex::decode(hash).map_err(|_| NuonoError::Validation("会话参数无效".into()))?;
    if value.len() != 32 {
        return Err(NuonoError::Validation("会话参数无效".into()));
    }
    Ok(value)
}

pub(crate) async fn locked_user(
    conn: &impl ConnectionTrait,
    user_id: i64,
) -> Result<users::orm::Model> {
    let mut query = users::orm::Entity::find_by_id(user_id);
    if conn.get_database_backend() == sea_orm::DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    query
        .one(conn)
        .await?
        .filter(|user| {
            user.status == "active"
                && user.account_kind == users::ACCOUNT_KIND_REGISTERED
                && user.password_hash.is_some()
        })
        .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))
}

pub(crate) async fn session_on(
    conn: &impl ConnectionTrait,
    hash: Vec<u8>,
    lock: bool,
) -> Result<Option<sessions::Model>> {
    let mut query = sessions::Entity::find()
        .filter(sessions::Column::TokenHash.eq(hash))
        .filter(sessions::Column::RevokedAtMs.is_null())
        .filter(sessions::Column::ExpiresAtMs.gt(crate::clock::db_now().timestamp_millis()));
    if lock && conn.get_database_backend() == sea_orm::DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    Ok(query
        .one(conn)
        .await?
        .filter(|session| session.expires_at_ms > crate::clock::db_now().timestamp_millis()))
}

/// 个人账号私聊作用域下的 Agent 写入操作共用账号生命周期锁；外部聊天平台的作用域不加此锁。
pub(super) async fn lock_private_account_on(tx: &impl ConnectionTrait, scope: &str) -> Result<()> {
    if let Some(user_id) = nuono_app::agent::account_chat_owner(scope) {
        locked_user(tx, user_id).await?;
    }
    Ok(())
}

pub(crate) async fn revoke_sessions_on(conn: &impl ConnectionTrait, user_id: i64) -> Result<u64> {
    Ok(sessions::Entity::update_many()
        .col_expr(
            sessions::Column::RevokedAtMs,
            Expr::value(crate::clock::db_now().timestamp_millis()),
        )
        .filter(sessions::Column::UserId.eq(user_id))
        .filter(sessions::Column::RevokedAtMs.is_null())
        .exec(conn)
        .await?
        .rows_affected)
}

pub(crate) async fn cancel_proofs_on(conn: &impl ConnectionTrait, user_id: i64) -> Result<u64> {
    Ok(challenges::Entity::update_many()
        .col_expr(challenges::Column::State, Expr::value("cancelled"))
        .col_expr(
            challenges::Column::Revision,
            Expr::col(challenges::Column::Revision).add(1),
        )
        .col_expr(
            challenges::Column::UpdatedAtMs,
            Expr::value(crate::clock::db_now().timestamp_millis()),
        )
        .filter(
            sea_orm::Condition::any()
                .add(challenges::Column::SubjectUserId.eq(user_id))
                .add(challenges::Column::RequesterUserId.eq(user_id)),
        )
        .filter(challenges::Column::State.is_in(["pending", "candidate"]))
        .exec(conn)
        .await?
        .rows_affected)
}

impl Database {
    pub async fn authenticate_session(&self, token: &str) -> Result<Option<users::Model>> {
        let now = crate::clock::db_now().timestamp_millis();
        let live_session = sea_orm::sea_query::Query::select()
            .column(sessions::Column::Id)
            .from(sessions::Entity)
            .and_where(
                Expr::col((sessions::Entity, sessions::Column::UserId))
                    .equals((users::orm::Entity, users::orm::Column::Id)),
            )
            .and_where(
                Expr::col((sessions::Entity, sessions::Column::AuthEpoch))
                    .equals((users::orm::Entity, users::orm::Column::AuthEpoch)),
            )
            .and_where(sessions::Column::TokenHash.eq(token_hash(token)))
            .and_where(sessions::Column::RevokedAtMs.is_null())
            .and_where(sessions::Column::ExpiresAtMs.gt(now))
            .to_owned();
        let user = users::orm::Entity::find()
            .filter(users::orm::Column::Status.eq("active"))
            .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
            .filter(users::orm::Column::PasswordHash.is_not_null())
            .filter(Expr::exists(live_session))
            .one(self.orm())
            .await?;
        if let Some(user) = &user {
            sessions::Entity::update_many()
                .col_expr(sessions::Column::LastSeenAtMs, Expr::value(now))
                .filter(sessions::Column::TokenHash.eq(token_hash(token)))
                .filter(sessions::Column::UserId.eq(user.id))
                .filter(
                    sea_orm::Condition::any()
                        .add(sessions::Column::LastSeenAtMs.is_null())
                        .add(sessions::Column::LastSeenAtMs.lt(now - 60_000)),
                )
                .exec(self.orm())
                .await?;
        }
        Ok(user.map(Into::into))
    }

    pub async fn revoke_session_token(&self, token: &str) -> Result<()> {
        let tx = self.begin_write().await?;
        if let Some(observed) = session_on(&tx, token_hash(token), false).await? {
            let mut account = users::orm::Entity::find_by_id(observed.user_id);
            if self.backend() == sea_orm::DbBackend::Postgres {
                account = account.lock_exclusive();
            }
            account.one(&tx).await?;
            let Some(session) = session_on(&tx, token_hash(token), true).await? else {
                tx.commit().await?;
                return Ok(());
            };
            sessions::Entity::update_many()
                .col_expr(
                    sessions::Column::RevokedAtMs,
                    Expr::value(crate::clock::db_now().timestamp_millis()),
                )
                .filter(sessions::Column::Id.eq(&session.id))
                .exec(&tx)
                .await?;
            challenges::Entity::update_many()
                .col_expr(challenges::Column::State, Expr::value("cancelled"))
                .filter(challenges::Column::SessionId.eq(&session.id))
                .filter(challenges::Column::State.is_in(["pending", "candidate"]))
                .exec(&tx)
                .await?;
            append_audit_on(
                &tx,
                AuditEvent {
                    actor_user_id: Some(session.user_id),
                    actor_plugin_id: None,
                    action: "session.revoked",
                    result: "success",
                    subject_user_id: Some(session.user_id),
                    group_id: None,
                    operation_id: None,
                    details: serde_json::json!({"session_id": session.id}),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[derive(Clone, Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AccountSessionView {
    pub id: String,
    pub current: bool,
    pub created_at_ms: String,
    pub expires_at_ms: String,
    pub last_seen_at_ms: Option<String>,
}
impl Database {
    pub async fn account_sessions(
        &self,
        user_id: i64,
        token: &str,
    ) -> Result<Vec<AccountSessionView>> {
        use sea_orm::QueryOrder;
        let now = crate::clock::db_now().timestamp_millis();
        let user = users::orm::Entity::find_by_id(user_id)
            .filter(users::orm::Column::Status.eq("active"))
            .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
        let rows = sessions::Entity::find()
            .filter(sessions::Column::UserId.eq(user_id))
            .filter(sessions::Column::AuthEpoch.eq(user.auth_epoch))
            .filter(sessions::Column::RevokedAtMs.is_null())
            .filter(sessions::Column::ExpiresAtMs.gt(now))
            .order_by_desc(sessions::Column::CreatedAtMs)
            .all(self.orm())
            .await?;
        let hash = token_hash(token);
        Ok(rows
            .into_iter()
            .map(|row| AccountSessionView {
                id: row.id,
                current: row.token_hash == hash,
                created_at_ms: row.created_at_ms.to_string(),
                expires_at_ms: row.expires_at_ms.to_string(),
                last_seen_at_ms: row.last_seen_at_ms.map(|v| v.to_string()),
            })
            .collect())
    }
    pub async fn revoke_account_session(&self, user_id: i64, token: &str, id: &str) -> Result<()> {
        let tx = self.begin_write().await?;
        let user = locked_user(&tx, user_id).await?;
        let valid = session_on(&tx, token_hash(token), true)
            .await?
            .is_some_and(|session| {
                session.user_id == user_id && session.auth_epoch == user.auth_epoch
            });
        if !valid {
            return Err(NuonoError::PermissionDenied("会话已失效".into()));
        }
        let now = crate::clock::db_now().timestamp_millis();
        let changed = sessions::Entity::update_many()
            .col_expr(sessions::Column::RevokedAtMs, Expr::value(now))
            .filter(sessions::Column::Id.eq(id))
            .filter(sessions::Column::UserId.eq(user_id))
            .filter(sessions::Column::RevokedAtMs.is_null())
            .exec(&tx)
            .await?
            .rows_affected;
        if changed > 0 {
            challenges::Entity::update_many()
                .col_expr(challenges::Column::State, Expr::value("cancelled"))
                .col_expr(
                    challenges::Column::Revision,
                    Expr::col(challenges::Column::Revision).add(1),
                )
                .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
                .filter(challenges::Column::SessionId.eq(id))
                .filter(challenges::Column::State.is_in(["pending", "candidate"]))
                .exec(&tx)
                .await?;
            append_audit_on(
                &tx,
                AuditEvent {
                    actor_user_id: Some(user_id),
                    actor_plugin_id: None,
                    action: "session.revoked",
                    result: "success",
                    subject_user_id: Some(user_id),
                    group_id: None,
                    operation_id: None,
                    details: serde_json::json!({"session_id":id}),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

impl Database {
    /// 更新账号头像元数据：与账号合并、删除共用同一把账号锁，磁盘上不可变的头像文件保持原路径不动。
    pub async fn set_account_avatar(&self, user_id: i64, value: serde_json::Value) -> Result<()> {
        let tx = self.begin_write().await?;
        locked_user(&tx, user_id).await?;
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_global_configs(key,value_json,value_version,revision,created_at_ms,updated_at_ms) VALUES($1,$2,1,1,$3,$3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,revision=nuono_global_configs.revision+1,updated_at_ms=excluded.updated_at_ms",[format!("admin_profile:{user_id}").into(),serde_json::to_string(&value)?.into(),crate::clock::db_now().timestamp_millis().into()])).await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user_id),
                actor_plugin_id: None,
                action: "account.avatar_updated",
                result: "success",
                subject_user_id: Some(user_id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::NewUser, test_support::test_db};
    async fn user(db: &Database, name: &str) -> users::Model {
        db.create_user(NewUser {
            username: name.into(),
            email: None,
            password: "Session-password-123!".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap()
    }
    async fn issue(db: &Database, user: &users::Model, token: &str) {
        let key = format!("auth_session_{}", hex::encode(token_hash(token)));
        assert!(
            db.create_session_for_verified_user(
                user,
                &key,
                chrono::Utc::now() + chrono::Duration::hours(1)
            )
            .await
            .unwrap()
        );
    }
    #[tokio::test]
    async fn session_table_is_the_only_authority_and_revocation_is_account_scoped() {
        let db = test_db(2).await;
        let alice = user(&db, "session-alice").await;
        let bob = user(&db, "session-bob").await;
        issue(&db, &alice, "alice-token").await;
        issue(&db, &bob, "bob-token").await;
        let forged = format!("auth_session_{}", hex::encode(token_hash("forged-token")));
        db.set_global_config(&forged,serde_json::json!({"user_id":alice.id.to_string(),"auth_epoch":"1","username":alice.username,"expires":"2099-01-01T00:00:00Z"})).await.unwrap();
        assert!(
            db.authenticate_session("forged-token")
                .await
                .unwrap()
                .is_none()
        );
        let alice_sessions = db.account_sessions(alice.id, "alice-token").await.unwrap();
        assert_eq!(alice_sessions.len(), 1);
        assert!(alice_sessions[0].current);
        db.revoke_account_session(
            alice.id,
            "alice-token",
            &db.account_sessions(bob.id, "bob-token").await.unwrap()[0].id,
        )
        .await
        .unwrap();
        assert!(
            db.authenticate_session("bob-token")
                .await
                .unwrap()
                .is_some()
        );
        db.revoke_session_token("alice-token").await.unwrap();
        assert!(
            db.authenticate_session("alice-token")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.authenticate_session("bob-token")
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn old_account_snapshot_cannot_create_a_session_after_epoch_changes() {
        let db = test_db(2).await;
        let before = user(&db, "epoch-account").await;
        users::orm::Entity::update_many()
            .col_expr(users::orm::Column::AuthEpoch, Expr::value(2_i64))
            .filter(users::orm::Column::Id.eq(before.id))
            .exec(db.orm())
            .await
            .unwrap();
        assert!(
            !db.create_session_for_verified_user(
                &before,
                &format!("auth_session_{}", hex::encode(token_hash("stale"))),
                chrono::Utc::now() + chrono::Duration::hours(1)
            )
            .await
            .unwrap()
        );
        let current = db.find_user_by_id(before.id).await.unwrap().unwrap();
        issue(&db, &current, "current").await;
        assert!(db.authenticate_session("current").await.unwrap().is_some());
        users::orm::Entity::update_many()
            .col_expr(users::orm::Column::Status, Expr::value("disabled"))
            .filter(users::orm::Column::Id.eq(before.id))
            .exec(db.orm())
            .await
            .unwrap();
        assert!(db.authenticate_session("current").await.unwrap().is_none());
        db.revoke_session_token("current").await.unwrap();
        db.shutdown().await.unwrap();
    }
}
