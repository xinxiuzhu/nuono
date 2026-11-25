//! 事件订阅的持久化查询与管理投影。

use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use crate::db::Database;
use crate::entity::subscriptions as orm;
use crate::error::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Subscription {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub subscriber_id: String,
    pub topic: String,
    pub target_id: Option<String>,
    pub enabled: bool,
    pub created_at: Option<String>,
}

impl From<orm::Model> for Subscription {
    fn from(m: orm::Model) -> Self {
        Subscription {
            id: m.id,
            subscriber_id: m.subscriber_id,
            topic: m.topic,
            target_id: m.target_id,
            enabled: m.enabled.unwrap_or(true),
            created_at: m.created_at.map(crate::clock::format_wire_ts),
        }
    }
}

pub async fn list(db: &Database) -> Result<Vec<Subscription>> {
    let rows = orm::Entity::find().all(db.orm()).await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn create(
    db: &Database,
    subscriber_id: &str,
    topic: &str,
    target_id: Option<&str>,
) -> Result<Subscription> {
    let now = crate::clock::db_now();
    let am = orm::ActiveModel {
        subscriber_id: Set(subscriber_id.to_string()),
        topic: Set(topic.to_string()),
        target_id: Set(target_id.map(str::to_string)),
        enabled: Set(Some(true)),
        created_at: Set(crate::clock::store_time(Some(now))),
        ..Default::default()
    };

    let inserted = orm::Entity::insert(am).exec(db.orm()).await?;
    let row = orm::Entity::find_by_id(inserted.last_insert_id)
        .one(db.orm())
        .await?
        .ok_or_else(|| {
            crate::error::NuonoError::DatabaseQuery(format!(
                "订阅 {} 插入后回读失败",
                inserted.last_insert_id
            ))
        })?;

    Ok(row.into())
}

pub async fn toggle(db: &Database, id: i64, enabled: bool) -> Result<()> {
    let result = orm::Entity::update_many()
        .set(orm::ActiveModel {
            enabled: Set(Some(enabled)),
            ..Default::default()
        })
        .filter(orm::Column::Id.eq(id))
        .exec(db.orm())
        .await?;

    if result.rows_affected == 0 {
        return Err(crate::error::NuonoError::NotFound(format!(
            "订阅 {} 不存在",
            id
        )));
    }
    Ok(())
}

pub async fn remove(db: &Database, id: i64) -> Result<()> {
    orm::Entity::delete_by_id(id).exec(db.orm()).await?;
    Ok(())
}

pub async fn push(
    db: &Database,
    topic: &str,
    target_id: Option<&str>,
) -> Result<Vec<Subscription>> {
    let mut select = orm::Entity::find()
        .filter(orm::Column::Enabled.eq(true))
        .filter(orm::Column::Topic.eq(topic));
    if let Some(tid) = target_id {
        select = select.filter(orm::Column::TargetId.eq(tid));
    }
    let rows = select.all(db.orm()).await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn ids(rows: Vec<Subscription>) -> BTreeSet<i64> {
        rows.into_iter().map(|row| row.id).collect()
    }

    #[tokio::test]
    async fn push_filters_topic_target_and_explicitly_enabled_rows() {
        let db = crate::test_support::test_db(2).await;
        let general = create(&db, "subscriber", "news", None).await.unwrap();
        let target_a = create(&db, "subscriber", "news", Some("oc_target_a"))
            .await
            .unwrap();
        let target_b = create(&db, "subscriber", "news", Some("oc_target_b"))
            .await
            .unwrap();
        let disabled = create(&db, "subscriber", "news", Some("oc_target_a"))
            .await
            .unwrap();
        toggle(&db, disabled.id, false).await.unwrap();
        let other_topic = create(&db, "subscriber", "events", Some("oc_target_a"))
            .await
            .unwrap();
        let legacy = orm::Entity::insert(orm::ActiveModel {
            subscriber_id: Set("legacy".into()),
            topic: Set("news".into()),
            target_id: Set(Some("oc_target_a".into())),
            enabled: Set(None),
            created_at: Set(None),
            ..Default::default()
        })
        .exec(db.orm())
        .await
        .unwrap();

        assert_eq!(
            ids(push(&db, "news", None).await.unwrap()),
            BTreeSet::from([general.id, target_a.id, target_b.id])
        );
        assert_eq!(
            ids(push(&db, "news", Some("oc_target_a")).await.unwrap()),
            BTreeSet::from([target_a.id])
        );
        assert_eq!(
            ids(push(&db, "events", Some("oc_target_a")).await.unwrap()),
            BTreeSet::from([other_topic.id])
        );
        assert!(push(&db, "missing", None).await.unwrap().is_empty());
        assert!(
            push(&db, "news", Some("missing-target"))
                .await
                .unwrap()
                .is_empty()
        );

        // 旧 NULL 开关在管理投影中显示启用，但推送查询只选择数据库里显式 true 的行。
        let all = list(&db).await.unwrap();
        assert_eq!(all.len(), 6);
        let legacy_row = all
            .iter()
            .find(|row| row.id == legacy.last_insert_id)
            .unwrap();
        assert!(legacy_row.enabled);
        assert_eq!(legacy_row.created_at, None);
        assert_eq!(legacy_row.target_id.as_deref(), Some("oc_target_a"));
        assert!(
            !all.iter()
                .find(|row| row.id == disabled.id)
                .unwrap()
                .enabled
        );
        assert_eq!(general.target_id, None);
        assert!(general.created_at.is_some());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn toggle_preserves_other_columns_and_missing_id_reports_database_error() {
        let db = crate::test_support::test_db(2).await;
        assert!(list(&db).await.unwrap().is_empty());
        let created = create(&db, "subscriber", "news", Some("native-target"))
            .await
            .unwrap();
        assert!(created.enabled);
        let mut expected = orm::Entity::find_by_id(created.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        expected.enabled = Some(false);
        toggle(&db, created.id, false).await.unwrap();
        assert_eq!(
            orm::Entity::find_by_id(created.id)
                .one(db.orm())
                .await
                .unwrap(),
            Some(expected)
        );

        remove(&db, created.id).await.unwrap();
        remove(&db, created.id).await.unwrap();
        assert!(list(&db).await.unwrap().is_empty());
        let err = toggle(&db, created.id, true).await.unwrap_err();
        assert!(matches!(
            err,
            crate::error::NuonoError::NotFound(ref message)
                if message == &format!("订阅 {} 不存在", created.id)
        ));
        db.shutdown().await.unwrap();
    }
}
