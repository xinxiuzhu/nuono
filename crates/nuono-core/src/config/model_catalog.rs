//! 在单次原子操作内完成 AI 模型记录变更、配置引用更新与内存快照发布。
use super::{ConfigManager, NuonoError, Result};
use nuono_app::{
    model_catalog::{AiModelRecord, ModelChange},
    settings::RuntimeConfigPatch,
};
impl ConfigManager {
    /// 未传配置补丁时保留现有模型绑定，并拒绝删除或停用仍被引用的模型；
    /// 调用方也可在同一事务中显式改绑其他模型并删除旧模型。
    pub async fn change_ai_model(
        &self,
        change: ModelChange<'_>,
        patch: RuntimeConfigPatch,
    ) -> Result<Option<AiModelRecord>> {
        let _guard = self.writes.lock().await;
        let runtime = self
            .runtime
            .read()
            .await
            .clone()
            .ok_or_else(|| NuonoError::ConfigLoad("配置管理器尚未挂接数据库".into()))?;
        let (effective, model) = runtime.change_model(change, &patch).await?;
        self.publish(serde_json::from_value(effective)?).await?;
        Ok(model)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/config/model_catalog/tests.rs"]
mod tests;
