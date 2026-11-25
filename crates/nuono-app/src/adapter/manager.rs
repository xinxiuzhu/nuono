use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::adapter::types::{AdapterCapabilitiesInfo, AdapterConfig, AdapterInfo, AdapterStatus};
use crate::platform::PlatformAdapter;
use nuono_error::{NuonoError, Result};

const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(5);

struct ManagedTask {
    handle: JoinHandle<()>,
    stop_tx: Option<watch::Sender<bool>>,
    stop_deadline: Option<Instant>,
}

impl ManagedTask {
    fn new(handle: JoinHandle<()>, stop_tx: Option<watch::Sender<bool>>) -> Self {
        Self {
            handle,
            stop_tx,
            stop_deadline: None,
        }
    }

    async fn stop(&mut self) {
        if self.stop_deadline.is_none() {
            let now = Instant::now();
            let deadline = if let Some(stop_tx) = &self.stop_tx {
                // 接收端可能已在任务执行结束时提前释放，但仍需给予相同的优雅退出宽限期以完成收尾清理。
                stop_tx.send_replace(true);
                now + GRACEFUL_STOP_TIMEOUT
            } else {
                self.handle.abort();
                now
            };
            self.stop_deadline = Some(deadline);
        }
        let deadline = self.stop_deadline.expect("stop deadline initialized");
        if tokio::time::timeout_at(deadline, &mut self.handle)
            .await
            .is_err()
        {
            self.handle.abort();
            let _ = (&mut self.handle).await;
        }
    }
}

impl Drop for ManagedTask {
    fn drop(&mut self) {
        if let Some(stop_tx) = &self.stop_tx {
            let _ = stop_tx.send(true);
        }
        self.handle.abort();
    }
}

#[derive(Default)]
struct AdapterLifecycle {
    task: Option<ManagedTask>,
    removing: bool,
}

impl AdapterLifecycle {
    async fn stop_task(&mut self) {
        if let Some(task) = &mut self.task {
            // 等待任务退出期间必须保留句柄；即使外部调用被取消，也不能丢失清理状态或让新旧任务并发重叠。
            task.stop().await;
            self.task.take();
        }
    }
}

struct AdapterEntry {
    config: AdapterConfig,
    adapter: Arc<dyn PlatformAdapter>,
    status: Mutex<AdapterStatus>,
    lifecycle: AsyncMutex<AdapterLifecycle>,
}

impl AdapterEntry {
    fn status(&self) -> AdapterStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_status(&self, status: AdapterStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = status;
    }

    fn info(&self) -> AdapterInfo {
        let caps = self.adapter.capabilities();
        AdapterInfo {
            id: self.config.id.clone(),
            adapter_type: self.config.adapter_type.clone(),
            name: self.adapter.platform_name().to_string(),
            status: self.status(),
            enabled: self.config.enabled,
            platform_name: self.adapter.platform_name().to_string(),
            capabilities: AdapterCapabilitiesInfo {
                send_message: caps.send_message,
                delete_message: caps.delete_message,
                send_image: caps.send_image,
                send_voice: caps.send_voice,
                send_video: caps.send_video,
                group_ban: caps.group_ban,
            },
        }
    }
}

/// 平台适配器注册表：状态展示与生命周期句柄管理。
///
/// 各平台的事件流由 server 直接送入 EventKernel；`attach_task` 把连接任务交给
/// 管理器。停止与替换在每个实例内串行，清理完成前保留句柄，且不占用注册表锁。
pub struct AdapterManager {
    adapters: RwLock<HashMap<String, Arc<AdapterEntry>>>,
}

impl AdapterManager {
    pub fn new() -> Self {
        Self {
            adapters: RwLock::new(HashMap::new()),
        }
    }

    async fn entry(&self, id: &str) -> Result<Arc<AdapterEntry>> {
        self.adapters
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| NuonoError::NotFound(format!("Adapter {id}")))
    }

    /// 为 Running 实例托管任务；安装句柄前等待旧托管任务退出，有停止信号时先给五秒收尾。
    /// 传入句柄已经运行；需要避免连接重叠的调用方应在 spawn 前停止旧实例。
    pub async fn attach_task(
        &self,
        id: &str,
        handle: JoinHandle<()>,
        stop_tx: Option<watch::Sender<bool>>,
    ) {
        // 用 ManagedTask 包装传入句柄，确保当前调用在等待锁或等待旧任务清理期间被取消时，新任务也会在 Drop 中被中止。
        let mut incoming = ManagedTask::new(handle, stop_tx);
        let Ok(entry) = self.entry(id).await else {
            warn!("attach_task: adapter {} 未注册，中止并等待后台任务", id);
            incoming.handle.abort();
            incoming.stop().await;
            return;
        };
        let mut lifecycle = entry.lifecycle.lock().await;
        if lifecycle.removing || entry.status() != AdapterStatus::Running {
            drop(lifecycle);
            incoming.handle.abort();
            incoming.stop().await;
            return;
        }
        lifecycle.stop_task().await;
        lifecycle.task = Some(incoming);
    }

    pub async fn register(
        &self,
        config: AdapterConfig,
        adapter: Arc<dyn PlatformAdapter>,
    ) -> Result<()> {
        let entry = Arc::new(AdapterEntry {
            config: config.clone(),
            adapter,
            status: Mutex::new(AdapterStatus::Stopped),
            lifecycle: AsyncMutex::new(AdapterLifecycle::default()),
        });

        let mut adapters = self.adapters.write().await;
        if adapters.contains_key(&config.id) {
            return Err(NuonoError::Validation(format!(
                "Adapter {} already exists",
                config.id
            )));
        }
        adapters.insert(config.id.clone(), entry);
        info!(
            "Adapter registered: {} ({})",
            config.id, config.adapter_type
        );
        Ok(())
    }

    pub async fn start(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).await?;
        let lifecycle = entry.lifecycle.lock().await;
        if lifecycle.removing {
            return Err(NuonoError::NotFound(format!("Adapter {id}")));
        }
        if entry.status() == AdapterStatus::Running {
            warn!("Adapter {} is already running", id);
            return Ok(());
        }
        entry.set_status(AdapterStatus::Running);
        info!("Adapter {} started", id);
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).await?;
        let mut lifecycle = entry.lifecycle.lock().await;
        lifecycle.stop_task().await;
        entry.set_status(AdapterStatus::Stopped);
        info!("Adapter {} stopped", id);
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        let ids: Vec<String> = self.adapters.read().await.keys().cloned().collect();
        for id in ids {
            if let Err(error) = self.stop(&id).await
                && !matches!(error, NuonoError::NotFound(_))
            {
                return Err(error);
            }
        }
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        let entry = self.entry(id).await?;
        let mut lifecycle = entry.lifecycle.lock().await;
        // 在清理完成前保持 removing 墓碑标记（即便当前等待方被取消），防止同 ID 的新注册绕过旧实例的清理流程。
        lifecycle.removing = true;
        lifecycle.stop_task().await;
        entry.set_status(AdapterStatus::Stopped);
        let mut adapters = self.adapters.write().await;
        if adapters
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &entry))
        {
            adapters.remove(id);
        }
        info!("Adapter {} removed", id);
        Ok(())
    }

    pub async fn get_all(&self) -> Vec<AdapterInfo> {
        let entries: Vec<_> = self.adapters.read().await.values().cloned().collect();
        entries.iter().map(|entry| entry.info()).collect()
    }

    pub async fn get(&self, id: &str) -> Option<AdapterInfo> {
        self.entry(id).await.ok().map(|entry| entry.info())
    }

    pub async fn get_adapter(&self, id: &str) -> Option<Arc<dyn PlatformAdapter>> {
        self.entry(id).await.ok().map(|entry| entry.adapter.clone())
    }

    pub async fn update_status(&self, id: &str, status: AdapterStatus) {
        if let Ok(entry) = self.entry(id).await {
            let lifecycle = entry.lifecycle.lock().await;
            if !lifecycle.removing {
                entry.set_status(status);
            }
        }
    }
}

impl Default for AdapterManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod fault_tests;
