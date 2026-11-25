//! 在数据库行锁保护下校验并条件更新富消息交互配置子树。
use nuono_app::message_interactions::MessageInteractionSettings;
use nuono_app::settings::{RuntimeConfigPatch, RuntimeSettingsPrecondition, merge_runtime_values};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{ConfigManager, NuonoError, Result};

pub fn message_interactions_revision(settings: &MessageInteractionSettings) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(settings)?)))
}

struct MessageInteractionsRevision<'a> {
    expected: &'a str,
    startup: &'a MessageInteractionSettings,
}

impl RuntimeSettingsPrecondition for MessageInteractionsRevision<'_> {
    fn check(&self, stored: &Value) -> Result<()> {
        // 早期数据库记录可能只存了显式修改过的部分子树；这里按与 build_candidate 一致的启动基线补齐缺失字段后再比对。
        let mut current = serde_json::to_value(self.startup)?;
        if let Some(value) = stored.get("message_interactions") {
            if !value.is_object() {
                return Err(NuonoError::ConfigValidation(
                    "数据库中的消息与交互设置必须是对象".into(),
                ));
            }
            merge_runtime_values(&mut current, value);
        }
        let current = serde_json::from_value(current)?;
        if message_interactions_revision(&current)? != self.expected {
            return Err(NuonoError::Conflict(
                "消息与交互设置已更新，请重新读取后确认修改；当前草稿未覆盖服务器。".into(),
            ));
        }
        Ok(())
    }
}

impl ConfigManager {
    pub async fn update_message_interactions_patch(
        &self,
        patch: RuntimeConfigPatch,
        expected_revision: &str,
    ) -> Result<()> {
        if !patch
            .settings
            .get("message_interactions")
            .is_some_and(Value::is_object)
            || expected_revision.len() != 64
            || !expected_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(NuonoError::ConfigValidation(
                "消息与交互设置需要有效的设置补丁与版本号".into(),
            ));
        }
        let _guard = self.writes.lock().await;
        let runtime = self
            .runtime
            .read()
            .await
            .clone()
            .ok_or_else(|| NuonoError::ConfigLoad("配置管理器尚未挂接数据库".into()))?;
        let check = MessageInteractionsRevision {
            expected: expected_revision,
            startup: &self.startup.message_interactions,
        };
        let effective = runtime.update_conditional(&patch, &check).await?;
        self.publish(serde_json::from_value(effective)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::message_interactions::MarkdownMode;

    #[test]
    fn partial_stored_policy_uses_the_effective_startup_baseline_for_revision() {
        let mut startup = MessageInteractionSettings::default();
        startup.default.markdown = MarkdownMode::Plain;
        let mut effective = startup.clone();
        effective.default.button_columns = 4;
        let revision = message_interactions_revision(&effective).unwrap();
        let check = MessageInteractionsRevision {
            expected: &revision,
            startup: &startup,
        };
        check
            .check(&serde_json::json!({
                "message_interactions": {"default": {"button_columns": 4}}
            }))
            .unwrap();
        assert!(check.check(&serde_json::json!({})).is_err());
    }
}
