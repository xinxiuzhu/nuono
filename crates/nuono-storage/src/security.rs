//! 持久化黑名单与过滤规则的查询及管理投影。
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, sea_query::Expr};

use crate::db::Database;
use crate::entity::filter_rules;
use crate::error::Result;

pub(crate) const BLACKLIST_USER_TYPE: &str = "blacklist_user";
pub(crate) const BLACKLIST_GROUP_TYPE: &str = "blacklist_group";

pub(crate) fn blacklist_filter_type(target_type: &str) -> String {
    match target_type {
        "user" => BLACKLIST_USER_TYPE.to_string(),
        "group" => BLACKLIST_GROUP_TYPE.to_string(),
        other => format!("blacklist_{other}"),
    }
}

fn blacklist_target_type(filter_type: &str) -> String {
    filter_type
        .strip_prefix("blacklist_")
        .unwrap_or(filter_type)
        .to_string()
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct BlacklistRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub target_type: String,
    pub target_id: String,
    pub reason: Option<String>,
    pub created_at: Option<String>,
}

pub async fn get_blacklists(db: &Database) -> Result<Vec<BlacklistRecord>> {
    let rows = filter_rules::Entity::find()
        .filter(filter_rules::Column::FilterType.is_in([BLACKLIST_USER_TYPE, BLACKLIST_GROUP_TYPE]))
        .order_by_desc(filter_rules::Column::Id)
        .all(db.orm())
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| BlacklistRecord {
            id: r.id,
            target_type: blacklist_target_type(&r.filter_type),
            target_id: r.target_id.unwrap_or_default(),
            reason: (!r.pattern.is_empty()).then_some(r.pattern),
            created_at: r.created_at.map(crate::clock::format_wire_ts),
        })
        .collect())
}

pub async fn add_blacklist(
    db: &Database,
    target_type: &str,
    target_id: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let now = crate::clock::db_now();
    let id = filter_rules::Entity::insert(filter_rules::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        filter_type: Set(blacklist_filter_type(target_type)),
        pattern: Set(reason.unwrap_or_default().to_string()),
        match_mode: Set("exact".to_string()),
        target_id: Set(Some(target_id.to_string())),
        enabled: Set(true),
        created_at: Set(crate::clock::store_time(Some(now))),
    })
    .exec(db.orm())
    .await?
    .last_insert_id;
    Ok(id)
}

pub async fn delete_blacklist(db: &Database, id: i64) -> Result<()> {
    filter_rules::Entity::delete_many()
        .filter(filter_rules::Column::Id.eq(id))
        .filter(filter_rules::Column::FilterType.is_in([BLACKLIST_USER_TYPE, BLACKLIST_GROUP_TYPE]))
        .exec(db.orm())
        .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct FilterRuleRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub filter_type: String,
    pub pattern: String,
    pub match_mode: Option<String>,
    pub target_id: Option<String>,
    pub enabled: bool,
    pub created_at: Option<String>,
}

pub async fn get_filter_rules(db: &Database) -> Result<Vec<FilterRuleRecord>> {
    let rows = filter_rules::Entity::find()
        .filter(
            filter_rules::Column::FilterType.is_not_in([BLACKLIST_USER_TYPE, BLACKLIST_GROUP_TYPE]),
        )
        .order_by_desc(filter_rules::Column::Id)
        .all(db.orm())
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| FilterRuleRecord {
            id: r.id,
            filter_type: r.filter_type,
            pattern: r.pattern,
            match_mode: Some(r.match_mode),
            target_id: r.target_id,
            enabled: r.enabled,
            created_at: r.created_at.map(crate::clock::format_wire_ts),
        })
        .collect())
}

pub async fn add_filter_rule(
    db: &Database,
    filter_type: &str,
    pattern: &str,
    match_mode: Option<&str>,
    target_id: Option<&str>,
) -> Result<i64> {
    let now = crate::clock::db_now();
    let id = filter_rules::Entity::insert(filter_rules::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        filter_type: Set(filter_type.to_string()),
        pattern: Set(pattern.to_string()),
        match_mode: Set(match_mode.unwrap_or("simple").to_string()),
        target_id: Set(target_id.map(str::to_string)),
        enabled: Set(true),
        created_at: Set(crate::clock::store_time(Some(now))),
    })
    .exec(db.orm())
    .await?
    .last_insert_id;
    Ok(id)
}

pub async fn toggle_filter_rule(db: &Database, id: i64, enabled: bool) -> Result<()> {
    filter_rules::Entity::update_many()
        .col_expr(filter_rules::Column::Enabled, Expr::value(enabled))
        .filter(filter_rules::Column::Id.eq(id))
        .exec(db.orm())
        .await?;
    Ok(())
}

pub async fn delete_filter_rule(db: &Database, id: i64) -> Result<()> {
    filter_rules::Entity::delete_by_id(id)
        .exec(db.orm())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveModelTrait;

    #[tokio::test]
    async fn blacklists_preserve_order_nullable_fields_and_unique_targets() {
        let db = crate::test_support::test_db(2).await;
        assert!(get_blacklists(&db).await.unwrap().is_empty());
        let first = add_blacklist(&db, "user", "native-user", Some("reason"))
            .await
            .unwrap();
        let second = add_blacklist(&db, "group", "native-group", None)
            .await
            .unwrap();
        filter_rules::ActiveModel {
            id: Set(second),
            created_at: Set(None),
            ..Default::default()
        }
        .update(db.orm())
        .await
        .unwrap();

        let rows = get_blacklists(&db).await.unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(rows[0].target_type, "group");
        assert_eq!(rows[0].target_id, "native-group");
        assert!(rows[0].reason.is_none());
        assert!(rows[0].created_at.is_none());
        assert_eq!(rows[1].reason.as_deref(), Some("reason"));
        assert!(rows[1].created_at.is_some());
        assert!(
            add_blacklist(&db, "user", "native-user", None)
                .await
                .is_err()
        );

        delete_blacklist(&db, second).await.unwrap();
        delete_blacklist(&db, second).await.unwrap();
        let remaining = get_blacklists(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, first);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn filter_rules_keep_null_enabled_default_and_toggle_only_selected_id() {
        let db = crate::test_support::test_db(2).await;
        assert!(get_filter_rules(&db).await.unwrap().is_empty());
        let first = add_filter_rule(&db, "keyword", "first", Some("regex"), Some("native-group"))
            .await
            .unwrap();
        let second = add_filter_rule(&db, "keyword", "second", None, None)
            .await
            .unwrap();
        filter_rules::ActiveModel {
            id: Set(second),
            created_at: Set(None),
            ..Default::default()
        }
        .update(db.orm())
        .await
        .unwrap();

        let rows = get_filter_rules(&db).await.unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert!(rows[0].enabled);
        assert_eq!(rows[0].match_mode.as_deref(), Some("simple"));
        assert!(rows[0].target_id.is_none());
        assert!(rows[0].created_at.is_none());
        assert_eq!(rows[1].pattern, "first");
        assert_eq!(rows[1].match_mode.as_deref(), Some("regex"));
        assert_eq!(rows[1].target_id.as_deref(), Some("native-group"));
        assert!(rows[1].enabled);
        assert!(rows[1].created_at.is_some());

        toggle_filter_rule(&db, first, false).await.unwrap();
        toggle_filter_rule(&db, i64::MAX, false).await.unwrap();
        let rows = get_filter_rules(&db).await.unwrap();
        assert!(rows[0].enabled);
        assert!(!rows[1].enabled);
        let raw_second = filter_rules::Entity::find_by_id(second)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(raw_second.enabled);
        toggle_filter_rule(&db, first, true).await.unwrap();
        assert!(
            get_filter_rules(&db)
                .await
                .unwrap()
                .iter()
                .all(|row| row.enabled)
        );

        delete_filter_rule(&db, second).await.unwrap();
        delete_filter_rule(&db, second).await.unwrap();
        let remaining = get_filter_rules(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, first);
        db.shutdown().await.unwrap();
    }
}
