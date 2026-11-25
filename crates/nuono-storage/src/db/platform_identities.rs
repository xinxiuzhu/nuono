//! 外部聊天平台用户身份绑定、网站注册原地转正（分支 A）与已登录绑定/合并（分支 B/C/D）。

use nuono_domain::{BotIdentity, IdentityKind, IdentityRef};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
    sea_query::{Expr, ExprTrait},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Database;
use super::account_merge::{
    merge_shadow_into_user_on, shadow_merge_plan_digest, shadow_merge_plan_on,
};
use super::account_state::{challenges, session_on, sessions, token_hash};
use super::users::{account_unique_conflict, new_shadow_user_active_model};
use crate::{
    entity::users,
    error::{NuonoError, Result},
    identity_resolver::{resolve_canonical_user_id_on, resolve_or_provision_on_tx},
};

const BINDING_TTL_MS: i64 = 300_000;
const MAX_RECENT_REQUESTS: u64 = 5;
const CHALLENGE_RETENTION_MS: i64 = 86_400_000;

pub(crate) mod identities {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "nuono_platform_identities")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub platform: String,
        pub namespace: String,
        pub identity_kind: String,
        pub native_user_id: String,
        pub user_id: i64,
        pub verified_at_ms: Option<i64>,
        pub revision: i64,
        pub created_at_ms: i64,
        pub updated_at_ms: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Serialize, Deserialize)]
struct BindingContext {
    bot: BotIdentity,
    expected_sender: Option<IdentityRef>,
    /// 聊天侧核验后记录的发送者身份（分支 B 下尚未入库时不提前创建占位账号）。
    #[serde(default)]
    observed_sender: Option<IdentityRef>,
}

/// 已验证的外部平台身份视图（整型 ID 与版本号均以无损十进制字符串序列化）。
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PlatformIdentityView {
    pub id: String,
    pub platform: String,
    pub namespace: String,
    pub identity_kind: String,
    pub native_user_id: String,
    pub revision: String,
}
impl From<identities::Model> for PlatformIdentityView {
    fn from(row: identities::Model) -> Self {
        Self {
            id: row.id.to_string(),
            platform: row.platform,
            namespace: row.namespace,
            identity_kind: row.identity_kind,
            native_user_id: row.native_user_id,
            revision: row.revision.to_string(),
        }
    }
}

/// 面向站长与管理员的全局平台身份视图，汇总每个已验证身份所属站点账号及生效聊天权限。
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct AccountPlatformIdentityView {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub user_role: String,
    pub user_status: String,
    pub platform: String,
    pub namespace: String,
    pub identity_kind: String,
    pub native_user_id: String,
    pub revision: String,
    pub verified_at_ms: Option<String>,
    pub chat_permission: String,
}

/// 平台身份绑定或注册认领请求视图（收到候选平台身份后，仍须在发起操作的原端确认）。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PlatformBindingView {
    pub id: String,
    pub state: String,
    pub expires_at_ms: String,
    pub candidate: Option<PlatformIdentityView>,
    pub data_claim_plan: Option<serde_json::Value>,
    pub data_claim_digest: Option<String>,
}

/// 发起平台身份绑定或注册认领后的响应（一次性验证口令 `command` 仅在签发时返回一次）。
#[derive(Serialize, utoipa::ToSchema)]
pub struct PlatformBindingIssued {
    pub request: PlatformBindingView,
    pub command: String,
}

pub(crate) fn identity_parts(identity: &IdentityRef) -> Result<(String, String, String, String)> {
    if *identity.kind() != IdentityKind::User {
        return Err(NuonoError::Validation("只能绑定平台用户身份".into()));
    }
    let fields = (
        identity.platform().as_str().to_owned(),
        identity
            .namespace()
            .map_or_else(String::new, |id| id.as_str().to_owned()),
        serde_json::to_string(identity.scheme())?,
        identity.value().as_str().to_owned(),
    );
    if fields.0.len() + fields.1.len() + fields.2.len() + fields.3.len() > 1024
        || [&fields.0, &fields.1, &fields.2, &fields.3]
            .iter()
            .any(|v| v.contains('\0'))
    {
        return Err(NuonoError::Validation("平台身份超出可存储范围".into()));
    }
    Ok(fields)
}

pub(crate) fn identity_query(
    identity: &IdentityRef,
) -> Result<sea_orm::Select<identities::Entity>> {
    let (platform, namespace, kind, native) = identity_parts(identity)?;
    Ok(identities::Entity::find()
        .filter(identities::Column::Platform.eq(platform))
        .filter(identities::Column::Namespace.eq(namespace))
        .filter(identities::Column::IdentityKind.eq(kind))
        .filter(identities::Column::NativeUserId.eq(native)))
}

fn synthetic_candidate_view(identity: &IdentityRef) -> Result<PlatformIdentityView> {
    let (platform, namespace, identity_kind, native_user_id) = identity_parts(identity)?;
    Ok(PlatformIdentityView {
        id: "0".into(),
        platform,
        namespace,
        identity_kind,
        native_user_id,
        revision: "1".into(),
    })
}

/// 将某条已绑定的平台身份从原正式账号解绑并回退到一条全新独立的 `shadow` 占位账号，
/// 既维持 `nuono_platform_identities.user_id NOT NULL` 强约束，又保留不可变的 `identity_id` 历史锚点。
async fn detach_identity_to_fresh_shadow_on(
    tx: &DatabaseTransaction,
    identity_id: i64,
    from_user_id: i64,
    expected_revision: Option<i64>,
    now: i64,
) -> Result<u64> {
    let fresh_shadow_id = users::orm::Entity::insert(new_shadow_user_active_model("", now))
        .exec(tx)
        .await?
        .last_insert_id;
    let mut update = identities::Entity::update_many()
        .col_expr(identities::Column::UserId, Expr::value(fresh_shadow_id))
        .col_expr(identities::Column::VerifiedAtMs, Expr::value(None::<i64>))
        .col_expr(identities::Column::UpdatedAtMs, Expr::value(now))
        .col_expr(
            identities::Column::Revision,
            Expr::col(identities::Column::Revision).add(1),
        )
        .filter(identities::Column::Id.eq(identity_id))
        .filter(identities::Column::UserId.eq(from_user_id));
    if let Some(revision) = expected_revision {
        update = update.filter(identities::Column::Revision.eq(revision));
    }
    let changed = update.exec(tx).await?.rows_affected;
    if changed == 0 {
        // 乐观锁未命中时回删刚插入的空占位行，避免留下孤儿记录。
        users::orm::Entity::delete_by_id(fresh_shadow_id)
            .exec(tx)
            .await?;
    }
    Ok(changed)
}

/// 账号删除时将其名下所有平台身份分别剥离至全新的独立占位账号（供 `account_store` 复用）。
pub(crate) async fn detach_all_identities_for_user_on(
    tx: &DatabaseTransaction,
    user_id: i64,
    now: i64,
) -> Result<()> {
    let rows = identities::Entity::find()
        .filter(identities::Column::UserId.eq(user_id))
        .order_by_asc(identities::Column::Id)
        .all(tx)
        .await?;
    for row in rows {
        detach_identity_to_fresh_shadow_on(tx, row.id, user_id, None, now).await?;
    }
    Ok(())
}

impl Database {
    async fn binding_account(
        &self,
        tx: &DatabaseTransaction,
        user_id: i64,
    ) -> Result<users::orm::Model> {
        let mut query = users::orm::Entity::find_by_id(user_id);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        query
            .one(tx)
            .await?
            .filter(|u| {
                u.status == "active"
                    && u.account_kind == users::ACCOUNT_KIND_REGISTERED
                    && u.password_hash.is_some()
            })
            .ok_or_else(|| NuonoError::PermissionDenied("请使用有效正式账号登录".into()))
    }

    async fn require_binding_session(
        &self,
        tx: &DatabaseTransaction,
        user: &users::orm::Model,
        session: &str,
    ) -> Result<sessions::Model> {
        session_on(tx, token_hash(session), true)
            .await?
            .filter(|row| row.user_id == user.id && row.auth_epoch == user.auth_epoch)
            .ok_or_else(|| NuonoError::PermissionDenied("登录会话已失效，请重新登录后绑定".into()))
    }

    /// 分支 A（原地转正）第 1 步：未登录用户在网站注册页签发一次性平台身份验证口令。
    pub async fn begin_platform_registration(
        &self,
        bot: BotIdentity,
        expected_sender: Option<IdentityRef>,
    ) -> Result<PlatformBindingIssued> {
        if let Some(sender) = &expected_sender {
            identity_parts(sender)?;
            if sender.platform() != bot.platform() {
                return Err(NuonoError::Validation("绑定平台与机器人不一致".into()));
            }
        }
        let now = crate::clock::db_now().timestamp_millis();
        let code = uuid::Uuid::new_v4().simple().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let tx = self.begin_write().await?;
        challenges::ActiveModel {
            id: Set(id.clone()),
            purpose: Set("registration".into()),
            requester_user_id: Set(None),
            subject_user_id: Set(None),
            subject_auth_epoch: Set(None),
            session_id: Set(None),
            token_hash: Set(Sha256::digest(code.as_bytes()).to_vec()),
            context_json: Set(serde_json::to_string(&BindingContext {
                bot,
                expected_sender,
                observed_sender: None,
            })?),
            candidate_identity_id: Set(None),
            state: Set("pending".into()),
            attempts: Set(0),
            expires_at_ms: Set(now + BINDING_TTL_MS),
            consumed_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(PlatformBindingIssued {
            request: PlatformBindingView {
                id,
                state: "pending".into(),
                expires_at_ms: (now + BINDING_TTL_MS).to_string(),
                candidate: None,
                data_claim_plan: None,
                data_claim_digest: None,
            },
            command: format!("绑定 {code}"),
        })
    }

    /// 分支 A（原地转正）第 2 步：查询未登录注册验证请求的核验状态与候选平台身份。
    pub async fn platform_registration_status(
        &self,
        challenge_id: &str,
    ) -> Result<PlatformBindingView> {
        let row = challenges::Entity::find_by_id(challenge_id)
            .filter(challenges::Column::Purpose.eq("registration"))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::NotFound("注册验证请求不存在".into()))?;
        let now = crate::clock::db_now().timestamp_millis();
        let candidate_row = match row.candidate_identity_id {
            Some(id) => identities::Entity::find_by_id(id).one(self.orm()).await?,
            None => None,
        };
        if row.state == "candidate"
            && row.expires_at_ms > now
            && let Some(identity) = &candidate_row
        {
            let canonical_id = resolve_canonical_user_id_on(self.orm(), identity.user_id).await?;
            let owner = users::orm::Entity::find_by_id(canonical_id)
                .one(self.orm())
                .await?
                .ok_or_else(|| NuonoError::Internal("平台身份所属账号不存在".into()))?;
            if owner.account_kind == users::ACCOUNT_KIND_REGISTERED {
                return Err(NuonoError::Conflict(
                    "该平台账号已绑定其他注册账号，请直接登录".into(),
                ));
            }
            if owner.status != "active" {
                return Err(NuonoError::PermissionDenied(
                    "该平台关联的占位账号已被停用，无法注册".into(),
                ));
            }
        }
        let state =
            if row.expires_at_ms <= now && matches!(row.state.as_str(), "pending" | "candidate") {
                "expired".to_owned()
            } else {
                row.state
            };
        Ok(PlatformBindingView {
            id: row.id,
            state,
            expires_at_ms: row.expires_at_ms.to_string(),
            candidate: candidate_row.map(Into::into),
            data_claim_plan: None,
            data_claim_digest: None,
        })
    }

    /// 分支 A（原地转正）第 3 步：核验平台身份归属后，直接把对应 `shadow` 占位记录原地升级为正式账号，
    /// 保持原 `user_id` 不变，无需迁移任何子表数据。
    pub async fn complete_platform_registration(
        &self,
        challenge_id: &str,
        username: &str,
        email: Option<&str>,
        password: &str,
    ) -> Result<users::Model> {
        let username = username.trim().to_owned();
        nuono_app::auth::validate_username(&username).map_err(NuonoError::Validation)?;
        nuono_app::auth::validate_password(password).map_err(NuonoError::Validation)?;
        let email = email
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty());
        if let Some(ref addr) = email {
            nuono_app::auth::validate_email(addr).map_err(NuonoError::Validation)?;
        }
        let password_hash = self.password_work.hash(password).await?;

        let tx = self.begin_write().await?;
        let initial = challenges::Entity::find_by_id(challenge_id)
            .filter(challenges::Column::Purpose.eq("registration"))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("注册验证请求不存在".into()))?;
        let identity_id = initial
            .candidate_identity_id
            .ok_or_else(|| NuonoError::Conflict("尚未收到平台身份验证消息".into()))?;
        let identity_snapshot = identities::Entity::find_by_id(identity_id)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Conflict("平台身份不存在".into()))?;
        let canonical_owner_id =
            resolve_canonical_user_id_on(&tx, identity_snapshot.user_id).await?;

        // 严格遵循「先锁账号，再锁身份与挑战」锁序。
        let mut user_query = users::orm::Entity::find_by_id(canonical_owner_id);
        if self.backend() == DbBackend::Postgres {
            user_query = user_query.lock_exclusive();
        }
        let owner = user_query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Conflict("占位账号不存在".into()))?;
        let mut challenge_query = challenges::Entity::find_by_id(challenge_id);
        if self.backend() == DbBackend::Postgres {
            challenge_query = challenge_query.lock_exclusive();
        }
        let row = challenge_query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("注册验证请求不存在".into()))?;
        let now = crate::clock::db_now().timestamp_millis();
        if row.state != "candidate" || row.expires_at_ms <= now {
            return Err(NuonoError::Conflict(
                "注册验证请求未就绪、已过期或已使用".into(),
            ));
        }
        let mut id_query = identities::Entity::find_by_id(identity_id);
        if self.backend() == DbBackend::Postgres {
            id_query = id_query.lock_exclusive();
        }
        let identity = id_query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Conflict("平台身份不存在".into()))?;
        if resolve_canonical_user_id_on(&tx, identity.user_id).await? != owner.id {
            return Err(NuonoError::Conflict(
                "平台身份归属已发生变化，请重试".into(),
            ));
        }
        if owner.account_kind != users::ACCOUNT_KIND_SHADOW {
            return Err(NuonoError::Conflict(
                "该平台账号已绑定其他注册账号，请直接登录".into(),
            ));
        }
        if owner.status != "active" {
            return Err(NuonoError::PermissionDenied(
                "该平台关联账号已被停用，无法注册转正".into(),
            ));
        }

        // 原地转正：直接更新原 shadow 行，user_id 保持不变，零子表迁移。
        let username_key = username.to_ascii_lowercase();
        let email_verified_at_ms = email.as_ref().map(|_| now);
        let updated = users::orm::Entity::update_many()
            .col_expr(
                users::orm::Column::AccountKind,
                Expr::value(users::ACCOUNT_KIND_REGISTERED),
            )
            .col_expr(users::orm::Column::Username, Expr::value(username.clone()))
            .col_expr(users::orm::Column::UsernameKey, Expr::value(username_key))
            .col_expr(users::orm::Column::Email, Expr::value(email.clone()))
            .col_expr(users::orm::Column::EmailKey, Expr::value(email.clone()))
            .col_expr(
                users::orm::Column::EmailVerifiedAtMs,
                Expr::value(email_verified_at_ms),
            )
            .col_expr(users::orm::Column::PasswordHash, Expr::value(password_hash))
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            )
            .col_expr(users::orm::Column::UpdatedAtMs, Expr::value(now))
            .filter(users::orm::Column::Id.eq(owner.id))
            .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_SHADOW))
            .filter(users::orm::Column::Status.eq("active"))
            .exec(&tx)
            .await
            .map_err(account_unique_conflict)?;
        if updated.rows_affected != 1 {
            return Err(NuonoError::Conflict("占位账号状态已变化，请重试".into()));
        }

        identities::Entity::update_many()
            .col_expr(identities::Column::UserId, Expr::value(owner.id))
            .col_expr(identities::Column::VerifiedAtMs, Expr::value(now))
            .col_expr(identities::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                identities::Column::Revision,
                Expr::col(identities::Column::Revision).add(1),
            )
            .filter(identities::Column::Id.eq(identity.id))
            .exec(&tx)
            .await?;

        let consumed = challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("consumed"))
            .col_expr(challenges::Column::ConsumedAtMs, Expr::value(now))
            .col_expr(challenges::Column::SubjectUserId, Expr::value(owner.id))
            .col_expr(
                challenges::Column::SubjectAuthEpoch,
                Expr::value(owner.auth_epoch),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .filter(challenges::Column::Id.eq(&row.id))
            .filter(challenges::Column::State.eq("candidate"))
            .filter(challenges::Column::Revision.eq(row.revision))
            .exec(&tx)
            .await?;
        if consumed.rows_affected != 1 {
            return Err(NuonoError::Conflict("注册验证请求已失效".into()));
        }

        super::append_audit_on(
            &tx,
            super::AuditEvent {
                actor_user_id: Some(owner.id),
                actor_plugin_id: None,
                action: "account.shadow_promoted",
                result: "success",
                subject_user_id: Some(owner.id),
                group_id: None,
                operation_id: Some(&row.id),
                details: serde_json::json!({
                    "identity_id": identity.id.to_string(),
                }),
            },
        )
        .await?;

        let promoted = users::orm::Entity::find_by_id(owner.id)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Internal("转正后账号读取失败".into()))?;
        tx.commit().await?;
        self.invalidate_identity_cache_for_user(owner.id);
        Ok(promoted.into())
    }

    /// 签发与当前已认证浏览器登录会话绑定的短期一次性平台绑定口令。
    ///
    /// # Errors
    /// 若账号未激活、登录会话无效、预期发送者平台与机器人不匹配或短期内请求过于频繁，则返回错误。
    pub async fn begin_platform_binding(
        &self,
        user_id: i64,
        bot: BotIdentity,
        session: &str,
        expected_sender: Option<IdentityRef>,
    ) -> Result<PlatformBindingIssued> {
        if session.is_empty() {
            return Err(NuonoError::PermissionDenied(
                "绑定需要已验证登录会话".into(),
            ));
        }
        if let Some(sender) = &expected_sender {
            identity_parts(sender)?;
            if sender.platform() != bot.platform() {
                return Err(NuonoError::Validation("绑定平台与机器人不一致".into()));
            }
        }
        let now = crate::clock::db_now().timestamp_millis();
        let tx = self.begin_write().await?;
        let user = self.binding_account(&tx, user_id).await?;
        let session_row = self.require_binding_session(&tx, &user, session).await?;
        let recent = challenges::Entity::find()
            .filter(challenges::Column::RequesterUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq("platform_binding"))
            .filter(challenges::Column::CreatedAtMs.gt(now - BINDING_TTL_MS))
            .count(&tx)
            .await?;
        if recent >= MAX_RECENT_REQUESTS {
            return Err(NuonoError::Conflict("绑定请求过于频繁，请稍后再试".into()));
        }
        let code = uuid::Uuid::new_v4().simple().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let row = challenges::ActiveModel {
            id: Set(id.clone()),
            purpose: Set("platform_binding".into()),
            requester_user_id: Set(Some(user_id)),
            subject_user_id: Set(Some(user_id)),
            subject_auth_epoch: Set(Some(user.auth_epoch)),
            session_id: Set(Some(session_row.id)),
            token_hash: Set(Sha256::digest(code.as_bytes()).to_vec()),
            context_json: Set(serde_json::to_string(&BindingContext {
                bot,
                expected_sender,
                observed_sender: None,
            })?),
            candidate_identity_id: Set(None),
            state: Set("pending".into()),
            attempts: Set(0),
            expires_at_ms: Set(now + BINDING_TTL_MS),
            consumed_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        };
        row.insert(&tx).await?;
        tx.commit().await?;
        Ok(PlatformBindingIssued {
            request: PlatformBindingView {
                id,
                state: "pending".into(),
                expires_at_ms: (now + BINDING_TTL_MS).to_string(),
                candidate: None,
                data_claim_plan: None,
                data_claim_digest: None,
            },
            command: format!("绑定 {code}"),
        })
    }

    /// 记录首个携带有效绑定或注册口令的聊天平台发送者身份（仅登记为待确认候选 `candidate`，不会直接完成绑定）。
    ///
    /// - 对已登录绑定（`platform_binding`）：若该平台身份尚未入库（分支 B），仅记录发送者信息而不提前创建废弃占位账号；
    ///   若已存在占位或正式记录（分支 C/D），记录对应 `candidate_identity_id` 供确认阶段处理。
    /// - 对未登录注册（`registration`，分支 A）：幂等解析或准备对应占位账号，供网站侧原地转正。
    pub async fn observe_platform_binding(
        &self,
        code: &str,
        bot: &BotIdentity,
        sender: &IdentityRef,
    ) -> Result<bool> {
        if code.len() != 32
            || !code.bytes().all(|c| c.is_ascii_hexdigit())
            || sender.platform() != bot.platform()
            || crate::identity_resolver::is_non_human_identity(sender)
        {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let mut query = challenges::Entity::find()
            .filter(challenges::Column::TokenHash.eq(Sha256::digest(code.as_bytes()).to_vec()))
            .filter(challenges::Column::Purpose.is_in(["platform_binding", "registration"]))
            .filter(challenges::Column::State.eq("pending"))
            .filter(challenges::Column::ExpiresAtMs.gt(now));
        let Some(snapshot) = query.clone().one(&tx).await? else {
            return Ok(false);
        };
        let is_registration = snapshot.purpose == "registration";
        let requester_user = if is_registration {
            None
        } else {
            let Some(user_id) = snapshot.requester_user_id else {
                return Ok(false);
            };
            // 与确认绑定、解绑及凭据变更保持相同的「先锁账号、后锁挑战记录」顺序，避免死锁。
            match self.binding_account(&tx, user_id).await {
                Ok(user) => Some(user),
                Err(NuonoError::PermissionDenied(_)) => return Ok(false),
                Err(error) => return Err(error),
            }
        };
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        // 等待账号锁期间时间可能已推进或挑战状态已变更，此处重新校验实时状态与过期时间。
        query = query
            .filter(challenges::Column::ExpiresAtMs.gt(crate::clock::db_now().timestamp_millis()));
        let Some(challenge) = query.one(&tx).await? else {
            return Ok(false);
        };
        if let Some(user) = &requester_user
            && challenge.subject_auth_epoch != Some(user.auth_epoch)
        {
            return Ok(false);
        }
        let mut context: BindingContext = serde_json::from_str(&challenge.context_json)?;
        if context.bot != *bot
            || context
                .expected_sender
                .as_ref()
                .is_some_and(|expected| expected != sender)
        {
            return Ok(false);
        }
        identity_parts(sender)?;
        context.observed_sender = Some(sender.clone());

        let candidate_identity_id = if is_registration {
            let resolved = resolve_or_provision_on_tx(&tx, sender, None).await?;
            Some(resolved.identity_id)
        } else {
            // 分支 B：若发送者尚未在 nuono_platform_identities 中出现，保持 candidate_identity_id 为 None，
            // 避免凭空建一条立刻要销毁的占位用户；分支 C/D 则直接关联已有记录。
            identity_query(sender)?
                .one(&tx)
                .await?
                .map(|existing| existing.id)
        };

        let changed = challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("candidate"))
            .col_expr(
                challenges::Column::CandidateIdentityId,
                Expr::value(candidate_identity_id),
            )
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context)?),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::Id.eq(challenge.id))
            .filter(challenges::Column::State.eq("pending"))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(changed.rows_affected == 1)
    }

    async fn owned_binding(
        &self,
        tx: &DatabaseTransaction,
        user_id: i64,
        id: &str,
        session: &str,
    ) -> Result<challenges::Model> {
        let user = self.binding_account(tx, user_id).await?;
        let session_row = self.require_binding_session(tx, &user, session).await?;
        let row = challenges::Entity::find_by_id(id)
            .filter(challenges::Column::RequesterUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq("platform_binding"))
            .one(tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("绑定请求不存在".into()))?;
        if session.is_empty()
            || row.session_id.as_deref() != Some(session_row.id.as_str())
            || row.subject_auth_epoch != Some(user.auth_epoch)
        {
            return Err(NuonoError::PermissionDenied(
                "请在发起绑定的原登录会话中操作".into(),
            ));
        }
        Ok(row)
    }

    /// 查询绑定请求详情与待确认候选身份（仅允许发起该绑定请求的原登录会话读取）。
    pub async fn platform_binding(
        &self,
        user_id: i64,
        id: &str,
        session: &str,
    ) -> Result<PlatformBindingView> {
        let tx = self.begin_write().await?;
        let row = self.owned_binding(&tx, user_id, id, session).await?;
        let context: BindingContext = serde_json::from_str(&row.context_json)?;
        let candidate_row = match row.candidate_identity_id {
            Some(identity_id) => {
                let mut query = identities::Entity::find_by_id(identity_id);
                if self.backend() == DbBackend::Postgres {
                    query = query.lock_exclusive();
                }
                query.one(&tx).await?
            }
            None => match &context.observed_sender {
                Some(sender) => {
                    let mut query = identity_query(sender)?;
                    if self.backend() == DbBackend::Postgres {
                        query = query.lock_exclusive();
                    }
                    query.one(&tx).await?
                }
                None => None,
            },
        };
        let mut data_claim_plan = None;
        let mut data_claim_digest = None;
        let now = crate::clock::db_now().timestamp_millis();
        if row.state == "candidate"
            && row.expires_at_ms > now
            && let Some(identity) = &candidate_row
        {
            let canonical_owner_id = resolve_canonical_user_id_on(&tx, identity.user_id).await?;
            if canonical_owner_id != user_id {
                let owner = users::orm::Entity::find_by_id(canonical_owner_id)
                    .one(&tx)
                    .await?
                    .ok_or_else(|| NuonoError::Conflict("平台身份归属账号不存在".into()))?;
                if owner.account_kind == users::ACCOUNT_KIND_REGISTERED {
                    // 分支 D：目标平台身份已归属另一正式注册账号，默认拒绝自动合并。
                    return Err(NuonoError::Conflict(
                        "该平台账号已绑定其他注册账号，请使用双端确认的账号合并流程".into(),
                    ));
                }
                // 分支 C：平台身份归属占位账号，预检该占位账号名下是否存在需确认合并的插件数据。
                let plan = shadow_merge_plan_on(&tx, owner.id, user_id, &row.id).await?;
                if plan.as_object().is_some_and(|p| !p.is_empty()) {
                    data_claim_digest = Some(shadow_merge_plan_digest(&plan)?);
                    data_claim_plan = Some(plan);
                }
            }
        }
        let candidate = match candidate_row {
            Some(existing) => Some(existing.into()),
            None if row.state == "candidate" => context
                .observed_sender
                .as_ref()
                .map(synthetic_candidate_view)
                .transpose()?,
            None => None,
        };
        let state =
            if row.expires_at_ms <= now && matches!(row.state.as_str(), "pending" | "candidate") {
                "expired".to_owned()
            } else {
                row.state
            };
        tx.commit().await?;
        Ok(PlatformBindingView {
            id: row.id,
            state,
            expires_at_ms: row.expires_at_ms.to_string(),
            candidate,
            data_claim_plan,
            data_claim_digest,
        })
    }

    /// 校验候选平台身份的乐观锁版本号（`identity_revision`）并确认绑定到当前账号。
    pub async fn confirm_platform_binding(
        &self,
        user_id: i64,
        id: &str,
        session: &str,
        identity_revision: i64,
    ) -> Result<()> {
        self.confirm_platform_binding_with_claim(user_id, id, session, identity_revision, None)
            .await
    }

    /// 确认平台身份绑定（覆盖分支 B 直接绑定、分支 C 占位合并、分支 D 已注册冲突拒绝）。
    pub async fn confirm_platform_binding_with_claim(
        &self,
        user_id: i64,
        id: &str,
        session: &str,
        identity_revision: i64,
        data_claim_digest: Option<&str>,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        // 先读取无锁快照以确定是否涉及占位账号合并，从而在加排他锁前按账号 ID 升序上锁，彻底消除死锁。
        let pre_row = challenges::Entity::find_by_id(id)
            .filter(challenges::Column::RequesterUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq("platform_binding"))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("绑定请求不存在".into()))?;
        let pre_context: BindingContext = serde_json::from_str(&pre_row.context_json)?;
        let pre_identity = match pre_row.candidate_identity_id {
            Some(identity_id) => identities::Entity::find_by_id(identity_id).one(&tx).await?,
            None => match &pre_context.observed_sender {
                Some(sender) => identity_query(sender)?.one(&tx).await?,
                None => None,
            },
        };
        let mut lock_user_ids = vec![user_id];
        if let Some(ref existing) = pre_identity {
            let owner_id = resolve_canonical_user_id_on(&tx, existing.user_id).await?;
            lock_user_ids.push(owner_id);
        }
        crate::group_authorizations::lock_accounts(&tx, &lock_user_ids).await?;

        let row = self.owned_binding(&tx, user_id, id, session).await?;
        let now = crate::clock::db_now().timestamp_millis();
        if row.state != "candidate" || row.expires_at_ms <= now {
            return Err(NuonoError::Conflict(
                "绑定请求未收到候选、已过期或已使用".into(),
            ));
        }
        let context: BindingContext = serde_json::from_str(&row.context_json)?;
        let existing_identity = match row.candidate_identity_id {
            Some(identity_id) => {
                let mut query = identities::Entity::find_by_id(identity_id);
                if self.backend() == DbBackend::Postgres {
                    query = query.lock_exclusive();
                }
                query.one(&tx).await?
            }
            None => match &context.observed_sender {
                Some(sender) => {
                    let mut query = identity_query(sender)?;
                    if self.backend() == DbBackend::Postgres {
                        query = query.lock_exclusive();
                    }
                    query.one(&tx).await?
                }
                None => None,
            },
        };

        let mut merged_shadow_id: Option<i64> = None;
        let bound_identity_id: i64;
        let mut claim_plan_for_audit: Option<serde_json::Value> = None;

        match existing_identity {
            None => {
                // 分支 B（直接绑定）：该平台身份尚未入库，直接插入并指向 target_user_id，不创建占位账号。
                let sender = context
                    .observed_sender
                    .as_ref()
                    .ok_or_else(|| NuonoError::Conflict("缺少待确认身份".into()))?;
                if identity_revision != 1 {
                    return Err(NuonoError::Conflict("身份版本不匹配，请刷新后重试".into()));
                }
                let (platform, namespace, kind, native) = identity_parts(sender)?;
                bound_identity_id = identities::Entity::insert(identities::ActiveModel {
                    platform: Set(platform),
                    namespace: Set(namespace),
                    identity_kind: Set(kind),
                    native_user_id: Set(native),
                    user_id: Set(user_id),
                    verified_at_ms: Set(Some(now)),
                    revision: Set(1),
                    created_at_ms: Set(now),
                    updated_at_ms: Set(now),
                    ..Default::default()
                })
                .exec(&tx)
                .await?
                .last_insert_id;
            }
            Some(identity) => {
                if identity.revision != identity_revision {
                    return Err(NuonoError::Conflict("身份已变化，请刷新后重试".into()));
                }
                let canonical_owner_id =
                    resolve_canonical_user_id_on(&tx, identity.user_id).await?;
                if canonical_owner_id == user_id {
                    // 已归属当前账号时仅补齐验证时间戳。
                    let changed = identities::Entity::update_many()
                        .col_expr(identities::Column::UserId, Expr::value(user_id))
                        .col_expr(identities::Column::VerifiedAtMs, Expr::value(now))
                        .col_expr(identities::Column::UpdatedAtMs, Expr::value(now))
                        .col_expr(
                            identities::Column::Revision,
                            Expr::col(identities::Column::Revision).add(1),
                        )
                        .filter(identities::Column::Id.eq(identity.id))
                        .filter(identities::Column::Revision.eq(identity_revision))
                        .exec(&tx)
                        .await?;
                    if changed.rows_affected != 1 {
                        return Err(NuonoError::Conflict("身份已变化，请刷新后重试".into()));
                    }
                    bound_identity_id = identity.id;
                } else {
                    // 若并发期间归属账号与无锁预读不一致，先确保按规范锁定该账号。
                    if !lock_user_ids.contains(&canonical_owner_id) {
                        crate::group_authorizations::lock_accounts(&tx, &[canonical_owner_id])
                            .await?;
                    }
                    let owner = users::orm::Entity::find_by_id(canonical_owner_id)
                        .one(&tx)
                        .await?
                        .ok_or_else(|| NuonoError::Conflict("平台身份归属账号不存在".into()))?;
                    if owner.account_kind == users::ACCOUNT_KIND_REGISTERED {
                        // 分支 D（已注册冲突）：默认拒绝自动合并，引导至双端确认合并流程。
                        return Err(NuonoError::Conflict(
                            "该平台账号已绑定其他注册账号，请使用双端确认的账号合并流程".into(),
                        ));
                    }
                    // 分支 C（占位合并）：单事务内将占位账号的数据、对话、权益与插件数据并入当前正式账号。
                    let claim_plan = shadow_merge_plan_on(&tx, owner.id, user_id, &row.id).await?;
                    if claim_plan.as_object().is_some_and(|p| !p.is_empty()) {
                        let expected_digest = shadow_merge_plan_digest(&claim_plan)?;
                        if data_claim_digest != Some(expected_digest.as_str()) {
                            return Err(NuonoError::Conflict(
                                "占位账号插件数据或合并计划已变化，请查看预览后确认归入账号".into(),
                            ));
                        }
                        claim_plan_for_audit = Some(claim_plan.clone());
                    }
                    merge_shadow_into_user_on(&tx, owner.id, user_id, &row.id, Some(&claim_plan))
                        .await?;
                    // 合并引擎已将身份迁移至 user_id 并递增 revision，此处补写 verified_at_ms。
                    identities::Entity::update_many()
                        .col_expr(identities::Column::VerifiedAtMs, Expr::value(now))
                        .col_expr(identities::Column::UpdatedAtMs, Expr::value(now))
                        .col_expr(
                            identities::Column::Revision,
                            Expr::col(identities::Column::Revision).add(1),
                        )
                        .filter(identities::Column::Id.eq(identity.id))
                        .filter(identities::Column::UserId.eq(user_id))
                        .exec(&tx)
                        .await?;
                    merged_shadow_id = Some(owner.id);
                    bound_identity_id = identity.id;
                }
            }
        }

        let consumed = challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("consumed"))
            .col_expr(challenges::Column::ConsumedAtMs, Expr::value(now))
            .col_expr(
                challenges::Column::CandidateIdentityId,
                Expr::value(bound_identity_id),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .filter(challenges::Column::Id.eq(&row.id))
            .filter(challenges::Column::State.eq("candidate"))
            .filter(challenges::Column::Revision.eq(row.revision))
            .exec(&tx)
            .await?;
        if consumed.rows_affected != 1 {
            return Err(NuonoError::Conflict("绑定请求已变化".into()));
        }
        if let Some(claim_plan) = claim_plan_for_audit {
            super::append_audit_on(
                &tx,
                super::AuditEvent {
                    actor_user_id: Some(user_id),
                    actor_plugin_id: None,
                    action: "identity.data_claimed",
                    result: "success",
                    subject_user_id: Some(user_id),
                    group_id: None,
                    operation_id: Some(&row.id),
                    details: serde_json::json!({
                        "identity_id": bound_identity_id.to_string(),
                        "plugins": claim_plan.as_object().map(|p| p.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                        "digest": shadow_merge_plan_digest(&claim_plan)?,
                    }),
                },
            )
            .await?;
        }
        self.binding_audit(
            &tx,
            user_id,
            "identity.bound",
            &row.id,
            bound_identity_id,
            now,
        )
        .await?;
        tx.commit().await?;

        // 提交后立即清除相关账号与身份的进程内 LRU 缓存。
        if let Some(shadow_id) = merged_shadow_id {
            self.invalidate_identity_cache_for_user(shadow_id);
        }
        self.invalidate_identity_cache_for_user(user_id);
        if let Some(ref sender) = context.observed_sender {
            self.invalidate_identity_cache_for_ref(sender);
        }
        Ok(())
    }

    /// 取消尚未完成的平台绑定请求（不影响候选平台身份当前的归属状态）。
    pub async fn cancel_platform_binding(
        &self,
        user_id: i64,
        id: &str,
        session: &str,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        self.owned_binding(&tx, user_id, id, session).await?;
        let changed = challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("cancelled"))
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .col_expr(
                challenges::Column::UpdatedAtMs,
                Expr::value(crate::clock::db_now().timestamp_millis()),
            )
            .filter(challenges::Column::Id.eq(id))
            .filter(challenges::Column::State.is_in(["pending", "candidate"]))
            .exec(&tx)
            .await?;
        if changed.rows_affected > 0 {
            super::account_state::append_audit_on(
                &tx,
                super::account_state::AuditEvent {
                    actor_user_id: Some(user_id),
                    actor_plugin_id: None,
                    action: "identity.binding_cancelled",
                    result: "success",
                    subject_user_id: Some(user_id),
                    group_id: None,
                    operation_id: Some(id),
                    details: serde_json::json!({}),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 列出指定账号当前已绑定的所有外部聊天平台身份。
    pub async fn platform_identities(&self, user_id: i64) -> Result<Vec<PlatformIdentityView>> {
        let rows = identities::Entity::find()
            .filter(identities::Column::UserId.eq(user_id))
            .order_by_asc(identities::Column::Id)
            .all(self.orm())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 列出全站所有已绑定且已验证的外部平台身份及其继承到的聊天权限等级（仅正式注册账号）。
    pub async fn list_account_platform_identities(
        &self,
    ) -> Result<Vec<AccountPlatformIdentityView>> {
        let sql = "SELECT pi.id, pi.user_id, u.username, u.role, u.status, \
                   pi.platform, pi.namespace, pi.identity_kind, pi.native_user_id, \
                   pi.revision, pi.verified_at_ms \
                   FROM nuono_platform_identities pi \
                   JOIN nuono_users u ON u.id = pi.user_id \
                   WHERE pi.verified_at_ms IS NOT NULL AND u.account_kind = 'registered' \
                   ORDER BY CASE u.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 ELSE 2 END, \
                            u.username ASC, pi.platform ASC, pi.id ASC";
        let rows = self
            .orm()
            .query_all_raw(Statement::from_string(self.backend(), sql))
            .await?;
        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.try_get("", "id")?;
            let user_id: i64 = row.try_get("", "user_id")?;
            let username: String = row.try_get("", "username")?;
            let user_role: String = row.try_get("", "role")?;
            let user_status: String = row.try_get("", "status")?;
            let platform: String = row.try_get("", "platform")?;
            let namespace: String = row.try_get("", "namespace")?;
            let identity_kind: String = row.try_get("", "identity_kind")?;
            let native_user_id: String = row.try_get("", "native_user_id")?;
            let revision: i64 = row.try_get("", "revision")?;
            let verified_at_ms: Option<i64> = row.try_get("", "verified_at_ms")?;
            let chat_permission = if user_status == "active" {
                match user_role.as_str() {
                    "owner" => "BotOwner",
                    "admin" => "BotAdmin",
                    _ => "Everyone",
                }
            } else {
                "Everyone"
            }
            .to_owned();
            views.push(AccountPlatformIdentityView {
                id: id.to_string(),
                user_id: user_id.to_string(),
                username,
                user_role,
                user_status,
                platform,
                namespace,
                identity_kind,
                native_user_id,
                revision: revision.to_string(),
                verified_at_ms: verified_at_ms.map(|v| v.to_string()),
                chat_permission,
            });
        }
        Ok(views)
    }

    /// 由站点所有者直接将外部聊天平台身份绑定到指定活跃正式账号（支持直接新建绑定及自动合并占位账号）。
    pub async fn admin_bind_platform_identity(
        &self,
        actor_user_id: i64,
        target_user_id: i64,
        identity: &IdentityRef,
    ) -> Result<PlatformIdentityView> {
        let (platform, namespace, kind, native) = identity_parts(identity)?;
        let tx = self.begin_write().await?;
        let pre_existing = identity_query(identity)?.one(&tx).await?;
        let mut lock_ids = vec![target_user_id];
        if let Some(ref existing) = pre_existing {
            lock_ids.push(resolve_canonical_user_id_on(&tx, existing.user_id).await?);
        }
        crate::group_authorizations::lock_accounts(&tx, &lock_ids).await?;

        let target_user = users::orm::Entity::find_by_id(target_user_id)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("目标账号不存在".into()))?;
        if target_user.status != "active"
            || target_user.account_kind != users::ACCOUNT_KIND_REGISTERED
        {
            return Err(NuonoError::Validation(
                "已停用或非正式账号无法绑定新的平台身份".into(),
            ));
        }

        let now = crate::clock::db_now().timestamp_millis();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let mut query = identity_query(identity)?;
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let existing = query.one(&tx).await?;
        let mut merged_shadow_id: Option<i64> = None;
        let (identity_id, final_revision) = match existing {
            None => {
                // 分支 B：直接插入并归属到目标账号。
                let id = identities::Entity::insert(identities::ActiveModel {
                    platform: Set(platform.clone()),
                    namespace: Set(namespace.clone()),
                    identity_kind: Set(kind.clone()),
                    native_user_id: Set(native.clone()),
                    user_id: Set(target_user_id),
                    verified_at_ms: Set(Some(now)),
                    revision: Set(1),
                    created_at_ms: Set(now),
                    updated_at_ms: Set(now),
                    ..Default::default()
                })
                .exec(&tx)
                .await?
                .last_insert_id;
                (id, 1)
            }
            Some(row) => {
                let owner_id = resolve_canonical_user_id_on(&tx, row.user_id).await?;
                if owner_id == target_user_id && row.verified_at_ms.is_some() {
                    return Err(NuonoError::Conflict("该平台身份已绑定到此账号".into()));
                }
                if owner_id != target_user_id {
                    let owner = users::orm::Entity::find_by_id(owner_id)
                        .one(&tx)
                        .await?
                        .ok_or_else(|| NuonoError::Conflict("原归属账号不存在".into()))?;
                    if owner.account_kind == users::ACCOUNT_KIND_REGISTERED {
                        return Err(NuonoError::Conflict(
                            "该平台身份已绑定到其他账号，请先解除原绑定".into(),
                        ));
                    }
                    merge_shadow_into_user_on(&tx, owner.id, target_user_id, &operation_id, None)
                        .await?;
                    merged_shadow_id = Some(owner.id);
                }
                let updated = identities::Entity::find_by_id(row.id)
                    .one(&tx)
                    .await?
                    .ok_or_else(|| NuonoError::Internal("平台身份读取失败".into()))?;
                let next_rev = updated.revision + 1;
                identities::Entity::update_many()
                    .col_expr(identities::Column::UserId, Expr::value(target_user_id))
                    .col_expr(identities::Column::VerifiedAtMs, Expr::value(now))
                    .col_expr(identities::Column::UpdatedAtMs, Expr::value(now))
                    .col_expr(identities::Column::Revision, Expr::value(next_rev))
                    .filter(identities::Column::Id.eq(row.id))
                    .exec(&tx)
                    .await?;
                (row.id, next_rev)
            }
        };

        super::append_audit_on(
            &tx,
            super::AuditEvent {
                actor_user_id: Some(actor_user_id),
                actor_plugin_id: None,
                action: "identity.admin_bound",
                result: "success",
                subject_user_id: Some(target_user_id),
                group_id: None,
                operation_id: Some(&operation_id),
                details: serde_json::json!({
                    "identity_id": identity_id.to_string(),
                    "platform": platform,
                    "namespace": namespace,
                    "native_user_id": native,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        if let Some(shadow_id) = merged_shadow_id {
            self.invalidate_identity_cache_for_user(shadow_id);
        }
        self.invalidate_identity_cache_for_user(target_user_id);
        self.invalidate_identity_cache_for_ref(identity);

        Ok(PlatformIdentityView {
            id: identity_id.to_string(),
            platform,
            namespace,
            identity_kind: kind,
            native_user_id: native,
            revision: final_revision.to_string(),
        })
    }

    /// 由站点所有者解除指定账号的某个外部平台身份绑定（解绑后该平台身份回退至全新独立占位账号）。
    pub async fn admin_unbind_platform_identity(
        &self,
        actor_user_id: i64,
        target_user_id: i64,
        identity_id: i64,
        revision: i64,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        let mut user_query = users::orm::Entity::find_by_id(target_user_id);
        if self.backend() == DbBackend::Postgres {
            user_query = user_query.lock_exclusive();
        }
        let _target_user = user_query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("目标账号不存在".into()))?;
        let now = crate::clock::db_now().timestamp_millis();
        let changed = detach_identity_to_fresh_shadow_on(
            &tx,
            identity_id,
            target_user_id,
            Some(revision),
            now,
        )
        .await?;
        if changed != 1 {
            return Err(NuonoError::Conflict("绑定已经变化，请刷新后重试".into()));
        }
        super::account_state::cancel_proofs_on(&tx, target_user_id).await?;
        let operation_id = uuid::Uuid::new_v4().to_string();
        super::append_audit_on(
            &tx,
            super::AuditEvent {
                actor_user_id: Some(actor_user_id),
                actor_plugin_id: None,
                action: "identity.admin_unbound",
                result: "success",
                subject_user_id: Some(target_user_id),
                group_id: None,
                operation_id: Some(&operation_id),
                details: serde_json::json!({
                    "identity_id": identity_id.to_string(),
                }),
            },
        )
        .await?;
        tx.commit().await?;
        self.invalidate_identity_cache_for_user(target_user_id);
        Ok(())
    }

    /// 解除单个外部平台身份与账号的绑定（保留账号自身数据及其他已绑定的平台身份）。
    pub async fn unbind_platform_identity(
        &self,
        user_id: i64,
        identity_id: i64,
        revision: i64,
    ) -> Result<()> {
        self.unbind_identity_on_account(user_id, identity_id, revision, None)
            .await
    }
    pub async fn unbind_platform_identity_in_session(
        &self,
        user_id: i64,
        identity_id: i64,
        revision: i64,
        session: &str,
    ) -> Result<()> {
        self.unbind_identity_on_account(user_id, identity_id, revision, Some(session))
            .await
    }
    async fn unbind_identity_on_account(
        &self,
        user_id: i64,
        identity_id: i64,
        revision: i64,
        session: Option<&str>,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        let user = self.binding_account(&tx, user_id).await?;
        if let Some(session) = session {
            self.require_binding_session(&tx, &user, session).await?;
        }
        let now = crate::clock::db_now().timestamp_millis();
        let changed =
            detach_identity_to_fresh_shadow_on(&tx, identity_id, user_id, Some(revision), now)
                .await?;
        if changed != 1 {
            return Err(NuonoError::Conflict("绑定已经变化，请刷新后重试".into()));
        }
        super::account_state::cancel_proofs_on(&tx, user_id).await?;
        self.binding_audit(
            &tx,
            user_id,
            "identity.unbound",
            &uuid::Uuid::new_v4().to_string(),
            identity_id,
            now,
        )
        .await?;
        tx.commit().await?;
        self.invalidate_identity_cache_for_user(user_id);
        Ok(())
    }

    async fn binding_audit(
        &self,
        tx: &DatabaseTransaction,
        user_id: i64,
        action: &str,
        operation: &str,
        identity_id: i64,
        now: i64,
    ) -> Result<()> {
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),
            "INSERT INTO nuono_audit_events (occurred_at_ms, actor_user_id, action, result, subject_user_id, operation_id, details_json) VALUES ($1,$2,$3,'success',$2,$4,$5)",
            [now.into(), user_id.into(), action.into(), operation.into(),
                serde_json::json!({"identity_id": identity_id.to_string()}).to_string().into()])).await?;
        Ok(())
    }

    /// 清理已过期超过 24 小时的账号挑战票据与过期会话（保留历史审计事件）。
    pub async fn delete_stale_account_challenges(&self) -> Result<u64> {
        let cutoff = crate::clock::db_now().timestamp_millis() - CHALLENGE_RETENTION_MS;
        let tx = self.begin_write().await?;
        for column in ["source_proof_id", "target_proof_id"] {
            tx.execute_raw(Statement::from_sql_and_values(self.backend(),format!("UPDATE nuono_account_merge_jobs SET {column}=NULL WHERE {column} IN (SELECT id FROM nuono_account_challenges WHERE expires_at_ms<=$1)"),[cutoff.into()])).await?;
        }
        let deleted = challenges::Entity::delete_many()
            .filter(challenges::Column::ExpiresAtMs.lte(cutoff))
            .exec(&tx)
            .await?
            .rows_affected;
        // 先解除挑战记录对过期会话的外键引用并将未完成的挑战置为 cancelled，再删除过期会话行。
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),"UPDATE nuono_account_challenges SET session_id=NULL,state=CASE WHEN state IN ('pending','candidate') THEN 'cancelled' ELSE state END,revision=revision+1,updated_at_ms=$2 WHERE session_id IN (SELECT id FROM nuono_sessions WHERE expires_at_ms<=$1)",[cutoff.into(),crate::clock::db_now().timestamp_millis().into()])).await?;
        sessions::Entity::delete_many()
            .filter(sessions::Column::ExpiresAtMs.lte(cutoff))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// 根据平台适配器提供的完整平台用户身份，解析其当前已验证绑定的活跃正式账号 ID（未绑定或占位账号返回 `None`）。
    pub async fn account_for_platform_identity(
        &self,
        identity: &IdentityRef,
    ) -> Result<Option<i64>> {
        let Some(row) = identity_query(identity)?.one(self.orm()).await? else {
            return Ok(None);
        };
        if row.verified_at_ms.is_none() {
            return Ok(None);
        }
        let canonical = resolve_canonical_user_id_on(self.orm(), row.user_id).await?;
        Ok(users::orm::Entity::find_by_id(canonical)
            .filter(users::orm::Column::Status.eq("active"))
            .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
            .one(self.orm())
            .await?
            .map(|user| user.id))
    }
}

pub(super) fn qq_identity(qq: &str) -> Result<IdentityRef> {
    IdentityRef::new(
        nuono_domain::Platform::OneBot,
        IdentityKind::User,
        nuono_domain::IdentityScheme::Native,
        qq,
        None::<String>,
    )
    .map_err(|_| NuonoError::Validation("QQ 身份无效".into()))
}

pub(super) async fn identity_binding_on<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
    identity: &IdentityRef,
) -> Result<bool> {
    Ok(identity_query(identity)?
        .filter(identities::Column::UserId.eq(user_id))
        .filter(identities::Column::VerifiedAtMs.is_not_null())
        .one(conn)
        .await?
        .is_some())
}

pub(super) async fn qq_binding_on<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
    qq: &str,
) -> Result<bool> {
    identity_binding_on(conn, user_id, &qq_identity(qq)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewUser;
    use nuono_domain::{IdentityScheme, Platform};

    async fn account(db: &Database, username: &str) -> i64 {
        db.create_user(NewUser {
            username: username.into(),
            email: None,
            password: "binding-test-password".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap()
        .id
    }

    async fn issue(
        db: &Database,
        user_id: i64,
        bot: BotIdentity,
        session: &str,
        expected_sender: Option<IdentityRef>,
    ) -> Result<PlatformBindingIssued> {
        let user = db.find_user_by_id(user_id).await?.unwrap();
        let key = format!(
            "auth_session_{}",
            hex::encode(Sha256::digest(session.as_bytes()))
        );
        db.create_session_for_verified_user(
            &user,
            &key,
            (crate::clock::db_now() + chrono::Duration::hours(1)).into(),
        )
        .await?;
        db.begin_platform_binding(user_id, bot, session, expected_sender)
            .await
    }

    fn bot() -> BotIdentity {
        BotIdentity::new(
            "bot-main",
            IdentityRef::new(
                Platform::OneBot,
                IdentityKind::Bot,
                IdentityScheme::Native,
                "123456",
                None::<String>,
            )
            .unwrap(),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn candidate_requires_original_session_confirmation_and_unbind_keeps_account() {
        let db = crate::test_support::test_db(2).await;
        let user = account(&db, "binding_user").await;
        let issued = issue(&db, user, bot(), "original-session", None)
            .await
            .unwrap();
        let sender = qq_identity("234567").unwrap();
        let code = issued.command.strip_prefix("绑定 ").unwrap();
        assert!(
            db.observe_platform_binding(code, &bot(), &sender)
                .await
                .unwrap()
        );
        // 分支 B：观测到未入库的新平台身份时，不提前创建废弃占位账号。
        assert_eq!(users::orm::Entity::find().count(db.orm()).await.unwrap(), 1);
        assert_eq!(
            db.account_for_platform_identity(&sender).await.unwrap(),
            None
        );
        assert!(
            db.platform_binding(user, &issued.request.id, "different-session")
                .await
                .is_err()
        );
        let candidate = db
            .platform_binding(user, &issued.request.id, "original-session")
            .await
            .unwrap()
            .candidate
            .unwrap();
        db.confirm_platform_binding(
            user,
            &issued.request.id,
            "original-session",
            candidate.revision.parse().unwrap(),
        )
        .await
        .unwrap();
        // 分支 B 提交后仍只有 1 条正式用户记录，未产生多余占位账号。
        assert_eq!(users::orm::Entity::find().count(db.orm()).await.unwrap(), 1);
        assert_eq!(
            db.account_for_platform_identity(&sender).await.unwrap(),
            Some(user)
        );
        assert!(
            db.confirm_platform_binding(
                user,
                &issued.request.id,
                "original-session",
                candidate.revision.parse().unwrap()
            )
            .await
            .is_err()
        );
        let bound = db.platform_identities(user).await.unwrap().remove(0);
        db.unbind_platform_identity(
            user,
            bound.id.parse().unwrap(),
            bound.revision.parse().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            db.account_for_platform_identity(&sender).await.unwrap(),
            None
        );
        assert_eq!(
            db.find_user_by_id(user).await.unwrap().unwrap().status,
            "active"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn first_sender_wins_but_wrong_bot_does_not_claim_and_other_account_cannot_steal() {
        let db = crate::test_support::test_db(2).await;
        let alice = account(&db, "binding_alice").await;
        let bob = account(&db, "binding_bob").await;
        let issued = issue(&db, alice, bot(), "a-session", None).await.unwrap();
        let sender = qq_identity("234567").unwrap();
        let other = qq_identity("345678").unwrap();
        let mut wrong_bot = bot();
        wrong_bot.instance_id = nuono_domain::NativeId::new("other-bot").unwrap();
        let code = issued.command.strip_prefix("绑定 ").unwrap();
        assert!(
            !db.observe_platform_binding(code, &wrong_bot, &sender)
                .await
                .unwrap()
        );
        assert!(
            db.observe_platform_binding(code, &bot(), &sender)
                .await
                .unwrap()
        );
        assert!(
            !db.observe_platform_binding(code, &bot(), &other)
                .await
                .unwrap()
        );
        let candidate = db
            .platform_binding(alice, &issued.request.id, "a-session")
            .await
            .unwrap()
            .candidate
            .unwrap();
        db.confirm_platform_binding(
            alice,
            &issued.request.id,
            "a-session",
            candidate.revision.parse().unwrap(),
        )
        .await
        .unwrap();
        // 分支 D：该 QQ 已绑定正式账号 alice，bob 尝试绑定必须被拒绝。
        let steal = issue(&db, bob, bot(), "b-session", None).await.unwrap();
        assert!(
            db.observe_platform_binding(
                steal.command.strip_prefix("绑定 ").unwrap(),
                &bot(),
                &sender
            )
            .await
            .unwrap()
        );
        assert!(matches!(
            db.platform_binding(bob, &steal.request.id, "b-session")
                .await,
            Err(NuonoError::Conflict(_))
        ));
        let revision = identity_query(&sender)
            .unwrap()
            .one(db.orm())
            .await
            .unwrap()
            .unwrap()
            .revision;
        assert!(
            db.confirm_platform_binding(bob, &steal.request.id, "b-session", revision)
                .await
                .is_err()
        );
        assert_eq!(
            db.account_for_platform_identity(&sender).await.unwrap(),
            Some(alice)
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shadow_user_blocked_from_web_auth_and_branch_a_promotes_in_place() {
        let db = crate::test_support::test_db(3).await;
        let qq = qq_identity("555666").unwrap();
        // 1. 群内交互懒创建占位账号并产生对话与个人权益。
        let shadow = db
            .resolve_or_provision_identity(&qq, Some("群友小A"))
            .await
            .unwrap();
        let shadow_model = db.find_user_by_id(shadow.user_id).await.unwrap().unwrap();
        assert_eq!(shadow_model.account_kind, users::ACCOUNT_KIND_SHADOW);
        // Web 登录、重置密码、创建会话、TOTP/MFA 操作必须全部拒绝占位用户。
        assert!(
            db.verify_user_password("", "anything-123456")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !db.create_session_for_verified_user(
                &shadow_model,
                &format!("auth_session_{}", hex::encode([7_u8; 32])),
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap()
        );
        assert!(
            db.issue_password_reset_token(shadow.user_id, "nobody@example.com", 10)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.issue_user_token(shadow.user_id, crate::db::TOKEN_RESET_PASSWORD, 10, None)
                .await
                .is_err()
        );
        assert!(!db.consume_totp_step(shadow.user_id, 1).await.unwrap());
        assert!(
            !db.consume_recovery_code(shadow.user_id, "hash")
                .await
                .unwrap()
        );

        // 为占位账号写入对话与权益数据，验证原地转正后 user_id 保持不变且数据无缝可见。
        let now = crate::clock::db_now().timestamp_millis();
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "INSERT INTO nuono_conversations (id,platform,scope_type,owner_user_id,native_chat_id,native_thread_id,title,created_at_ms,updated_at_ms) \
                 VALUES ('conv-branch-a','onebot','private',$1,'555666','','私聊测试',$2,$2)",
                [shadow.user_id.into(), now.into()],
            ))
            .await
            .unwrap();
        db.upsert_entitlement(&nuono_app::entitlement::EntitlementUpsert {
            subject_type: nuono_app::entitlement::SUBJECT_USER.into(),
            subject_id: shadow.user_id,
            feature_key: "personal.ai_chat".into(),
            source: "trial".into(),
            source_ref: Some("trial-a".into()),
            starts_at_ms: now - 1000,
            expires_at_ms: Some(now + 86_400_000),
            limits_json: "{}".into(),
        })
        .await
        .unwrap();

        // 2. 分支 A：网站未登录发起平台验证注册并原地转正。
        let issued = db.begin_platform_registration(bot(), None).await.unwrap();
        let code = issued.command.strip_prefix("绑定 ").unwrap();
        assert!(
            db.observe_platform_binding(code, &bot(), &qq)
                .await
                .unwrap()
        );
        let status = db
            .platform_registration_status(&issued.request.id)
            .await
            .unwrap();
        assert_eq!(status.state, "candidate");
        let promoted = db
            .complete_platform_registration(
                &issued.request.id,
                "promoted_user",
                Some("promoted@example.com"),
                "Promoted-Password-123!",
            )
            .await
            .unwrap();
        // 关键验收：user_id 完全保持原占位账号 ID 不变！
        assert_eq!(promoted.id, shadow.user_id);
        assert_eq!(promoted.account_kind, users::ACCOUNT_KIND_REGISTERED);
        assert_eq!(promoted.username, "promoted_user");
        assert!(
            db.verify_user_password("promoted_user", "Promoted-Password-123!")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            db.account_for_platform_identity(&qq).await.unwrap(),
            Some(shadow.user_id)
        );
        // 原有对话与权益无缝归属同一 user_id。
        assert!(
            db.has_active_entitlement_at(
                nuono_app::entitlement::SUBJECT_USER,
                promoted.id,
                "personal.ai_chat",
                now,
            )
            .await
            .unwrap()
        );
        let conv_owner: i64 = db
            .orm()
            .query_one_raw(Statement::from_string(
                db.backend(),
                "SELECT owner_user_id FROM nuono_conversations WHERE id='conv-branch-a'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "owner_user_id")
            .unwrap();
        assert_eq!(conv_owner, promoted.id);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn branch_c_merges_shadow_into_registered_account_and_evicts_lru_cache() {
        let db = crate::test_support::test_db(3).await;
        // 1. 先有 QQ 在群内交互产生占位账号与数据（对话、权益、user:{id} KV）。
        let qq = qq_identity("777888").unwrap();
        let shadow = db
            .resolve_or_provision_identity(&qq, Some("群友小C"))
            .await
            .unwrap();
        let now = crate::clock::db_now().timestamp_millis();
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "INSERT INTO nuono_conversations (id,platform,scope_type,owner_user_id,native_chat_id,native_thread_id,title,created_at_ms,updated_at_ms) \
                 VALUES ('conv-branch-c','onebot','private',$1,'777888','','占位私聊',$2,$2)",
                [shadow.user_id.into(), now.into()],
            ))
            .await
            .unwrap();
        db.upsert_entitlement(&nuono_app::entitlement::EntitlementUpsert {
            subject_type: nuono_app::entitlement::SUBJECT_USER.into(),
            subject_id: shadow.user_id,
            feature_key: "personal.ai_chat".into(),
            source: "trial".into(),
            source_ref: Some("trial-c".into()),
            starts_at_ms: now - 1000,
            expires_at_ms: Some(now + 3_600_000),
            limits_json: "{}".into(),
        })
        .await
        .unwrap();
        // 同时给插件写入 user:{shadow} KV 检验迁移。
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "INSERT INTO nuono_plugins (plugin_id,created_at_ms,updated_at_ms) VALUES ('kv_probe',$1,$1)",
                [now.into()],
            ))
            .await
            .unwrap();
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "INSERT INTO nuono_plugin_kv (plugin_id,collection,scope_key,record_key,generation,value_json,value_version,revision,created_at_ms,updated_at_ms) \
                 VALUES ('kv_probe','prefs',$1,'theme','00000000-0000-0000-0000-000000000001','\"dark\"',1,1,$2,$2)",
                [format!("user:{}", shadow.user_id).into(), now.into()],
            ))
            .await
            .unwrap();

        // 2. 用户在网站独立注册并登录，再绑定已有占位数据的 QQ（触发分支 C 自动合并）。
        let target = account(&db, "web_registered_c").await;
        let issued = issue(&db, target, bot(), "branch-c-session", None)
            .await
            .unwrap();
        let code = issued.command.strip_prefix("绑定 ").unwrap();
        assert!(
            db.observe_platform_binding(code, &bot(), &qq)
                .await
                .unwrap()
        );
        let preview = db
            .platform_binding(target, &issued.request.id, "branch-c-session")
            .await
            .unwrap();
        let rev: i64 = preview.candidate.unwrap().revision.parse().unwrap();
        db.confirm_platform_binding(target, &issued.request.id, "branch-c-session", rev)
            .await
            .unwrap();

        // 3. 验证：源占位账号变为 merged 指向 target；目标 Web 会话保持有效；缓存失效后解析直接返回 target！
        let shadow_after = db.find_user_by_id(shadow.user_id).await.unwrap().unwrap();
        assert_eq!(shadow_after.status, "merged");
        assert!(
            db.authenticate_session("branch-c-session")
                .await
                .unwrap()
                .is_some(),
            "分支 C 合并占位账号不得踢下线已登录正式账号的当前会话"
        );
        let resolved_after = db.resolve_or_provision_identity(&qq, None).await.unwrap();
        assert_eq!(
            resolved_after.user_id, target,
            "提交后必须立即清除 LRU 缓存，再次解析必须指向目标正式账号"
        );
        assert_eq!(
            db.resolve_canonical_user_id(shadow.user_id).await.unwrap(),
            target
        );
        // 对话、权益与 user:{id} KV 均已完整迁移至目标账号。
        let conv_owner: i64 = db
            .orm()
            .query_one_raw(Statement::from_string(
                db.backend(),
                "SELECT owner_user_id FROM nuono_conversations WHERE id='conv-branch-c'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "owner_user_id")
            .unwrap();
        assert_eq!(conv_owner, target);
        assert!(
            db.has_active_entitlement_at(
                nuono_app::entitlement::SUBJECT_USER,
                target,
                "personal.ai_chat",
                now,
            )
            .await
            .unwrap()
        );
        let migrated_kv = db
            .orm()
            .query_one_raw(Statement::from_sql_and_values(
                db.backend(),
                "SELECT value_json FROM nuono_plugin_kv WHERE plugin_id='kv_probe' AND collection='prefs' AND scope_key=$1 AND record_key='theme'",
                [format!("user:{target}").into()],
            ))
            .await
            .unwrap();
        assert!(migrated_kv.is_some());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_opposite_merges_are_deadlock_free_and_single_winner() {
        let db = crate::test_support::test_db(4).await;
        let a = account(&db, "merge_race_a").await;
        let b = account(&db, "merge_race_b").await;
        let svc_a = db.account_merge_service();
        let svc_b = db.account_merge_service();
        let (res_ab, res_ba) = tokio::join!(svc_a.merge(a, b), svc_b.merge(b, a));
        let winners = [res_ab.is_ok(), res_ba.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(winners, 1, "并发 A<->B 合并应无死锁且仅有一方成功");
        let ua = db.find_user_by_id(a).await.unwrap().unwrap();
        let ub = db.find_user_by_id(b).await.unwrap().unwrap();
        assert!(
            (ua.status == "merged" && ub.status == "active")
                || (ub.status == "merged" && ua.status == "active")
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn password_change_invalidates_pending_proof_and_username_lookup_is_normalized() {
        let db = crate::test_support::test_db(2).await;
        let user = account(&db, "Mixed_Name").await;
        assert_eq!(
            db.find_user_by_username("mixed_name")
                .await
                .unwrap()
                .unwrap()
                .id,
            user
        );
        let issued = issue(&db, user, bot(), "session", None).await.unwrap();
        db.set_user_password(user, "replacement-password")
            .await
            .unwrap();
        assert!(
            db.platform_binding(user, &issued.request.id, "session")
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn logged_out_session_cannot_confirm_an_existing_candidate() {
        let db = crate::test_support::test_db(2).await;
        let user = account(&db, "logged_out_binding").await;
        let issued = issue(&db, user, bot(), "logout-session", None)
            .await
            .unwrap();
        let sender = qq_identity("234567").unwrap();
        db.observe_platform_binding(
            issued.command.strip_prefix("绑定 ").unwrap(),
            &bot(),
            &sender,
        )
        .await
        .unwrap();
        let candidate = db
            .platform_binding(user, &issued.request.id, "logout-session")
            .await
            .unwrap()
            .candidate
            .unwrap();
        db.revoke_session_token("logout-session").await.unwrap();
        assert!(
            db.confirm_platform_binding(
                user,
                &issued.request.id,
                "logout-session",
                candidate.revision.parse().unwrap()
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.account_for_platform_identity(&sender).await.unwrap(),
            None
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_removes_old_proofs_but_keeps_live_candidates_and_audit() {
        let db = crate::test_support::test_db(2).await;
        let user = account(&db, "cleanup_binding").await;
        let consumed = issue(&db, user, bot(), "consumed-session", None)
            .await
            .unwrap();
        let sender = qq_identity("234567").unwrap();
        db.observe_platform_binding(
            consumed.command.strip_prefix("绑定 ").unwrap(),
            &bot(),
            &sender,
        )
        .await
        .unwrap();
        let candidate = db
            .platform_binding(user, &consumed.request.id, "consumed-session")
            .await
            .unwrap()
            .candidate
            .unwrap();
        db.confirm_platform_binding(
            user,
            &consumed.request.id,
            "consumed-session",
            candidate.revision.parse().unwrap(),
        )
        .await
        .unwrap();
        let expired = issue(&db, user, bot(), "expired-session", None)
            .await
            .unwrap();
        let old = crate::clock::db_now().timestamp_millis() - 2 * CHALLENGE_RETENTION_MS;
        challenges::Entity::update_many()
            .col_expr(challenges::Column::CreatedAtMs, Expr::value(old))
            .col_expr(
                challenges::Column::ExpiresAtMs,
                Expr::value(old + BINDING_TTL_MS),
            )
            .filter(
                challenges::Column::Id
                    .is_in([consumed.request.id.clone(), expired.request.id.clone()]),
            )
            .exec(db.orm())
            .await
            .unwrap();
        let live = issue(&db, user, bot(), "live-session", None).await.unwrap();
        db.observe_platform_binding(
            live.command.strip_prefix("绑定 ").unwrap(),
            &bot(),
            &qq_identity("345678").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(db.delete_stale_account_challenges().await.unwrap(), 2);
        assert_eq!(
            db.platform_binding(user, &live.request.id, "live-session")
                .await
                .unwrap()
                .state,
            "candidate"
        );
        let audit = db
            .orm()
            .query_one_raw(Statement::from_string(
                db.backend(),
                "SELECT COUNT(*) AS count FROM nuono_audit_events WHERE action='identity.bound'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audit.try_get::<i64>("", "count").unwrap(), 1);
        db.shutdown().await.unwrap();
    }

    #[test]
    fn native_identity_scope_preserves_scheme_namespace_and_leading_zeroes() {
        let a = IdentityRef::new(
            Platform::Other("custom".into()),
            IdentityKind::User,
            IdentityScheme::Native,
            "00123",
            Some("tenant-a"),
        )
        .unwrap();
        let b = IdentityRef::new(
            Platform::Other("custom".into()),
            IdentityKind::User,
            IdentityScheme::Native,
            "00123",
            Some("tenant-b"),
        )
        .unwrap();
        assert_ne!(identity_parts(&a).unwrap(), identity_parts(&b).unwrap());
        assert_eq!(identity_parts(&a).unwrap().3, "00123");
    }
}
