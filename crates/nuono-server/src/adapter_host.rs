//! 平台源码适配器的宿主接口实现，向各聊天平台适配器屏蔽底层数据库与服务端状态。
use crate::interaction_ingress::RuntimeIngress;
use nuono_adapter::{
    AdapterHost, ImageResolver, RouteLease, RuntimeActionSink, RuntimeBotRoute, UnifiedEvent,
};
use nuono_core::error::{NuonoError, Result};
use nuono_core::{
    core::{EventKernel, RuntimeActionRouter},
    media::HostImageResolver,
};
use std::sync::Arc;

pub(crate) struct SourceAdapterHost {
    platform: String,
    instance_id: String,
    kernel: Arc<EventKernel>,
    pub(crate) ingress: RuntimeIngress,
}

struct Lease {
    router: Arc<RuntimeActionRouter>,
    route: RuntimeBotRoute,
    generation: u64,
}
impl RouteLease for Lease {}
impl Drop for Lease {
    fn drop(&mut self) {
        self.router.unregister(&self.route, self.generation);
    }
}

impl SourceAdapterHost {
    pub(crate) fn new(platform: &str, instance_id: &str, kernel: Arc<EventKernel>) -> Arc<Self> {
        Arc::new(Self {
            platform: platform.into(),
            instance_id: instance_id.into(),
            kernel,
            ingress: RuntimeIngress::new(),
        })
    }
    fn validate_route(&self, route: &RuntimeBotRoute) -> Result<()> {
        if route.platform != self.platform
            || route.instance_id != self.instance_id
            || route.bot_self_id.is_empty()
        {
            return Err(NuonoError::Validation(
                "适配器提交的路由与当前实例不匹配".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AdapterHost for SourceAdapterHost {
    fn images(&self) -> Arc<dyn ImageResolver> {
        Arc::new(HostImageResolver::new())
    }
    fn register_route(
        &self,
        route: RuntimeBotRoute,
        sink: Arc<dyn RuntimeActionSink>,
    ) -> Result<Box<dyn RouteLease>> {
        self.validate_route(&route)?;
        let router = self.kernel.action_router();
        let generation = router.register(route.clone(), sink);
        Ok(Box::new(Lease {
            router,
            route,
            generation,
        }))
    }
    async fn submit(
        &self,
        event: UnifiedEvent,
        sink: Arc<dyn RuntimeActionSink>,
        route: RuntimeBotRoute,
    ) -> Result<()> {
        self.validate_route(&route)?;
        self.ingress
            .submit(
                &self.platform,
                self.kernel.clone(),
                event,
                Some(sink),
                Some(route),
            )
            .await
            .map(|_| ())
    }
    async fn shutdown_ingress(&self) {
        self.ingress.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    struct Sink;
    #[async_trait::async_trait]
    impl RuntimeActionSink for Sink {
        async fn send_action(&self, _: Value) -> Result<()> {
            Ok(())
        }
        async fn call_action(&self, _: Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }
    #[tokio::test]
    async fn cancelled_source_session_drops_only_its_own_route_generation() {
        let router = Arc::new(RuntimeActionRouter::new());
        let route = RuntimeBotRoute::new("fixture", "instance-a", "bot-a");
        let old = Lease {
            generation: router.register(route.clone(), Arc::new(Sink)),
            router: router.clone(),
            route: route.clone(),
        };
        let new = Lease {
            generation: router.register(route.clone(), Arc::new(Sink)),
            router: router.clone(),
            route: route.clone(),
        };
        drop(old);
        assert!(router.contains(&route));
        let task = tokio::spawn(async move {
            let _lease = new;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert!(!router.contains(&route));
    }
}
