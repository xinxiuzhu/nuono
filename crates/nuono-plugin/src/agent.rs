//! Agent 扩展工具与运行时子模块重导出。
//!
//! 顶层导出的类型是插件声明与实现 Agent 工具的标准接口；下方同时重导出应用层 Agent
//! 策略、注册表及执行子模块供宿主集成与插件测试使用。

pub mod account;

pub use nuono_ai::tools::{ToolContext, ToolInputSchema};
pub use nuono_app::agent::contracts::{
    AgentContext, AgentNativeContext, AgentTool, AgentToolContext, AgentToolImage, AgentToolOutput,
    AutonomyLevel, MAX_TOOL_IMAGE_BYTES, MAX_TOOL_IMAGES, MAX_TOOL_MEDIA_BYTES, ToolExecution,
    ToolExecutionStatus, ToolParameter, ToolParameterType, ToolResource, ToolResourceMode,
    ToolRiskLevel, ToolScheduling,
};

/// 应用层 Agent 策略类型重导出。
pub use nuono_app::agent::AgentPolicy;
/// 供宿主集成与插件测试使用的工具注册表重导出。
pub use nuono_app::agent::tool::{BuiltinToolRegistry, ToolRegistry};
/// 应用层 Agent 编排与存储子模块重导出。
pub use nuono_app::agent::{
    children, execution, input, memory, policy, ports, run, runtime, store, tool,
};
