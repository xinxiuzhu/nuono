//! 运行时事件处理管线：按既定阶段顺序执行，并区分“事件已消费（阻断后续阶段）”与“阶段执行报错”语义。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEventStage {
    WaitSession,
    Filter,
    CoreCommands,
    PluginHost,
    TriggerDispatch,
    Matcher,
    AgentRuntime,
    EventPublish,
}

impl RuntimeEventStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaitSession => "wait_session",
            Self::Filter => "filter",
            Self::CoreCommands => "core_commands",
            Self::PluginHost => "plugin_host",
            Self::TriggerDispatch => "trigger_dispatch",
            Self::Matcher => "matcher",
            Self::AgentRuntime => "agent_runtime",
            Self::EventPublish => "event_publish",
        }
    }
}

impl fmt::Display for RuntimeEventStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 真实运行时事件管线顺序定义。
///
/// 顺序是语义的一部分：等待回复最先抢占，订阅/过滤在插件前拒绝，插件、触发器、
/// matcher、agent 依次竞争消费权；只有仍未 `blocked` 的消息才会持久化并发布。
#[derive(Debug, Clone)]
pub struct RuntimeEventPipeline {
    stages: Vec<RuntimeEventStage>,
}

impl RuntimeEventPipeline {
    pub fn new(stages: Vec<RuntimeEventStage>) -> Self {
        Self { stages }
    }

    pub fn stages(&self) -> &[RuntimeEventStage] {
        &self.stages
    }
}

impl Default for RuntimeEventPipeline {
    fn default() -> Self {
        Self::new(vec![
            RuntimeEventStage::WaitSession,
            RuntimeEventStage::Filter,
            RuntimeEventStage::CoreCommands,
            RuntimeEventStage::PluginHost,
            RuntimeEventStage::TriggerDispatch,
            RuntimeEventStage::Matcher,
            RuntimeEventStage::AgentRuntime,
            RuntimeEventStage::EventPublish,
        ])
    }
}

/// 运行时管线的消费结果。
///
/// `blocked` 表示事件已被某阶段认领或拒绝，不表示处理失败；阶段错误被各阶段
/// 边界记录后通常继续执行，因此调用方应通过外层 `Result` 判断入口级失败。
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeEventProcessResult {
    pub blocked: bool,
    pub is_message: bool,
}

/// 供管线阶段判断的事件处理状态（与底层协议负载和连接句柄解耦）。
/// “是否已消费（`consumed`）”与“阶段是否报错”相互独立。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessingState {
    pub is_message: bool,
    pub consumed: bool,
}

/// 事件阶段上下文接口：具体运行时可在内部携带平台协议数据，对管线编排层仅暴露 `ProcessingState`。
pub trait StageContext: Send {
    fn processing_state(&self) -> ProcessingState;
}

/// 阶段执行失败时的安全错误类别（脱敏后的错误分类名，详细错误堆栈仅记入服务端日志）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{category}")]
pub struct StageFailure {
    pub category: String,
}

impl StageFailure {
    pub fn new(category: impl Into<String>) -> Self {
        Self {
            category: category.into(),
        }
    }
}

/// 单个事件处理阶段的执行器接口（由宿主组合会话等待、过滤器、内置命令、插件宿主与 Agent 运行时实现）。
#[async_trait::async_trait]
pub trait StageExecutor<C: StageContext>: Send + Sync {
    async fn execute_stage(
        &self,
        stage: RuntimeEventStage,
        ctx: &mut C,
    ) -> Result<(), StageFailure>;
}

/// 按既定管线顺序依次执行各阶段，并在每个阶段后通知调用方。
pub async fn process_event<C, E>(
    executor: &E,
    pipeline: &RuntimeEventPipeline,
    ctx: &mut C,
    mut after_stage: impl FnMut(RuntimeEventStage, Option<&StageFailure>) + Send,
) -> RuntimeEventProcessResult
where
    C: StageContext,
    E: StageExecutor<C> + ?Sized,
{
    for &stage in pipeline.stages() {
        let failure = run_stage(executor, stage, ctx).await;
        after_stage(stage, failure.as_ref());
    }
    let state = ctx.processing_state();
    RuntimeEventProcessResult {
        blocked: state.consumed,
        is_message: state.is_message,
    }
}

/// 判断单个阶段是否需要执行，并执行对应阶段逻辑。
/// 注意：阶段返回错误本身不会自动将事件标记为已消费，是否阻断后续阶段由各阶段处理器显式设置。
pub async fn run_stage<C, E>(
    executor: &E,
    stage: RuntimeEventStage,
    ctx: &mut C,
) -> Option<StageFailure>
where
    C: StageContext,
    E: StageExecutor<C> + ?Sized,
{
    let before = ctx.processing_state();
    let executed =
        stage == RuntimeEventStage::WaitSession || (before.is_message && !before.consumed);
    if executed {
        executor.execute_stage(stage, ctx).await.err()
    } else {
        None
    }
}
