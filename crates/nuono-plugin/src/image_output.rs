//! 共享的图片输出配置与由宿主提供、支持运行期动态读取的图片编码策略。
//!
//! 读取配置本身不会触发图片编码或上传；像素编码功能在启用 `image` feature 时可用，且始终接收一份显式的配置快照。

use std::sync::Arc;

use crate::NuonoResult;

pub use nuono_app::image_output::{ImageOutputFormat, ImageOutputSettings};

#[cfg(feature = "image")]
mod encoding;
#[cfg(feature = "image")]
pub use encoding::encode;

/// 宿主图片输出配置读取接口（每次调用都读取最新生效的配置；不向编码器暴露任何配置写入权限、密钥或文件系统访问能力）。
#[async_trait::async_trait]
pub trait ImageOutputSettingsSource: Send + Sync + 'static {
    async fn settings(&self) -> NuonoResult<ImageOutputSettings>;
}

/// 可克隆的宿主图片输出策略句柄。未注入宿主配置源的嵌入式或测试上下文会自动回退使用共享默认设置，
/// 确保现有的 `PluginContext` 与 `PluginTaskScope` 构造函数无需额外配置即可工作。
#[derive(Clone, Default)]
pub struct PluginImageOutput {
    source: Option<Arc<dyn ImageOutputSettingsSource>>,
}

impl std::fmt::Debug for PluginImageOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginImageOutput")
            .field("configured", &self.source.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginImageOutput {
    pub fn from_source(source: Arc<dyn ImageOutputSettingsSource>) -> Self {
        Self {
            source: Some(source),
        }
    }

    pub async fn settings(&self) -> NuonoResult<ImageOutputSettings> {
        let settings = match &self.source {
            Some(source) => source.settings().await?,
            None => ImageOutputSettings::default(),
        };
        settings.validate()?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NuonoError, PluginTaskScope};
    use tokio::sync::RwLock;

    struct Source(RwLock<ImageOutputSettings>);

    #[async_trait::async_trait]
    impl ImageOutputSettingsSource for Source {
        async fn settings(&self) -> NuonoResult<ImageOutputSettings> {
            Ok(*self.0.read().await)
        }
    }

    #[tokio::test]
    async fn existing_scope_constructors_and_missing_source_use_shared_defaults() {
        let expected = ImageOutputSettings::default();
        assert_eq!(
            PluginImageOutput::default().settings().await.unwrap(),
            expected
        );
        let scope = PluginTaskScope::new("image-defaults");
        assert_eq!(scope.image_output_settings().await.unwrap(), expected);
        scope.shutdown().await;
        assert!(scope.image_output_settings().await.is_err());
    }

    #[tokio::test]
    async fn source_updates_are_visible_to_existing_handles_and_task_scopes() {
        let source = Arc::new(Source(RwLock::new(ImageOutputSettings::default())));
        let output = PluginImageOutput::from_source(source.clone());
        let scope = PluginTaskScope::new("image-policy").with_image_output(output.clone());
        let before = scope.image_output_settings().await.unwrap();
        let updated = ImageOutputSettings {
            format: ImageOutputFormat::Jpeg,
            jpeg_quality: 92,
        };
        *source.0.write().await = updated;
        assert_eq!(output.settings().await.unwrap(), updated);
        assert_eq!(scope.image_output_settings().await.unwrap(), updated);
        assert_eq!(before, ImageOutputSettings::default());
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_host_settings_and_source_errors_are_not_replaced_with_defaults() {
        let source = Arc::new(Source(RwLock::new(ImageOutputSettings {
            format: ImageOutputFormat::Png,
            jpeg_quality: 0,
        })));
        assert!(
            PluginImageOutput::from_source(source)
                .settings()
                .await
                .is_err()
        );
        struct FailedSource;
        #[async_trait::async_trait]
        impl ImageOutputSettingsSource for FailedSource {
            async fn settings(&self) -> NuonoResult<ImageOutputSettings> {
                Err(NuonoError::ConfigLoad("fixture source unavailable".into()))
            }
        }
        let error = PluginImageOutput::from_source(Arc::new(FailedSource))
            .settings()
            .await
            .unwrap_err();
        assert!(matches!(error, NuonoError::ConfigLoad(_)));
    }
}
