//! 宿主侧图片读取适配器，复用插件 SDK 的有界媒体读取实现。

use nuono_app::media::{ImageResolver, ImageUpload};
use nuono_error::{NuonoError, Result};

pub struct HostImageResolver {
    http: nuono_plugin::PluginHttpClient,
}

impl HostImageResolver {
    pub fn new() -> Self {
        Self {
            http: nuono_plugin::PluginHttpClient::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
        }
    }
}

impl Default for HostImageResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ImageResolver for HostImageResolver {
    async fn resolve_image(&self, source: &str, max_bytes: usize) -> Result<ImageUpload> {
        nuono_plugin::media::resolve_image(source, self.http.raw(), max_bytes)
            .await
            .map_err(|error| NuonoError::MessageSend(error.to_string()))
    }
}
