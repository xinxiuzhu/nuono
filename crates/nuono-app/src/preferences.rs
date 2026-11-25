//! Dashboard 全局偏好属于业务设置；图片内容仍是文件资产。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeConfig {
    pub wallpaper_enabled: bool,
    pub wallpaper_url: String,
    pub wallpaper_blur: u32,
    pub wallpaper_opacity: f32,
    pub performance_mode: bool,
    pub primary_color: String,
    pub glass_blur: u32,
    #[serde(default)]
    pub extra: HashMap<String, String>,
}
impl ThemeConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.wallpaper_opacity.is_finite() || !(0.0..=1.0).contains(&self.wallpaper_opacity) {
            return Err(NuonoError::Validation(
                "壁纸透明度必须在 0 到 1 之间".into(),
            ));
        }
        Ok(())
    }
}
#[async_trait::async_trait]
pub trait ThemeStore: Send + Sync {
    async fn load_theme(&self) -> Result<Option<ThemeConfig>>;
    async fn initialize_theme(&self, seed: &ThemeConfig) -> Result<ThemeConfig>;
    async fn save_theme(&self, theme: &ThemeConfig) -> Result<()>;
}
pub struct ThemeService<'a> {
    store: &'a dyn ThemeStore,
}
impl<'a> ThemeService<'a> {
    pub fn new(store: &'a dyn ThemeStore) -> Self {
        Self { store }
    }
    pub async fn get(&self) -> Result<ThemeConfig> {
        let config = self
            .store
            .load_theme()
            .await?
            .ok_or_else(|| NuonoError::ConfigLoad("主题设置尚未初始化".into()))?;
        config.validate()?;
        Ok(config)
    }
    pub async fn initialize(&self, seed: &ThemeConfig) -> Result<ThemeConfig> {
        seed.validate()?;
        self.store.initialize_theme(seed).await
    }
    pub async fn save(&self, theme: &ThemeConfig) -> Result<()> {
        theme.validate()?;
        self.store.save_theme(theme).await
    }
}
