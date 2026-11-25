use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use crate::db::Database;
use crate::entity::subscriptions as orm;
use crate::error::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// 订阅记录（运行时）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    pub subscriber_id: String,
    pub topic: String,
    pub target_id: Option<String>,
    pub enabled: bool,
    pub created_at: Option<String>,
}

impl From<orm::Model> for SubscribeRecord {
    fn from(m: orm::Model) -> Self {
        SubscribeRecord {
            id: m.id,
            subscriber_id: m.subscriber_id,
            topic: m.topic,
            target_id: m.target_id,
            enabled: m.enabled.unwrap_or(true),
            created_at: m.created_at.map(crate::clock::format_wire_ts),
        }
    }
}

/// 订阅管理器
///
/// 提供订阅的增删查推功能。
/// 与 gsuid_core 的 GsCoreSubscribe 对标，支持：
/// - session 模式（同 group/user 只保留一条）
/// - single 模式（同 group 可多条）
/// - push（遍历所有订阅者并发送消息）
pub struct SubscribeManager {
    db: Database,
    /// 已注册的订阅主题类型: topic -> description
    registered_topics: DashMap<String, String>,
}

impl SubscribeManager {
    pub fn new(db: Database) -> Arc<Self> {
        Arc::new(Self {
            db,
            registered_topics: DashMap::new(),
        })
    }

    /// 注册订阅主题（插件声明自己支持的订阅类型）
    pub fn register_topic(&self, topic: &str, description: &str) {
        info!(topic = %topic, desc = %description, "📝 注册订阅主题");
        self.registered_topics
            .insert(topic.to_string(), description.to_string());
    }

    /// 添加订阅
    pub async fn add_subscribe(
        &self,
        subscriber_id: &str,
        topic: &str,
        target_id: Option<&str>,
    ) -> Result<SubscribeRecord> {
        let now = crate::clock::db_now();
        let am = orm::ActiveModel {
            subscriber_id: Set(subscriber_id.to_string()),
            topic: Set(topic.to_string()),
            target_id: Set(target_id.map(str::to_string)),
            enabled: Set(Some(true)),
            created_at: Set(crate::clock::store_time(Some(now))),
            ..Default::default()
        };
        let inserted = orm::Entity::insert(am).exec(self.db.orm()).await?;
        let row = orm::Entity::find_by_id(inserted.last_insert_id)
            .one(self.db.orm())
            .await?
            .ok_or_else(|| {
                crate::error::NuonoError::DatabaseQuery(format!(
                    "订阅 {} 插入后回读失败",
                    inserted.last_insert_id
                ))
            })?;

        info!(subscriber = %subscriber_id, topic = %topic, "✅ 订阅已添加");
        Ok(row.into())
    }

    /// 获取指定主题的所有启用订阅
    pub async fn get_subscribes(&self, topic: &str) -> Result<Vec<SubscribeRecord>> {
        let rows = orm::Entity::find()
            .filter(orm::Column::Topic.eq(topic))
            .filter(orm::Column::Enabled.eq(true))
            .all(self.db.orm())
            .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 删除订阅
    pub async fn delete_subscribe(&self, topic: &str, subscriber_id: &str) -> Result<bool> {
        let result = orm::Entity::delete_many()
            .filter(orm::Column::Topic.eq(topic))
            .filter(orm::Column::SubscriberId.eq(subscriber_id))
            .exec(self.db.orm())
            .await?;

        let deleted = result.rows_affected > 0;
        if deleted {
            info!(topic = %topic, subscriber = %subscriber_id, "🗑️ 订阅已删除");
        }
        Ok(deleted)
    }

    /// 推送消息到所有订阅者
    ///
    /// 返回 (成功数, 失败数)
    pub async fn push(
        &self,
        topic: &str,
        message: &str,
        bot_txs: &DashMap<i64, mpsc::Sender<nuono_app::transport::TransportFrame>>,
    ) -> Result<(usize, usize)> {
        let subscribers = self.get_subscribes(topic).await?;
        let mut success = 0;
        let mut fail = 0;

        for sub in &subscribers {
            let target = sub.target_id.as_deref().unwrap_or(&sub.subscriber_id);
            let api_call = serde_json::json!({
                "action": "send_group_msg",
                "params": {
                    "group_id": target.parse::<i64>().unwrap_or(0),
                    "message": [{"type": "text", "data": {"text": message}}]
                }
            });
            let json_str = serde_json::to_string(&api_call).unwrap_or_default();

            let mut sent = false;
            for entry in bot_txs.iter() {
                if entry
                    .value()
                    .send(nuono_app::transport::TransportFrame::Text(json_str.clone()))
                    .await
                    .is_ok()
                {
                    sent = true;
                    break;
                }
            }
            if sent {
                success += 1;
            } else {
                fail += 1;
                warn!(topic = %topic, subscriber = %sub.subscriber_id, "订阅推送失败：无可用 bot 连接");
            }
        }

        if success > 0 || fail > 0 {
            info!(topic = %topic, success, fail, "📡 订阅推送完成");
        }
        Ok((success, fail))
    }

    /// 列出所有订阅
    pub async fn list_all(&self) -> Result<Vec<SubscribeRecord>> {
        let rows = orm::Entity::find().all(self.db.orm()).await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 切换订阅启用状态
    pub async fn toggle(&self, id: i64, enabled: bool) -> Result<()> {
        let result = orm::Entity::update_many()
            .set(orm::ActiveModel {
                enabled: Set(Some(enabled)),
                ..Default::default()
            })
            .filter(orm::Column::Id.eq(id))
            .exec(self.db.orm())
            .await?;
        if result.rows_affected == 0 {
            return Err(crate::error::NuonoError::NotFound(format!(
                "订阅 {id} 不存在"
            )));
        }
        Ok(())
    }

    /// 获取已注册的主题列表
    pub fn registered_topics(&self) -> Vec<(String, String)> {
        self.registered_topics
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }
}
