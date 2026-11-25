//! 单个插件实例的执行租约与批次句柄；宿主集合锁仅用于保护这些轻量可克隆句柄的列表快照。
use super::*;
use std::future::Future;
use tokio::sync::{Mutex, OwnedMutexGuard, watch};

pub(super) const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// 每次激活都会递增的调用批次号（epoch）；一旦停用退役便永久失效，即使同一实例再次启用也会分配新批次号。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EpochState {
    Preparing,
    Active,
    Retired,
}

#[derive(Clone)]
pub(super) struct InvocationEpoch {
    cancelled: watch::Sender<EpochState>,
    active: Arc<RwLock<()>>,
}

impl InvocationEpoch {
    pub(super) fn new() -> Self {
        Self {
            cancelled: watch::channel(EpochState::Active).0,
            active: Arc::new(RwLock::new(())),
        }
    }
    pub(super) fn preparing() -> Self {
        let epoch = Self::new();
        epoch.cancelled.send_replace(EpochState::Preparing);
        epoch
    }
    pub(super) fn is_preparing(&self) -> bool {
        *self.cancelled.borrow() == EpochState::Preparing
    }
    fn activate(&self) {
        self.cancelled.send_if_modified(|state| {
            if *state == EpochState::Preparing {
                *state = EpochState::Active;
                true
            } else {
                false
            }
        });
    }
    pub(super) fn cancel(&self) {
        self.cancelled.send_replace(EpochState::Retired);
    }
    pub(super) async fn quiesce(&self) {
        let _all_calls_finished = self.active.write().await;
    }
    pub(super) async fn run<F: Future>(&self, future: F) -> Option<F::Output> {
        let mut cancelled = self.cancelled.subscribe();
        if *cancelled.borrow() != EpochState::Active {
            return None;
        }
        tokio::select! {
            biased;
            _ = cancelled.changed() => None,
            result = async {
                let _active = self.active.read().await;
                future.await
            } => Some(result),
        }
    }
}

#[derive(Clone)]
pub(super) struct HostedPlugin {
    name: Arc<str>,
    pub(super) plugin: Arc<RwLock<LoadedPlugin>>,
    epoch: Arc<std::sync::RwLock<InvocationEpoch>>,
}
impl From<LoadedPlugin> for HostedPlugin {
    fn from(plugin: LoadedPlugin) -> Self {
        Self {
            name: plugin.name().into(),
            plugin: Arc::new(RwLock::new(plugin)),
            epoch: Arc::new(std::sync::RwLock::new(InvocationEpoch::new())),
        }
    }
}
impl HostedPlugin {
    pub(super) fn with_epoch(plugin: LoadedPlugin, epoch: InvocationEpoch) -> Self {
        let hosted = Self::from(plugin);
        *hosted.epoch.write().unwrap_or_else(|e| e.into_inner()) = epoch;
        hosted
    }
    pub(super) fn name(&self) -> &str {
        &self.name
    }
    pub(super) fn invocation(&self) -> PluginInvocation {
        PluginInvocation {
            hosted: self.clone(),
            epoch: self.epoch.read().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
    pub(super) async fn quiesce(&self) {
        let epoch = self.invocation().epoch;
        epoch.quiesce().await;
    }
    pub(super) fn retire(&self) {
        self.epoch
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .cancel();
    }
    /// 仅在同时持有该插件的生命周期操作锁与实例写锁时调用，用于递增并签发新的调用批次号。
    pub(super) fn renew(&self) -> InvocationEpoch {
        self.retire();
        InvocationEpoch::preparing()
    }
    pub(super) fn publish(&self, epoch: &InvocationEpoch) {
        *self.epoch.write().unwrap_or_else(|e| e.into_inner()) = epoch.clone();
    }
}

#[derive(Clone)]
pub(crate) struct PluginInvocation {
    pub(super) hosted: HostedPlugin,
    pub(super) epoch: InvocationEpoch,
}
impl PluginInvocation {
    pub(super) fn name(&self) -> &str {
        self.hosted.name()
    }
    pub(super) async fn read(&self) -> Option<tokio::sync::RwLockReadGuard<'_, LoadedPlugin>> {
        self.epoch.run(self.hosted.plugin.read()).await
    }
    pub(crate) async fn run<F: Future>(&self, future: F) -> Option<F::Output> {
        self.epoch
            .run(nuono_plugin::core::log_context::scope(
                Some(self.name().to_owned()),
                Box::pin(future),
            ))
            .await
    }
}

impl PluginHost {
    /// 仅当发起等待时的原始插件调用批次仍然有效时才返回对应句柄；
    /// 迟到的旧回复绝不能被投递给重载替换后的新插件实例。
    pub(crate) async fn wait_reply_scope(
        &self,
        invocation: &PluginInvocation,
    ) -> Option<Arc<PluginTaskScope>> {
        invocation
            .run(async {
                self.task_scopes
                    .read()
                    .await
                    .get(invocation.name())
                    .filter(|scope| !scope.is_closed())
                    .cloned()
            })
            .await
            .flatten()
    }

    /// 获取生命周期准入读锁：仅宿主整体关闭（shutdown）会获取写锁排他，不同插件的启停与重载可并发持有读锁。
    pub(super) async fn admit_lifecycle(&self) -> Result<tokio::sync::RwLockReadGuard<'_, ()>> {
        if self.closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::error::NuonoError::Conflict(
                "插件宿主正在关闭".into(),
            ));
        }
        let guard = self.lifecycle_admission.read().await;
        if self.closing.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::error::NuonoError::Conflict(
                "插件宿主正在关闭".into(),
            ));
        }
        Ok(guard)
    }

    pub(super) async fn plugin_snapshot(&self) -> Vec<PluginInvocation> {
        self.plugins
            .read()
            .await
            .iter()
            .map(HostedPlugin::invocation)
            .collect()
    }
    pub(super) async fn hosted_plugin(&self, name: &str) -> Option<HostedPlugin> {
        self.plugins
            .read()
            .await
            .iter()
            .find(|p| p.name() == name)
            .cloned()
    }
    fn plugin_operation_lock(&self, name: &str) -> Arc<Mutex<()>> {
        let mut operations = self
            .lifecycle_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        operations.retain(|_, operation| operation.strong_count() > 0);
        if let Some(operation) = operations.get(name).and_then(std::sync::Weak::upgrade) {
            return operation;
        }
        let operation = Arc::new(Mutex::new(()));
        operations.insert(name.to_owned(), Arc::downgrade(&operation));
        operation
    }
    pub(super) async fn plugin_operation(&self, name: &str) -> OwnedMutexGuard<()> {
        self.plugin_operation_lock(name).lock_owned().await
    }
    pub(super) fn try_plugin_operation(&self, name: &str) -> Option<OwnedMutexGuard<()>> {
        self.plugin_operation_lock(name).try_lock_owned().ok()
    }
    pub(super) async fn remove_hosted_plugin(&self, hosted: &HostedPlugin) {
        hosted.retire();
        self.plugins
            .write()
            .await
            .retain(|p| !Arc::ptr_eq(&p.plugin, &hosted.plugin));
    }
    pub(super) async fn lifecycle_call<F, T>(future: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        tokio::time::timeout(LIFECYCLE_TIMEOUT, Self::call_with_catch(future))
            .await
            .map_err(|_| anyhow::anyhow!("插件生命周期回调超过 5 秒"))?
    }
}

/// 激活周期的同步守卫：即使启用或重载 Future 被中途取消、尚未运行异步清理，也会在析构时立即撤销调用批次（fail-closed）。
pub(super) struct ActivationGuard {
    epoch: InvocationEpoch,
    scope: Option<Arc<PluginTaskScope>>,
    committed: bool,
}
impl ActivationGuard {
    pub(super) fn new(epoch: InvocationEpoch) -> Self {
        Self {
            epoch,
            scope: None,
            committed: false,
        }
    }
    pub(super) fn attach(&mut self, scope: Arc<PluginTaskScope>) {
        self.scope = Some(scope);
    }
    pub(super) fn prepared(mut self) {
        self.committed = true;
    }
    pub(super) fn commit(mut self) {
        self.epoch.activate();
        if let Some(scope) = &self.scope {
            scope.open_business();
        }
        self.committed = true;
    }
}
impl Drop for ActivationGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.epoch.cancel();
            if let Some(scope) = &self.scope {
                scope.abort_all();
            }
        }
    }
}
