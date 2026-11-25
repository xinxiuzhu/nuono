//! 开源统一功能权益仓储：管理 `nuono_entitlements` 表的写入、撤销与运行时准入判定。
//!
//! 不同来源（`billing`、`manual`、`trial`）按 `(subject_type, subject_id, feature_key, source)`
//! 独立存储、互不覆盖；运行时只要任意来源处于有效期内即判定为已授权。

use nuono_app::entitlement::{
    EntitlementRecord, EntitlementUpsert, FEATURE_GROUP_BOT_ACCESS, FEATURE_USER_PRIVATE_BOT,
    FEATURE_USER_WEB_AI, SOURCE_BILLING, SOURCE_MANUAL, SOURCE_TRIAL, STATUS_ACTIVE,
    STATUS_REVOKED, SUBJECT_GROUP, SUBJECT_USER,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Statement};

use crate::{
    Database,
    entity::entitlements,
    error::{NuonoError, Result},
};

fn validate_subject_type(subject_type: &str) -> Result<()> {
    if matches!(subject_type, SUBJECT_GROUP | SUBJECT_USER) {
        Ok(())
    } else {
        Err(NuonoError::Validation(
            "权益主体类型仅支持 group 或 user".into(),
        ))
    }
}

fn validate_source(source: &str) -> Result<()> {
    if matches!(source, SOURCE_BILLING | SOURCE_MANUAL | SOURCE_TRIAL) {
        Ok(())
    } else {
        Err(NuonoError::Validation(
            "权益来源仅支持 billing、manual 或 trial".into(),
        ))
    }
}

fn validate_feature_key(feature_key: &str) -> Result<()> {
    if feature_key.is_empty() || feature_key.len() > 128 || feature_key.contains('\0') {
        return Err(NuonoError::Validation(
            "功能权益键长度须为 1 至 128 字节且不能包含空字符".into(),
        ));
    }
    Ok(())
}

fn validate_upsert(input: &EntitlementUpsert) -> Result<()> {
    validate_subject_type(&input.subject_type)?;
    if input.subject_id <= 0 {
        return Err(NuonoError::Validation("权益主体 ID 须为正整数".into()));
    }
    validate_feature_key(&input.feature_key)?;
    validate_source(&input.source)?;
    if let Some(source_ref) = &input.source_ref
        && (source_ref.is_empty() || source_ref.len() > 256 || source_ref.contains('\0'))
    {
        return Err(NuonoError::Validation(
            "权益来源关联 ID 长度须为 1 至 256 字节且不能包含空字符".into(),
        ));
    }
    if input
        .expires_at_ms
        .is_some_and(|expires| expires <= input.starts_at_ms)
    {
        return Err(NuonoError::Validation(
            "权益到期时间必须晚于生效时间".into(),
        ));
    }
    if input.limits_json.contains('\0') {
        return Err(NuonoError::Validation("权益配额参数不能包含空字符".into()));
    }
    let parsed = nuono_domain::scalar::parse_json(&input.limits_json)?;
    if !parsed.is_object() {
        return Err(NuonoError::Validation(
            "权益配额参数必须是 JSON 对象".into(),
        ));
    }
    Ok(())
}

fn into_record(model: entitlements::Model) -> EntitlementRecord {
    EntitlementRecord {
        id: model.id,
        subject_type: model.subject_type,
        subject_id: model.subject_id,
        feature_key: model.feature_key,
        source: model.source,
        source_ref: model.source_ref,
        status: model.status,
        starts_at_ms: model.starts_at_ms,
        expires_at_ms: model.expires_at_ms,
        limits_json: model.limits_json,
        updated_at_ms: model.updated_at_ms,
    }
}

/// 在指定连接或事务上幂等写入或刷新一条功能权益。
pub async fn upsert_entitlement_on<C: ConnectionTrait>(
    conn: &C,
    input: &EntitlementUpsert,
) -> Result<EntitlementRecord> {
    validate_upsert(input)?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let backend = conn.get_database_backend();
    conn.execute_raw(Statement::from_sql_and_values(
        backend,
        "INSERT INTO nuono_entitlements \
         (subject_type, subject_id, feature_key, source, source_ref, status, starts_at_ms, expires_at_ms, limits_json, updated_at_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (subject_type, subject_id, feature_key, source) DO UPDATE SET \
         source_ref = excluded.source_ref, \
         status = excluded.status, \
         starts_at_ms = excluded.starts_at_ms, \
         expires_at_ms = excluded.expires_at_ms, \
         limits_json = excluded.limits_json, \
         updated_at_ms = excluded.updated_at_ms",
        [
            input.subject_type.clone().into(),
            input.subject_id.into(),
            input.feature_key.clone().into(),
            input.source.clone().into(),
            input.source_ref.clone().into(),
            STATUS_ACTIVE.into(),
            input.starts_at_ms.into(),
            input.expires_at_ms.into(),
            input.limits_json.clone().into(),
            now_ms.into(),
        ],
    ))
    .await?;
    let row = entitlements::Entity::find()
        .filter(entitlements::Column::SubjectType.eq(&input.subject_type))
        .filter(entitlements::Column::SubjectId.eq(input.subject_id))
        .filter(entitlements::Column::FeatureKey.eq(&input.feature_key))
        .filter(entitlements::Column::Source.eq(&input.source))
        .one(conn)
        .await?
        .ok_or_else(|| NuonoError::DatabaseQuery("写入功能权益后未找到对应记录".into()))?;
    Ok(into_record(row))
}

/// 在指定连接或事务上撤销指定来源的功能权益，不影响同一主体的其他来源。
pub async fn revoke_entitlement_on<C: ConnectionTrait>(
    conn: &C,
    subject_type: &str,
    subject_id: i64,
    feature_key: &str,
    source: &str,
) -> Result<bool> {
    validate_subject_type(subject_type)?;
    if subject_id <= 0 {
        return Err(NuonoError::Validation("权益主体 ID 须为正整数".into()));
    }
    validate_feature_key(feature_key)?;
    validate_source(source)?;
    let now_ms = crate::clock::db_now().timestamp_millis();
    let result = conn
        .execute_raw(Statement::from_sql_and_values(
            conn.get_database_backend(),
            "UPDATE nuono_entitlements \
             SET status = $1, updated_at_ms = $2 \
             WHERE subject_type = $3 AND subject_id = $4 AND feature_key = $5 AND source = $6 \
               AND status <> $1",
            [
                STATUS_REVOKED.into(),
                now_ms.into(),
                subject_type.into(),
                subject_id.into(),
                feature_key.into(),
                source.into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 在指定连接或事务上判断主体在 `now_ms` 时刻是否具备有效功能权益。
pub async fn has_active_entitlement_at<C: ConnectionTrait>(
    conn: &C,
    subject_type: &str,
    subject_id: i64,
    feature_key: &str,
    now_ms: i64,
) -> Result<bool> {
    validate_subject_type(subject_type)?;
    if subject_id <= 0 {
        return Ok(false);
    }
    validate_feature_key(feature_key)?;
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            conn.get_database_backend(),
            "SELECT id FROM nuono_entitlements \
             WHERE subject_type = $1 AND subject_id = $2 AND feature_key = $3 \
               AND status = $4 AND starts_at_ms <= $5 \
               AND (expires_at_ms IS NULL OR expires_at_ms > $5) \
             LIMIT 1",
            [
                subject_type.into(),
                subject_id.into(),
                feature_key.into(),
                STATUS_ACTIVE.into(),
                now_ms.into(),
            ],
        ))
        .await?;
    Ok(row.is_some())
}

/// 列出指定主体名下的全部功能权益记录（含已撤销与已过期记录，便于管理界面排查）。
pub async fn list_subject_entitlements_on<C: ConnectionTrait>(
    conn: &C,
    subject_type: &str,
    subject_id: i64,
) -> Result<Vec<EntitlementRecord>> {
    validate_subject_type(subject_type)?;
    if subject_id <= 0 {
        return Ok(Vec::new());
    }
    let rows = entitlements::Entity::find()
        .filter(entitlements::Column::SubjectType.eq(subject_type))
        .filter(entitlements::Column::SubjectId.eq(subject_id))
        .order_by_asc(entitlements::Column::FeatureKey)
        .order_by_asc(entitlements::Column::Source)
        .all(conn)
        .await?;
    Ok(rows.into_iter().map(into_record).collect())
}

impl Database {
    /// 幂等写入或刷新一条功能权益记录。
    pub async fn upsert_entitlement(&self, input: &EntitlementUpsert) -> Result<EntitlementRecord> {
        let tx = self.begin_write().await?;
        let record = upsert_entitlement_on(&tx, input).await?;
        tx.commit().await?;
        Ok(record)
    }

    /// 撤销指定主体、功能与来源的权益记录。
    pub async fn revoke_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
        source: &str,
    ) -> Result<bool> {
        let tx = self.begin_write().await?;
        let changed =
            revoke_entitlement_on(&tx, subject_type, subject_id, feature_key, source).await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// 判断主体在当前时刻是否拥有指定功能权益。
    pub async fn has_active_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
    ) -> Result<bool> {
        self.has_active_entitlement_at(
            subject_type,
            subject_id,
            feature_key,
            crate::clock::db_now().timestamp_millis(),
        )
        .await
    }

    /// 判断主体在指定毫秒时刻是否拥有指定功能权益。
    pub async fn has_active_entitlement_at(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
        now_ms: i64,
    ) -> Result<bool> {
        has_active_entitlement_at(self.orm(), subject_type, subject_id, feature_key, now_ms).await
    }

    /// 快捷判定：群是否拥有机器人准入权益（`group:bot_access`）。
    pub async fn has_group_bot_access(&self, group_id: i64) -> Result<bool> {
        self.has_active_entitlement(SUBJECT_GROUP, group_id, FEATURE_GROUP_BOT_ACCESS)
            .await
    }

    /// 快捷判定：用户是否拥有网页端 AI 聊天权益（`user:web_ai`）。
    pub async fn has_user_web_ai(&self, user_id: i64) -> Result<bool> {
        self.has_active_entitlement(SUBJECT_USER, user_id, FEATURE_USER_WEB_AI)
            .await
    }

    /// 快捷判定：用户是否拥有 IM 私聊机器人权益（`user:private_bot`）。
    pub async fn has_user_private_bot(&self, user_id: i64) -> Result<bool> {
        self.has_active_entitlement(SUBJECT_USER, user_id, FEATURE_USER_PRIVATE_BOT)
            .await
    }

    /// 列出主体名下的全部功能权益记录。
    pub async fn list_subject_entitlements(
        &self,
        subject_type: &str,
        subject_id: i64,
    ) -> Result<Vec<EntitlementRecord>> {
        list_subject_entitlements_on(self.orm(), subject_type, subject_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manual_and_billing_sources_coexist_without_overwriting() {
        let db = crate::test_support::test_db(2).await;
        let now = 1_760_000_000_000_i64;

        let manual = db
            .upsert_entitlement(&EntitlementUpsert {
                subject_type: SUBJECT_GROUP.into(),
                subject_id: 42,
                feature_key: FEATURE_GROUP_BOT_ACCESS.into(),
                source: SOURCE_MANUAL.into(),
                source_ref: Some("staff:1".into()),
                starts_at_ms: now,
                expires_at_ms: None,
                limits_json: "{}".into(),
            })
            .await
            .unwrap();
        assert_eq!(manual.status, STATUS_ACTIVE);
        assert_eq!(manual.expires_at_ms, None);

        let billing = db
            .upsert_entitlement(&EntitlementUpsert {
                subject_type: SUBJECT_GROUP.into(),
                subject_id: 42,
                feature_key: FEATURE_GROUP_BOT_ACCESS.into(),
                source: SOURCE_BILLING.into(),
                source_ref: Some("sub-001".into()),
                starts_at_ms: now,
                expires_at_ms: Some(now + 60_000),
                limits_json: r#"{"max_members":200}"#.into(),
            })
            .await
            .unwrap();
        assert_eq!(billing.source, SOURCE_BILLING);

        let all = db
            .list_subject_entitlements(SUBJECT_GROUP, 42)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // 撤销计费来源后，永久人工来源仍然有效。
        assert!(
            db.revoke_entitlement(SUBJECT_GROUP, 42, FEATURE_GROUP_BOT_ACCESS, SOURCE_BILLING)
                .await
                .unwrap()
        );
        assert!(
            db.has_active_entitlement_at(SUBJECT_GROUP, 42, FEATURE_GROUP_BOT_ACCESS, now + 90_000)
                .await
                .unwrap()
        );

        // 撤销人工来源后，不再具备准入权益。
        assert!(
            db.revoke_entitlement(SUBJECT_GROUP, 42, FEATURE_GROUP_BOT_ACCESS, SOURCE_MANUAL)
                .await
                .unwrap()
        );
        assert!(
            !db.has_active_entitlement_at(SUBJECT_GROUP, 42, FEATURE_GROUP_BOT_ACCESS, now + 10)
                .await
                .unwrap()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn time_window_and_validation_boundaries_are_enforced() {
        let db = crate::test_support::test_db(2).await;
        let start = 1_760_000_000_000_i64;
        let end = start + 10_000;

        db.upsert_entitlement(&EntitlementUpsert {
            subject_type: SUBJECT_USER.into(),
            subject_id: 7,
            feature_key: FEATURE_USER_WEB_AI.into(),
            source: SOURCE_TRIAL.into(),
            source_ref: None,
            starts_at_ms: start,
            expires_at_ms: Some(end),
            limits_json: r#"{"daily_limit":20}"#.into(),
        })
        .await
        .unwrap();

        assert!(
            !db.has_active_entitlement_at(SUBJECT_USER, 7, FEATURE_USER_WEB_AI, start - 1)
                .await
                .unwrap()
        );
        assert!(
            db.has_active_entitlement_at(SUBJECT_USER, 7, FEATURE_USER_WEB_AI, start)
                .await
                .unwrap()
        );
        assert!(
            !db.has_active_entitlement_at(SUBJECT_USER, 7, FEATURE_USER_WEB_AI, end)
                .await
                .unwrap()
        );

        // 到期时间不晚于生效时间应被拒绝。
        assert!(
            db.upsert_entitlement(&EntitlementUpsert {
                subject_type: SUBJECT_USER.into(),
                subject_id: 7,
                feature_key: FEATURE_USER_PRIVATE_BOT.into(),
                source: SOURCE_BILLING.into(),
                source_ref: None,
                starts_at_ms: start,
                expires_at_ms: Some(start),
                limits_json: "{}".into(),
            })
            .await
            .is_err()
        );

        // 非 JSON 对象的 limits_json 应被拒绝。
        assert!(
            db.upsert_entitlement(&EntitlementUpsert {
                subject_type: SUBJECT_USER.into(),
                subject_id: 7,
                feature_key: FEATURE_USER_PRIVATE_BOT.into(),
                source: SOURCE_BILLING.into(),
                source_ref: None,
                starts_at_ms: start,
                expires_at_ms: None,
                limits_json: "[]".into(),
            })
            .await
            .is_err()
        );
        db.shutdown().await.unwrap();
    }
}
