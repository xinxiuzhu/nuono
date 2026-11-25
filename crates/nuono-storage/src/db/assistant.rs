use chrono::{DateTime, Utc};
use nuono_app::assistant::{AssistantGrant, AssistantSettings};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, QueryFilter, QuerySelect, Set,
};

use super::{Database, STATUS_ACTIVE};
use crate::{
    clock::db_now,
    entity::{account_assistant as accounts, users::orm as users},
    error::{NuonoError, Result},
};

const DAY_SECONDS: i64 = 86_400;

mod turns;
pub use turns::{AssistantTurnAdmission, AssistantTurnRequest};

pub struct AssistantAccount {
    pub available: bool,
    pub remaining: u32,
    pub qq: Option<String>,
    pub daily_limit: u32,
    pub grant_expires_at: Option<DateTime<Utc>>,
    pub quota_resets_at: DateTime<Utc>,
}

pub enum AssistantReservation {
    Accepted { day: i64 },
    Duplicate,
    Unavailable,
    Exhausted,
}

impl accounts::Model {
    fn limit(&self, settings: &AssistantSettings, now: impl Into<DateTime<Utc>>) -> u32 {
        let grant = self
            .grant_until
            .map(Into::into)
            .zip(self.grant_daily_limit.map(|limit| limit as u32));
        settings.effective_limit(grant, now.into())
    }

    fn requests(&self, day: i64) -> Result<Vec<String>> {
        if self.usage_day == day {
            Ok(serde_json::from_str(&self.request_ids)?)
        } else {
            Ok(Vec::new())
        }
    }

    pub(crate) fn context_epochs(&self) -> Result<std::collections::BTreeMap<String, i64>> {
        Ok(serde_json::from_str(&self.context_epochs_json)?)
    }

    pub(crate) fn context_epoch(&self, scope_key: &str) -> Result<i64> {
        Ok(self.context_epochs()?.get(scope_key).copied().unwrap_or(0))
    }
}

pub(crate) async fn companion_epoch_on<C: sea_orm::ConnectionTrait>(
    conn: &C,
    user_id: i64,
    scope_key: &str,
) -> Result<i64> {
    let Some(row) = accounts::Entity::find_by_id(user_id).one(conn).await? else {
        return Ok(0);
    };
    row.context_epoch(scope_key)
}

impl Database {
    // 串行化同一账号下的所有个人助手状态变更（包括首次插入 nuono_account_assistant 行）。
    async fn active_assistant_user(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        user_id: i64,
    ) -> Result<bool> {
        let mut query =
            users::Entity::find_by_id(user_id).filter(users::Column::Status.eq(STATUS_ACTIVE));
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        Ok(query.one(tx).await?.is_some())
    }

    pub async fn assistant_account(
        &self,
        user_id: i64,
        settings: &AssistantSettings,
    ) -> Result<AssistantAccount> {
        let now = db_now();
        let quota_resets_at = now
            .date_naive()
            .succ_opt()
            .and_then(|day| day.and_hms_opt(0, 0, 0))
            .map(|time| time.and_utc())
            .ok_or_else(|| NuonoError::Validation("日期超出范围".into()))?;
        let qq = self
            .platform_identities(user_id)
            .await?
            .into_iter()
            .find(|identity| {
                identity.platform == "onebot"
                    && identity.namespace.is_empty()
                    && identity.identity_kind == "\"native\""
            })
            .map(|identity| identity.native_user_id);
        let Some(row) = (if self.is_ai_module_installed().await? {
            accounts::Entity::find_by_id(user_id)
                .one(self.orm())
                .await?
        } else {
            None
        }) else {
            let limit = settings.effective_limit(None, now.into());
            return Ok(AssistantAccount {
                available: limit > 0,
                remaining: limit,
                qq,
                daily_limit: limit,
                grant_expires_at: None,
                quota_resets_at,
            });
        };
        let limit = row.limit(settings, now);
        let used = row.requests(now.timestamp().div_euclid(DAY_SECONDS))?.len() as u32;
        Ok(AssistantAccount {
            available: limit > 0,
            remaining: limit.saturating_sub(used),
            qq,
            daily_limit: limit,
            grant_expires_at: row.grant_until.map(Into::into),
            quota_resets_at,
        })
    }

    pub async fn reserve_assistant_message(
        &self,
        user_id: i64,
        settings: &AssistantSettings,
        request_id: &str,
        sender: Option<&str>,
    ) -> Result<AssistantReservation> {
        use AssistantReservation::*;
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        if !self.active_assistant_user(&tx, user_id).await? {
            return Ok(Unavailable);
        }
        let row = accounts::Entity::find_by_id(user_id).one(&tx).await?;
        if let Some(sender) = sender
            && !super::platform_identities::qq_binding_on(&tx, user_id, sender).await?
        {
            return Ok(Unavailable);
        }
        let now = db_now();
        let limit = row.as_ref().map_or_else(
            || settings.effective_limit(None, now.into()),
            |row| row.limit(settings, now),
        );
        if limit == 0 {
            return Ok(Unavailable);
        }
        let day = now.timestamp().div_euclid(DAY_SECONDS);
        let mut requests = match &row {
            Some(row) => row.requests(day)?,
            None => Vec::new(),
        };
        if requests.iter().any(|id| id == request_id) {
            return Ok(Duplicate);
        }
        if requests.len() >= limit as usize {
            return Ok(Exhausted);
        }
        requests.push(request_id.to_owned());
        let request_ids = Set(serde_json::to_string(&requests)?);
        if row.is_some() {
            accounts::ActiveModel {
                user_id: Set(user_id),
                usage_day: Set(day),
                request_ids,
                ..Default::default()
            }
            .update(&tx)
            .await?;
        } else {
            accounts::ActiveModel {
                user_id: Set(user_id),
                usage_day: Set(day),
                request_ids,
                ..Default::default()
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Accepted { day })
    }

    pub async fn release_assistant_message(
        &self,
        user_id: i64,
        day: i64,
        request_id: &str,
    ) -> Result<()> {
        if !self.is_ai_module_installed().await? {
            return Ok(());
        }
        let tx = self.begin_write().await?;
        if !self.active_assistant_user(&tx, user_id).await? {
            return Ok(());
        }
        let row = accounts::Entity::find_by_id(user_id)
            .filter(accounts::Column::UsageDay.eq(day))
            .one(&tx)
            .await?;
        if let Some(row) = row {
            let mut requests = row.requests(day)?;
            requests.retain(|id| id != request_id);
            accounts::ActiveModel {
                user_id: Set(user_id),
                request_ids: Set(serde_json::to_string(&requests)?),
                ..Default::default()
            }
            .update(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 读取指定账号已保存的个人助手额度授权（包括已过期的授权），不会因为查询而自动创建个人助手数据行。
    pub async fn assistant_grant(&self, user_id: i64) -> Result<Option<AssistantGrant>> {
        if self.find_user_by_id(user_id).await?.is_none() {
            return Err(NuonoError::NotFound("账号不存在".into()));
        }
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        Ok(accounts::Entity::find_by_id(user_id)
            .one(self.orm())
            .await?
            .and_then(|row| {
                row.grant_until
                    .zip(row.grant_daily_limit)
                    .map(|(expires_at, daily_limit)| AssistantGrant {
                        expires_at: expires_at.into(),
                        daily_limit: daily_limit as u32,
                    })
            }))
    }

    pub async fn set_assistant_grant(
        &self,
        user_id: i64,
        grant: Option<(DateTime<Utc>, u32)>,
    ) -> Result<()> {
        if grant.is_some() {
            self.ensure_ai_module().await?;
        } else if !self.is_ai_module_installed().await? {
            let tx = self.begin_write().await?;
            if !self.active_assistant_user(&tx, user_id).await? {
                return Err(NuonoError::NotFound("找不到可用的账号".into()));
            }
            tx.commit().await?;
            return Ok(());
        }
        let tx = self.begin_write().await?;
        set_grant_on(
            &tx,
            self.backend(),
            user_id,
            grant.map(|(expires_at, daily_limit)| AssistantGrant {
                expires_at,
                daily_limit,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn assistant_account_for_qq(&self, qq: &str) -> Result<Option<i64>> {
        self.account_for_platform_identity(&super::platform_identities::qq_identity(qq)?)
            .await
    }

    pub async fn unlink_assistant_qq(&self, user_id: i64) -> Result<()> {
        if let Some(identity) =
            self.platform_identities(user_id)
                .await?
                .into_iter()
                .find(|identity| {
                    identity.platform == "onebot"
                        && identity.namespace.is_empty()
                        && identity.identity_kind == "\"native\""
                })
        {
            let id = identity
                .id
                .parse()
                .map_err(|_| NuonoError::Internal("身份编号无效".into()))?;
            let revision = identity
                .revision
                .parse()
                .map_err(|_| NuonoError::Internal("身份版本无效".into()))?;
            self.unbind_platform_identity(user_id, id, revision).await?;
        }
        Ok(())
    }
}

// 所有额度授权变更与对话消息准入均须先锁定对应的 users 行（包括首次创建个人助手记录时）。
pub(crate) async fn grant_on(
    tx: &sea_orm::DatabaseTransaction,
    backend: DbBackend,
    user_id: i64,
) -> Result<Option<AssistantGrant>> {
    let mut query =
        users::Entity::find_by_id(user_id).filter(users::Column::Status.eq(STATUS_ACTIVE));
    if backend == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    if query.one(tx).await?.is_none() {
        return Err(NuonoError::NotFound("找不到可用的账号".into()));
    }
    Ok(accounts::Entity::find_by_id(user_id)
        .one(tx)
        .await?
        .and_then(|row| {
            row.grant_until
                .zip(row.grant_daily_limit)
                .map(|(expires_at, daily_limit)| AssistantGrant {
                    expires_at: expires_at.into(),
                    daily_limit: daily_limit as u32,
                })
        }))
}

pub(crate) async fn set_grant_on(
    tx: &sea_orm::DatabaseTransaction,
    backend: DbBackend,
    user_id: i64,
    grant: Option<AssistantGrant>,
) -> Result<()> {
    if let Some(grant) = &grant {
        grant.validate()?;
    }
    grant_on(tx, backend, user_id).await?;
    let update = accounts::ActiveModel {
        user_id: Set(user_id),
        grant_until: Set(crate::clock::store_time(
            grant.as_ref().map(|grant| grant.expires_at),
        )),
        grant_daily_limit: Set(grant.as_ref().map(|grant| grant.daily_limit as i32)),
        ..Default::default()
    };
    if accounts::Entity::find_by_id(user_id)
        .one(tx)
        .await?
        .is_some()
    {
        update.update(tx).await?;
    } else if grant.is_some() {
        update.insert(tx).await?;
    }
    Ok(())
}
