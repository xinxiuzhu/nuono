//! 联网搜索设置与密钥凭据的带版本比对（CAS）原子更新，提交成功后再发布新内存快照。
use super::{ConfigManager, NuonoError, Result};
use nuono_app::{
    settings::{
        RuntimeConfigPatch, RuntimeConfigSnapshot, RuntimeSettingsPrecondition,
        merge_runtime_values,
    },
    web_settings::{WebProvider, WebSettings},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn web_settings_revision(settings: &WebSettings) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(settings)?)))
}
struct WebRevision<'a> {
    expected: &'a str,
    startup: &'a WebSettings,
}
impl RuntimeSettingsPrecondition for WebRevision<'_> {
    fn check(&self, _: &Value) -> Result<()> {
        Err(NuonoError::Internal(
            "联网配置需要同时核对设置与密钥".into(),
        ))
    }
    fn check_snapshot(&self, stored: &RuntimeConfigSnapshot) -> Result<()> {
        let mut value = serde_json::to_value(self.startup)?;
        if let Some(settings) = stored.settings.pointer("/ai/web") {
            merge_runtime_values(&mut value, settings);
        }
        let mut current: WebSettings = serde_json::from_value(value)?;
        current.credentials = Default::default();
        for provider in WebProvider::ALL {
            *current.credentials.field_mut(provider) =
                stored.secrets.get(provider.secret_key()).cloned();
        }
        if web_settings_revision(&current)? != self.expected {
            return Err(NuonoError::Conflict(
                "联网设置或密钥已在其他页面修改，请刷新核对；当前草稿未覆盖服务端".into(),
            ));
        }
        Ok(())
    }
}
impl ConfigManager {
    pub async fn update_web_settings(
        &self,
        settings: Value,
        secrets: Vec<(String, Option<String>)>,
        expected_revision: &str,
    ) -> Result<()> {
        if !settings.is_object()
            || expected_revision.len() != 64
            || !expected_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(NuonoError::ConfigValidation(
                "联网设置需要有效的补丁和版本号".into(),
            ));
        }
        if settings
            .as_object()
            .is_some_and(|o| o.keys().any(|key| key != "search" && key != "fetch"))
            || secrets
                .iter()
                .any(|(key, _)| !WebProvider::ALL.iter().any(|p| p.secret_key() == key))
        {
            return Err(NuonoError::ConfigValidation(
                "联网配置包含不支持的字段".into(),
            ));
        }
        let _guard = self.writes.lock().await;
        let runtime = self
            .runtime
            .read()
            .await
            .clone()
            .ok_or_else(|| NuonoError::ConfigLoad("配置管理器尚未挂接数据库".into()))?;
        let check = WebRevision {
            expected: expected_revision,
            startup: &self.startup.ai.web,
        };
        let patch = RuntimeConfigPatch {
            settings: json!({"ai":{"web":settings}}),
            secrets,
        };
        let effective = runtime.update_conditional(&patch, &check).await?;
        self.publish(serde_json::from_value(effective)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_observes_secret_rotation_and_partial_legacy_settings() {
        let mut current = WebSettings::default();
        current.credentials.tavily_api_key = Some("fixture-one".into());
        let revision = web_settings_revision(&current).unwrap();
        let startup = WebSettings::default();
        let check = WebRevision {
            expected: &revision,
            startup: &startup,
        };
        let mut snapshot = RuntimeConfigSnapshot {
            settings: json!({"ai":{"web":{"search":{"max_results":5}}}}),
            secrets: [("ai.web.tavily_api_key".into(), "fixture-one".into())].into(),
        };
        check.check_snapshot(&snapshot).unwrap();
        snapshot
            .secrets
            .insert("ai.web.tavily_api_key".into(), "fixture-two".into());
        assert!(matches!(
            check.check_snapshot(&snapshot),
            Err(NuonoError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn settings_and_secret_commit_atomically_and_reject_stale_versions() {
        let _env = crate::test_support::env_lock().await;
        let db = crate::test_support::test_db(2).await;
        let root = std::env::temp_dir().join(format!("nuono-web-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("nuono.toml");
        super::super::NuonoConfig::default()
            .save(&path)
            .await
            .unwrap();
        let before_file = std::fs::read(&path).unwrap();
        let config = ConfigManager::load(Some(path.clone())).await.unwrap();
        config.attach_database(db.clone()).await.unwrap();
        let revision = web_settings_revision(&config.snapshot().await.ai.web).unwrap();
        assert!(
            config
                .update_web_settings(json!({"search":{"enabled":true}}), vec![], &revision)
                .await
                .is_err()
        );
        config
            .update_web_settings(
                json!({"search":{"enabled":true}}),
                vec![("ai.web.tavily_api_key".into(), Some("fixture-key".into()))],
                &revision,
            )
            .await
            .unwrap();
        let live = config.snapshot().await;
        assert!(live.ai.web.search.enabled);
        assert_eq!(
            live.ai.web.credentials.tavily_api_key.as_deref(),
            Some("fixture-key")
        );
        assert!(matches!(
            config
                .update_web_settings(json!({"fetch":{"timeout_secs":20}}), vec![], &revision)
                .await,
            Err(NuonoError::Conflict(_))
        ));
        let stored = db.load_runtime_settings().await.unwrap();
        assert!(!stored.settings.to_string().contains("fixture-key"));
        let current_revision = web_settings_revision(&live.ai.web).unwrap();
        assert!(
            config
                .update_web_settings(
                    json!({}),
                    vec![("ai.web.tavily_api_key".into(), None)],
                    &current_revision
                )
                .await
                .is_err()
        );
        assert_eq!(
            db.get_secret("ai.web.tavily_api_key")
                .await
                .unwrap()
                .as_deref(),
            Some("fixture-key")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before_file);
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
