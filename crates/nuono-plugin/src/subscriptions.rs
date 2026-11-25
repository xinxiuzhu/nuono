//! 插件订阅：在宿主数据库中保存主题、推送目标和插件自己的附加数据。
//! 不启动线程或调度器；用 `ctx.schedule` 决定何时发送。
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Bot, Database, Event, RuntimeActionReceipt, RuntimeMessageTarget, SendableMessage};

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 一条会话订阅。群聊中同一主题只保存一条，重复订阅更新附加数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub topic: String,
    pub target: RuntimeMessageTarget,
    pub data: Value,
}

impl Subscription {
    /// 严格使用保存的实例和机器人路由。机器人离线时返回错误，不改投其他机器人。
    pub async fn send(
        &self,
        bot: &Bot,
        message: impl Into<SendableMessage>,
    ) -> Result<RuntimeActionReceipt> {
        Ok(bot.send_routed(&self.target, message).await?)
    }

    pub fn data_as<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_value(self.data.clone())?)
    }
}

/// 轻量的插件订阅仓库，复用宿主数据库及插件 KV 的原子 upsert。
/// 使用插件名下的独立集合，不受 `ctx.kv().clear()` 影响。没有额外连接池。
#[derive(Debug, Clone)]
pub struct Subscriptions {
    db: Database,
    plugin_id: String,
}

impl Subscriptions {
    /// 通常通过 `ctx.subscriptions()` 获取；显式构造可用于测试。
    pub fn new(db: Database, plugin: &str) -> Self {
        Self {
            db,
            plugin_id: plugin.to_owned(),
        }
    }

    fn collection(&self) -> Result<crate::db::PluginStateCollection> {
        Ok(crate::db::PluginStateCollection::new(
            &self.plugin_id,
            "subscriptions_v1",
            crate::db::PluginStateScope::global(),
        )?)
    }

    fn key(topic: &str, target: &RuntimeMessageTarget) -> Result<String> {
        ensure!(!topic.trim().is_empty(), "订阅主题不能为空");
        target.validate()?;
        // plugin_kv.key 最多 200 个字符。对完整身份摘要，长 openid 也不截断。
        let identity = serde_json::to_vec(&(topic, target))?;
        Ok(digest(&identity))
    }

    /// 为当前群聊、私聊或频道订阅。权限由指令的 permission 声明控制。
    pub async fn subscribe(
        &self,
        topic: &str,
        event: &Event<'_>,
        data: impl Serialize,
    ) -> Result<Subscription> {
        self.set(topic, event.message_target()?, data).await
    }

    /// 显式指定目标，可用于用户私聊订阅等业务。不会推断或补全机器人身份。
    pub async fn set(
        &self,
        topic: &str,
        target: RuntimeMessageTarget,
        data: impl Serialize,
    ) -> Result<Subscription> {
        let key = Self::key(topic, &target)?;
        let record = Subscription {
            topic: topic.to_string(),
            target,
            data: serde_json::to_value(data)?,
        };
        self.db
            .state_set(
                &self.collection()?,
                &key,
                1,
                serde_json::to_string(&record)?,
            )
            .await?;
        Ok(record)
    }

    pub async fn unsubscribe(&self, topic: &str, event: &Event<'_>) -> Result<bool> {
        self.remove(topic, &event.message_target()?).await
    }

    pub async fn remove(&self, topic: &str, target: &RuntimeMessageTarget) -> Result<bool> {
        Ok(self
            .db
            .state_delete(&self.collection()?, &Self::key(topic, target)?)
            .await?)
    }

    /// 获取一个主题的订阅。损坏记录会明确报错，不悄悄丢弃。
    /// 取消订阅不会撤销调用方已经取出的记录或已发送的请求。
    pub async fn list(&self, topic: &str) -> Result<Vec<Subscription>> {
        ensure!(!topic.trim().is_empty(), "订阅主题不能为空");
        let mut records = Vec::new();
        let collection = self.collection()?;
        let mut after = None;
        loop {
            let page = self
                .db
                .state_list(&collection, after.as_deref(), 100)
                .await?;
            let complete = page.len() < 100;
            for (key, value) in page {
                ensure!(value.value_version == 1, "订阅记录版本不受支持");
                let record: Subscription =
                    serde_json::from_value(value.value).context("订阅记录解析失败")?;
                ensure!(
                    Self::key(&record.topic, &record.target)? == key,
                    "订阅记录身份不一致"
                );
                after = Some(key);
                if record.topic == topic {
                    records.push(record);
                }
            }
            if complete {
                break;
            }
        }
        Ok(records)
    }
}
