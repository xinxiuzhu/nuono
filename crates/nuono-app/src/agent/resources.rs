//! Agent 活跃任务与并发模型请求的进程内准入配额。
use std::sync::Arc;

use nuono_error::{NuonoError, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Agent 运行时的并发资源上限（同一个 `AgentRuntime` 克隆出的所有实例共享此配额）。
///
/// 根任务在通过准入（甚至尚未被首次轮询）时即占用一个活跃执行槽位；
/// 当进入持久化暂停（等待人工审批或等待子任务）时释放该槽位；
/// 后续恢复根任务、收集子任务结果或独立恢复已审批的子任务时，需重新申请执行槽位。
/// 由已准入父任务在进程内直接驱动的子任务则复用父任务的执行槽位（同时受子任务自身的并发上限约束）。
#[derive(Debug, Clone, Copy)]
pub struct AgentResourceLimits {
    pub max_active_executions: usize,
    pub max_model_requests: usize,
}

impl Default for AgentResourceLimits {
    fn default() -> Self {
        Self {
            max_active_executions: 16,
            max_model_requests: 4,
        }
    }
}

pub(super) struct AgentResources {
    executions: Arc<Semaphore>,
    pub(super) models: Arc<Semaphore>,
}

impl AgentResources {
    pub(super) fn new(limits: AgentResourceLimits) -> Result<Self> {
        if !(1..=1024).contains(&limits.max_active_executions)
            || !(1..=256).contains(&limits.max_model_requests)
        {
            return Err(NuonoError::Validation(
                "Agent 活跃执行名额须为 1 至 1024，模型请求名额须为 1 至 256".into(),
            ));
        }
        Ok(Self {
            executions: Arc::new(Semaphore::new(limits.max_active_executions)),
            models: Arc::new(Semaphore::new(limits.max_model_requests)),
        })
    }

    /// 非阻塞地尝试获取一个活跃执行槽位（不排队等待；若已满则立即返回冲突错误，已持久化的暂停任务可稍后重试）。
    pub(super) fn try_execution(&self) -> Result<OwnedSemaphorePermit> {
        self.executions
            .clone()
            .try_acquire_owned()
            .map_err(|_| NuonoError::Conflict("Agent 活跃执行名额已满，请稍后重试".into()))
    }
}
