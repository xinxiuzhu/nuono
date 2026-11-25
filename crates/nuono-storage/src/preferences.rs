use crate::{
    Database,
    entity::global_configs,
    error::{NuonoError, Result},
};
use nuono_app::preferences::{ThemeConfig, ThemeStore};
use sea_orm::{EntityTrait, Set};
const KEY: &str = "dashboard.theme.v1";
#[async_trait::async_trait]
impl ThemeStore for Database {
    async fn load_theme(&self) -> Result<Option<ThemeConfig>> {
        self.get_global_config(KEY)
            .await?
            .map(|v| {
                serde_json::from_value(v)
                    .map_err(|e| NuonoError::ConfigLoad(format!("主题设置无效: {e}")))
            })
            .transpose()
    }
    async fn initialize_theme(&self, seed: &ThemeConfig) -> Result<ThemeConfig> {
        let tx = self.begin_write().await?;
        global_configs::Entity::insert(global_configs::ActiveModel {
            key: Set(KEY.into()),
            value: Set(serde_json::to_string(seed)?),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            created_at_ms: Set(crate::clock::db_now().timestamp_millis()),
            ..Default::default()
        })
        .on_conflict_do_nothing()
        .exec_without_returning(&tx)
        .await?;
        let row = global_configs::Entity::find_by_id(KEY)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::ConfigLoad("主题初始化后未找到记录".into()))?;
        let config: ThemeConfig = serde_json::from_str(&row.value)?;
        config.validate()?;
        tx.commit().await?;
        Ok(config)
    }
    async fn save_theme(&self, theme: &ThemeConfig) -> Result<()> {
        self.set_global_config(KEY, serde_json::to_value(theme)?)
            .await
    }
}
