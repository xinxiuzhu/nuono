//! 插件配置的受管持久化写入接口（由宿主统一负责落盘、Schema 校验与乐观并发控制）。
use std::sync::Arc;

use crate::{Result, Value};

/// 已校验配置文件的最新生效值及其对应的并发控制版本号（`revision`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfigSnapshot {
    pub values: Value,
    pub revision: String,
}

/// 由宿主提供并预先绑定到特定插件的配置更新底层接口。
/// 实现方必须在持有写锁期间核对当前文件版本是否与 `expected_revision` 一致；所有方法均不接受外部传入插件名或文件路径。
#[async_trait::async_trait]
pub trait PluginConfigUpdatePort: Send + Sync + 'static {
    /// 读取并校验当前磁盘上的最新配置文件（包含尚未被文件监听轮询到的外部手工编辑）。
    async fn read_latest(&self) -> Result<PluginConfigSnapshot>;

    /// 仅更新 `patch` 中显式提供的已声明配置字段，保留其余字段及文件中未声明的自定义项；
    /// 仅在配置文件原子落盘成功后才向内存中的实时配置句柄发布新值。
    async fn update_patch(
        &self,
        patch: Value,
        expected_revision: &str,
    ) -> Result<PluginConfigSnapshot>;
}

/// 绑定到当前插件作用域的可克隆配置写入句柄（通过 `ctx.config_updates()` 获取）。
///
/// 保存操作会同时更新磁盘配置文件与内存实时配置句柄。注意：`Plugin::on_config_changed` 回调
/// 会在当前插件处理函数释放锁之后，由宿主的配置监听器异步触发；因此 `update_patch` 返回成功并不代表插件派生的连接或缓存已经完成刷新。
/// 若在未注入宿主端口的测试环境中调用，默认句柄会明确返回 `NotImplemented` 错误，而不会在内存中假装保存成功。
#[derive(Clone, Default)]
pub struct PluginConfigUpdates {
    port: Option<Arc<dyn PluginConfigUpdatePort>>,
}

impl std::fmt::Debug for PluginConfigUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConfigUpdates")
            .field("configured", &self.port.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginConfigUpdates {
    /// 供宿主或测试注入具体实现（该实现自身已固定绑定对应插件的命名空间）。
    pub fn from_port(port: Arc<dyn PluginConfigUpdatePort>) -> Self {
        Self { port: Some(port) }
    }

    fn port(&self) -> Result<&dyn PluginConfigUpdatePort> {
        self.port.as_deref().ok_or_else(|| {
            crate::error::NuonoError::NotImplemented("宿主未提供插件配置持久化端口".into()).into()
        })
    }

    pub async fn read_latest(&self) -> Result<PluginConfigSnapshot> {
        self.port()?.read_latest().await
    }

    /// 读取最新通过校验的一致配置快照及其版本号（`revision`）。
    pub async fn snapshot_with_revision(&self) -> Result<PluginConfigSnapshot> {
        self.read_latest().await
    }

    pub async fn update_patch(
        &self,
        patch: Value,
        expected_revision: &str,
    ) -> Result<PluginConfigSnapshot> {
        let port = self.port()?;
        anyhow::ensure!(patch.is_object(), "插件配置补丁必须是 JSON 对象");
        anyhow::ensure!(
            !expected_revision.trim().is_empty(),
            "插件配置保存需要版本号"
        );
        port.update_patch(patch, expected_revision).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::Mutex;

    struct FakePort(Mutex<PluginConfigSnapshot>);

    #[async_trait::async_trait]
    impl PluginConfigUpdatePort for FakePort {
        async fn read_latest(&self) -> Result<PluginConfigSnapshot> {
            Ok(self.0.lock().await.clone())
        }

        async fn update_patch(&self, patch: Value, expected: &str) -> Result<PluginConfigSnapshot> {
            let mut state = self.0.lock().await;
            anyhow::ensure!(state.revision == expected, "fixture revision conflict");
            state
                .values
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            state.revision = "next".into();
            Ok(state.clone())
        }
    }

    #[tokio::test]
    async fn missing_port_reports_unsupported_for_both_operations() {
        let updates = PluginConfigUpdates::default();
        assert!(matches!(
            updates
                .read_latest()
                .await
                .unwrap_err()
                .downcast_ref::<crate::error::NuonoError>(),
            Some(crate::error::NuonoError::NotImplemented(_))
        ));
        assert!(
            updates
                .update_patch(json!({"count":2}), "previous")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn injected_port_clones_share_revision_and_invalid_requests_never_reach_it() {
        let updates =
            PluginConfigUpdates::from_port(Arc::new(FakePort(Mutex::new(PluginConfigSnapshot {
                values: json!({"count":1,"keep":true}),
                revision: "initial".into(),
            }))));
        let before = updates.snapshot_with_revision().await.unwrap();
        assert!(
            updates
                .update_patch(json!([1]), &before.revision)
                .await
                .is_err()
        );
        assert!(updates.update_patch(json!({}), "").await.is_err());
        assert_eq!(updates.read_latest().await.unwrap(), before);
        let saved = updates
            .clone()
            .update_patch(json!({"count":2}), &before.revision)
            .await
            .unwrap();
        assert_eq!(saved.values, json!({"count":2,"keep":true}));
        assert_eq!(updates.read_latest().await.unwrap(), saved);
        assert!(
            updates
                .update_patch(json!({"count":3}), &before.revision)
                .await
                .is_err()
        );
        assert_eq!(updates.read_latest().await.unwrap(), saved);
    }
}
