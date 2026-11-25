//! 插件声明准备与持续业务运行之间的边界。
use super::*;
use std::sync::atomic::Ordering;

pub(super) struct PendingActivation {
    account_merge: Option<Arc<dyn nuono_app::account_merge::AccountMergeParticipant>>,
}

impl PendingActivation {
    fn from_declaration(declaration: &PluginDeclaration) -> Self {
        Self {
            account_merge: declaration.account_merge.clone(),
        }
    }

    fn publish(self, host: &PluginHost, name: &str) -> Result<()> {
        if let Some(participant) = self.account_merge {
            nuono_app::account_merge::register_account_merge_participant(name, participant)?;
        }
        host.extensions.activate(name);
        Ok(())
    }
}

impl PluginHost {
    /// 在注入插件前进入准备阶段；生产宿主应直接使用准备模式构造器。
    ///
    /// 此方法不用于暂停已经运行的实例，运行中的插件应通过禁用入口停止。
    pub fn prepare_business(&self) -> Result<()> {
        let _admission = self
            .lifecycle_admission
            .try_write()
            .map_err(|_| crate::error::NuonoError::Conflict("插件加载已经开始".into()))?;
        let plugins = self
            .plugins
            .try_read()
            .map_err(|_| crate::error::NuonoError::Conflict("插件加载已经开始".into()))?;
        if !plugins.is_empty() || self.closing.load(Ordering::Acquire) {
            return Err(crate::error::NuonoError::Conflict(
                "只能在新宿主加载插件前进入准备阶段".into(),
            ));
        }
        self.business_running.store(false, Ordering::Release);
        Ok(())
    }

    /// 立即撤销后续启动许可；随后由 `shutdown` 等待全部任务和回调收尾。
    pub fn request_business_shutdown(&self) {
        {
            let _publication = self
                .business_publication
                .lock()
                .expect("business publication lock poisoned");
            self.closing.store(true, Ordering::Release);
            self.business_running.store(false, Ordering::Release);
        }
        let scopes = self
            .task_scopes
            .try_read()
            .map(|scopes| scopes.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for scope in scopes {
            scope.abort_all();
        }
    }

    pub(super) fn commit_activation(&self, activation: ActivationGuard) -> Result<()> {
        let _publication = self
            .business_publication
            .lock()
            .expect("business publication lock poisoned");
        if self.closing.load(Ordering::Acquire) {
            return Err(crate::error::NuonoError::Conflict(
                "插件宿主正在关闭".into(),
            ));
        }
        activation.commit();
        Ok(())
    }

    /// 是否已经允许插件开始业务；与外部连接是否健康无关。
    pub fn business_is_running(&self) -> bool {
        self.business_running.load(Ordering::Acquire) && !self.closing.load(Ordering::Acquire)
    }

    pub(super) async fn publish_or_defer_extensions(
        &self,
        declaration: &PluginDeclaration,
    ) -> Result<()> {
        let pending = PendingActivation::from_declaration(declaration);
        if self.business_is_running() {
            pending.publish(self, &declaration.name)
        } else {
            self.pending_activations
                .write()
                .await
                .insert(declaration.name.clone(), pending);
            Ok(())
        }
    }

    /// 管理入口开放且初始管理员已创建后，启动各插件的业务周期。
    ///
    /// 调用幂等，并与加载、启停、重建及宿主关闭串行。插件应在 `on_enable`
    /// 注册受管后台任务，外部服务的连接和重试在任务内部进行。
    /// 单个插件启动失败会收束该插件并继续启动其余插件。
    pub async fn start_business(&self) -> Result<()> {
        let _admission = self.lifecycle_admission.write().await;
        if self.closing.load(Ordering::Acquire) {
            return Err(crate::error::NuonoError::Conflict(
                "插件宿主正在关闭".into(),
            ));
        }
        if self.business_is_running() {
            return Ok(());
        }
        let plugins = self.plugins.read().await.clone();
        for hosted in plugins {
            let name = hosted.name().to_string();
            let _operation = self.plugin_operation(&name).await;
            let epoch = hosted.invocation().epoch;
            let pending = self.pending_activations.write().await.remove(&name);
            let Some(pending) = pending else { continue };
            if !epoch.is_preparing() {
                continue;
            }
            let mut activation = ActivationGuard::new(epoch);
            if let Some(scope) = self.task_scopes.read().await.get(&name).cloned() {
                activation.attach(scope);
            }
            let mut plugin = hosted.plugin.write().await;
            let started = self
                .diagnostic_metrics(&name)
                .observe("on_enable", Self::lifecycle_call(plugin.on_enable()))
                .await;
            if self.closing.load(Ordering::Acquire) {
                return Err(crate::error::NuonoError::Conflict(
                    "插件宿主正在关闭".into(),
                ));
            }
            let started = started.and_then(|()| pending.publish(self, &name).map_err(Into::into));
            if let Err(error) = started {
                drop(activation);
                let cleanup = self
                    .cleanup_failed_activation(&name, Some(&mut plugin))
                    .await;
                self.remove_hosted_plugin(&hosted).await;
                warn!(target: "nuono::plugins", plugin = %name,
                    error = %activation_failed_error(&name, error.to_string(), cleanup),
                    "插件运行启动失败，已停止该插件");
                continue;
            }
            // 调用入口发布后才放行后台任务，避免任务使用尚未提交的能力。
            self.commit_activation(activation)?;
            info!(target: "nuono::plugins", plugin = %name, "运行已启动（外部连接状态独立检查）");
        }
        {
            let _publication = self
                .business_publication
                .lock()
                .expect("business publication lock poisoned");
            if self.closing.load(Ordering::Acquire) {
                return Err(crate::error::NuonoError::Conflict(
                    "插件宿主正在关闭".into(),
                ));
            }
            self.business_running.store(true, Ordering::Release);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/business_tests.rs"]
mod tests;
