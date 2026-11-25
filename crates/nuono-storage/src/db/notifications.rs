//! 站内通知的持久化。
//!
//! 通知是「写入点直接插入、前端轮询未读数」的最简机制 —— 不建队列、不建 WebSocket
//! 推送通道。轮询间隔 30 秒，与顶栏健康徽章同频，后台开销可以忽略。
//!
//! 所有读取、已读及删除操作都带 `user_id` 过滤：通知只属于接收者本人，接口层绝不做
//! 「按 id 读/改」的裸操作，否则任何登录用户都能猜 id 读别人的通知。

use super::Database;
use crate::entity::{global_configs, notifications};
use crate::error::Result;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};

const OPERATIONS_NOTICE_STATE_KEY: &str = "operations.notice_state.v1";

async fn save_operations_notice_state(tx: &DatabaseTransaction, states: &str) -> Result<()> {
    let value = nuono_domain::scalar::parse_json(states)?;
    super::set_global_config_on(tx, OPERATIONS_NOTICE_STATE_KEY, &value).await
}

impl Database {
    /// 运维状态只来自当前数据库；空库没有上一次运行或另一数据库的状态。
    pub async fn load_operations_notice_state(&self) -> Result<Option<String>> {
        self.timed(
            "load_operations_notice_state",
            "nuono_global_configs",
            async {
                Ok(
                    global_configs::Entity::find_by_id(OPERATIONS_NOTICE_STATE_KEY)
                        .one(self.orm())
                        .await?
                        .map(|row| row.value),
                )
            },
        )
        .await
    }

    /// 通知与生成通知的状态快照同一事务提交，避免故障重试或换库后错误回放。
    /// 即使没有接收者或通知，也保存本次状态，以免随后误报已恢复。
    pub async fn publish_operations_notifications(
        &self,
        user_ids: &[i64],
        messages: &[(String, String, String)],
        states: &str,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        save_operations_notice_state(&tx, states).await?;
        for (title, body, link) in messages {
            for user_id in user_ids {
                notifications::ActiveModel {
                    user_id: Set(*user_id),
                    kind: Set("operations".into()),
                    title: Set(title.clone()),
                    body: Set(body.clone()),
                    link: Set((!link.is_empty()).then(|| link.clone())),
                    read_at: Set(None),
                    created_at: Set(crate::clock::store_time(crate::clock::db_now())),
                    ..Default::default()
                }
                .insert(&tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// 清理指定页面的运维通知，并在同一事务中更新去重状态。
    pub async fn clear_operations_notifications(
        &self,
        links: &[&str],
        remaining_states: Option<&str>,
    ) -> Result<u64> {
        let tx = self.begin_write().await?;
        let deleted = notifications::Entity::delete_many()
            .filter(notifications::Column::Kind.eq("operations"))
            .filter(notifications::Column::Link.is_in(links.iter().copied()))
            .exec(&tx)
            .await?;
        if let Some(states) = remaining_states {
            save_operations_notice_state(&tx, states).await?;
        }
        tx.commit().await?;
        Ok(deleted.rows_affected)
    }

    /// 一次状态变化的通知整体提交，避免中途失败后重试给部分管理员重复发信。
    pub async fn push_notifications_atomic(
        &self,
        user_ids: &[i64],
        kind: &str,
        messages: &[(String, String, String)],
    ) -> Result<()> {
        if user_ids.is_empty() || messages.is_empty() {
            return Ok(());
        }
        let tx = self.begin_write().await?;
        for (title, body, link) in messages {
            for user_id in user_ids {
                notifications::ActiveModel {
                    user_id: Set(*user_id),
                    kind: Set(kind.into()),
                    title: Set(title.clone()),
                    body: Set(body.clone()),
                    link: Set((!link.is_empty()).then(|| link.clone())),
                    read_at: Set(None),
                    created_at: Set(crate::clock::store_time(crate::clock::db_now())),
                    ..Default::default()
                }
                .insert(&tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn push_notification(
        &self,
        user_id: i64,
        kind: &str,
        title: &str,
        body: &str,
        link: Option<&str>,
    ) -> Result<i64> {
        self.timed("push_notification", "nuono_notifications", async {
            let row = notifications::ActiveModel {
                user_id: Set(user_id),
                kind: Set(kind.to_string()),
                title: Set(title.to_string()),
                body: Set(body.to_string()),
                link: Set(link.filter(|l| !l.is_empty()).map(str::to_string)),
                read_at: Set(None),
                created_at: Set(crate::clock::store_time(crate::clock::db_now())),
                ..Default::default()
            }
            .insert(self.orm())
            .await?;
            Ok(row.id)
        })
        .await
    }

    pub async fn list_notifications(
        &self,
        user_id: i64,
        limit: u64,
    ) -> Result<Vec<notifications::Model>> {
        self.timed("list_notifications", "nuono_notifications", async {
            Ok(notifications::Entity::find()
                .filter(notifications::Column::UserId.eq(user_id))
                .order_by_desc(notifications::Column::Id)
                .limit(limit.clamp(1, 200))
                .all(self.orm())
                .await?)
        })
        .await
    }

    pub async fn mark_notification_read(&self, user_id: i64, id: i64) -> Result<bool> {
        self.timed("mark_notification_read", "nuono_notifications", async {
            let result = notifications::Entity::update_many()
                .col_expr(
                    notifications::Column::ReadAt,
                    Expr::value(crate::clock::db_now()),
                )
                .filter(notifications::Column::Id.eq(id))
                .filter(notifications::Column::UserId.eq(user_id))
                .filter(notifications::Column::ReadAt.is_null())
                .exec(self.orm())
                .await?;
            Ok(result.rows_affected == 1)
        })
        .await
    }

    pub async fn mark_all_notifications_read(&self, user_id: i64) -> Result<u64> {
        self.timed(
            "mark_all_notifications_read",
            "nuono_notifications",
            async {
                Ok(notifications::Entity::update_many()
                    .col_expr(
                        notifications::Column::ReadAt,
                        Expr::value(crate::clock::db_now()),
                    )
                    .filter(notifications::Column::UserId.eq(user_id))
                    .filter(notifications::Column::ReadAt.is_null())
                    .exec(self.orm())
                    .await?
                    .rows_affected)
            },
        )
        .await
    }

    pub async fn unread_notification_count(&self, user_id: i64) -> Result<u64> {
        self.timed("unread_notification_count", "nuono_notifications", async {
            Ok(notifications::Entity::find()
                .filter(notifications::Column::UserId.eq(user_id))
                .filter(notifications::Column::ReadAt.is_null())
                .count(self.orm())
                .await?)
        })
        .await
    }

    pub async fn delete_notification(&self, user_id: i64, id: i64) -> Result<bool> {
        self.timed("delete_notification", "nuono_notifications", async {
            let result = notifications::Entity::delete_many()
                .filter(notifications::Column::Id.eq(id))
                .filter(notifications::Column::UserId.eq(user_id))
                .exec(self.orm())
                .await?;
            Ok(result.rows_affected == 1)
        })
        .await
    }

    pub async fn delete_notifications_for_user(&self, user_id: i64) -> Result<u64> {
        self.timed(
            "delete_notifications_for_user",
            "nuono_notifications",
            async {
                Ok(notifications::Entity::delete_many()
                    .filter(notifications::Column::UserId.eq(user_id))
                    .exec(self.orm())
                    .await?
                    .rows_affected)
            },
        )
        .await
    }

    pub async fn delete_notifications_older_than(&self, days: u64) -> Result<u64> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
        self.timed(
            "delete_notifications_older_than",
            "nuono_notifications",
            async {
                Ok(notifications::Entity::delete_many()
                    .filter(notifications::Column::CreatedAt.lt((cutoff).timestamp_millis()))
                    .exec(self.orm())
                    .await?
                    .rows_affected)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::{NewUser, ROLE_USER, STATUS_ACTIVE};

    async fn test_db() -> Database {
        crate::test_support::test_db(2).await
    }

    async fn user(db: &Database, name: &str) -> i64 {
        db.create_user(NewUser {
            username: name.to_string(),
            email: None,
            password: "correct-horse-battery".to_string(),
            role: ROLE_USER.to_string(),
            status: STATUS_ACTIVE.to_string(),
            email_verified: true,
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn notification_batch_does_not_partially_deliver() {
        let db = test_db().await;
        let alice = user(&db, "batch-alice").await;
        let messages = vec![("故障".into(), "检查配置".into(), "/plugins".into())];
        use sea_orm::ConnectionTrait;
        db.orm()
            .execute_raw(sea_orm::Statement::from_string(
                db.backend(),
                "CREATE UNIQUE INDEX notification_batch_once ON nuono_notifications(kind, user_id)",
            ))
            .await
            .unwrap();
        assert!(
            db.push_notifications_atomic(&[alice, alice], "operations", &messages)
                .await
                .is_err()
        );
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 0);
        db.push_notifications_atomic(&[alice], "operations", &messages)
            .await
            .unwrap();
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn operations_notifications_and_state_commit_together() {
        let db = test_db().await;
        let alice = user(&db, "operations-alice").await;
        assert_eq!(db.load_operations_notice_state().await.unwrap(), None);
        db.publish_operations_notifications(&[], &[], "{}")
            .await
            .unwrap();

        use sea_orm::ConnectionTrait;
        db.orm()
            .execute_raw(sea_orm::Statement::from_string(
                db.backend(),
                "CREATE UNIQUE INDEX operations_notification_once ON nuono_notifications(kind, user_id)",
            ))
            .await
            .unwrap();
        let messages = vec![("故障".into(), "检查配置".into(), "/plugins".into())];
        let states = r#"{"database":{"severity":"warning"}}"#;
        // 第二个收件人插入失败时，第一条通知与先更新的状态都必须回滚。
        assert!(
            db.publish_operations_notifications(&[alice, alice], &messages, states)
                .await
                .is_err()
        );
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 0);
        assert_eq!(
            db.load_operations_notice_state().await.unwrap().as_deref(),
            Some("{}")
        );

        db.publish_operations_notifications(&[alice], &messages, states)
            .await
            .unwrap();
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);
        assert_eq!(
            db.load_operations_notice_state().await.unwrap().as_deref(),
            Some(states)
        );
        // 传入非法的状态快照 JSON 时必须整体拒绝并回滚，既不能清除现有通知，也不能覆盖无关的运行时配置。
        db.set_global_config("runtime.settings.v1", serde_json::json!({"probe": true}))
            .await
            .unwrap();
        assert!(
            db.clear_operations_notifications(&["/plugins"], Some("invalid JSON"))
                .await
                .is_err()
        );
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);
        assert_eq!(
            db.load_operations_notice_state().await.unwrap().as_deref(),
            Some(states)
        );
        assert_eq!(
            db.get_global_config("runtime.settings.v1").await.unwrap(),
            Some(serde_json::json!({"probe": true}))
        );
        // 无状态变化、无通知时也可更新快照。
        db.publish_operations_notifications(&[alice], &[], "{}")
            .await
            .unwrap();
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);
        assert_eq!(
            db.load_operations_notice_state().await.unwrap().as_deref(),
            Some("{}")
        );
        assert_eq!(
            db.clear_operations_notifications(&["/plugins"], Some(states))
                .await
                .unwrap(),
            1
        );
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 0);
        assert_eq!(
            db.load_operations_notice_state().await.unwrap().as_deref(),
            Some(states)
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn notifications_are_scoped_to_their_recipient() {
        let db = test_db().await;
        let alice = user(&db, "alice").await;
        let bob = user(&db, "bob").await;

        let id = db
            .push_notification(
                alice,
                "claim_reviewed",
                "审核通过",
                "你的群认领申请已通过",
                Some("/groups"),
            )
            .await
            .unwrap();

        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);
        assert_eq!(db.unread_notification_count(bob).await.unwrap(), 0);
        // bob 读不到也标不了 alice 的通知。
        assert!(!db.mark_notification_read(bob, id).await.unwrap());
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 1);

        assert!(db.mark_notification_read(alice, id).await.unwrap());
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 0);
        // 已读的重复标记不计数。
        assert!(!db.mark_notification_read(alice, id).await.unwrap());
        // 猜到通知 ID 不能删除别人的通知；接收者可删除且不会重复计数。
        assert!(!db.delete_notification(bob, id).await.unwrap());
        assert_eq!(db.list_notifications(alice, 10).await.unwrap().len(), 1);
        assert!(db.delete_notification(alice, id).await.unwrap());
        assert!(!db.delete_notification(alice, id).await.unwrap());
        assert!(db.list_notifications(alice, 10).await.unwrap().is_empty());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mark_all_read_and_delete_roundtrip() {
        let db = test_db().await;
        let alice = user(&db, "alice").await;

        for i in 0..3 {
            db.push_notification(alice, "test", &format!("t{i}"), "b", None)
                .await
                .unwrap();
        }
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 3);
        assert_eq!(db.mark_all_notifications_read(alice).await.unwrap(), 3);
        assert_eq!(db.unread_notification_count(alice).await.unwrap(), 0);

        // 列表倒序，最新在前。
        let list = db.list_notifications(alice, 10).await.unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].id > list[1].id);

        assert_eq!(db.delete_notifications_for_user(alice).await.unwrap(), 3);
        assert!(db.list_notifications(alice, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retention_sweeps_only_old_notifications() {
        let db = test_db().await;
        let alice = user(&db, "alice").await;

        let old_id = db
            .push_notification(alice, "old", "t", "b", None)
            .await
            .unwrap();
        // 直接把旧通知的 created_at 改到 30 天前，模拟自然过期。
        notifications::Entity::update_many()
            .col_expr(
                notifications::Column::CreatedAt,
                Expr::value(crate::clock::parse_db_ts("2000-01-01 00:00:00").unwrap()),
            )
            .filter(notifications::Column::Id.eq(old_id))
            .exec(db.orm())
            .await
            .unwrap();
        db.push_notification(alice, "fresh", "t", "b", None)
            .await
            .unwrap();

        let removed = db.delete_notifications_older_than(7).await.unwrap();
        assert_eq!(removed, 1, "只该清掉过期的那一条");
        let list = db.list_notifications(alice, 10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, "fresh");
    }
}
