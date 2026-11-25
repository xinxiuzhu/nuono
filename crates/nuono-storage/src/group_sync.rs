//! 群组同步与群内机器人归属持久化（只维护群显示资料和机器人存在关系，不授予账号权限）。
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::{
    Database,
    error::{NuonoError, Result},
    groups::GroupIdentity,
};
use sea_orm::{ConnectionTrait, Statement};

/// 群组内已记录的机器人条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBotRow {
    pub group_id: i64,
    pub platform: String,
    pub bot_id: String,
    pub synced_at_ms: i64,
}

impl Database {
    /// 同步一条 OneBot 群记录，返回内部群对象 ID 以及是否为新建记录。
    pub async fn upsert_synced_group(&self, g: &serde_json::Value) -> Result<(i64, bool)> {
        let native = g
            .get("group_id")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .ok_or_else(|| NuonoError::InvalidInput("平台返回的群号无效".into()))?;
        let number = native
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0 && n.to_string() == native)
            .ok_or_else(|| NuonoError::InvalidInput("OneBot 群号必须为规范正整数".into()))?;
        let identity = GroupIdentity::onebot(&number.to_string());
        let existed = self.group_for_onebot(&native).await?.is_some();
        let name = g
            .get("group_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let group = self.ensure_group(&identity, name).await?;
        // 平台返回非空群名时才覆盖，避免协议端偶发空名把已保存的名称冲掉。
        if !name.is_empty() && group.display_name != name {
            self.orm()
                .execute_raw(Statement::from_sql_and_values(
                    self.backend(),
                    "UPDATE nuono_groups SET display_name=$1,updated_at_ms=$2,revision=revision+1 WHERE id=$3",
                    [
                        name.into(),
                        chrono::Utc::now().timestamp_millis().into(),
                        group.id.into(),
                    ],
                ))
                .await?;
        }
        Ok((group.id, !existed))
    }

    /// 幂等记录某机器人存在于指定群中（供入站群事件热路径使用，已存在时不重复写盘）。
    pub async fn record_group_bot(
        &self,
        group_id: i64,
        platform: &str,
        bot_id: &str,
    ) -> Result<()> {
        let platform = platform.trim();
        let bot_id = bot_id.trim();
        if platform.is_empty() || bot_id.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp_millis();
        self.orm()
            .execute_raw(Statement::from_sql_and_values(
                self.backend(),
                "INSERT INTO nuono_group_bots (group_id, platform, bot_id, synced_at_ms) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (group_id, platform, bot_id) DO NOTHING",
                [group_id.into(), platform.into(), bot_id.into(), now.into()],
            ))
            .await?;
        Ok(())
    }

    /// 按一次全量群列表同步结果覆盖某个机器人的在群集合（补齐新入群并清理已退群的关联）。
    pub async fn replace_bot_groups(
        &self,
        platform: &str,
        bot_id: &str,
        group_ids: &[i64],
    ) -> Result<()> {
        let platform = platform.trim();
        let bot_id = bot_id.trim();
        if platform.is_empty() || bot_id.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp_millis();
        let target: BTreeSet<i64> = group_ids.iter().copied().collect();
        let tx = self.begin_write().await?;
        let backend = self.backend();
        let existing_rows = tx
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                "SELECT group_id FROM nuono_group_bots WHERE platform=$1 AND bot_id=$2",
                [platform.into(), bot_id.into()],
            ))
            .await?;
        let mut stale = Vec::new();
        for row in existing_rows {
            let group_id: i64 = row.try_get("", "group_id")?;
            if !target.contains(&group_id) {
                stale.push(group_id);
            }
        }
        for group_id in stale {
            tx.execute_raw(Statement::from_sql_and_values(
                backend,
                "DELETE FROM nuono_group_bots WHERE group_id=$1 AND platform=$2 AND bot_id=$3",
                [group_id.into(), platform.into(), bot_id.into()],
            ))
            .await?;
        }
        for group_id in target {
            tx.execute_raw(Statement::from_sql_and_values(
                backend,
                "INSERT INTO nuono_group_bots (group_id, platform, bot_id, synced_at_ms) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (group_id, platform, bot_id) \
                 DO UPDATE SET synced_at_ms = excluded.synced_at_ms",
                [group_id.into(), platform.into(), bot_id.into(), now.into()],
            ))
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 读取全部群组与机器人的存在关系。
    pub async fn all_group_bots(&self) -> Result<Vec<GroupBotRow>> {
        let rows = self
            .orm()
            .query_all_raw(Statement::from_string(
                self.backend(),
                "SELECT group_id, platform, bot_id, synced_at_ms \
                 FROM nuono_group_bots \
                 ORDER BY group_id, platform, bot_id",
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(GroupBotRow {
                    group_id: row.try_get("", "group_id")?,
                    platform: row.try_get("", "platform")?,
                    bot_id: row.try_get("", "bot_id")?,
                    synced_at_ms: row.try_get("", "synced_at_ms")?,
                })
            })
            .collect()
    }

    /// 按内部群 ID 批量汇总各群内的机器人记录。
    pub async fn list_group_bots(
        &self,
        group_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<GroupBotRow>>> {
        if group_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let wanted: HashSet<i64> = group_ids.iter().copied().collect();
        let mut grouped: HashMap<i64, Vec<GroupBotRow>> = HashMap::new();
        for row in self.all_group_bots().await? {
            if wanted.contains(&row.group_id) {
                grouped.entry(row.group_id).or_default().push(row);
            }
        }
        Ok(grouped)
    }

    /// 查询指定账号已验证的平台身份（platform, namespace, native_user_id）。
    pub async fn verified_user_platform_identities(
        &self,
        user_id: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = self
            .orm()
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT platform, namespace, native_user_id \
                 FROM nuono_platform_identities \
                 WHERE user_id=$1 AND verified_at_ms IS NOT NULL \
                 ORDER BY id",
                [user_id.into()],
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("", "platform")?,
                    row.try_get("", "namespace")?,
                    row.try_get("", "native_user_id")?,
                ))
            })
            .collect()
    }
}
