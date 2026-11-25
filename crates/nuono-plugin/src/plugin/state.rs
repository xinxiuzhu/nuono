//! 普通插件运行状态的宿主管理入口。

use crate::db::{Database, PluginStateCollection};
use crate::error::Result;
use serde::Serialize;

pub use crate::db::{PluginStateScope, PluginStateValue};

/// 与一个插件、集合和作用域绑定的共享状态存取器。
#[derive(Debug, Clone)]
pub struct PluginStateStore {
    db: Database,
    collection: PluginStateCollection,
}

impl PluginStateStore {
    pub(crate) fn new(db: Database, collection: PluginStateCollection) -> Self {
        Self { db, collection }
    }

    /// 读取状态及其值格式版本，缺失时返回 `None`。
    ///
    /// # Errors
    /// 键无效、记录损坏或存储查询失败时返回错误。
    pub async fn get(&self, key: &str) -> Result<Option<PluginStateValue>> {
        self.db.state_get(&self.collection, key).await
    }

    /// 原子替换一条状态的完整值及格式版本。
    ///
    /// # Errors
    /// 键、版本、JSON 编码或大小不符合约定，或存储写入失败时返回错误。
    pub async fn set<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value_version: i32,
        value: &T,
    ) -> Result<()> {
        let value_json = serde_json::to_string(value)?;
        self.db
            .state_set(&self.collection, key, value_version, value_json)
            .await
    }

    /// 删除一条状态，并返回它之前是否存在。
    ///
    /// # Errors
    /// 键无效或存储写入失败时返回错误。
    pub async fn delete(&self, key: &str) -> Result<bool> {
        self.db.state_delete(&self.collection, key).await
    }

    /// 按键升序取一页。用上一页最后一个键作为下一页的 `after`。
    ///
    /// # Errors
    /// 游标无效、条数超出 1–100、记录损坏或存储查询失败时返回错误。
    pub async fn list(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, PluginStateValue)>> {
        self.db.state_list(&self.collection, after, limit).await
    }
}
