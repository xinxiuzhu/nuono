//! 应用层消息过滤策略类型重导出。
pub use nuono_app::filter::*;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use sea_orm::ConnectionTrait;

    async fn test_db() -> Database {
        crate::test_support::test_db(1).await
    }

    #[tokio::test]
    async fn current_group_blacklist_and_content_controls_remain_effective() {
        let db = test_db().await;
        let handler = FilterHandler::new(db.clone());
        let mut event = serde_json::json!({
            "post_type": "message", "self_id": "10001", "user_id": "20002",
            "raw_message": "hello"
        });
        assert_eq!(
            handler.evaluate_event(&event).await.unwrap(),
            FilterResponse::Continue
        );
        let blocked = nuono_storage::security::add_blacklist(&db, "user", "20002", None)
            .await
            .unwrap();
        assert_eq!(
            handler.evaluate_event(&event).await.unwrap(),
            FilterResponse::Stop
        );
        nuono_storage::security::delete_blacklist(&db, blocked)
            .await
            .unwrap();

        let group = db
            .ensure_group(
                &nuono_storage::groups::GroupIdentity::onebot("30003"),
                "group",
            )
            .await
            .unwrap();
        db.orm()
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                db.backend(),
                "UPDATE nuono_groups SET enabled = $1 WHERE id = $2",
                [false.into(), group.id.into()],
            ))
            .await
            .unwrap();
        event["group_id"] = "30003".into();
        assert_eq!(
            handler.evaluate_event(&event).await.unwrap(),
            FilterResponse::Stop
        );

        event.as_object_mut().unwrap().remove("group_id");
        nuono_storage::security::add_filter_rule(&db, "receive", "denied", Some("simple"), None)
            .await
            .unwrap();
        event["raw_message"] = "denied".into();
        assert_eq!(
            handler.evaluate_event(&event).await.unwrap(),
            FilterResponse::Stop
        );
        event["raw_message"] = "allowed".into();
        assert_eq!(
            handler.evaluate_event(&event).await.unwrap(),
            FilterResponse::Continue
        );
        drop(handler);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn security_query_error_is_distinct_and_runtime_fails_closed() {
        let db = test_db().await;
        // 测试桩：通过显式 DDL 删除 filter_rules 表，模拟查询该表失败的场景。
        db.orm()
            .execute_raw(sea_orm::Statement::from_string(
                db.backend(),
                "DROP TABLE nuono_filter_rules",
            ))
            .await
            .unwrap();
        let handler = FilterHandler::new(db.clone());
        let event = serde_json::json!({
            "post_type": "message",
            "self_id": 10001,
            "user_id": 20002,
            "raw_message": "hello"
        });

        assert!(handler.evaluate_event(&event).await.is_err());
        assert_eq!(
            handler.handle_event(&event).await.unwrap(),
            FilterResponse::Stop
        );

        drop(handler);
        db.shutdown().await.unwrap();
    }
}
