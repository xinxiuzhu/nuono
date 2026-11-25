//! 工具注册表、内置基础工具以及基于共享调度器的工具执行逻辑。
//! 公共工具扩展接口定义在 [`super::contracts`] 中，本模块将其原样重导出以兼容旧有引用路径。
pub use super::contracts::{
    AgentTool, AgentToolContext, AgentToolImage, AgentToolOutput, MAX_TOOL_IMAGE_BYTES,
    MAX_TOOL_IMAGES, MAX_TOOL_MEDIA_BYTES, ToolExecution, ToolExecutionStatus, ToolParameter,
    ToolParameterType, ToolResource, ToolResourceMode, ToolRiskLevel, ToolScheduling,
};
use super::scheduling::{ToolPermit, ToolScheduler};
use super::{AgentContext, AutonomyLevel};
use nuono_ai::tools::ToolInputSchema;
use serde_json::Value;

#[path = "tool_media.rs"]
mod media;
pub(super) use media::{append_tool_result, trim_tool_images};

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::time::timeout;

use super::store::{AgentMemoryRecord, AgentMemoryUpdateRecord, AgentStore, MemoryEntry};
use crate::ai::ChatTool;
use nuono_error::{NuonoError, Result};

/// 当长期记忆总开关关闭时，拦截所有内置 `memory.*` 工具的调用（长期记忆与普通短期会话历史相互独立）。
pub(super) fn memory_tool_rejection(name: &str, enabled: bool) -> Option<String> {
    (!enabled
        && matches!(
            name,
            "memory.read" | "memory.write" | "memory.update" | "memory.delete"
        ))
    .then(|| "长期记忆已关闭；请在 AI 回复设置中启用后再使用记忆工具".to_string())
}

pub struct ToolRegistry {
    tools: DashMap<String, Arc<RegisteredTool>>,
    scheduler: ToolScheduler,
}

struct RegisteredTool {
    tool: Arc<dyn AgentTool>,
    schema: Value,
    input: Option<ToolInputSchema>,
}

pub(super) struct ToolPlan {
    pub(super) name: String,
    tool: Option<Arc<RegisteredTool>>,
    pub(super) risk: Option<ToolRiskLevel>,
    pub(super) scheduling: ToolScheduling,
    error: Option<String>,
}

impl ToolPlan {
    pub(super) fn permits_parallel_dispatch(&self) -> bool {
        self.error.is_none()
            && matches!(self.scheduling, ToolScheduling::Concurrent { .. })
            && matches!(
                self.risk,
                Some(ToolRiskLevel::ReadOnly | ToolRiskLevel::LowRisk)
            )
    }
}

pub(super) struct ToolLease {
    pub(super) plan: ToolPlan,
    _permit: ToolPermit,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::with_concurrency_limit(4).expect("valid default tool concurrency")
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 所有会话和审批共享的工具并发上限，默认 4，范围 1..=64。
    pub fn with_concurrency_limit(limit: usize) -> Result<Self> {
        Ok(Self {
            tools: DashMap::new(),
            scheduler: ToolScheduler::new(limit)?,
        })
    }

    pub fn concurrency_limit(&self) -> usize {
        self.scheduler.limit()
    }

    pub(super) fn plan(&self, name: &str, ctx: &AgentContext, input: &Value) -> ToolPlan {
        let tool = self.tools.get(name).map(|entry| entry.value().clone());
        let risk = tool.as_ref().map(|entry| entry.tool.risk_level());
        let input_error = tool
            .as_ref()
            .and_then(|entry| entry.input.as_ref())
            .and_then(|schema| schema.validate(input).err())
            .map(|error| error.to_string());
        let scheduling = tool
            .as_ref()
            .filter(|_| input_error.is_none())
            .map(|entry| entry.tool.scheduling(ctx, input))
            .unwrap_or_default()
            .normalize();
        let (scheduling, error) = match scheduling {
            Ok(scheduling) => (scheduling, input_error),
            Err(error) => (ToolScheduling::Exclusive, Some(error.to_string())),
        };
        ToolPlan {
            name: name.into(),
            tool,
            risk,
            scheduling,
            error,
        }
    }

    pub(super) async fn acquire(&self, plan: ToolPlan) -> ToolLease {
        let permit = self.scheduler.acquire(&plan.scheduling).await;
        ToolLease {
            plan,
            _permit: permit,
        }
    }

    pub(super) fn lease_rejection(&self, lease: &ToolLease) -> Option<String> {
        if let Some(error) = &lease.plan.error {
            return Some(error.clone());
        }
        let current = self
            .tools
            .get(&lease.plan.name)
            .map(|entry| entry.value().clone());
        let same = match (&current, &lease.plan.tool) {
            (Some(current), Some(planned)) => {
                Arc::ptr_eq(current, planned) && Some(current.tool.risk_level()) == lease.plan.risk
            }
            (None, None) => true,
            _ => false,
        };
        (!same).then(|| "工具在排队期间已变更，请重新发起调用".into())
    }

    pub fn register<T>(&self, tool: T) -> Result<()>
    where
        T: AgentTool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    pub fn register_arc(&self, tool: Arc<dyn AgentTool>) -> Result<()> {
        let name = tool.name().trim();
        if name.is_empty() {
            return Err(NuonoError::ConfigValidation(
                "Agent tool name cannot be empty".to_string(),
            ));
        }
        if name == super::children::DELEGATE_TOOL {
            return Err(NuonoError::ConfigValidation(
                "agent.delegate 是宿主保留的编排工具名".into(),
            ));
        }
        let input = tool
            .input_schema()
            .map(ToolInputSchema::compile)
            .transpose()?;
        let schema = input
            .as_ref()
            .map(|input| input.value().clone())
            .unwrap_or_else(|| parameters_to_json_schema(tool.parameters()));
        match self.tools.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(NuonoError::ConfigValidation(
                format!("Agent tool already registered: {name}"),
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(Arc::new(RegisteredTool {
                    tool,
                    schema,
                    input,
                }));
                Ok(())
            }
        }
    }

    pub fn unregister(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.tools.remove(name).map(|(_, entry)| entry.tool.clone())
    }

    pub fn unregister_by_prefix(&self, prefix: &str) -> usize {
        let names = self
            .tools
            .iter()
            .filter_map(|entry| {
                entry
                    .key()
                    .starts_with(prefix)
                    .then(|| entry.key().to_string())
            })
            .collect::<Vec<_>>();
        let count = names.len();
        for name in names {
            self.unregister(&name);
        }
        count
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.tools.get(name).map(|entry| entry.tool.clone())
    }

    pub fn list(&self) -> Vec<Arc<dyn AgentTool>> {
        let mut tools = self
            .tools
            .iter()
            .map(|entry| entry.tool.clone())
            .collect::<Vec<_>>();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        tools
    }

    pub fn chat_tools(&self, autonomy_level: AutonomyLevel) -> Vec<ChatTool> {
        // 先克隆出已注册工具列表并释放 DashMap 分片锁，再调用各工具的元数据方法，避免持锁回调插件代码。
        let registered: Vec<_> = self
            .tools
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        let mut tools: Vec<_> = registered
            .into_iter()
            .filter(|entry| entry.tool.risk_level().allowed_by(autonomy_level))
            .map(|entry| {
                ChatTool::new(
                    entry.tool.name().to_string(),
                    entry.tool.description().to_string(),
                    entry.schema.clone(),
                )
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// 生成向模型暴露的工具列表（若长期记忆未启用，则对主任务与子任务统一隐藏 `memory.*` 工具）。
    pub fn chat_tools_with_memory(
        &self,
        autonomy_level: AutonomyLevel,
        memory_enabled: bool,
    ) -> Vec<ChatTool> {
        self.chat_tools(autonomy_level)
            .into_iter()
            .filter(|tool| memory_tool_rejection(&tool.name, memory_enabled).is_none())
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        autonomy_level: AutonomyLevel,
        ctx: AgentToolContext,
        input: Value,
    ) -> ToolExecution {
        self.execute_with_timeout(name, autonomy_level, ctx, input, None)
            .await
    }

    /// 在自主等级门控后执行工具并生成统一回执。
    ///
    /// 未取得审批的 RequiresApproval 工具一律拒绝；应用服务负责创建审批与策略校验。
    /// timeout 仅限制 `call`；Agent 任务的总时限同时覆盖调度等待。
    pub async fn execute_with_timeout(
        &self,
        name: &str,
        autonomy_level: AutonomyLevel,
        ctx: AgentToolContext,
        input: Value,
        timeout_override: Option<Duration>,
    ) -> ToolExecution {
        let plan = self.plan(name, &ctx.agent, &input);
        let lease = self.acquire(plan).await;
        self.execute_leased(&lease, autonomy_level, ctx, input, timeout_override)
            .await
    }

    pub(super) async fn execute_leased(
        &self,
        lease: &ToolLease,
        autonomy_level: AutonomyLevel,
        ctx: AgentToolContext,
        input: Value,
        timeout_override: Option<Duration>,
    ) -> ToolExecution {
        let name = &lease.plan.name;
        if let Some(error) = self.lease_rejection(lease) {
            return ToolExecution {
                tool_name: name.clone(),
                risk_level: lease.plan.risk,
                status: ToolExecutionStatus::Rejected,
                input,
                output: serde_json::json!({"error":error}),
                images: Vec::new(),
                elapsed_ms: 0,
            };
        }
        let Some(registered) = &lease.plan.tool else {
            return ToolExecution {
                tool_name: name.to_string(),
                risk_level: None,
                status: ToolExecutionStatus::Failed,
                input,
                output: serde_json::json!({"error": "tool not found"}),
                images: Vec::new(),
                elapsed_ms: 0,
            };
        };
        let tool = &registered.tool;

        let risk_level = lease.plan.risk.expect("registered tool risk");
        if !risk_level.allowed_by(autonomy_level)
            || (risk_level == ToolRiskLevel::RequiresApproval
                && autonomy_level != AutonomyLevel::AdminApproved)
        {
            return ToolExecution {
                tool_name: name.to_string(),
                risk_level: Some(risk_level),
                status: ToolExecutionStatus::Rejected,
                input,
                output: serde_json::json!({
                    "error": "tool rejected by autonomy level",
                    "risk_level": risk_level,
                    "autonomy_level": autonomy_level,
                }),
                images: Vec::new(),
                elapsed_ms: 0,
            };
        }

        let started = Instant::now();
        let timeout_duration = timeout_override.unwrap_or_else(|| tool.timeout());
        let result = timeout(timeout_duration, async {
            let output = tool.call_with_media(ctx, input.clone()).await?;
            output.validate()?;
            Ok::<_, NuonoError>(output)
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        match result {
            Ok(Ok(output)) => ToolExecution {
                tool_name: tool.name().to_string(),
                risk_level: Some(risk_level),
                status: ToolExecutionStatus::Succeeded,
                input,
                output: output.value,
                images: output.images,
                elapsed_ms,
            },
            Ok(Err(e)) => ToolExecution {
                tool_name: tool.name().to_string(),
                risk_level: Some(risk_level),
                status: ToolExecutionStatus::Failed,
                input,
                output: serde_json::json!({"error": e.to_string()}),
                images: Vec::new(),
                elapsed_ms,
            },
            Err(_) => ToolExecution {
                tool_name: tool.name().to_string(),
                risk_level: Some(risk_level),
                status: ToolExecutionStatus::Timeout,
                input,
                output: serde_json::json!({"error": "tool execution timeout"}),
                images: Vec::new(),
                elapsed_ms,
            },
        }
    }
}

fn parameters_to_json_schema(parameters: Vec<ToolParameter>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for parameter in parameters {
        if parameter.required {
            required.push(Value::String(parameter.name.clone()));
        }

        properties.insert(
            parameter.name,
            serde_json::json!({
                "type": parameter_type_json_name(parameter.parameter_type),
                "description": parameter.description,
            }),
        );
    }

    let mut schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });

    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }

    schema
}

fn parameter_type_json_name(parameter_type: ToolParameterType) -> &'static str {
    match parameter_type {
        ToolParameterType::String => "string",
        ToolParameterType::Number => "number",
        ToolParameterType::Integer => "integer",
        ToolParameterType::Boolean => "boolean",
        ToolParameterType::Object => "object",
        ToolParameterType::Array => "array",
    }
}

pub struct BuiltinToolRegistry;

impl BuiltinToolRegistry {
    pub fn reply_only() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    pub fn read_only() -> Arc<ToolRegistry> {
        let registry = ToolRegistry::new();
        Self::register_read_only_into(&registry).expect("built-in tool registration must be valid");
        Arc::new(registry)
    }

    pub fn register_read_only_into(registry: &ToolRegistry) -> Result<()> {
        registry.register(DescribeConversationTool)?;
        registry.register(MemoryReadTool)?;
        registry.register(MemoryWriteTool)?;
        registry.register(MemoryUpdateTool)?;
        registry.register(MemoryDeleteTool)
    }
}

struct DescribeConversationTool;

#[async_trait]
impl AgentTool for DescribeConversationTool {
    fn name(&self) -> &str {
        "conversation.describe"
    }

    fn description(&self) -> &str {
        "Return the current platform, target and sender metadata."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::concurrent([])
    }

    async fn call(&self, ctx: AgentToolContext, _input: Value) -> Result<Value> {
        Ok(serde_json::json!({
            "bot_id": ctx.agent.bot_id,
            "platform": ctx.agent.platform,
            "message_type": ctx.agent.message_type,
            "user_id": ctx.agent.user_id,
            "group_id": ctx.agent.group_id,
            "channel_id": ctx.agent.channel_id,
            "message_id": ctx.agent.message_id,
        }))
    }
}

struct MemoryReadTool;

#[async_trait]
impl AgentTool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory.read"
    }

    fn description(&self) -> &str {
        "Read enabled persistent memories for the current conversation or a specified scope."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::ReadOnly
    }

    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::concurrent([ToolResource::read("agent_memories")])
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        vec![
            ToolParameter::new(
                "scope_type",
                ToolParameterType::String,
                "Optional scope: global, session, user, or group.",
            ),
            ToolParameter::new(
                "scope_key",
                ToolParameterType::String,
                "Optional exact scope key. Defaults to the current context for the selected scope.",
            ),
            ToolParameter::new(
                "limit",
                ToolParameterType::Integer,
                "Maximum number of memories to return.",
            ),
        ]
    }

    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value> {
        let db = ctx.db.as_ref().ok_or_else(|| {
            NuonoError::ConfigValidation("memory.read requires database context".to_string())
        })?;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(12)
            .clamp(1, 50);
        let scopes = if let Some(scope_type) = input
            .get("scope_type")
            .and_then(|v| v.as_str())
            .map(normalize_memory_scope)
            .transpose()?
        {
            let scope_key = match input.get("scope_key").and_then(|v| v.as_str()) {
                Some(value) if !value.trim().is_empty() => value.trim().to_string(),
                _ => default_memory_scope_key(&ctx.agent, &scope_type)?,
            };
            vec![(scope_type, scope_key)]
        } else {
            memory_scopes(&ctx.agent)
        };
        for (scope_type, scope_key) in &scopes {
            authorize_memory_scope(&ctx.agent, scope_type, scope_key)?;
        }
        let policy = ctx.memory_policy.unwrap_or_default();
        let rows = super::memory::recall(
            db.as_ref(),
            &scopes,
            &ctx.agent.content,
            &policy,
            limit as usize,
        )
        .await?;

        Ok(serde_json::json!({
            "memories": rows.into_iter().map(memory_json).collect::<Vec<_>>(),
        }))
    }
}

struct MemoryWriteTool;

#[async_trait]
impl AgentTool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory.write"
    }

    fn description(&self) -> &str {
        "Create a persistent memory. This changes future Agent behavior and requires approval."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::RequiresApproval
    }

    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::concurrent([ToolResource::write("agent_memories")])
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        vec![
            ToolParameter::new(
                "scope_type",
                ToolParameterType::String,
                "Scope: global, session, user, or group. Defaults to session.",
            ),
            ToolParameter::new(
                "scope_key",
                ToolParameterType::String,
                "Optional exact scope key. Defaults to the current context for the selected scope.",
            ),
            ToolParameter::new(
                "memory_key",
                ToolParameterType::String,
                "Stable memory key.",
            )
            .required(),
            ToolParameter::new("content", ToolParameterType::String, "Memory content.").required(),
            ToolParameter::new("tags", ToolParameterType::Array, "Optional string tags."),
            ToolParameter::new(
                "expires_at",
                ToolParameterType::String,
                "Optional expiration time as a decimal UTC Unix-millisecond string.",
            ),
        ]
    }

    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value> {
        let db = ctx.db.as_ref().ok_or_else(|| {
            NuonoError::ConfigValidation("memory.write requires database context".to_string())
        })?;
        let scope_type = input
            .get("scope_type")
            .and_then(|v| v.as_str())
            .map(normalize_memory_scope)
            .transpose()?
            .unwrap_or_else(|| "session".to_string());
        let scope_key = match input.get("scope_key").and_then(|v| v.as_str()) {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => default_memory_scope_key(&ctx.agent, &scope_type)?,
        };
        authorize_memory_scope(&ctx.agent, &scope_type, &scope_key)?;
        let memory_key = required_string(&input, "memory_key")?;
        let content = required_string(&input, "content")?;
        let tags = input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::trim))
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tags_json = serde_json::to_string(&tags).ok();
        let expires_at = memory_expiry_patch(&input)?.flatten();
        let actor = Some(format!(
            "agent:{}:{}",
            ctx.agent.platform, ctx.agent.user_id
        ));

        let row = db
            .upsert_agent_memory(AgentMemoryRecord {
                scope_type,
                scope_key,
                memory_key,
                content,
                tags_json,
                enabled: true,
                expires_at,
                actor,
            })
            .await?;
        let (row, created) = row;
        let (event_type, action) = if created {
            ("agent.memory_created", "tool_created")
        } else {
            ("agent.memory_updated", "tool_upsert_updated")
        };
        record_memory_tool_audit(db, event_type, &row, action).await;

        Ok(serde_json::json!({
            "memory": memory_json(row),
            "created": created,
        }))
    }
}

struct MemoryUpdateTool;

#[async_trait]
impl AgentTool for MemoryUpdateTool {
    fn input_schema(&self) -> Option<Value> {
        let mut schema = parameters_to_json_schema(self.parameters());
        schema["properties"]["id"] = serde_json::json!({"type":["string","integer"],"pattern":"^[1-9][0-9]*$","minimum":1,"maximum":9007199254740991_i64,"description":"Decimal memory ID; large IDs must be strings."});
        Some(schema)
    }

    fn name(&self) -> &str {
        "memory.update"
    }

    fn description(&self) -> &str {
        "Update an existing persistent memory by id. This changes future Agent behavior and requires approval."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::RequiresApproval
    }

    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::concurrent([ToolResource::write("agent_memories")])
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        vec![
            ToolParameter::new("id", ToolParameterType::Integer, "Memory id to update.").required(),
            ToolParameter::new(
                "memory_key",
                ToolParameterType::String,
                "Optional replacement memory key.",
            ),
            ToolParameter::new(
                "content",
                ToolParameterType::String,
                "Optional replacement memory content.",
            ),
            ToolParameter::new(
                "tags",
                ToolParameterType::Array,
                "Optional replacement string tags.",
            ),
            ToolParameter::new(
                "enabled",
                ToolParameterType::Boolean,
                "Optional enabled state.",
            ),
            ToolParameter::new(
                "expires_at",
                ToolParameterType::String,
                "Optional expiration time as a decimal UTC Unix-millisecond string. Pass null or empty string to clear.",
            ),
        ]
    }

    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value> {
        let db = ctx.db.as_ref().ok_or_else(|| {
            NuonoError::ConfigValidation("memory.update requires database context".to_string())
        })?;
        let id = memory_id(&input)?;
        let memory_key = optional_string(&input, "memory_key");
        let content = optional_string(&input, "content");
        let tags_json = input.get("tags").and_then(|value| {
            value.as_array().map(|values| {
                let tags = values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::trim))
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
            })
        });
        let enabled = input.get("enabled").and_then(|value| value.as_bool());
        let expires_at = memory_expiry_patch(&input)?;
        if memory_key.is_none()
            && content.is_none()
            && tags_json.is_none()
            && enabled.is_none()
            && expires_at.is_none()
        {
            return Err(NuonoError::ConfigValidation(
                "memory.update requires at least one field to update".to_string(),
            ));
        }
        let actor = Some(format!(
            "agent:{}:{}",
            ctx.agent.platform, ctx.agent.user_id
        ));
        let existing = db
            .get_agent_memory(id)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("memory {id} not found")))?;
        authorize_memory_scope(&ctx.agent, &existing.scope_type, &existing.scope_key)?;

        let row = db
            .update_agent_memory_in_scopes(
                id,
                &memory_scopes(&ctx.agent),
                AgentMemoryUpdateRecord {
                    scope_type: None,
                    scope_key: None,
                    memory_key,
                    content,
                    tags_json,
                    enabled,
                    expires_at,
                    actor,
                },
            )
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("memory {id} not found")))?;
        record_memory_tool_audit(db, "agent.memory_updated", &row, "tool_updated").await;

        Ok(serde_json::json!({
            "memory": memory_json(row),
        }))
    }
}

struct MemoryDeleteTool;

#[async_trait]
impl AgentTool for MemoryDeleteTool {
    fn input_schema(&self) -> Option<Value> {
        let mut schema = parameters_to_json_schema(self.parameters());
        schema["properties"]["id"] = serde_json::json!({"type":["string","integer"],"pattern":"^[1-9][0-9]*$","minimum":1,"maximum":9007199254740991_i64,"description":"Decimal memory ID; large IDs must be strings."});
        Some(schema)
    }

    fn name(&self) -> &str {
        "memory.delete"
    }

    fn description(&self) -> &str {
        "Delete a persistent memory by id. This changes future Agent behavior and requires approval."
    }

    fn risk_level(&self) -> ToolRiskLevel {
        ToolRiskLevel::RequiresApproval
    }

    fn scheduling(&self, _ctx: &AgentContext, _input: &Value) -> ToolScheduling {
        ToolScheduling::concurrent([ToolResource::write("agent_memories")])
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        vec![
            ToolParameter::new("id", ToolParameterType::Integer, "Memory id to delete.").required(),
        ]
    }

    async fn call(&self, ctx: AgentToolContext, input: Value) -> Result<Value> {
        let db = ctx.db.as_ref().ok_or_else(|| {
            NuonoError::ConfigValidation("memory.delete requires database context".to_string())
        })?;
        let id = memory_id(&input)?;
        let before = db.get_agent_memory(id).await?;
        if let Some(row) = &before {
            authorize_memory_scope(&ctx.agent, &row.scope_type, &row.scope_key)?;
        }
        let deleted = db
            .delete_agent_memory_in_scopes(id, &memory_scopes(&ctx.agent))
            .await?;
        if deleted && let Some(row) = before.as_ref() {
            record_memory_tool_audit(db, "agent.memory_deleted", row, "tool_deleted").await;
        }

        Ok(serde_json::json!({
            "id": id.to_string(),
            "deleted": deleted,
        }))
    }
}

fn required_string(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| NuonoError::ConfigValidation(format!("{key} is required")))
}

fn optional_string(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn memory_id(input: &Value) -> Result<i64> {
    input
        .get("id")
        .and_then(nuono_domain::scalar::wire_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| NuonoError::Validation("id 必须是有效的正整数编号".into()))
}

fn memory_expiry_patch(input: &Value) -> Result<Option<Option<chrono::DateTime<chrono::Utc>>>> {
    let invalid = || NuonoError::Validation("expires_at 必须是有效日期、空字符串或 null".into());
    match input.get("expires_at") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(
            crate::clock::parse_db_ts(value.trim()).ok_or_else(invalid)?,
        ))),
        Some(_) => Err(invalid()),
    }
}

fn authorize_memory_scope(ctx: &AgentContext, scope_type: &str, scope_key: &str) -> Result<()> {
    if memory_scopes(ctx)
        .iter()
        .any(|(kind, key)| kind == scope_type && key == scope_key)
    {
        Ok(())
    } else {
        Err(NuonoError::PermissionDenied(
            "不能访问当前上下文之外的记忆作用域".into(),
        ))
    }
}

fn normalize_memory_scope(value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "global" | "session" | "user" | "group" => Ok(value.trim().to_ascii_lowercase()),
        _ => Err(NuonoError::ConfigValidation(
            "scope_type must be global, session, user, or group".to_string(),
        )),
    }
}

fn default_memory_scope_key(ctx: &AgentContext, scope_type: &str) -> Result<String> {
    match scope_type {
        "global" => Ok("global".to_string()),
        "session" => Ok(ctx.session_id()),
        "user" => Ok(ctx.user_scope_key()),
        "group" => ctx.group_scope_key().ok_or_else(|| {
            NuonoError::ConfigValidation("group scope requires group_id".to_string())
        }),
        _ => Err(NuonoError::ConfigValidation(
            "unsupported memory scope".to_string(),
        )),
    }
}

fn memory_scopes(ctx: &AgentContext) -> Vec<(String, String)> {
    let mut scopes = vec![
        ("global".to_string(), "global".to_string()),
        ("session".to_string(), ctx.session_id()),
        ("user".to_string(), ctx.user_scope_key()),
    ];
    if let Some(scope_key) = ctx.group_scope_key() {
        scopes.push(("group".to_string(), scope_key));
    }
    scopes
}

fn memory_json(row: MemoryEntry) -> Value {
    let tags = row
        .tags_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default();
    serde_json::json!({
        "id": row.id.to_string(),
        "scope_type": row.scope_type,
        "scope_key": row.scope_key,
        "memory_key": row.memory_key,
        "content": row.content,
        "tags": tags,
        "enabled": row.enabled,
        "expires_at": row.expires_at.map(crate::clock::format_wire_ts),
        "updated_at": crate::clock::format_wire_ts(row.updated_at),
    })
}

async fn record_memory_tool_audit(
    db: &Arc<dyn AgentStore>,
    event_type: &str,
    row: &MemoryEntry,
    action: &str,
) {
    let data = serde_json::json!({
        "memory_id": row.id.to_string(),
        "action": action,
        "scope_type": &row.scope_type,
        "scope_key": &row.scope_key,
        "memory_key": &row.memory_key,
        "content_len": row.content.len(),
        "enabled": row.enabled,
        "expires_at": &row.expires_at.map(crate::clock::format_wire_ts),
        "created_by": &row.created_by,
        "updated_by": &row.updated_by,
        "created_at": &crate::clock::format_wire_ts(row.created_at),
        "updated_at": &crate::clock::format_wire_ts(row.updated_at),
    });
    if let Err(e) = db.record_event(event_type, None, None, data).await {
        tracing::warn!(error = %e, memory_id = row.id, event_type, "Agent memory tool audit failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent_ctx() -> AgentContext {
        AgentContext {
            account_chat: None,
            persona: None,
            native: None,
            bot_id: 42,
            platform: "onebot".to_string(),
            message_type: "private".to_string(),
            user_id: 1001,
            group_id: None,
            channel_id: None,
            message_id: Some("m1".to_string()),
            raw_message: "hello".to_string(),
            content: "hello".to_string(),
            images: Vec::new(),
            reply_to_message_id: None,
        }
    }

    #[tokio::test]
    async fn typed_media_obeys_execution_gates_and_legacy_json_contract() {
        struct VisualTool;
        #[async_trait]
        impl AgentTool for VisualTool {
            fn name(&self) -> &str {
                "visual.lookup"
            }
            fn description(&self) -> &str {
                "typed media fixture"
            }
            async fn call(&self, _ctx: AgentToolContext, _input: Value) -> Result<Value> {
                Err(NuonoError::Internal(
                    "legacy method must not hide media override".into(),
                ))
            }
            async fn call_with_media(
                &self,
                _ctx: AgentToolContext,
                _input: Value,
            ) -> Result<AgentToolOutput> {
                AgentToolOutput::new(json!({"found":true})).with_images(vec![
                    AgentToolImage::inline_bytes("image/png", b"\x89PNG\r\n\x1a\n")?,
                ])
            }
        }
        let registry = ToolRegistry::new();
        registry.register(VisualTool).unwrap();
        registry.register(DescribeConversationTool).unwrap();
        let rejected = registry
            .execute(
                "visual.lookup",
                AutonomyLevel::ReplyOnly,
                AgentToolContext::new(agent_ctx()),
                json!({}),
            )
            .await;
        assert_eq!(rejected.status, ToolExecutionStatus::Rejected);
        assert!(rejected.images.is_empty());
        let result = registry
            .execute(
                "visual.lookup",
                AutonomyLevel::LowRiskTools,
                AgentToolContext::new(agent_ctx()),
                json!({}),
            )
            .await;
        assert_eq!(result.status, ToolExecutionStatus::Succeeded);
        assert_eq!(result.output, json!({"found":true}));
        let saved = serde_json::to_value(&result).unwrap();
        let restored: ToolExecution = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(restored.images, result.images);
        let mut messages = Vec::new();
        append_tool_result(&mut messages, "call-visual".into(), &restored);
        assert_eq!(
            messages[0].images,
            vec![result.images[0].source().to_owned()]
        );
        assert!(!messages[0].content.contains("data:image"));
        assert!(messages[0].content.contains("source_sha256"));
        let mut oversized = saved;
        oversized["images"] = json!(vec![result.images[0].clone(); MAX_TOOL_IMAGES + 1]);
        assert!(serde_json::from_value::<ToolExecution>(oversized).is_err());
        assert!(AgentToolImage::public_https("https://127.0.0.1/image.png").is_err());
        assert!(AgentToolImage::inline_data_uri("data:image/png;base64,SGVsbG8=").is_err());
        let legacy = registry
            .execute(
                "conversation.describe",
                AutonomyLevel::LowRiskTools,
                AgentToolContext::new(agent_ctx()),
                json!({}),
            )
            .await;
        assert!(legacy.images.is_empty());
        assert_eq!(legacy.text_result(), serde_json::to_value(&legacy).unwrap());
        let mut old = serde_json::to_value(&legacy).unwrap();
        old.as_object_mut().unwrap().remove("images");
        assert!(
            serde_json::from_value::<ToolExecution>(old)
                .unwrap()
                .images
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reply_only_rejects_read_only_tools() {
        let registry = BuiltinToolRegistry::read_only();

        let result = registry
            .execute(
                "conversation.describe",
                AutonomyLevel::ReplyOnly,
                AgentToolContext::new(agent_ctx()),
                json!({}),
            )
            .await;

        assert_eq!(result.status, ToolExecutionStatus::Rejected);
    }

    #[tokio::test]
    async fn low_risk_can_run_read_only_tools() {
        let registry = BuiltinToolRegistry::read_only();

        let result = registry
            .execute(
                "conversation.describe",
                AutonomyLevel::LowRiskTools,
                AgentToolContext::new(agent_ctx()),
                json!({}),
            )
            .await;

        assert_eq!(result.status, ToolExecutionStatus::Succeeded);
        assert_eq!(result.output["platform"], "onebot");
    }

    #[test]
    fn memory_mutation_tools_require_approval() {
        let registry = BuiltinToolRegistry::read_only();

        assert_eq!(
            registry.get("memory.read").unwrap().risk_level(),
            ToolRiskLevel::ReadOnly
        );
        assert_eq!(
            registry.get("memory.write").unwrap().risk_level(),
            ToolRiskLevel::RequiresApproval
        );
        assert_eq!(
            registry.get("memory.update").unwrap().risk_level(),
            ToolRiskLevel::RequiresApproval
        );
        assert_eq!(
            registry.get("memory.delete").unwrap().risk_level(),
            ToolRiskLevel::RequiresApproval
        );
    }

    #[test]
    fn disabled_memory_is_not_exposed_even_with_admin_autonomy() {
        let registry = BuiltinToolRegistry::read_only();
        for autonomy in [
            AutonomyLevel::LowRiskTools,
            AutonomyLevel::ApprovedTools,
            AutonomyLevel::AdminApproved,
        ] {
            let tools = registry.chat_tools_with_memory(autonomy, false);
            assert!(
                tools
                    .iter()
                    .any(|tool| tool.name == "conversation.describe")
            );
            assert!(tools.iter().all(|tool| !tool.name.starts_with("memory.")));
        }
        let tools = registry.chat_tools_with_memory(AutonomyLevel::AdminApproved, true);
        for name in [
            "memory.read",
            "memory.write",
            "memory.update",
            "memory.delete",
        ] {
            assert!(tools.iter().any(|tool| tool.name == name));
            assert!(memory_tool_rejection(name, false).is_some());
            assert!(memory_tool_rejection(name, true).is_none());
        }
        assert!(memory_tool_rejection("plugin.example.lookup", false).is_none());
    }

    #[test]
    fn unregister_by_prefix_removes_scoped_tools() {
        let registry = ToolRegistry::new();
        registry.register(DescribeConversationTool).unwrap();
        registry
            .register_arc(Arc::new(ScopedTestTool {
                name: "plugin.alpha.lookup",
            }))
            .unwrap();
        registry
            .register_arc(Arc::new(ScopedTestTool {
                name: "plugin.beta.lookup",
            }))
            .unwrap();

        assert_eq!(registry.unregister_by_prefix("plugin.alpha."), 1);
        assert!(registry.get("plugin.alpha.lookup").is_none());
        assert!(registry.get("plugin.beta.lookup").is_some());
        assert!(registry.get("conversation.describe").is_some());
    }

    #[test]
    fn chat_tools_include_allowed_tool_schema() {
        let registry = ToolRegistry::new();
        registry.register(SchemaTestTool).unwrap();

        let tools = registry.chat_tools(AutonomyLevel::LowRiskTools);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "schema.lookup");
        assert_eq!(tools[0].parameters_schema["type"], "object");
        assert_eq!(
            tools[0].parameters_schema["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(tools[0].parameters_schema["required"][0], "query");
    }

    struct ScopedTestTool {
        name: &'static str,
    }

    #[tokio::test]
    async fn tool_timeout_releases_resources_for_the_next_invocation() {
        struct SlowTool;
        #[async_trait]
        impl AgentTool for SlowTool {
            fn name(&self) -> &str {
                "slow"
            }
            fn description(&self) -> &str {
                "timeout fixture"
            }
            fn scheduling(&self, _: &AgentContext, _: &Value) -> ToolScheduling {
                ToolScheduling::concurrent([ToolResource::write("shared")])
            }
            async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
                if input["slow"] == true {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Ok(input)
            }
        }
        let registry = ToolRegistry::with_concurrency_limit(1).unwrap();
        registry.register(SlowTool).unwrap();
        let timed_out = registry
            .execute_with_timeout(
                "slow",
                AutonomyLevel::LowRiskTools,
                AgentToolContext::new(agent_ctx()),
                json!({"slow":true}),
                Some(Duration::from_millis(20)),
            )
            .await;
        assert_eq!(timed_out.status, ToolExecutionStatus::Timeout);
        let next = tokio::time::timeout(
            Duration::from_secs(1),
            registry.execute(
                "slow",
                AutonomyLevel::LowRiskTools,
                AgentToolContext::new(agent_ctx()),
                json!({"slow":false}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(next.status, ToolExecutionStatus::Succeeded);
    }

    #[async_trait]
    impl AgentTool for ScopedTestTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test"
        }

        async fn call(&self, _ctx: AgentToolContext, _input: Value) -> Result<Value> {
            Ok(json!({}))
        }
    }

    struct SchemaTestTool;

    #[async_trait]
    impl AgentTool for SchemaTestTool {
        fn name(&self) -> &str {
            "schema.lookup"
        }

        fn description(&self) -> &str {
            "lookup with schema"
        }

        fn risk_level(&self) -> ToolRiskLevel {
            ToolRiskLevel::LowRisk
        }

        fn parameters(&self) -> Vec<ToolParameter> {
            vec![ToolParameter::new("query", ToolParameterType::String, "search query").required()]
        }

        async fn call(&self, _ctx: AgentToolContext, _input: Value) -> Result<Value> {
            Ok(json!({}))
        }
    }
}
