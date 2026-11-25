//! 规范群组身份持久化（群组身份仅由平台、命名空间与原生群/频道 ID 唯一确定，与消息传输路由或系统账号归属无关）。
use crate::{
    Database,
    entity::groups,
    error::{NuonoError, Result},
};
use nuono_domain::ConversationRef;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set, Statement,
    sea_query::OnConflict,
};

pub use crate::group_sync::GroupBotRow;
pub use groups::Model as Group;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct GroupIdentity {
    pub platform: String,
    pub namespace: String,
    pub conversation_kind: String,
    pub native_parent_id: String,
    pub native_group_id: String,
}

impl GroupIdentity {
    pub fn onebot(native: &str) -> Self {
        Self {
            platform: "onebot".into(),
            namespace: String::new(),
            conversation_kind: "group:native".into(),
            native_parent_id: String::new(),
            native_group_id: native.into(),
        }
    }

    pub fn from_conversation(conversation: &ConversationRef) -> Result<Option<Self>> {
        conversation
            .validate()
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        let (kind, identity, parent) = match conversation {
            ConversationRef::Direct { .. } => return Ok(None),
            ConversationRef::Group { group } => ("group", group, None),
            ConversationRef::Channel { channel, parent } => ("channel", channel, parent.as_ref()),
            _ => return Ok(None),
        };
        let scheme = serde_json::to_value(identity.scheme())?;
        let scheme = scheme
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("json:{}", scheme));
        // conversation_kind 包含平台适配器的 ID 编码方案（scheme），确保不同的 OpenID 空间与原生账号 ID 空间绝不会冲突；
        // native_parent_id 序列化时包含父级完整身份标识与显式格式版本号。
        Ok(Some(Self {
            platform: identity.platform().as_str().to_owned(),
            namespace: identity
                .namespace()
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("")
                .to_owned(),
            conversation_kind: format!("{kind}:{scheme}"),
            native_parent_id: parent
                .map(|id| serde_json::to_string(&serde_json::json!([1, id])))
                .transpose()?
                .unwrap_or_default(),
            native_group_id: identity.value().as_str().to_owned(),
        }))
    }

    fn query(&self) -> sea_orm::Select<groups::Entity> {
        groups::Entity::find()
            .filter(groups::Column::Platform.eq(&self.platform))
            .filter(groups::Column::Namespace.eq(&self.namespace))
            .filter(groups::Column::ConversationKind.eq(&self.conversation_kind))
            .filter(groups::Column::NativeParentId.eq(&self.native_parent_id))
            .filter(groups::Column::NativeGroupId.eq(&self.native_group_id))
    }
}

impl Database {
    pub async fn group_for_onebot(&self, native: &str) -> Result<Option<Group>> {
        Ok(GroupIdentity::onebot(native)
            .query()
            .one(self.orm())
            .await?)
    }
    pub async fn is_conversation_enabled(&self, conversation: &ConversationRef) -> Result<bool> {
        self.is_conversation_enabled_with_context(conversation, "", None)
            .await
    }

    /// 检查会话是否启用，并在群组首次建档或当前名称为空时顺手回填群名与在群机器人归属。
    pub async fn is_conversation_enabled_with_context(
        &self,
        conversation: &ConversationRef,
        display_name: &str,
        bot: Option<(&str, &str)>,
    ) -> Result<bool> {
        let Some(identity) = GroupIdentity::from_conversation(conversation)? else {
            return Ok(true);
        };
        let group = self.ensure_group(&identity, display_name).await?;
        if let Some((platform, bot_id)) = bot {
            self.record_group_bot(group.id, platform, bot_id).await?;
        }
        Ok(group.enabled && nuono_app::entitlement::is_group_active(group.id).await)
    }

    pub async fn group_by_id(&self, group_id: i64) -> Result<Option<Group>> {
        Ok(groups::Entity::find_by_id(group_id).one(self.orm()).await?)
    }

    pub async fn group_for_conversation(
        &self,
        conversation: &ConversationRef,
    ) -> Result<Option<Group>> {
        match GroupIdentity::from_conversation(conversation)? {
            Some(identity) => Ok(identity.query().one(self.orm()).await?),
            None => Ok(None),
        }
    }

    pub async fn ensure_group(
        &self,
        identity: &GroupIdentity,
        display_name: &str,
    ) -> Result<Group> {
        if [
            &identity.platform,
            &identity.namespace,
            &identity.conversation_kind,
            &identity.native_parent_id,
            &identity.native_group_id,
        ]
        .iter()
        .map(|value| value.len())
        .sum::<usize>()
            > 1024
        {
            return Err(NuonoError::Validation("完整群身份超过1024字节".into()));
        }
        if identity.platform.is_empty()
            || identity.conversation_kind.is_empty()
            || identity.native_group_id.is_empty()
        {
            return Err(NuonoError::Validation("群身份不完整".into()));
        }
        if let Some(existing) = identity.query().one(self.orm()).await? {
            let trimmed = display_name.trim();
            if existing.display_name.trim().is_empty() && !trimmed.is_empty() {
                let now = chrono::Utc::now().timestamp_millis();
                self.orm()
                    .execute_raw(Statement::from_sql_and_values(
                        self.backend(),
                        "UPDATE nuono_groups SET display_name=$1,updated_at_ms=$2,revision=revision+1 WHERE id=$3 AND display_name=''",
                        [trimmed.into(), now.into(), existing.id.into()],
                    ))
                    .await?;
                if let Some(updated) = self.group_by_id(existing.id).await? {
                    return Ok(updated);
                }
            }
            return Ok(existing);
        }
        let now = chrono::Utc::now().timestamp_millis();
        groups::Entity::insert(groups::ActiveModel {
            platform: Set(identity.platform.clone()),
            namespace: Set(identity.namespace.clone()),
            conversation_kind: Set(identity.conversation_kind.clone()),
            native_parent_id: Set(identity.native_parent_id.clone()),
            native_group_id: Set(identity.native_group_id.clone()),
            display_name: Set(display_name.to_owned()),
            enabled: Set(true),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([
                groups::Column::Platform,
                groups::Column::Namespace,
                groups::Column::ConversationKind,
                groups::Column::NativeParentId,
                groups::Column::NativeGroupId,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(self.orm())
        .await?;
        identity
            .query()
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::NotFound("群对象初始化失败".into()))
    }

    pub async fn all_groups(&self) -> Result<Vec<Group>> {
        Ok(groups::Entity::find()
            .order_by_asc(groups::Column::Id)
            .all(self.orm())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn complete_group_identity_is_unique_without_bot_or_owner_identity() {
        let db = crate::test_support::test_db(4).await;
        let base = GroupIdentity {
            platform: "qq".into(),
            namespace: "".into(),
            conversation_kind: "group:numeric".into(),
            native_parent_id: "".into(),
            native_group_id: "00123".into(),
        };
        let (a, b) = tokio::join!(db.ensure_group(&base, "A"), db.ensure_group(&base, "B"));
        let a = a.unwrap();
        assert_eq!(a.id, b.unwrap().id);
        for alternative in [
            GroupIdentity {
                platform: "wechat".into(),
                ..base.clone()
            },
            GroupIdentity {
                namespace: "tenant".into(),
                ..base.clone()
            },
            GroupIdentity {
                native_parent_id: "space".into(),
                ..base.clone()
            },
            GroupIdentity {
                conversation_kind: "channel:numeric".into(),
                ..base.clone()
            },
            GroupIdentity {
                native_group_id: "123".into(),
                ..base.clone()
            },
        ] {
            assert_ne!(a.id, db.ensure_group(&alternative, "").await.unwrap().id);
        }
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn sync_preserves_shared_object_switch_and_does_not_create_authority() {
        let db = crate::test_support::test_db(2).await;
        let (first_id, created) = db
            .upsert_synced_group(&serde_json::json!({"group_id":123,"group_name":"First"}))
            .await
            .unwrap();
        assert!(created);
        let group = db.group_for_onebot("123").await.unwrap().unwrap();
        assert_eq!(first_id, group.id);
        let conversation = ConversationRef::group(
            nuono_domain::IdentityRef::new(
                nuono_domain::Platform::OneBot,
                nuono_domain::IdentityKind::Group,
                nuono_domain::IdentityScheme::Native,
                "123",
                None::<String>,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            GroupIdentity::from_conversation(&conversation).unwrap(),
            Some(GroupIdentity::onebot("123"))
        );
        assert_eq!(
            db.group_for_conversation(&conversation)
                .await
                .unwrap()
                .unwrap()
                .id,
            group.id
        );
        nuono_app::groups::GroupStore::toggle_group(&db, group.id, false)
            .await
            .unwrap();
        let (second_id, created_again) = db
            .upsert_synced_group(&serde_json::json!({"group_id":123,"group_name":"Renamed"}))
            .await
            .unwrap();
        assert_eq!(second_id, group.id);
        assert!(!created_again);
        let refreshed = db.group_by_id(group.id).await.unwrap().unwrap();
        assert_eq!(refreshed.display_name, "Renamed");
        assert!(!refreshed.enabled);
        assert!(!db.is_group_enabled("123").await.unwrap());
        assert!(!db.is_conversation_enabled(&conversation).await.unwrap());
        assert!(
            db.group_configuration_grants(group.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.upsert_synced_group(&serde_json::json!({"group_id":"00123"}))
                .await
                .is_err()
        );
        // 覆盖同步应补齐新群并清理已退群，但不影响其他机器人的存在记录。
        let other = db
            .ensure_group(&GroupIdentity::onebot("456"), "Second")
            .await
            .unwrap();
        db.replace_bot_groups("onebot", "10001", &[group.id, other.id])
            .await
            .unwrap();
        db.record_group_bot(group.id, "onebot", "10002")
            .await
            .unwrap();
        db.replace_bot_groups("onebot", "10001", &[other.id])
            .await
            .unwrap();
        let map = db.list_group_bots(&[group.id, other.id]).await.unwrap();
        assert_eq!(
            map.get(&group.id)
                .unwrap()
                .iter()
                .map(|r| r.bot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["10002"]
        );
        assert_eq!(
            map.get(&other.id)
                .unwrap()
                .iter()
                .map(|r| r.bot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["10001"]
        );
        db.shutdown().await.unwrap();
    }
}
