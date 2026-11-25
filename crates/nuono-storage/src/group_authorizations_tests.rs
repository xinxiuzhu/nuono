//! 群组授权与插件代签发测试：使用通用计费测试桩插件模拟私有支付集成如何通过公开的插件事务接口签发群组权限。
use super::*;
use nuono_app::plugin_data::{PluginDataStore, PluginDataTransaction};
use serde_json::json;

async fn user(db: &Database, name: &str, role: &str) -> i64 {
    db.create_user(crate::db::NewUser {
        username: name.into(),
        email: None,
        password: "Fixture-password-123!".into(),
        role: role.into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap()
    .id
}
async fn fixture() -> (Database, i64, i64, i64, i64) {
    let db = crate::test_support::test_db(5).await;
    let owner = user(&db, "fixture-owner", "owner").await;
    let red = user(&db, "fixture-red", "user").await;
    let blue = user(&db, "fixture-blue", "user").await;
    let group = db
        .ensure_group(
            &crate::groups::GroupIdentity {
                platform: "onebot".into(),
                namespace: "".into(),
                conversation_kind: "group:native".into(),
                native_parent_id: "".into(),
                native_group_id: "123".into(),
            },
            "Fixture",
        )
        .await
        .unwrap()
        .id;
    for plugin in ["billing_fixture", "example", "other_issuer"] {
        db.sync_plugin(plugin, "1", &json!({})).await.unwrap();
    }
    db.set_plugin_grant_delegation(
        owner,
        "billing_fixture",
        &PluginGrantDelegation {
            all_plugins: false,
            plugin_ids: vec!["example".into()],
        },
    )
    .await
    .unwrap();
    // 这些表属于计费测试桩插件私有，框架核心本身不包含任何商业订单或价格表。
    db.orm().execute_unprepared("CREATE TABLE billing_fixture_period (group_id BIGINT PRIMARY KEY, period_id TEXT NOT NULL, expires_at_ms BIGINT NOT NULL); CREATE TABLE billing_fixture_order (order_id TEXT PRIMARY KEY, user_id BIGINT NOT NULL, group_id BIGINT NOT NULL, duration_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL);").await.unwrap();
    db.orm().execute_raw(Statement::from_sql_and_values(db.backend(),"INSERT INTO billing_fixture_period(group_id,period_id,expires_at_ms) VALUES($1,'initial',0)",[group.into()])).await.unwrap();
    (db, owner, red, blue, group)
}
async fn rows(
    tx: &dyn PluginDataTransaction,
    sql: &str,
    values: Vec<sea_orm::Value>,
) -> Result<Vec<sea_orm::QueryResult>> {
    let backend = if tx.backend() == nuono_app::plugin_data::PluginDatabaseBackend::Postgres {
        DbBackend::Postgres
    } else {
        DbBackend::Sqlite
    };
    Ok(tx
        .orm_query(Statement::from_sql_and_values(backend, sql, values))
        .await?)
}
async fn purchase(
    db: &Database,
    order: &str,
    user_id: i64,
    group: i64,
    duration: i64,
    now: i64,
    fail_after_grant: bool,
) -> Result<i64> {
    let tx = PluginDataStore::begin_for_plugin(db, "billing_fixture").await?;
    // 先查出群内现有受益人并按统一顺序锁定相关账号行，再加锁串行化当前群组的计费周期行。
    let before = tx.group_grants(group).await?;
    let mut ids = before.iter().map(|g| g.user_id).collect::<Vec<_>>();
    ids.push(user_id);
    tx.lock_accounts(&ids).await?;
    let lock = if db.backend() == DbBackend::Postgres {
        " FOR UPDATE"
    } else {
        ""
    };
    let period = rows(
        tx.as_ref(),
        &format!(
            "SELECT period_id,expires_at_ms FROM billing_fixture_period WHERE group_id=$1{lock}"
        ),
        vec![group.into()],
    )
    .await?
    .pop()
    .unwrap();
    let grants = tx.group_grants(group).await?;
    if grants.iter().any(|g| !ids.contains(&g.user_id)) {
        return Err(NuonoError::Conflict(
            "新增受益人，重读并按顺序锁账号后重试".into(),
        ));
    }
    if let Some(old) = rows(
        tx.as_ref(),
        "SELECT * FROM billing_fixture_order WHERE order_id=$1",
        vec![order.into()],
    )
    .await?
    .pop()
    {
        if old.try_get::<i64>("", "user_id")? != user_id
            || old.try_get::<i64>("", "group_id")? != group
            || old.try_get::<i64>("", "duration_ms")? != duration
        {
            return Err(NuonoError::Conflict("订单内容不一致".into()));
        }
        let end = old.try_get("", "expires_at_ms")?;
        tx.commit().await?;
        return Ok(end);
    }
    let old_end: i64 = period.try_get("", "expires_at_ms")?;
    let old_period: String = period.try_get("", "period_id")?;
    let fresh = old_end <= now;
    let period_id = if fresh {
        uuid::Uuid::new_v4().to_string()
    } else {
        old_period
    };
    let end = old_end
        .max(now)
        .checked_add(duration)
        .ok_or_else(|| NuonoError::Validation("期限越界".into()))?;
    let live = grants
        .iter()
        .filter(|g| {
            g.revoked_at_ms.is_none()
                && g.source_key.starts_with(&format!("{period_id}:"))
                && g.expires_at_ms.is_some_and(|expires| expires > now)
        })
        .collect::<Vec<_>>();
    for grant in &live {
        tx.renew_group_grant(grant.id, grant.revision, Some(end))
            .await?;
    }
    if !live.iter().any(|g| g.user_id == user_id) {
        tx.issue_group_grant(&ManualGroupGrant {
            user_id,
            group_id: group,
            plugin_id: Some("example".into()),
            source_key: format!("{period_id}:{order}"),
            valid_from_ms: now,
            expires_at_ms: Some(end),
        })
        .await?;
    }
    if fail_after_grant {
        return Err(NuonoError::Internal("fixture injected failure".into()));
    }
    tx.orm_execute(Statement::from_sql_and_values(
        db.backend(),
        "UPDATE billing_fixture_period SET period_id=$1,expires_at_ms=$2 WHERE group_id=$3",
        [period_id.into(), end.into(), group.into()],
    ))
    .await?;
    tx.orm_execute(Statement::from_sql_and_values(db.backend(),"INSERT INTO billing_fixture_order(order_id,user_id,group_id,duration_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5)",[order.into(),user_id.into(),group.into(),duration.into(),end.into()])).await?;
    tx.commit().await?;
    Ok(end)
}
#[tokio::test]
async fn billing_periods_share_extension_but_never_revive_other_expired_or_revoked_members() {
    let (db, owner, red, blue, group) = fixture().await;
    let now = crate::clock::db_now().timestamp_millis();
    let end = purchase(&db, "red-1", red, group, 60_000, now, false)
        .await
        .unwrap();
    let end2 = purchase(&db, "blue-1", blue, group, 60_000, now + 1, false)
        .await
        .unwrap();
    assert_eq!(end2, end + 60_000);
    assert_eq!(
        purchase(&db, "blue-1", blue, group, 60_000, now + 2, false)
            .await
            .unwrap(),
        end2
    );
    let grants = db.group_configuration_grants(group).await.unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().all(|g| g.expires_at_ms == Some(end2)));
    let red_grant = grants.iter().find(|g| g.user_id == red).unwrap().id;
    db.revoke_group_configuration(owner, group, red_grant)
        .await
        .unwrap();
    purchase(&db, "blue-2", blue, group, 60_000, now + 3, false)
        .await
        .unwrap();
    assert!(
        db.group_configuration_grants(group)
            .await
            .unwrap()
            .iter()
            .find(|g| g.id == red_grant)
            .unwrap()
            .revoked_at_ms
            .is_some()
    );
    let boundary = end2 + 60_000;
    purchase(&db, "red-new-period", red, group, 60_000, boundary, false)
        .await
        .unwrap();
    let grants = db.group_configuration_grants(group).await.unwrap();
    assert_eq!(
        grants
            .iter()
            .filter(|g| g.revoked_at_ms.is_none() && g.expires_at_ms.is_some_and(|t| t > boundary))
            .map(|g| g.user_id)
            .collect::<Vec<_>>(),
        vec![red]
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn business_failure_rolls_back_grant_and_audit_and_issuer_cannot_cross_sources() {
    let (db, owner, red, _, group) = fixture().await;
    let now = crate::clock::db_now().timestamp_millis();
    assert!(
        purchase(&db, "fail", red, group, 60_000, now, true)
            .await
            .is_err()
    );
    assert!(
        db.group_configuration_grants(group)
            .await
            .unwrap()
            .is_empty()
    );
    let request = ManualGroupGrant {
        user_id: red,
        group_id: group,
        plugin_id: Some("example".into()),
        source_key: "manual".into(),
        valid_from_ms: now,
        expires_at_ms: None,
    };
    let manual = db.grant_group_configuration(owner, &request).await.unwrap();
    let tx = PluginDataStore::begin_for_plugin(&db, "billing_fixture")
        .await
        .unwrap();
    assert!(
        tx.revoke_group_grant(manual.id, manual.revision)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    db.set_plugin_grant_delegation(owner, "billing_fixture", &PluginGrantDelegation::default())
        .await
        .unwrap();
    let tx = PluginDataStore::begin_for_plugin(&db, "billing_fixture")
        .await
        .unwrap();
    assert!(tx.issue_group_grant(&request).await.is_err());
    tx.rollback().await.unwrap();
    assert!(
        db.has_group_configuration_grant(red, group, Some("example"))
            .await
            .unwrap()
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn duplicate_callbacks_serialize_and_changed_order_payload_is_rejected() {
    let (db, _, red, _, group) = fixture().await;
    let now = crate::clock::db_now().timestamp_millis();
    let (a, b) = tokio::join!(
        purchase(&db, "same", red, group, 60_000, now, false),
        purchase(&db, "same", red, group, 60_000, now, false)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(db.group_configuration_grants(group).await.unwrap().len(), 1);
    assert!(
        purchase(&db, "same", red, group, 120_000, now, false)
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
