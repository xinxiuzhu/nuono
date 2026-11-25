use crate::{
    Database,
    entity::groups,
    error::{NuonoError, Result},
};
use nuono_app::groups::{GroupRecord, GroupStore};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Statement,
    sea_query::ExprTrait,
};
use std::collections::HashMap;

fn default_platform_label(platform: &str) -> String {
    match platform {
        "onebot" => "OneBot v11".into(),
        "qq_official" => "QQ 官方机器人".into(),
        "feishu" => "飞书".into(),
        "telegram" => "Telegram".into(),
        "discord" => "Discord".into(),
        other if !other.is_empty() => other.to_owned(),
        _ => "未知平台".into(),
    }
}

#[async_trait::async_trait]
impl GroupStore for Database {
    async fn list_groups(&self, user: Option<i64>) -> Result<Vec<GroupRecord>> {
        let mut query = groups::Entity::find();
        if let Some(user) = user {
            use sea_orm::sea_query::{Alias, Expr, Query};
            let now = chrono::Utc::now().timestamp_millis();
            let sources = Query::select()
                .column(Alias::new("group_id"))
                .from(Alias::new("nuono_group_authorizations"))
                .and_where(Expr::col(Alias::new("user_id")).eq(user))
                .and_where(Expr::col(Alias::new("capability")).eq("manage_plugin_config"))
                .and_where(Expr::col(Alias::new("revoked_at_ms")).is_null())
                .and_where(Expr::col(Alias::new("valid_from_ms")).lte(now))
                .and_where(
                    Expr::col(Alias::new("expires_at_ms"))
                        .is_null()
                        .or(Expr::col(Alias::new("expires_at_ms")).gt(now)),
                )
                .to_owned();
            query = query.filter(Expr::col(groups::Column::Id).in_subquery(sources));
        }
        let rows = query
            .order_by_asc(groups::Column::Id)
            .all(self.orm())
            .await?;
        let mut last_messages: HashMap<String, i64> = HashMap::new();
        if !rows.is_empty() {
            // 按消息表汇总各群最后一条消息的时间，避免切换开关或改名时误把更新时间当成群活跃时间。
            let activity_rows = self
                .orm()
                .query_all_raw(Statement::from_string(
                    self.backend(),
                    "SELECT native_group_id, MAX(occurred_at_ms) AS last_ts \
                     FROM nuono_messages \
                     WHERE native_group_id IS NOT NULL AND native_group_id <> '' \
                     GROUP BY native_group_id",
                ))
                .await?;
            for row in activity_rows {
                if let (Ok(gid), Ok(ts)) = (
                    row.try_get::<String>("", "native_group_id"),
                    row.try_get::<i64>("", "last_ts"),
                ) {
                    last_messages.insert(gid, ts);
                }
            }
        }
        Ok(rows
            .into_iter()
            .map(|g| {
                let supports_member_management = g.platform == "onebot"
                    && g.namespace.is_empty()
                    && g.conversation_kind == "group:native"
                    && g.native_parent_id.is_empty();
                let source_label = default_platform_label(&g.platform);
                let last_activity = last_messages
                    .get(&g.native_group_id)
                    .map(|ts| ts.to_string());
                GroupRecord {
                    group_id: g.id.to_string(),
                    native_group_id: g.native_group_id,
                    platform: g.platform,
                    namespace: g.namespace,
                    source_label,
                    supports_member_management,
                    group_name: Some(g.display_name),
                    enabled: Some(g.enabled),
                    ai_blocked: false,
                    last_activity,
                    bots: Vec::new(),
                }
            })
            .collect())
    }
    async fn group_exists(&self, group: i64) -> Result<bool> {
        Ok(self.group_by_id(group).await?.is_some())
    }
    async fn toggle_group(&self, group: i64, enabled: bool) -> Result<()> {
        let result = groups::Entity::update_many()
            .col_expr(
                groups::Column::Enabled,
                sea_orm::sea_query::Expr::value(enabled),
            )
            .col_expr(
                groups::Column::Revision,
                sea_orm::sea_query::Expr::col(groups::Column::Revision).add(1),
            )
            .col_expr(
                groups::Column::UpdatedAtMs,
                sea_orm::sea_query::Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(groups::Column::Id.eq(group))
            .exec(self.orm())
            .await?;
        if result.rows_affected == 0 {
            return Err(NuonoError::NotFound("群不存在".into()));
        }
        Ok(())
    }
    async fn rename_group(&self, group: i64, display_name: &str) -> Result<()> {
        let trimmed = display_name.trim();
        if trimmed.chars().count() > 120 {
            return Err(NuonoError::InvalidInput("群名称不能超过 120 个字符".into()));
        }
        let result = groups::Entity::update_many()
            .col_expr(
                groups::Column::DisplayName,
                sea_orm::sea_query::Expr::value(trimmed.to_owned()),
            )
            .col_expr(
                groups::Column::Revision,
                sea_orm::sea_query::Expr::col(groups::Column::Revision).add(1),
            )
            .col_expr(
                groups::Column::UpdatedAtMs,
                sea_orm::sea_query::Expr::value(chrono::Utc::now().timestamp_millis()),
            )
            .filter(groups::Column::Id.eq(group))
            .exec(self.orm())
            .await?;
        if result.rows_affected == 0 {
            return Err(NuonoError::NotFound("群不存在".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_store::{DIRECTION_INBOUND, ROLE_USER};
    use crate::entity::messages;
    use crate::groups::GroupIdentity;
    use sea_orm::Set;

    #[tokio::test]
    async fn list_groups_exposes_platform_capabilities_real_activity_and_rename() {
        let db = crate::test_support::test_db(4).await;
        let ob = db
            .ensure_group(&GroupIdentity::onebot("100001"), "")
            .await
            .unwrap();
        let qq = db
            .ensure_group(
                &GroupIdentity {
                    platform: "qq_official".into(),
                    namespace: "100000001".into(),
                    conversation_kind: "group:openid".into(),
                    native_parent_id: String::new(),
                    native_group_id: "DEMO_GROUP_OPENID_0001".into(),
                },
                "",
            )
            .await
            .unwrap();

        // 给 OneBot 群写入一条消息记录；官方群不写消息，验证最后活跃不会误拿 updated_at_ms 充数。
        messages::Entity::insert(messages::ActiveModel {
            platform: Set("onebot".into()),
            bot_instance_id: Set(Some("200001".into())),
            native_group_id: Set(Some("100001".into())),
            direction: Set(DIRECTION_INBOUND.into()),
            role: Set(ROLE_USER.into()),
            text_content: Set("hello".into()),
            content_blocks_json: Set("[]".into()),
            occurred_at_ms: Set(1_795_200_000_000),
            created_at_ms: Set(1_795_200_000_000),
            ..Default::default()
        })
        .exec(db.orm())
        .await
        .unwrap();

        db.rename_group(qq.id, "  测试官方群  ").await.unwrap();

        let listed = db.list_groups(None).await.unwrap();
        assert_eq!(listed.len(), 2);

        let ob_row = listed
            .iter()
            .find(|r| r.group_id == ob.id.to_string())
            .unwrap();
        assert!(ob_row.supports_member_management);
        assert_eq!(ob_row.source_label, "OneBot v11");
        assert_eq!(ob_row.last_activity.as_deref(), Some("1795200000000"));

        let qq_row = listed
            .iter()
            .find(|r| r.group_id == qq.id.to_string())
            .unwrap();
        assert!(!qq_row.supports_member_management);
        assert_eq!(qq_row.namespace, "100000001");
        assert_eq!(qq_row.group_name.as_deref(), Some("测试官方群"));
        assert_eq!(qq_row.last_activity, None);
    }
}
