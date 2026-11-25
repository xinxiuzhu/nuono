use dashmap::DashMap;
use std::time::Instant;

/// 命令/触发器使用统计
///
/// 对应 gsuid_core 的 count_data 功能。
/// 跟踪各平台、群组、用户的指令调用频次。
#[derive(Debug, Clone)]
pub struct PlatformStats {
    /// 接收消息数
    pub receive: u64,
    /// 发送消息数
    pub send: u64,
    /// 执行命令数
    pub command: u64,
    /// 各群组的命令统计: group_id -> { command_name -> count }
    pub groups: DashMap<String, DashMap<String, u64>>,
    /// 各用户的命令统计: user_id -> { command_name -> count }
    pub users: DashMap<String, DashMap<String, u64>>,
}

impl Default for PlatformStats {
    fn default() -> Self {
        Self {
            receive: 0,
            send: 0,
            command: 0,
            groups: DashMap::new(),
            users: DashMap::new(),
        }
    }
}

/// 全局命令统计管理器
pub struct CommandStatsManager {
    /// 按 bot_id + bot_self_id 索引的统计数据
    stats: DashMap<String, PlatformStats>,
    /// 点击数（QPS 相关）
    pub hits: DashMap<String, u64>,
    /// 启动时间
    pub start_time: Instant,
    /// 最大 QPS
    pub max_qps: DashMap<String, u64>,
    /// 最大执行时间
    pub max_runtime: DashMap<String, (String, f64)>,
    /// 总处理时间
    pub total_time: DashMap<String, f64>,
    /// 总处理数
    pub total_count: DashMap<String, u64>,
}

impl std::fmt::Debug for CommandStatsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandStatsManager")
            .field("platform_count", &self.stats.len())
            .field("start_time", &self.start_time)
            .finish_non_exhaustive()
    }
}

impl CommandStatsManager {
    pub fn new() -> Self {
        Self {
            stats: DashMap::new(),
            hits: DashMap::new(),
            start_time: Instant::now(),
            max_qps: DashMap::new(),
            max_runtime: DashMap::new(),
            total_time: DashMap::new(),
            total_count: DashMap::new(),
        }
    }

    fn key(bot_id: &str, bot_self_id: &str) -> String {
        format!("{}:{}", bot_id, bot_self_id)
    }

    /// 获取指定平台的统计
    pub fn get_or_create(
        &self,
        bot_id: &str,
        bot_self_id: &str,
    ) -> dashmap::mapref::one::RefMut<'_, String, PlatformStats> {
        let key = Self::key(bot_id, bot_self_id);
        // entry API 原子化 check-then-insert，避免并发下 insert/get_mut 之间的竞态。
        self.stats.entry(key).or_default()
    }

    /// 记录收到消息
    pub fn record_receive(&self, bot_id: &str, bot_self_id: &str) {
        let mut stats = self.get_or_create(bot_id, bot_self_id);
        stats.receive += 1;
    }

    /// 记录发送消息
    pub fn record_send(&self, bot_id: &str, bot_self_id: &str) {
        let mut stats = self.get_or_create(bot_id, bot_self_id);
        stats.send += 1;
    }

    /// 记录命令触发
    pub fn record_command(
        &self,
        bot_id: &str,
        bot_self_id: &str,
        command: &str,
        user_id: &str,
        group_id: Option<&str>,
    ) {
        let mut stats = self.get_or_create(bot_id, bot_self_id);
        stats.command += 1;

        if let Some(gid) = group_id {
            let group_map = stats.groups.entry(gid.to_string()).or_default();
            *group_map.entry(command.to_string()).or_insert(0) += 1;
        }

        let user_map = stats.users.entry(user_id.to_string()).or_default();
        *user_map.entry(command.to_string()).or_insert(0) += 1;
    }

    /// 获取群组数量
    pub fn group_count(&self, bot_id: &str, bot_self_id: &str) -> usize {
        let stats = self.get_or_create(bot_id, bot_self_id);
        stats.groups.len()
    }

    /// 获取用户数量
    pub fn user_count(&self, bot_id: &str, bot_self_id: &str) -> usize {
        let stats = self.get_or_create(bot_id, bot_self_id);
        stats.users.len()
    }

    /// 记录 QPS 命中
    pub fn record_hit(&self, bot_id: &str, bot_self_id: &str) {
        let key = Self::key(bot_id, bot_self_id);
        let mut current = self.hits.entry(key.clone()).or_insert(0);
        *current += 1;
        let current_val = *current;
        drop(current);
        let mut qps = self.max_qps.entry(key).or_insert(0);
        if current_val > *qps {
            *qps = current_val;
        }
    }

    /// 释放 QPS 计数（请求完成时调用）
    pub fn release_hit(&self, bot_id: &str, bot_self_id: &str) {
        let key = Self::key(bot_id, bot_self_id);
        if let Some(mut hits) = self.hits.get_mut(&key) {
            *hits = hits.saturating_sub(1);
        }
    }

    /// 记录执行时间
    pub fn record_runtime(
        &self,
        bot_id: &str,
        bot_self_id: &str,
        func_name: &str,
        runtime_secs: f64,
    ) {
        let key = Self::key(bot_id, bot_self_id);
        let mut entry = self
            .max_runtime
            .entry(key.clone())
            .or_insert_with(|| (String::new(), 0.0));
        if runtime_secs > entry.1 {
            *entry = (func_name.to_string(), runtime_secs);
        }
        *self.total_time.entry(key.clone()).or_insert(0.0) += runtime_secs;
        *self.total_count.entry(key).or_insert(0) += 1;
    }

    /// 获取 JSON 格式的摘要统计
    pub fn get_summary(&self, bot_id: &str, bot_self_id: &str) -> serde_json::Value {
        let stats = self.get_or_create(bot_id, bot_self_id);
        let key = Self::key(bot_id, bot_self_id);
        let avg_time = self
            .total_time
            .get(&key)
            .map(|t| {
                let count: u64 = self.total_count.get(&key).map(|x| *x).unwrap_or(1).max(1);
                *t / count as f64
            })
            .unwrap_or(0.0);

        serde_json::json!({
            "receive": stats.receive,
            "send": stats.send,
            "command": stats.command,
            "groups": stats.groups.len(),
            "users": stats.users.len(),
            "current_qps": self.hits.get(&key).map(|x| *x).unwrap_or(0),
            "max_qps": self.max_qps.get(&key).map(|x| *x).unwrap_or(0),
            "max_runtime_secs": self.max_runtime.get(&key).map(|x| format!("{}: {:.3}s", x.0, x.1)).unwrap_or_default(),
            "avg_runtime_secs": format!("{:.3}", avg_time),
            "total_count": self.total_count.get(&key).map(|x| *x).unwrap_or(0),
            "uptime_secs": self.start_time.elapsed().as_secs(),
        })
    }

    /// 获取热门命令排行
    pub fn top_commands(
        &self,
        bot_id: &str,
        bot_self_id: &str,
        limit: usize,
    ) -> Vec<(String, u64)> {
        let stats = self.get_or_create(bot_id, bot_self_id);
        let mut all: Vec<(String, u64)> = Vec::new();

        for user_map in stats.users.iter() {
            for cmd in user_map.value().iter() {
                match all.iter_mut().find(|(name, _)| name == cmd.key()) {
                    Some((_, count)) => *count += *cmd.value(),
                    None => all.push((cmd.key().clone(), *cmd.value())),
                }
            }
        }

        all.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        all.truncate(limit);
        all
    }

    /// 获取活跃群组排行
    pub fn top_groups(&self, bot_id: &str, bot_self_id: &str, limit: usize) -> Vec<(String, u64)> {
        let stats = self.get_or_create(bot_id, bot_self_id);
        let mut all: Vec<(String, u64)> = stats
            .groups
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.value().iter().map(|e| *e.value()).sum(),
                )
            })
            .collect();

        all.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        all.truncate(limit);
        all
    }
}

impl Default for CommandStatsManager {
    fn default() -> Self {
        Self::new()
    }
}
