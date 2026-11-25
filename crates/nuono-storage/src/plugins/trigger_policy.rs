//! 宿主管理的插件触发限流与准入策略（独立于插件自身的业务配置）。
use crate::error::{NuonoError, Result};

/// 插件触发限流策略。更新时须提交完整结构体，防止省略字段意外重置已保存的配置值。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PluginTriggerPolicy {
    /// 单个用户两次获准触发插件调用之间的最小冷却间隔（毫秒）；0 表示不限制。
    #[schema(minimum = 0, maximum = 3600000, default = 1000)]
    pub user_cooldown_ms: u64,
    /// 单个用户允许同时执行的最大并发调用数；0 表示关闭并发防重限制。
    #[schema(minimum = 0, maximum = 32, default = 1)]
    pub max_in_flight_per_user: u32,
    /// 每个群组每分钟补充的令牌配额；设为 0 表示关闭群组限流。
    #[schema(minimum = 0, maximum = 60000, default = 30)]
    pub group_requests_per_minute: u32,
    /// 每个群组令牌桶允许累积的最大突发令牌数（含初始容量）。
    #[schema(minimum = 1, maximum = 1000, default = 3)]
    pub group_burst: u32,
}

impl Default for PluginTriggerPolicy {
    fn default() -> Self {
        Self {
            user_cooldown_ms: 1000,
            max_in_flight_per_user: 1,
            group_requests_per_minute: 30,
            group_burst: 3,
        }
    }
}

impl PluginTriggerPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.user_cooldown_ms > 3_600_000 {
            return Err(NuonoError::Validation(
                "用户冷却时间必须为 0–3600000 毫秒".into(),
            ));
        }
        if self.max_in_flight_per_user > 32 {
            return Err(NuonoError::Validation("每用户并发上限必须为 0–32".into()));
        }
        if self.group_requests_per_minute > 60_000 {
            return Err(NuonoError::Validation(
                "每群每分钟额度必须为 0–60000".into(),
            ));
        }
        if !(1..=1000).contains(&self.group_burst) {
            return Err(NuonoError::Validation("每群突发容量必须为 1–1000".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_independent_disable_values_are_valid() {
        assert_eq!(
            serde_json::to_value(PluginTriggerPolicy::default()).unwrap(),
            json!({
                "user_cooldown_ms":1000, "max_in_flight_per_user":1,
                "group_requests_per_minute":30, "group_burst":3
            })
        );
        for policy in [
            PluginTriggerPolicy::default(),
            PluginTriggerPolicy {
                user_cooldown_ms: 0,
                max_in_flight_per_user: 0,
                group_requests_per_minute: 0,
                group_burst: 1,
            },
            PluginTriggerPolicy {
                user_cooldown_ms: 3_600_000,
                max_in_flight_per_user: 32,
                group_requests_per_minute: 60_000,
                group_burst: 1000,
            },
        ] {
            policy.validate().unwrap();
        }
    }

    #[test]
    fn complete_payload_rejects_unknown_missing_and_wrong_typed_fields() {
        let valid = serde_json::to_value(PluginTriggerPolicy::default()).unwrap();
        for key in valid.as_object().unwrap().keys() {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<PluginTriggerPolicy>(missing).is_err(),
                "{key}"
            );
            for bad in [json!(null), json!(-1), json!(0.5), json!("1"), json!(true)] {
                let mut malformed = valid.clone();
                malformed[key] = bad;
                assert!(
                    serde_json::from_value::<PluginTriggerPolicy>(malformed).is_err(),
                    "{key}"
                );
            }
        }
        let mut unknown = valid;
        unknown["enabled"] = json!(true);
        assert!(serde_json::from_value::<PluginTriggerPolicy>(unknown).is_err());
    }

    #[test]
    fn typed_values_still_need_explicit_domain_validation() {
        for policy in [
            PluginTriggerPolicy {
                user_cooldown_ms: 3_600_001,
                ..Default::default()
            },
            PluginTriggerPolicy {
                user_cooldown_ms: u64::MAX,
                ..Default::default()
            },
            PluginTriggerPolicy {
                max_in_flight_per_user: 33,
                ..Default::default()
            },
            PluginTriggerPolicy {
                group_requests_per_minute: 60_001,
                ..Default::default()
            },
            PluginTriggerPolicy {
                group_burst: 0,
                ..Default::default()
            },
            PluginTriggerPolicy {
                group_burst: 1001,
                ..Default::default()
            },
        ] {
            assert!(policy.validate().is_err());
        }
    }
}
