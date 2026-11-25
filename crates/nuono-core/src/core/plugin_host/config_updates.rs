//! 插件上下文内的配置持久化；保存过程中绝不重入插件回调，避免死锁。
use std::sync::Arc;

use nuono_plugin::{PluginConfigSnapshot, PluginConfigUpdatePort, PluginConfigUpdates};

use crate::config::PluginConfigManager;

struct ScopedConfigUpdates {
    manager: Arc<PluginConfigManager>,
    plugin_name: String,
}

pub(super) fn for_plugin(
    manager: Arc<PluginConfigManager>,
    plugin_name: &str,
) -> PluginConfigUpdates {
    PluginConfigUpdates::from_port(Arc::new(ScopedConfigUpdates {
        manager,
        plugin_name: plugin_name.to_string(),
    }))
}

#[async_trait::async_trait]
impl PluginConfigUpdatePort for ScopedConfigUpdates {
    async fn read_latest(&self) -> anyhow::Result<PluginConfigSnapshot> {
        Ok(self.manager.read_latest(&self.plugin_name).await?)
    }

    async fn update_patch(
        &self,
        patch: serde_json::Value,
        expected_revision: &str,
    ) -> anyhow::Result<PluginConfigSnapshot> {
        // 数据库提交后已同步更新内存中的 config_handle 快照。
        // 后台配置监听器会独立比对宿主记录的上次回调版本，并在当前调用方释放插件锁后补触发 on_config_changed；
        // 若在此处直接调用 refresh_plugin_config，会因当前回调（on_event / on_load）已持有插件锁而发生死锁。
        Ok(self
            .manager
            .update_config_versioned(&self.plugin_name, patch, expected_revision)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::plugin_fields_to_schema;
    use nuono_plugin::PluginConfigField;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        root: PathBuf,
        manager: Arc<PluginConfigManager>,
        updates: PluginConfigUpdates,
    }

    impl Fixture {
        async fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("nuono-config-port-{}", uuid::Uuid::new_v4()));
            let manager = Arc::new(PluginConfigManager::new(root.clone()));
            for name in ["PluginA", "PluginB"] {
                tokio::fs::create_dir_all(root.join(name)).await.unwrap();
                tokio::fs::write(
                    root.join(name).join("config.toml"),
                    "count = 2\nlabel = \"keep\"\nlegacy = { nested = [1, 2] }\n",
                )
                .await
                .unwrap();
                manager.register(name, schema()).await.unwrap();
            }
            Self {
                root,
                updates: for_plugin(manager.clone(), "PluginA"),
                manager,
            }
        }
        fn path(&self) -> PathBuf {
            self.root.join("PluginA/config.toml")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn schema() -> crate::config::ConfigSchema {
        plugin_fields_to_schema(&[
            PluginConfigField::number("count", "Count", "", 2)
                .with_min(0)
                .with_max(20),
            PluginConfigField::text("label", "Label", "", "keep"),
        ])
        .unwrap()
    }

    #[tokio::test]
    async fn scoped_port_persists_and_preserves_unpatched_and_unknown_file_fields() {
        let f = Fixture::new().await;
        let other_before = tokio::fs::read(f.root.join("PluginB/config.toml"))
            .await
            .unwrap();
        let before = f.updates.read_latest().await.unwrap();
        let handle = f.manager.config_handle("PluginA").await.unwrap();
        let taken = handle.snapshot();
        let saved = f
            .updates
            .update_patch(json!({"count":7}), &before.revision)
            .await
            .unwrap();
        assert_eq!(saved.values["count"], 7);
        assert_eq!(saved.values["label"], "keep");
        assert_eq!(saved.values["legacy"]["nested"], json!([1, 2]));
        assert_ne!(saved.revision, before.revision);
        assert_eq!(handle.snapshot().as_ref(), &saved.values);
        assert_eq!(taken["count"], 2);
        assert_eq!(
            tokio::fs::read(f.root.join("PluginB/config.toml"))
                .await
                .unwrap(),
            other_before
        );

        // 新建配置管理器实例时，能从磁盘文件完整还原此前保存的配置值。
        let restarted = PluginConfigManager::new(f.root.clone());
        restarted.register("PluginA", schema()).await.unwrap();
        assert_eq!(restarted.read_latest("PluginA").await.unwrap(), saved);
        assert_eq!(f.updates.snapshot_with_revision().await.unwrap(), saved);
    }

    #[tokio::test]
    async fn invalid_or_undeclared_fields_and_namespace_attempts_never_write() {
        let f = Fixture::new().await;
        let before = f.updates.read_latest().await.unwrap();
        let bytes = tokio::fs::read(f.path()).await.unwrap();
        for patch in [
            json!({"count":-1}),
            json!({"count":"not a number"}),
            json!({"unknown":1}),
            json!({"legacy":"replace"}),
            json!({"plugin_name":"PluginB","count":5}),
            json!({"../PluginB/count":5}),
            json!({"count":5,"label":false}),
            json!({"count":null}),
        ] {
            assert!(
                f.updates
                    .update_patch(patch, &before.revision)
                    .await
                    .is_err()
            );
            assert_eq!(tokio::fs::read(f.path()).await.unwrap(), bytes);
            assert_eq!(f.updates.read_latest().await.unwrap(), before);
        }
        assert_eq!(
            f.manager.read_latest("PluginB").await.unwrap().values["count"],
            2
        );
    }

    #[tokio::test]
    async fn unseen_external_edits_conflict_and_strict_reads_do_not_hide_invalid_files() {
        let f = Fixture::new().await;
        let before = f.updates.read_latest().await.unwrap();
        let manual = "count = 9\nlabel = \"external\"\nlegacy = { nested = [3] }\n";
        tokio::fs::write(f.path(), manual).await.unwrap();
        // 在外部手动改写文件与持旧版本号保存之间不执行任何刷新或读取，验证保存时仍能识别版本冲突。
        assert!(
            f.updates
                .update_patch(json!({"count":4}), &before.revision)
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(f.path()).await.unwrap(), manual);
        let latest = f.updates.read_latest().await.unwrap();
        assert_eq!(latest.values["count"], 9);
        assert_ne!(latest.revision, before.revision);
        let saved = f
            .updates
            .update_patch(json!({"count":4}), &latest.revision)
            .await
            .unwrap();
        assert_eq!(saved.values["label"], "external");
        assert_eq!(saved.values["legacy"]["nested"], json!([3]));
        let handle = f.manager.config_handle("PluginA").await.unwrap();

        let malformed = "count = \"wrong type\"\n";
        tokio::fs::write(f.path(), malformed).await.unwrap();
        assert!(f.updates.read_latest().await.is_err());
        assert!(
            f.updates
                .update_patch(json!({"count":6}), &saved.revision)
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read_to_string(f.path()).await.unwrap(),
            malformed
        );
        assert_eq!(handle.snapshot().as_ref(), &saved.values);
        tokio::fs::remove_file(f.path()).await.unwrap();
        assert!(f.updates.read_latest().await.is_err());
    }

    #[tokio::test]
    async fn concurrent_revisioned_saves_have_one_winner_then_can_merge_after_reading() {
        let f = Fixture::new().await;
        let before = f.updates.read_latest().await.unwrap();
        let other = f.updates.clone();
        let (a, b) = tokio::join!(
            f.updates.update_patch(json!({"count":8}), &before.revision),
            other.update_patch(json!({"label":"second"}), &before.revision),
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let winner = match (&a, &b) {
            (Ok(saved), _) | (_, Ok(saved)) => saved,
            _ => unreachable!(),
        };
        assert_eq!(&f.updates.read_latest().await.unwrap(), winner);
        let remaining = if a.is_ok() {
            json!({"label":"second"})
        } else {
            json!({"count":8})
        };
        let saved = f
            .updates
            .update_patch(remaining, &winner.revision)
            .await
            .unwrap();
        assert_eq!(saved.values["count"], 8);
        assert_eq!(saved.values["label"], "second");
        assert_eq!(saved.values["legacy"]["nested"], json!([1, 2]));
    }
}
