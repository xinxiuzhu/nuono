//! 模型工具调用与人工审批决策的统一执行入口。
use super::ports::AgentConfiguration;
use super::runtime::AgentToolCall;
use super::store::{AgentStore, AgentToolApprovalRecord, AgentToolReceiptRecord, ToolApproval};
use super::tool::{
    AgentToolContext, ToolExecution, ToolExecutionStatus, ToolPlan, ToolRegistry, ToolRiskLevel,
    memory_tool_rejection,
};
use super::{AgentContext, AgentPolicy, AutonomyLevel};
use crate::auth::{CallerIdentity, Capability};
use nuono_error::{NuonoError, Result};
use std::sync::Arc;
use tracing::warn;

pub struct ToolExecutionService {
    db: Arc<dyn AgentStore>,
    config: Arc<dyn AgentConfiguration>,
    tools: Arc<ToolRegistry>,
}

enum ExecutionOrigin<'a> {
    Request {
        reason: &'a str,
        run_id: Option<&'a str>,
    },
    Approved {
        run_id: Option<&'a str>,
    },
}

impl ToolExecutionService {
    pub fn new(
        db: Arc<dyn AgentStore>,
        config: Arc<dyn AgentConfiguration>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self { db, config, tools }
    }
    fn policy(&self, config: &crate::settings::AIConfig) -> AgentPolicy {
        AgentPolicy::from_ai_config(config)
    }
    pub async fn execute(
        &self,
        ctx: AgentContext,
        autonomy: AutonomyLevel,
        name: &str,
        input: serde_json::Value,
        reason: &str,
    ) -> Result<ToolExecution> {
        let plan = self.tools.plan(name, &ctx, &input);
        self.execute_inner(
            ctx,
            autonomy,
            plan,
            input,
            ExecutionOrigin::Request {
                reason,
                run_id: None,
            },
        )
        .await
    }
    pub async fn execute_for_run(
        &self,
        ctx: AgentContext,
        autonomy: AutonomyLevel,
        name: &str,
        input: serde_json::Value,
        run_id: &str,
    ) -> Result<ToolExecution> {
        let plan = self.tools.plan(name, &ctx, &input);
        self.execute_planned_for_run(ctx, autonomy, plan, input, run_id)
            .await
    }
    pub(super) async fn execute_planned_for_run(
        &self,
        ctx: AgentContext,
        autonomy: AutonomyLevel,
        plan: ToolPlan,
        input: serde_json::Value,
        run_id: &str,
    ) -> Result<ToolExecution> {
        self.execute_inner(
            ctx,
            autonomy,
            plan,
            input,
            ExecutionOrigin::Request {
                reason: "Agent task tool call",
                run_id: Some(run_id),
            },
        )
        .await
    }
    async fn execute_inner(
        &self,
        ctx: AgentContext,
        autonomy: AutonomyLevel,
        plan: ToolPlan,
        input: serde_json::Value,
        origin: ExecutionOrigin<'_>,
    ) -> Result<ToolExecution> {
        let (reason, approved, run_id) = match origin {
            ExecutionOrigin::Request { reason, run_id } => (reason, false, run_id),
            ExecutionOrigin::Approved { run_id } => ("approved", true, run_id),
        };
        let lease = self.tools.acquire(plan).await;
        let name = lease.plan.name.as_str();
        let risk = lease.plan.risk;
        let mut inherited_autonomy = None;
        // 等待并发槽位或资源锁期间，全局策略可能已变更，或父任务可能已被取消，因此在拿到租约后需重新校验任务状态。
        let run_rejection = if let Some(id) = run_id {
            match self.db.get_agent_run(id).await? {
                Some(run) if matches!(run.status.as_str(), "running" | "waiting_approval") => {
                    inherited_autonomy = super::children::child_autonomy(&run)?;
                    if let Some(parent_id) = &run.parent_id {
                        if !self
                            .db
                            .get_agent_run(parent_id)
                            .await?
                            .is_some_and(|parent| super::run::is_active(&parent.status))
                        {
                            Some("父 Agent 任务已停止".to_string())
                        } else {
                            super::children::child_tool_rejection(&run, name, risk)?
                        }
                    } else {
                        None
                    }
                }
                _ => Some("关联 Agent 任务已停止".to_string()),
            }
        } else {
            None
        };
        // 若等待期间全局配置的自主等级被调低，未获人工特批的模型调用需按较低的等级执行。
        let current = self.config.ai_settings().await;
        let autonomy = if approved {
            autonomy
        } else {
            autonomy
                .min(current.agent_autonomy_level())
                .min(inherited_autonomy.unwrap_or(AutonomyLevel::AdminApproved))
        };
        // 在实际执行前重新加载最新策略（包括人工审批通过后的执行）。
        let policy = self.policy(&current);
        let call = AgentToolCall {
            name: name.to_string(),
            input,
        };
        let execution = if let Some(reason) = (!ctx.enabled_in(&current))
            .then(|| "AI 功能已停用".to_string())
            .or_else(|| memory_tool_rejection(name, current.agent_memory_enabled))
            .or(run_rejection)
            .or_else(|| self.tools.lease_rejection(&lease))
            .or_else(|| policy.context_rejection(&ctx))
            .or_else(|| policy.tool_rejection(name))
        {
            ToolExecution {
                tool_name: name.into(),
                risk_level: risk,
                status: ToolExecutionStatus::Rejected,
                input: call.input,
                output: serde_json::json!({"error":reason,"policy":"agent_tool_policy"}),
                images: Vec::new(),
                elapsed_ms: 0,
            }
        } else if risk == Some(ToolRiskLevel::RequiresApproval)
            && autonomy != AutonomyLevel::AdminApproved
            && ToolRiskLevel::RequiresApproval.allowed_by(autonomy)
        {
            self.create_pending_approval(&ctx, &call, reason, run_id, risk)
                .await?
        } else {
            let intent = ToolExecution {
                tool_name: name.into(),
                risk_level: risk,
                status: ToolExecutionStatus::Started,
                input: call.input.clone(),
                output: serde_json::json!({"phase":"execution_intent"}),
                images: Vec::new(),
                elapsed_ms: 0,
            };
            self.save_tool_receipt(&ctx, &intent).await?;
            // 记录执行意图期间记忆开关可能被动态关闭，因此在真正操作记忆存储前再次确认开关状态。
            let memory_rejection = if memory_tool_rejection(name, false).is_some() {
                let latest = self.config.ai_settings().await;
                memory_tool_rejection(name, latest.agent_memory_enabled)
            } else {
                None
            };
            if let Some(reason) = memory_rejection {
                ToolExecution {
                    tool_name: name.into(),
                    risk_level: risk,
                    status: ToolExecutionStatus::Rejected,
                    input: call.input,
                    output: serde_json::json!({"error":reason,"policy":"agent_tool_policy"}),
                    images: Vec::new(),
                    elapsed_ms: 0,
                }
            } else {
                self.tools
                    .execute_leased(
                        &lease,
                        autonomy,
                        AgentToolContext::with_store(ctx.clone(), self.db.clone())
                            .with_memory_policy(current.agent_memory_policy.clone()),
                        call.input,
                        None,
                    )
                    .await
            }
        };
        if let Err(error) = self.save_tool_receipt(&ctx, &execution).await {
            warn!(%error, tool=name,"Tool completed but final receipt persistence failed; execution must not be retried");
        }
        Ok(execution)
    }
    pub async fn decide(
        &self,
        caller: &CallerIdentity,
        id: i64,
        approved: bool,
        reason: Option<String>,
    ) -> Result<ToolApproval> {
        if !caller.has(Capability::ManageRuntime) {
            return Err(NuonoError::PermissionDenied("manage_runtime".into()));
        }
        if approved
            && self
                .db
                .agent_run_for_approval(id)
                .await?
                .is_some_and(|run| run.status == "cancelled")
        {
            return Err(NuonoError::Conflict("关联 Agent 任务已取消".into()));
        }
        let row = self
            .db
            .decide_agent_tool_approval(id, approved, Some(caller.username.clone()), reason)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("审批记录 {id}")))?;
        if !approved || row.status != "approved" {
            return Ok(row);
        }
        let Some(claimed) = self.db.claim_agent_tool_approval_for_execution(id).await? else {
            return self
                .db
                .get_agent_tool_approval(id)
                .await?
                .ok_or_else(|| NuonoError::NotFound(format!("审批记录 {id}")));
        };
        let legacy_ctx = AgentContext {
            account_chat: None,
            persona: None,
            native: None,
            bot_id: claimed.bot_id,
            platform: claimed.platform,
            message_type: claimed.message_type,
            user_id: claimed.user_id,
            group_id: claimed.group_id,
            channel_id: claimed.channel_id,
            message_id: claimed.message_id,
            raw_message: claimed.reason.clone().unwrap_or_default(),
            content: claimed.reason.unwrap_or_default(),
            images: Vec::new(),
            reply_to_message_id: None,
        };
        let ctx = match claimed.context_json.as_deref() {
            Some(value) => match serde_json::from_str(value) {
                Ok(ctx) => ctx,
                Err(error) => {
                    self.db
                        .mark_agent_tool_approval_status(id, "execute_failed")
                        .await?;
                    return Err(NuonoError::Validation(format!("审批上下文无效: {error}")));
                }
            },
            None => legacy_ctx,
        };
        let result = match claimed
            .input_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
        {
            Ok(input) => {
                let input = input.unwrap_or_else(|| serde_json::json!({}));
                let plan = self.tools.plan(&claimed.tool_name, &ctx, &input);
                self.execute_inner(
                    ctx,
                    AutonomyLevel::AdminApproved,
                    plan,
                    input,
                    ExecutionOrigin::Approved {
                        run_id: claimed.run_id.as_deref(),
                    },
                )
                .await
            }
            Err(error) => Err(NuonoError::Validation(format!("审批参数无效: {error}"))),
        };
        let status = if result
            .as_ref()
            .is_ok_and(|r| r.status == ToolExecutionStatus::Succeeded)
        {
            "executed"
        } else {
            "execute_failed"
        };
        let execution = match &result {
            Ok(execution) => serde_json::to_string(execution),
            Err(error) => serde_json::to_string(&serde_json::json!({"error":error.to_string()})),
        }
        .map_err(|error| NuonoError::Internal(error.to_string()))?;
        let row = self
            .db
            .complete_agent_tool_approval(id, status, execution)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("审批记录 {id}")))?;
        result?;
        Ok(row)
    }
    pub(super) async fn save_tool_receipt(
        &self,
        ctx: &AgentContext,
        execution: &ToolExecution,
    ) -> nuono_error::Result<()> {
        let error = execution
            .output
            .get("error")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);

        self.db
            .save_agent_tool_receipt(AgentToolReceiptRecord {
                context_json: Some(serde_json::to_string(ctx)?),
                session_id: ctx.session_id(),
                bot_id: ctx.bot_id,
                platform: ctx.platform.clone(),
                message_type: ctx.message_type.clone(),
                user_id: ctx.user_id,
                group_id: ctx.group_id,
                channel_id: ctx.channel_id.clone(),
                message_id: ctx.message_id.clone(),
                tool_name: execution.tool_name.clone(),
                risk_level: execution
                    .risk_level
                    .map(|risk| format!("{risk:?}"))
                    .unwrap_or_else(|| "Unknown".to_string()),
                status: format!("{:?}", execution.status),
                input_json: Some(execution.input.to_string()),
                output_json: Some(if execution.images.is_empty() {
                    execution.output.to_string()
                } else {
                    serde_json::json!({
                        "value": execution.output,
                        "images": execution.image_metadata(),
                    })
                    .to_string()
                }),
                error,
                elapsed_ms: execution.elapsed_ms,
            })
            .await
    }

    async fn create_pending_approval(
        &self,
        ctx: &AgentContext,
        tool_call: &AgentToolCall,
        assistant_tool_call: &str,
        run_id: Option<&str>,
        risk_level: Option<ToolRiskLevel>,
    ) -> Result<ToolExecution> {
        let approval_id = self
            .db
            .create_agent_tool_approval(AgentToolApprovalRecord {
                run_id: run_id.map(str::to_string),
                context_json: Some(serde_json::to_string(ctx).expect("agent context serializes")),
                session_id: ctx.session_id(),
                bot_id: ctx.bot_id,
                platform: ctx.platform.clone(),
                message_type: ctx.message_type.clone(),
                user_id: ctx.user_id,
                group_id: ctx.group_id,
                channel_id: ctx.channel_id.clone(),
                message_id: ctx.message_id.clone(),
                tool_name: tool_call.name.clone(),
                risk_level: risk_level
                    .map(|risk| format!("{risk:?}"))
                    .unwrap_or_else(|| "Unknown".to_string()),
                input_json: Some(tool_call.input.to_string()),
                reason: Some(assistant_tool_call.chars().take(2000).collect()),
            })
            .await?;

        Ok(ToolExecution {
            tool_name: tool_call.name.clone(),
            risk_level,
            status: ToolExecutionStatus::PendingApproval,
            input: tool_call.input.clone(),
            output: serde_json::json!({
                "approval_id": approval_id.to_string(),
                "status": "pending_approval",
                "message": "tool call requires manual approval before execution"
            }),
            images: Vec::new(),
            elapsed_ms: 0,
        })
    }
}
