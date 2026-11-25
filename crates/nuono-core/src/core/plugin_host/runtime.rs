//! 基于宿主共享插件能力的事件运行时与工具审批服务装配。

use std::sync::Arc;

use super::PluginHost;
use crate::agent::AgentRuntime;
use crate::config::ConfigManager;
use crate::core::RuntimeActionRouter;
use crate::db::Database;
use nuono_app::agent::execution::ToolExecutionService;

impl PluginHost {
    /// 模型调用与人工审批执行共用宿主维护的实时工具注册表；
    /// 因此插件在运行期的启用与停用也会即时反映到已创建的服务实例中。
    pub fn tool_execution_service(&self, config: Arc<ConfigManager>) -> ToolExecutionService {
        ToolExecutionService::new(
            Arc::new(self.db.clone()),
            config,
            self.tool_registry.clone(),
        )
    }

    /// 组装事件运行时并将其交互策略挂载到当前宿主的动作路由器上；
    /// 重新绑定会保留已有的共享工具与路由状态，不会触发插件重载。
    pub(crate) fn bind_runtime(
        self: &Arc<Self>,
        db: Database,
        config: Arc<ConfigManager>,
    ) -> AgentRuntime {
        crate::ai::web_tools::register(&self.tool_registry, config.clone());
        let runtime =
            AgentRuntime::with_tool_registry(db, config.clone(), self.tool_registry.clone());
        *self
            .agent_runtime
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(runtime.clone());
        self.action_router.set_interaction_port(Arc::new(
            super::interactions::HostInteractionPort::new(self, config),
        ));
        runtime
    }

    /// 返回与连接管理器及各消息平台适配器共用的宿主动作路由器。
    pub(crate) fn action_router(&self) -> Arc<RuntimeActionRouter> {
        self.action_router.clone()
    }
}
