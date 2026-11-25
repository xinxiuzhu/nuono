//! 给属性宏处理函数共享宿主能力，免去每个插件重复写 `Option<PluginContext>`。
use std::sync::RwLock;

use crate::{PluginContext, Result, anyhow};

/// 在插件里保存 `Arc<PluginRuntime>`，在 `on_load` 绑定，再注入指令的第四个参数。
/// 获取的是当前生命周期的上下文；插件尚未加载或已经停用时明确报错。
#[derive(Default)]
pub struct PluginRuntime(RwLock<Option<PluginContext>>);

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bound = self.0.read().is_ok_and(|ctx| ctx.is_some());
        f.debug_struct("PluginRuntime")
            .field("bound", &bound)
            .finish()
    }
}

impl PluginRuntime {
    pub async fn image_output_settings(&self) -> Result<crate::ImageOutputSettings> {
        self.context()?.image_output_settings().await
    }

    pub fn bind(&self, context: &PluginContext) -> Result<()> {
        anyhow::ensure!(!context.tasks.is_closed(), "不能绑定已停用的插件上下文");
        *self.0.write().expect("锁已中毒") = Some(context.clone());
        Ok(())
    }

    pub fn context(&self) -> Result<PluginContext> {
        let context = self.0.read().expect("锁已中毒");
        let context = context
            .as_ref()
            .ok_or_else(|| anyhow!("插件尚未加载，无法使用宿主能力"))?;
        anyhow::ensure!(
            !context.tasks.is_closed(),
            "插件已停用，无法使用旧的宿主上下文"
        );
        Ok(context.clone())
    }
}
