//! 站内通知实体。
//!
//! 通知在写入点插入、前端轮询读取，接收者可以标记已读或删除自己的通知。
//! `link` 是前端路由（如 `/groups?tab=claims`），点击跳转时顺带标记已读。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_notifications")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 接收者 `users.id`。账号删除时通知随账号一并删除
    /// （`service::users::delete_user` 的事务里）。
    pub user_id: i64,
    /// 通知类型，如 `claim_reviewed` / `subscription_expiring`。前端按类型选图标。
    pub kind: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub read_at: Option<crate::clock::UtcMillis>,
    pub created_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
