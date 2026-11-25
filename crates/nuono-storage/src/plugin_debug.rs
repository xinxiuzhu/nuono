//! 插件 KV 的管理查询投影。

use sea_orm::{EntityTrait, QueryOrder, QuerySelect};

use crate::db::Database;
use crate::entity::plugin_kv;
use crate::error::Result;
use crate::plugin_kv::NAMESPACE_COLLECTION;

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PluginKvRecord {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub updated_at: Option<String>,
}

pub async fn get_plugin_kv(db: &Database, limit: u64) -> Result<Vec<PluginKvRecord>> {
    let rows = plugin_kv::Entity::find()
        .order_by_desc(plugin_kv::Column::UpdatedAt)
        .limit(limit)
        .all(db.orm())
        .await?;
    rows.into_iter()
        .map(|row| {
            let (namespace, value) = if row.collection == NAMESPACE_COLLECTION {
                (row.plugin_id, serde_json::from_str(&row.value_json)?)
            } else {
                (
                    format!("{}/{}/{}", row.plugin_id, row.collection, row.scope_key),
                    row.value_json,
                )
            };
            Ok(PluginKvRecord {
                namespace,
                key: row.record_key,
                value,
                updated_at: Some(crate::clock::format_wire_ts(row.updated_at)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveValue::Set;

    #[tokio::test]
    async fn kv_inspection_limits_rows_in_descending_timestamp_order() {
        let db = crate::test_support::test_db(2).await;
        for (namespace, timestamp, value) in [
            ("old", "2000-01-01T00:00:00Z", "plain text"),
            ("new", "2020-01-01T00:00:00Z", r#"{"value":"新"}"#),
            ("middle", "2010-01-01T00:00:00Z", "middle value"),
        ] {
            db.sync_plugin(namespace, "1", &serde_json::json!({}))
                .await
                .unwrap();
            plugin_kv::Entity::insert(plugin_kv::ActiveModel {
                generation: sea_orm::Set(uuid::Uuid::new_v4().to_string()),
                expires_at_ms: sea_orm::Set(None),
                plugin_id: Set(namespace.into()),
                collection: Set(NAMESPACE_COLLECTION.into()),
                scope_key: Set("global".into()),
                record_key: Set("same-key".into()),
                value_json: Set(serde_json::to_string(value).unwrap()),
                value_version: Set(1),
                created_at_ms: Set(chrono::DateTime::parse_from_rfc3339(timestamp)
                    .unwrap()
                    .timestamp_millis()),
                updated_at: Set(crate::clock::store_time(
                    chrono::DateTime::parse_from_rfc3339(timestamp)
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                )),
                ..Default::default()
            })
            .exec(db.orm())
            .await
            .unwrap();
        }
        let latest = get_plugin_kv(&db, 2).await.unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].namespace, "new");
        assert_eq!(latest[0].key, "same-key");
        assert_eq!(latest[0].value, r#"{"value":"新"}"#);
        assert_eq!(latest[0].updated_at.as_deref(), Some("1577836800000"));
        assert_eq!(latest[1].namespace, "middle");
        assert_eq!(latest[1].updated_at.as_deref(), Some("1262304000000"));
        assert!(get_plugin_kv(&db, 0).await.unwrap().is_empty());
        let all = get_plugin_kv(&db, 100).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].namespace, "old");
        assert_eq!(all[2].value, "plain text");
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn kv_inspection_preserves_unix_epoch_timestamp() {
        let db = crate::test_support::test_db(2).await;
        db.sync_plugin("epoch", "1", &serde_json::json!({}))
            .await
            .unwrap();
        plugin_kv::Entity::insert(plugin_kv::ActiveModel {
            generation: sea_orm::Set(uuid::Uuid::new_v4().to_string()),
            expires_at_ms: sea_orm::Set(None),
            plugin_id: Set("epoch".into()),
            collection: Set(NAMESPACE_COLLECTION.into()),
            scope_key: Set("global".into()),
            record_key: Set("epoch".into()),
            value_json: Set(serde_json::to_string("value").unwrap()),
            value_version: Set(1),
            created_at_ms: Set(0),
            updated_at: Set(crate::clock::store_time(
                chrono::DateTime::from_timestamp_millis(0).unwrap(),
            )),
            ..Default::default()
        })
        .exec(db.orm())
        .await
        .unwrap();
        assert_eq!(
            get_plugin_kv(&db, 1).await.unwrap()[0]
                .updated_at
                .as_deref(),
            Some("0")
        );
        db.shutdown().await.unwrap();
    }
}
