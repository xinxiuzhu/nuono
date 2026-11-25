//! 插件 SDK 与 Agent 运行时共享的工具扩展接口定义。
//!
//! 工具实现方只需依赖本模块的类型，无需引入工具注册表、内置工具或调度器。
//! 具体的工具执行与并发资源协调分别位于 `tool` 和 `scheduling` 模块（`tool` 模块也重导出了这些类型以保持兼容）。
//! `AgentToolContext` 保留了现有的存储层和记忆策略字段，作为 `nuono-app` 内部的模块边界。

use super::store::AgentStore;
pub use super::{AgentContext, AgentNativeContext, AutonomyLevel};
use async_trait::async_trait;
use nuono_ai::media::validate_images;
pub use nuono_ai::media::{
    AgentToolImage, MAX_TOOL_IMAGE_BYTES, MAX_TOOL_IMAGES, MAX_TOOL_MEDIA_BYTES,
};
pub use nuono_ai::tools::ToolContext;
use nuono_error::Result;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// Agent tool 风险等级。
///
/// `ReadOnly` 不应改变持久化状态；`LowRisk` 允许受控、可恢复的副作用；
/// `RequiresApproval` 必须先创建人工审批；`AdminOnly` 仅在最高自主等级开放。该枚举只描述
/// 权限门槛，不会验证调用者身份或自动回滚副作用；并发由 `ToolScheduling` 独立声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolRiskLevel {
    ReadOnly,
    LowRisk,
    RequiresApproval,
    AdminOnly,
}

impl ToolRiskLevel {
    pub fn allowed_by(self, autonomy_level: AutonomyLevel) -> bool {
        match autonomy_level {
            AutonomyLevel::ReplyOnly => false,
            AutonomyLevel::LowRiskTools => matches!(self, Self::ReadOnly | Self::LowRisk),
            AutonomyLevel::ApprovedTools => {
                matches!(
                    self,
                    Self::ReadOnly | Self::LowRisk | Self::RequiresApproval
                )
            }
            AutonomyLevel::AdminApproved => true,
        }
    }
}

/// Tool 参数类型，先用于文档、Dashboard 和后续 function-call schema。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolParameterType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameter {
    pub name: String,
    pub description: String,
    pub parameter_type: ToolParameterType,
    pub required: bool,
}

impl ToolParameter {
    pub fn new(
        name: impl Into<String>,
        parameter_type: ToolParameterType,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameter_type,
            required: false,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolExecution {
    pub tool_name: String,
    pub risk_level: Option<ToolRiskLevel>,
    pub status: ToolExecutionStatus,
    pub input: Value,
    pub output: Value,
    /// 独立的图片附件列表（以多模态消息形式传递，不会混入传给模型的 JSON 文本中）。
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_images"
    )]
    pub images: Vec<AgentToolImage>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolExecutionStatus {
    Started,
    Succeeded,
    Rejected,
    PendingApproval,
    Timeout,
    Failed,
}

/// 工具访问的共享资源。相同键在同一个宿主工具注册表中协调，不自动添加插件前缀。
/// 声明必须覆盖所有可能冲突的访问；它不替代数据库事务或宿主之外的锁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResource {
    pub key: String,
    pub mode: ToolResourceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResourceMode {
    Read,
    Write,
}

impl ToolResource {
    pub fn read(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            mode: ToolResourceMode::Read,
        }
    }

    pub fn write(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            mode: ToolResourceMode::Write,
        }
    }
}

/// 风险等级控制权限，本声明独立控制并发。未声明的工具独占整个调度器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolScheduling {
    #[default]
    Exclusive,
    /// 无共享状态的工具可声明空资源；资源允许共享读，写与任何访问互斥。
    Concurrent { resources: Vec<ToolResource> },
}

impl ToolScheduling {
    pub fn concurrent(resources: impl IntoIterator<Item = ToolResource>) -> Self {
        Self::Concurrent {
            resources: resources.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentToolOutput {
    pub value: Value,
    pub images: Vec<AgentToolImage>,
}
impl AgentToolOutput {
    pub fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
        }
    }
    pub fn with_images(mut self, images: Vec<AgentToolImage>) -> Result<Self> {
        self.images = images;
        self.validate()?;
        Ok(self)
    }
    pub fn validate(&self) -> Result<()> {
        validate_images(&self.images)
    }
}

fn deserialize_images<'de, D: Deserializer<'de>>(
    de: D,
) -> std::result::Result<Vec<AgentToolImage>, D::Error> {
    let images = Vec::<AgentToolImage>::deserialize(de)?;
    validate_images(&images).map_err(serde::de::Error::custom)?;
    Ok(images)
}

/// Agent 运行时传递给扩展工具的执行上下文。
///
/// 为保持现有调用方兼容，存储层接口与记忆策略字段仍保持公开。
#[derive(Debug, Clone)]
pub struct AgentToolContext {
    pub agent: AgentContext,
    pub db: Option<Arc<dyn AgentStore>>,
    pub memory_policy: Option<super::memory::MemoryPolicy>,
}

impl AgentToolContext {
    pub fn new(agent: AgentContext) -> Self {
        Self {
            agent,
            db: None,
            memory_policy: None,
        }
    }

    pub fn with_store(agent: AgentContext, db: Arc<dyn AgentStore>) -> Self {
        Self {
            agent,
            db: Some(db),
            memory_policy: None,
        }
    }
    pub fn with_memory_policy(mut self, policy: super::memory::MemoryPolicy) -> Self {
        self.memory_policy = Some(policy);
        self
    }
    pub fn with_db(agent: AgentContext, db: impl AgentStore + 'static) -> Self {
        Self {
            agent,
            db: Some(Arc::new(db)),
            memory_policy: None,
        }
    }
}

impl TryFrom<AgentToolContext> for ToolContext {
    type Error = nuono_error::NuonoError;

    fn try_from(ctx: AgentToolContext) -> Result<Self> {
        let agent = ctx.agent;
        if let Some(native) = agent.native {
            return Ok(Self {
                bot: native.bot,
                actor: native.actor,
                conversation: native.conversation,
                message_id: agent.message_id,
            });
        }
        // 只有旧版 OneBot 路径可以直接从数字 ID 构造身份；其他平台必须携带各自的原生命名空间身份信息。
        if agent.platform != "onebot" {
            return Err(nuono_error::NuonoError::Validation(
                "此平台的工具调用需要原生身份上下文".into(),
            ));
        }
        use nuono_domain::{
            BotIdentity, ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform,
        };
        let identity = |kind, value: String| {
            IdentityRef::new(
                Platform::OneBot,
                kind,
                IdentityScheme::Numeric,
                value,
                None::<String>,
            )
            .map_err(|_| nuono_error::NuonoError::Validation("工具调用身份无效".into()))
        };
        let actor = identity(IdentityKind::User, agent.user_id.to_string())?;
        let conversation = match agent.group_id {
            Some(id) => ConversationRef::Group {
                group: identity(IdentityKind::Group, id.to_string())?,
            },
            None => ConversationRef::Direct {
                participant: actor.clone(),
                conversation: None,
            },
        };
        Ok(Self {
            bot: BotIdentity::new(
                agent.bot_id.to_string(),
                identity(IdentityKind::Bot, agent.bot_id.to_string())?,
                None,
            )
            .map_err(|_| nuono_error::NuonoError::Validation("工具调用 Bot 身份无效".into()))?,
            actor,
            conversation: Some(conversation),
            message_id: agent.message_id,
        })
    }
}

/// 可由 Agent 调用的工具。
///
/// 实现者应把 `name` 视为稳定的审计标识，准确声明风险等级，并自行校验所有输入。若
/// `call` 可能产生外部副作用，应支持幂等重试；执行超时并不保证底层 I/O 已取消。
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        Vec::new()
    }

    /// 完整的 JSON Schema 对象定义。宿主会在工具注册时对其进行快照与合法性校验，
    /// 并在发起审批或实际执行前校验调用参数。支持文档内本地引用（`$ref`），拒绝外部引用；返回 `None` 时沿用 `parameters()` 的简化定义。
    fn input_schema(&self) -> Option<Value> {
        None
    }

    /// 为本次调用声明共享资源。默认独占；仅在完整声明冲突资源后启用并发。
    /// 声明由宿主代码产生，必须在本次调用期间稳定；模型参数不能自行授权并发。
    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::Exclusive
    }

    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value>;

    /// 支持返回图片附件的工具调用入口（默认退化为调用 `call`）。
    /// 宿主对其应用与 `call` 完全一致的风险控制、并发调度、超时限制与生命周期检查。
    async fn call_with_media(
        &self,
        ctx: AgentToolContext,
        input: Value,
    ) -> Result<AgentToolOutput> {
        self.call(ctx, input).await.map(AgentToolOutput::new)
    }
}
