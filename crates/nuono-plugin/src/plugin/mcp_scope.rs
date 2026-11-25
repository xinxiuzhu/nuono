//! 将 MCP 传输连接纳入插件的任务退出屏障统一管理。
use super::*;
use rmcp::{RoleClient, service::RunningService};

impl PluginTaskScope {
    pub(crate) fn bind_mcp_service(
        &self,
        service: RunningService<RoleClient, ()>,
    ) -> std::result::Result<(), RunningService<RoleClient, ()>> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Err(service);
        };
        let Ok(slot) = self.async_slots.clone().try_acquire_owned() else {
            return Err(service);
        };
        let tasks = self.tasks.lock().expect("锁已中毒");
        if self.is_closed() {
            return Err(service);
        }
        let cancellation = service.cancellation_token();
        // 插件关闭时触发取消令牌，但交由 `rmcp` 自身完成传输层关闭与子进程/连接回收；
        // 若直接强行 abort `waiter` 任务，会导致 `rmcp` 的清理过程脱离插件的 `shutdown` 退出屏障。
        self.on_shutdown(move || cancellation.cancel());
        let waiter = ScopedFuture {
            future: Box::pin(async move {
                if let Err(error) = service.waiting().await {
                    tracing::warn!(%error, "MCP transport cleanup failed");
                }
            }),
            _slot: slot,
            _completion: self.activity.register(),
        };
        runtime.spawn(waiter);
        drop(tasks);
        Ok(())
    }
}
