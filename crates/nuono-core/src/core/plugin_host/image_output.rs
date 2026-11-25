//! 向插件提供实时生效的图片输出编码策略，同时避免向插件暴露完整的宿主配置结构。
use std::sync::Arc;

use nuono_plugin::image_output::{
    ImageOutputSettings, ImageOutputSettingsSource, PluginImageOutput,
};

struct HostImageOutput(Arc<crate::config::ConfigManager>);

#[async_trait::async_trait]
impl ImageOutputSettingsSource for HostImageOutput {
    async fn settings(&self) -> nuono_plugin::NuonoResult<ImageOutputSettings> {
        Ok(self.0.snapshot().await.image_output)
    }
}

pub(crate) fn for_config(config: Arc<crate::config::ConfigManager>) -> PluginImageOutput {
    PluginImageOutput::from_source(Arc::new(HostImageOutput(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::image_output::ImageOutputFormat;

    #[tokio::test]
    async fn existing_plugin_scope_reads_new_settings_without_reloading_the_plugin() {
        let root =
            std::env::temp_dir().join(format!("nuono-image-output-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let manager = Arc::new(crate::config::ConfigManager::for_test(
            crate::config::NuonoConfig::default(),
            root.join("nuono.toml"),
        ));
        let scope = crate::plugin::PluginTaskScope::new("synthetic-image-output")
            .with_image_output(for_config(manager.clone()));
        assert_eq!(
            scope.image_output_settings().await.unwrap(),
            ImageOutputSettings::default()
        );
        let mut next = manager.get().await;
        next.image_output = ImageOutputSettings {
            format: ImageOutputFormat::Jpeg,
            jpeg_quality: 35,
        };
        manager.replace_runtime(next).await.unwrap();
        assert_eq!(
            scope.image_output_settings().await.unwrap().jpeg_quality,
            35
        );
        assert_eq!(
            scope.image_output_settings().await.unwrap().format,
            ImageOutputFormat::Jpeg
        );
        scope.shutdown().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
