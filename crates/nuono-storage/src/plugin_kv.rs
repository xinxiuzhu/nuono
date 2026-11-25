use crate::db::Database;
use crate::entity::plugin_kv;
use crate::error::Result;
use nuono_app::plugin_data::{
    PluginStateCollection, PluginStateRecord, PluginStateStamp, PluginStateValue,
};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect,
    Statement,
};
use serde::{Deserialize, Serialize};

pub(crate) const NAMESPACE_COLLECTION: &str = "_kv";

pub(crate) struct RecordSet<'a, C: sea_orm::ConnectionTrait> {
    connection: &'a C,
    plugin_id: &'a str,
    collection: &'a str,
    scope_key: &'a str,
}

impl<'a, C: sea_orm::ConnectionTrait> RecordSet<'a, C> {
    pub(crate) fn managed(connection: &'a C, collection: &'a PluginStateCollection) -> Self {
        Self {
            connection,
            plugin_id: collection.plugin_id(),
            collection: collection.name(),
            scope_key: collection.scope_key(),
        }
    }

    fn key(&self, key: &str) -> (String, String, String, String) {
        (
            self.plugin_id.into(),
            self.collection.into(),
            self.scope_key.into(),
            key.into(),
        )
    }

    fn scope(&self) -> Condition {
        Condition::all()
            .add(plugin_kv::Column::PluginId.eq(self.plugin_id))
            .add(plugin_kv::Column::Collection.eq(self.collection))
            .add(plugin_kv::Column::ScopeKey.eq(self.scope_key))
    }

    pub(crate) async fn find(&self, key: &str) -> Result<Option<plugin_kv::Model>> {
        Ok(plugin_kv::Entity::find_by_id(self.key(key))
            .filter(live(crate::clock::db_now().timestamp_millis()))
            .one(self.connection)
            .await?)
    }

    pub(crate) async fn save(&self, key: &str, value_json: String, version: i32) -> Result<()> {
        if value_json.len() > 64 * 1024 || version <= 0 {
            return Err(crate::error::NuonoError::Validation(
                "插件状态超过大小限制或版本无效".into(),
            ));
        }
        nuono_domain::scalar::parse_json(&value_json)?;
        self.lock_account().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let backend = self.connection.get_database_backend();
        // 仅当内容、格式版本或过期时间发生实质变化时才更新，避免同值重复写造成空转刷盘与修订号膨胀；
        // PostgreSQL 使用 IS DISTINCT FROM、SQLite 使用 IS NOT，保证可空列 expires_at_ms 在 NULL 与非空切换时不漏更新。
        let sql = if backend == sea_orm::DbBackend::Postgres {
            "INSERT INTO nuono_plugin_kv(plugin_id,collection,scope_key,record_key,value_json,value_version,revision,created_at_ms,updated_at_ms,generation,expires_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,1,$7,$7,$8,NULL) \
             ON CONFLICT(plugin_id,collection,scope_key,record_key) DO UPDATE \
             SET value_json=excluded.value_json,value_version=excluded.value_version,revision=nuono_plugin_kv.revision+1,updated_at_ms=excluded.updated_at_ms,expires_at_ms=NULL \
             WHERE nuono_plugin_kv.value_json IS DISTINCT FROM excluded.value_json \
                OR nuono_plugin_kv.value_version IS DISTINCT FROM excluded.value_version \
                OR nuono_plugin_kv.expires_at_ms IS DISTINCT FROM excluded.expires_at_ms"
        } else {
            "INSERT INTO nuono_plugin_kv(plugin_id,collection,scope_key,record_key,value_json,value_version,revision,created_at_ms,updated_at_ms,generation,expires_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,1,$7,$7,$8,NULL) \
             ON CONFLICT(plugin_id,collection,scope_key,record_key) DO UPDATE \
             SET value_json=excluded.value_json,value_version=excluded.value_version,revision=nuono_plugin_kv.revision+1,updated_at_ms=excluded.updated_at_ms,expires_at_ms=NULL \
             WHERE nuono_plugin_kv.value_json IS NOT excluded.value_json \
                OR nuono_plugin_kv.value_version IS NOT excluded.value_version \
                OR nuono_plugin_kv.expires_at_ms IS NOT excluded.expires_at_ms"
        };
        self.connection
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                backend,
                sql,
                [
                    self.plugin_id.into(),
                    self.collection.into(),
                    self.scope_key.into(),
                    key.into(),
                    value_json.into(),
                    i64::from(version).into(),
                    now.into(),
                    uuid::Uuid::new_v4().to_string().into(),
                ],
            ))
            .await?;
        Ok(())
    }

    pub(crate) async fn create(
        &self,
        key: &str,
        version: i32,
        value: serde_json::Value,
        expires: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.lock_account().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let encoded = validate_write(version, &value, expires, now)?;
        let generation = uuid::Uuid::new_v4().to_string();
        let row=self.connection.query_one_raw(Statement::from_sql_and_values(self.connection.get_database_backend(),
            "INSERT INTO nuono_plugin_kv(plugin_id,collection,scope_key,record_key,value_json,value_version,generation,revision,created_at_ms,updated_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,1,$8,$8,$9) ON CONFLICT(plugin_id,collection,scope_key,record_key) DO UPDATE SET value_json=excluded.value_json,value_version=excluded.value_version,generation=excluded.generation,revision=1,created_at_ms=excluded.created_at_ms,updated_at_ms=excluded.updated_at_ms,expires_at_ms=excluded.expires_at_ms WHERE nuono_plugin_kv.expires_at_ms IS NOT NULL AND nuono_plugin_kv.expires_at_ms<=$8 RETURNING *",
            [self.plugin_id.into(),self.collection.into(),self.scope_key.into(),key.into(),encoded.into(),version.into(),generation.into(),now.into(),expires.into()])).await?;
        row.map(|row| record(&plugin_kv::Model::from_query_result(&row, "")?))
            .transpose()
    }
    pub(crate) async fn compare_exchange(
        &self,
        key: &str,
        expected: &PluginStateStamp,
        version: i32,
        value: serde_json::Value,
        expires: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.lock_account().await?;
        validate_stamp(expected)?;
        let now = crate::clock::db_now().timestamp_millis();
        let encoded = validate_write(version, &value, expires, now)?;
        let row=self.connection.query_one_raw(Statement::from_sql_and_values(self.connection.get_database_backend(),
            "UPDATE nuono_plugin_kv SET value_json=$1,value_version=$2,revision=revision+1,updated_at_ms=$3,expires_at_ms=$4 WHERE plugin_id=$5 AND collection=$6 AND scope_key=$7 AND record_key=$8 AND generation=$9 AND revision=$10 AND revision<9223372036854775807 AND (expires_at_ms IS NULL OR expires_at_ms>$3) RETURNING *",
            [encoded.into(),version.into(),now.into(),expires.into(),self.plugin_id.into(),self.collection.into(),self.scope_key.into(),key.into(),expected.generation.clone().into(),expected.revision.into()])).await?;
        row.map(|row| record(&plugin_kv::Model::from_query_result(&row, "")?))
            .transpose()
    }
    pub(crate) async fn delete_if(&self, key: &str, expected: &PluginStateStamp) -> Result<bool> {
        self.lock_account().await?;
        validate_stamp(expected)?;
        Ok(plugin_kv::Entity::delete_many()
            .filter(self.scope())
            .filter(plugin_kv::Column::RecordKey.eq(key))
            .filter(plugin_kv::Column::Generation.eq(&expected.generation))
            .filter(plugin_kv::Column::Revision.eq(expected.revision))
            .filter(live(crate::clock::db_now().timestamp_millis()))
            .exec(self.connection)
            .await?
            .rows_affected
            == 1)
    }
    pub(crate) async fn prune_expired(&self, limit: u32) -> Result<u64> {
        if !(1..=1000).contains(&limit) {
            return Err(crate::error::NuonoError::Validation(
                "到期清理每批须为1至1000条".into(),
            ));
        }
        self.lock_account().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let rows = plugin_kv::Entity::find()
            .filter(self.scope())
            .filter(plugin_kv::Column::ExpiresAtMs.lte(now))
            .order_by_asc(plugin_kv::Column::RecordKey)
            .limit(u64::from(limit))
            .all(self.connection)
            .await?;
        if rows.is_empty() {
            return Ok(0);
        }
        Ok(plugin_kv::Entity::delete_many()
            .filter(self.scope())
            .filter(plugin_kv::Column::ExpiresAtMs.lte(now))
            .filter(plugin_kv::Column::RecordKey.is_in(rows.into_iter().map(|r| r.record_key)))
            .exec(self.connection)
            .await?
            .rows_affected)
    }
    async fn lock_account(&self) -> Result<()> {
        if let Some(id) = self.scope_key.strip_prefix("account:") {
            let id = id
                .parse::<i64>()
                .map_err(|_| crate::error::NuonoError::Validation("账号状态范围无效".into()))?;
            let sql = if self.connection.get_database_backend() == sea_orm::DbBackend::Postgres {
                "SELECT id FROM nuono_users WHERE id=$1 AND status='active' FOR UPDATE"
            } else {
                "SELECT id FROM nuono_users WHERE id=$1 AND status='active'"
            };
            if self
                .connection
                .query_one_raw(sea_orm::Statement::from_sql_and_values(
                    self.connection.get_database_backend(),
                    sql,
                    [id.into()],
                ))
                .await?
                .is_none()
            {
                return Err(crate::error::NuonoError::PermissionDenied(
                    "账号已合并、注销或不可用".into(),
                ));
            }
        }
        Ok(())
    }
    pub(crate) async fn delete(&self, key: &str) -> Result<bool> {
        self.lock_account().await?;
        let result = plugin_kv::Entity::delete_by_id(self.key(key))
            .exec(self.connection)
            .await?;
        Ok(result.rows_affected > 0)
    }

    pub(crate) async fn list(
        &self,
        after: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Vec<plugin_kv::Model>> {
        let mut query = plugin_kv::Entity::find()
            .filter(self.scope())
            .filter(live(crate::clock::db_now().timestamp_millis()))
            .order_by_asc(plugin_kv::Column::RecordKey)
            .limit(limit);
        if let Some(after) = after {
            query = query.filter(plugin_kv::Column::RecordKey.gt(after));
        }
        Ok(query.all(self.connection).await?)
    }
}

pub(crate) fn live(now: i64) -> Condition {
    Condition::any()
        .add(plugin_kv::Column::ExpiresAtMs.is_null())
        .add(plugin_kv::Column::ExpiresAtMs.gt(now))
}
fn validate_stamp(stamp: &PluginStateStamp) -> Result<()> {
    if stamp.revision <= 0 || uuid::Uuid::parse_str(&stamp.generation).is_err() {
        return Err(crate::error::NuonoError::Validation(
            "状态版本标记无效".into(),
        ));
    }
    Ok(())
}
fn validate_write(
    version: i32,
    value: &serde_json::Value,
    expires: Option<i64>,
    now: i64,
) -> Result<String> {
    let raw = serde_json::to_string(value)?;
    if version <= 0 || raw.len() > 65536 || expires.is_some_and(|v| v <= now) {
        return Err(crate::error::NuonoError::Validation(
            "状态格式版本、大小或到期时间无效".into(),
        ));
    }
    nuono_domain::scalar::parse_json(&raw)?;
    Ok(raw)
}
pub(crate) fn record(row: &plugin_kv::Model) -> Result<PluginStateRecord> {
    Ok(PluginStateRecord {
        value_version: row.value_version,
        value: nuono_domain::scalar::parse_json(&row.value_json)?,
        generation: row.generation.clone(),
        revision: row.revision,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at.timestamp_millis(),
        expires_at_ms: row.expires_at_ms,
    })
}

pub(crate) fn state_value(row: &plugin_kv::Model) -> Result<PluginStateValue> {
    Ok(PluginStateValue {
        value_version: row.value_version,
        value: nuono_domain::scalar::parse_json(&row.value_json)?,
    })
}

/// 插件键值存储
///
/// namespace 必须是已经注册的插件 ID；每个插件独立隔离数据。
/// 提供简单的 KV CRUD，插件无需定义自己的数据库模型。
///
/// # 使用示例
/// 文档测试使用独立的内存 SQLite 数据库；应用中传入宿主提供的数据库连接。
/// ```rust
/// # fn main() -> nuono_storage::error::Result<()> {
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
/// # let db = nuono_storage::Database::from_config(&nuono_storage::config::DatabaseConfig {
/// #     url: "sqlite::memory:".into(), max_connections: 1,
/// # }).await?;
/// use nuono_storage::plugin_kv::PluginKVStore;
///
/// # db.sync_plugin("my_plugin", "1", &serde_json::json!({})).await?;
/// let kv = PluginKVStore::new(db.clone(), "my_plugin");
/// kv.set("greeting", "hello world").await?;
/// let val = kv.get("greeting").await?;
/// assert_eq!(val.as_deref(), Some("hello world"));
/// # db.shutdown().await?;
/// # Ok(())
/// # })
/// # }
/// ```
#[derive(Clone)]
pub struct PluginKVStore {
    db: Database,
    namespace: String,
}

impl PluginKVStore {
    pub fn new(db: Database, namespace: impl Into<String>) -> Self {
        Self {
            db,
            namespace: namespace.into(),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    fn records(&self) -> RecordSet<'_, sea_orm::DatabaseConnection> {
        RecordSet {
            connection: self.db.orm(),
            plugin_id: &self.namespace,
            collection: NAMESPACE_COLLECTION,
            scope_key: "global",
        }
    }

    /// 获取键值
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        self.records()
            .find(key)
            .await?
            .map(|row| serde_json::from_str(&row.value_json).map_err(Into::into))
            .transpose()
    }

    /// 获取并反序列化为指定类型
    pub async fn get_as<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(val) => {
                let parsed: T = serde_json::from_str(&val)
                    .map_err(|e| crate::error::NuonoError::Deserialization(e.to_string()))?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// 设置键值
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        self.records()
            .save(key, serde_json::to_string(value)?, 1)
            .await
    }

    /// 序列化并设置键值
    pub async fn set_as<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_string(value)
            .map_err(|e| crate::error::NuonoError::Serialization(e.to_string()))?;
        self.set(key, &json).await
    }

    /// 删除键值
    pub async fn delete(&self, key: &str) -> Result<bool> {
        self.records().delete(key).await
    }

    /// 列出所有键值
    pub async fn list(&self) -> Result<Vec<(String, String)>> {
        self.records()
            .list(None, None)
            .await?
            .into_iter()
            .map(|row| Ok((row.record_key, serde_json::from_str(&row.value_json)?)))
            .collect()
    }

    /// 列出所有键值并反序列化
    pub async fn list_as<T: for<'de> Deserialize<'de>>(&self) -> Result<Vec<(String, T)>> {
        let pairs = self.list().await?;
        let mut result = Vec::new();
        for (key, val) in pairs {
            if let Ok(parsed) = serde_json::from_str::<T>(&val) {
                result.push((key, parsed));
            }
        }
        Ok(result)
    }

    /// 检查键是否存在
    pub async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.records().find(key).await?.is_some())
    }

    /// 清空此命名空间的所有数据
    pub async fn clear(&self) -> Result<u64> {
        Ok(plugin_kv::Entity::delete_many()
            .filter(self.records().scope())
            .exec(self.db.orm())
            .await?
            .rows_affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn namespaces_isolate_upserts_deletes_and_clear() {
        let db = crate::test_support::test_db(3).await;
        for plugin in ["first", "second"] {
            db.sync_plugin(plugin, "1", &serde_json::json!({}))
                .await
                .unwrap();
        }
        let first = PluginKVStore::new(db.clone(), "first");
        let second = PluginKVStore::new(db.clone(), "second");
        assert_eq!(first.get("key").await.unwrap(), None);
        first.set("key", "old").await.unwrap();
        first.set("key", "new").await.unwrap();
        second.set("key", "separate").await.unwrap();
        assert_eq!(first.get("key").await.unwrap().as_deref(), Some("new"));
        assert_eq!(
            second.get("key").await.unwrap().as_deref(),
            Some("separate")
        );
        assert_eq!(
            first.list().await.unwrap(),
            vec![("key".into(), "new".into())]
        );
        assert!(first.exists("key").await.unwrap());
        assert!(first.delete("key").await.unwrap());
        assert!(!first.delete("key").await.unwrap());
        assert!(!first.exists("key").await.unwrap());
        first.set_as("json", &vec![1, 2]).await.unwrap();
        assert_eq!(
            first.get_as::<Vec<i32>>("json").await.unwrap(),
            Some(vec![1, 2])
        );
        assert_eq!(first.clear().await.unwrap(), 1);
        assert_eq!(
            second.get("key").await.unwrap().as_deref(),
            Some("separate")
        );
        db.shutdown().await.unwrap();
    }
}

impl Database {
    /// 宿主定时执行的插件过期 KV 状态分批清理（每条记录在执行删除时都会带原 revision 与 expires_at_ms 条件二次校验，避免误删期间已续期或重建的新数据）。
    pub async fn delete_expired_plugin_state(&self, limit: u32) -> Result<u64> {
        if !(1..=1000).contains(&limit) {
            return Err(crate::error::NuonoError::Validation(
                "状态清理批次无效".into(),
            ));
        }
        // 先在事务外做只读探测，库中无过期记录时直接返回，避免定时任务空抢 SQLite 写锁。
        let probe_now = crate::clock::db_now().timestamp_millis();
        let has_expired = plugin_kv::Entity::find()
            .filter(plugin_kv::Column::ExpiresAtMs.lte(probe_now))
            .limit(1)
            .one(self.orm())
            .await?
            .is_some();
        if !has_expired {
            return Ok(0);
        }
        let tx = self.begin_write().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let rows = plugin_kv::Entity::find()
            .filter(plugin_kv::Column::ExpiresAtMs.lte(now))
            .order_by_asc(plugin_kv::Column::ExpiresAtMs)
            .order_by_asc(plugin_kv::Column::PluginId)
            .order_by_asc(plugin_kv::Column::Collection)
            .order_by_asc(plugin_kv::Column::ScopeKey)
            .order_by_asc(plugin_kv::Column::RecordKey)
            .limit(u64::from(limit))
            .all(&tx)
            .await?;
        let mut count = 0;
        for row in rows {
            count += plugin_kv::Entity::delete_many()
                .filter(plugin_kv::Column::PluginId.eq(row.plugin_id))
                .filter(plugin_kv::Column::Collection.eq(row.collection))
                .filter(plugin_kv::Column::ScopeKey.eq(row.scope_key))
                .filter(plugin_kv::Column::RecordKey.eq(row.record_key))
                .filter(plugin_kv::Column::Generation.eq(row.generation))
                .filter(plugin_kv::Column::Revision.eq(row.revision))
                .filter(plugin_kv::Column::ExpiresAtMs.lte(now))
                .exec(&tx)
                .await?
                .rows_affected;
        }
        tx.commit().await?;
        Ok(count)
    }

    /// 列出属于指定规范群的全部插件定时推送订阅（来自插件 SDK `ctx.subscriptions()` 写入的 `subscriptions_v1`）。
    pub async fn list_group_plugin_subscriptions(
        &self,
        group: &crate::groups::Group,
    ) -> Result<Vec<GroupPluginSubscription>> {
        let now = crate::clock::db_now().timestamp_millis();
        let rows = plugin_kv::Entity::find()
            .filter(plugin_kv::Column::Collection.eq(PLUGIN_SUBSCRIPTIONS_COLLECTION))
            .filter(plugin_kv::Column::ScopeKey.eq("global"))
            .filter(live(now))
            .order_by_asc(plugin_kv::Column::PluginId)
            .order_by_asc(plugin_kv::Column::RecordKey)
            .all(self.orm())
            .await?;
        let mut items = Vec::new();
        for row in rows {
            if let Some(sub) = parse_group_plugin_subscription(&row, group) {
                items.push(sub);
            }
        }
        Ok(items)
    }

    /// 删除属于指定规范群的一条插件推送订阅。
    ///
    /// 仅当目标记录确实指向该群时才执行删除，防止跨群越权删除。
    pub async fn delete_group_plugin_subscription(
        &self,
        group: &crate::groups::Group,
        plugin_id: &str,
        key: &str,
    ) -> Result<bool> {
        let tx = self.begin_write().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let Some(row) = plugin_kv::Entity::find_by_id((
            plugin_id.to_owned(),
            PLUGIN_SUBSCRIPTIONS_COLLECTION.to_owned(),
            "global".to_owned(),
            key.to_owned(),
        ))
        .filter(live(now))
        .one(&tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };
        if parse_group_plugin_subscription(&row, group).is_none() {
            tx.rollback().await?;
            return Ok(false);
        }
        let affected = plugin_kv::Entity::delete_many()
            .filter(plugin_kv::Column::PluginId.eq(plugin_id))
            .filter(plugin_kv::Column::Collection.eq(PLUGIN_SUBSCRIPTIONS_COLLECTION))
            .filter(plugin_kv::Column::ScopeKey.eq("global"))
            .filter(plugin_kv::Column::RecordKey.eq(key))
            .exec(&tx)
            .await?
            .rows_affected;
        tx.commit().await?;
        Ok(affected > 0)
    }
}

const PLUGIN_SUBSCRIPTIONS_COLLECTION: &str = "subscriptions_v1";

/// 群维度下的一条插件定时推送订阅投影（由插件通过 `ctx.subscriptions()` 写入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupPluginSubscription {
    pub plugin_id: String,
    pub key: String,
    pub topic: String,
    pub platform: String,
    pub instance_id: String,
    pub bot_id: String,
    pub data: serde_json::Value,
    pub updated_at_ms: i64,
}

fn parse_group_plugin_subscription(
    row: &plugin_kv::Model,
    group: &crate::groups::Group,
) -> Option<GroupPluginSubscription> {
    let val: serde_json::Value = serde_json::from_str(&row.value_json).ok()?;
    let topic = val.get("topic")?.as_str()?.to_owned();
    let target = val.get("target")?;
    let route = target.get("route")?;
    let platform = route.get("platform")?.as_str()?;
    if !platform.eq_ignore_ascii_case(&group.platform) {
        return None;
    }
    let kind = target.get("kind")?;
    let matches_target = match kind.get("type").and_then(serde_json::Value::as_str) {
        Some("group") => {
            kind.get("group_id").and_then(serde_json::Value::as_str)
                == Some(group.native_group_id.as_str())
        }
        Some("channel") => {
            let same_channel = kind.get("channel_id").and_then(serde_json::Value::as_str)
                == Some(group.native_group_id.as_str());
            let same_parent = if group.native_parent_id.is_empty() {
                true
            } else {
                kind.get("parent").and_then(serde_json::Value::as_str)
                    == Some(group.native_parent_id.as_str())
            };
            same_channel && same_parent
        }
        _ => false,
    };
    if !matches_target {
        return None;
    }
    Some(GroupPluginSubscription {
        plugin_id: row.plugin_id.clone(),
        key: row.record_key.clone(),
        topic,
        platform: platform.to_owned(),
        instance_id: route
            .get("instance_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        bot_id: route
            .get("bot_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        data: val.get("data").cloned().unwrap_or(serde_json::Value::Null),
        updated_at_ms: row.updated_at.timestamp_millis(),
    })
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use nuono_app::plugin_data::{PluginDataStore, PluginStateScope};
    use sea_orm::ConnectionTrait;
    use serde_json::json;
    async fn fixture() -> (Database, PluginStateCollection) {
        let db = crate::test_support::test_db(4).await;
        db.sync_plugin("atomic_probe", "1", &json!({}))
            .await
            .unwrap();
        let collection =
            PluginStateCollection::new("atomic_probe", "tickets", PluginStateScope::global())
                .unwrap();
        (db, collection)
    }
    #[tokio::test]
    async fn atomic_creation_has_one_winner_and_stale_stamps_cannot_touch_recreated_keys() {
        let (db, collection) = fixture().await;
        let mut tasks = Vec::new();
        for owner in 0..12 {
            let db = db.clone();
            let collection = collection.clone();
            tasks.push(tokio::spawn(async move {
                db.state_create(&collection, "lease", 1, json!({"owner":owner}), None)
                    .await
                    .unwrap()
            }));
        }
        let mut winners = Vec::new();
        for task in tasks {
            if let Some(row) = task.await.unwrap() {
                winners.push(row);
            }
        }
        assert_eq!(winners.len(), 1);
        let first = winners.pop().unwrap();
        let next = db
            .state_compare_exchange(
                &collection,
                "lease",
                &first.stamp(),
                1,
                json!({"owner":"renewed"}),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.generation, next.generation);
        assert_eq!(next.revision, first.revision + 1);
        assert!(
            db.state_compare_exchange(&collection, "lease", &first.stamp(), 1, json!({}), None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !db.state_delete_if(&collection, "lease", &first.stamp())
                .await
                .unwrap()
        );
        assert!(
            db.state_delete_if(&collection, "lease", &next.stamp())
                .await
                .unwrap()
        );
        let replacement = db
            .state_create(&collection, "lease", 1, json!("new"), None)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(replacement.generation, first.generation);
        assert!(
            !db.state_delete_if(&collection, "lease", &first.stamp())
                .await
                .unwrap()
        );
        assert!(
            db.state_compare_exchange(
                &collection,
                "lease",
                &first.stamp(),
                1,
                json!("stale"),
                None
            )
            .await
            .unwrap()
            .is_none()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn expiry_reads_as_absent_and_replacement_cannot_be_pruned_by_old_generation() {
        let (db, collection) = fixture().await;
        let old = db
            .state_create(&collection, "expired", 1, json!("old"), None)
            .await
            .unwrap()
            .unwrap();
        let now = crate::clock::db_now().timestamp_millis();
        db.orm().execute_raw(Statement::from_sql_and_values(db.backend(),"UPDATE nuono_plugin_kv SET created_at_ms=$1,updated_at_ms=$1,expires_at_ms=$2 WHERE plugin_id='atomic_probe' AND record_key='expired'",[(now-2000).into(),(now-1000).into()])).await.unwrap();
        assert!(
            db.state_record(&collection, "expired")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.state_get(&collection, "expired")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.state_list(&collection, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.state_compare_exchange(
                &collection,
                "expired",
                &old.stamp(),
                1,
                json!("too late"),
                None
            )
            .await
            .unwrap()
            .is_none()
        );
        let new = db
            .state_create(&collection, "expired", 1, json!("new"), Some(now + 60_000))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(new.generation, old.generation);
        assert_eq!(db.state_prune_expired(&collection, 10).await.unwrap(), 0);
        assert!(
            !db.state_delete_if(&collection, "expired", &old.stamp())
                .await
                .unwrap()
        );
        db.orm().execute_raw(Statement::from_sql_and_values(db.backend(),"UPDATE nuono_plugin_kv SET created_at_ms=$1,updated_at_ms=$1,expires_at_ms=$2 WHERE plugin_id='atomic_probe'",[(now-2000).into(),(now-1000).into()])).await.unwrap();
        assert_eq!(db.delete_expired_plugin_state(1).await.unwrap(), 1);
        assert!(
            db.state_create(&collection, "invalid_expiry", 1, json!({}), Some(now - 1))
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn transactional_state_rolls_back_and_keeps_collections_isolated() {
        let (db, collection) = fixture().await;
        let tx = db.begin_for_plugin("atomic_probe").await.unwrap();
        let row = tx
            .state_create(&collection, "receipt", 1, json!({"done":true}), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tx.state_record(&collection, "receipt")
                .await
                .unwrap()
                .unwrap()
                .stamp(),
            row.stamp()
        );
        tx.rollback().await.unwrap();
        assert!(
            db.state_record(&collection, "receipt")
                .await
                .unwrap()
                .is_none()
        );
        let tx = db.begin_for_plugin("atomic_probe").await.unwrap();
        let other =
            PluginStateCollection::new("another_plugin", "tickets", PluginStateScope::global())
                .unwrap();
        assert!(
            tx.state_create(&other, "receipt", 1, json!({}), None)
                .await
                .is_err()
        );
        assert!(
            tx.state_create(&collection, "oversized", 1, json!("中".repeat(65536)), None)
                .await
                .is_err()
        );
        tx.state_create(&collection, "receipt", 1, json!({"done":true}), None)
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert!(
            db.state_record(&collection, "receipt")
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn save_skips_noop_writes_and_updates_when_value_version_or_expiry_changes() {
        let (db, collection) = fixture().await;
        assert_eq!(db.delete_expired_plugin_state(100).await.unwrap(), 0);

        let payload = serde_json::to_string(&json!({"theme": "dark"})).unwrap();
        db.state_set(&collection, "prefs", 1, payload.clone())
            .await
            .unwrap();
        let first = db
            .state_record(&collection, "prefs")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.expires_at_ms, None);

        // 将 updated_at_ms 回拨 5 秒后重复写入相同内容与版本，验证不触发空写（revision 与 updated_at_ms 保持不变）。
        let old_ts = first.updated_at_ms - 5_000;
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "UPDATE nuono_plugin_kv SET updated_at_ms=$1 WHERE plugin_id='atomic_probe' AND record_key='prefs'",
                [old_ts.into()],
            ))
            .await
            .unwrap();
        db.state_set(&collection, "prefs", 1, payload.clone())
            .await
            .unwrap();
        let after_noop = db
            .state_record(&collection, "prefs")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_noop.revision, 1);
        assert_eq!(after_noop.updated_at_ms, old_ts);

        // 格式版本变化时应正常更新并递增 revision。
        db.state_set(&collection, "prefs", 2, payload.clone())
            .await
            .unwrap();
        let after_ver = db
            .state_record(&collection, "prefs")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_ver.revision, 2);
        assert_eq!(after_ver.value_version, 2);

        // 带 expires_at_ms 的记录，即使以相同内容与版本调用 state_set，也必须因可空列从非空变为 NULL 而触发更新。
        let now = crate::clock::db_now().timestamp_millis();
        let with_ttl = db
            .state_create(
                &collection,
                "ttl_key",
                1,
                json!({"theme": "dark"}),
                Some(now + 60_000),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with_ttl.revision, 1);
        assert!(with_ttl.expires_at_ms.is_some());

        db.state_set(&collection, "ttl_key", 1, payload.clone())
            .await
            .unwrap();
        let cleared_ttl = db
            .state_record(&collection, "ttl_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared_ttl.revision, 2);
        assert_eq!(cleared_ttl.expires_at_ms, None);

        // 过期时间清为 NULL 后再以相同内容与版本写入，应跳过空写。
        db.state_set(&collection, "ttl_key", 1, payload)
            .await
            .unwrap();
        let still_same = db
            .state_record(&collection, "ttl_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_same.revision, 2);

        db.shutdown().await.unwrap();
    }
}
