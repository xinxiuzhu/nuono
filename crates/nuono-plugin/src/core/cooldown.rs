use dashmap::DashMap;
use std::time::{Duration, Instant};
use tracing::trace;

/// 冷却追踪器 — 防止同一用户短时间内反复触发
///
/// 对应 gsuid_core 的 cooldown_tracker。
/// 支持用户冷却、命令冷却、群冷却等多维度限频。
#[derive(Debug)]
pub struct CooldownTracker {
    /// 用户 ID 到上次触发时间的映射
    user_cooldowns: DashMap<String, Instant>,
    /// (用户 ID, 命令名称) 到上次触发时间的映射
    command_cooldowns: DashMap<(String, String), Instant>,
    /// 群组 ID 到上次触发时间的映射
    group_cooldowns: DashMap<String, Instant>,
}

impl CooldownTracker {
    pub fn new() -> Self {
        Self {
            user_cooldowns: DashMap::new(),
            command_cooldowns: DashMap::new(),
            group_cooldowns: DashMap::new(),
        }
    }

    /// 检查用户是否在冷却中
    /// 返回 true 表示该用户/事件应该被忽略
    pub fn is_user_on_cooldown(&self, user_id: &str, cooldown_secs: u64) -> bool {
        if cooldown_secs == 0 {
            return false;
        }

        if let Some(last) = self.user_cooldowns.get(user_id)
            && last.elapsed() < Duration::from_secs(cooldown_secs)
        {
            trace!("用户 {} 处于冷却中", user_id);
            return true;
        }

        self.user_cooldowns
            .insert(user_id.to_string(), Instant::now());
        false
    }

    /// 检查命令冷却
    pub fn is_command_on_cooldown(&self, user_id: &str, command: &str, cooldown_secs: u64) -> bool {
        if cooldown_secs == 0 {
            return false;
        }

        let key = (user_id.to_string(), command.to_string());
        if let Some(last) = self.command_cooldowns.get(&key)
            && last.elapsed() < Duration::from_secs(cooldown_secs)
        {
            trace!("命令 {} 对用户 {} 处于冷却中", command, user_id);
            return true;
        }

        self.command_cooldowns.insert(key, Instant::now());
        false
    }

    /// 检查群冷却
    pub fn is_group_on_cooldown(&self, group_id: &str, cooldown_secs: u64) -> bool {
        if cooldown_secs == 0 {
            return false;
        }

        if let Some(last) = self.group_cooldowns.get(group_id)
            && last.elapsed() < Duration::from_secs(cooldown_secs)
        {
            trace!("群 {} 处于冷却中", group_id);
            return true;
        }

        self.group_cooldowns
            .insert(group_id.to_string(), Instant::now());
        false
    }

    /// 重置用户冷却
    pub fn reset_user(&self, user_id: &str) {
        self.user_cooldowns.remove(user_id);
    }

    /// 清理超过 5 分钟的过期冷却记录
    pub fn cleanup(&self) {
        let now = Instant::now();
        let threshold = Duration::from_secs(300);

        self.user_cooldowns
            .retain(|_, v| now.duration_since(*v) < threshold);
        self.command_cooldowns
            .retain(|_, v| now.duration_since(*v) < threshold);
        self.group_cooldowns
            .retain(|_, v| now.duration_since(*v) < threshold);
    }

    /// 获取当前冷却中的用户数量
    pub fn user_cooldown_count(&self) -> usize {
        self.user_cooldowns.len()
    }
}

impl Default for CooldownTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_user_cooldown() {
        let tracker = CooldownTracker::new();
        assert!(!tracker.is_user_on_cooldown("user1", 1));
        assert!(tracker.is_user_on_cooldown("user1", 1));
        sleep(Duration::from_millis(1100));
        assert!(!tracker.is_user_on_cooldown("user1", 1));
    }

    #[test]
    fn test_command_cooldown() {
        let tracker = CooldownTracker::new();
        assert!(!tracker.is_command_on_cooldown("user1", "cmd1", 1));
        assert!(tracker.is_command_on_cooldown("user1", "cmd1", 1));
        assert!(!tracker.is_command_on_cooldown("user1", "cmd2", 1));
    }

    #[test]
    fn test_zero_cooldown() {
        let tracker = CooldownTracker::new();
        assert!(!tracker.is_user_on_cooldown("user1", 0));
        assert!(!tracker.is_user_on_cooldown("user1", 0));
    }
}
