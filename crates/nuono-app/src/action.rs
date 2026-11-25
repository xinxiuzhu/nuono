//! 平台出站动作的校验、鉴权与审计分发。
//!
//! 应用层在实际发送前负责校验权限与记录审计日志，底层 `ActionTransport` 负责具体的协议发送。
//! 本模块仅做参数校验并单次下发，底层传输错误与回执原样返回，不做隐式重试。

use crate::transport::{ActionTransport, TransportAction};
use nuono_domain::{ActionError, ActionErrorKind, ActionReceipt};

fn validate(action: &TransportAction) -> Result<(), ActionError> {
    use nuono_domain::{OutboundAction, QueryOperation};
    let target_platform = match &action.action {
        OutboundAction::SendFile { target, .. }
        | OutboundAction::SendMessage { target, .. }
        | OutboundAction::EditMessage { target, .. }
        | OutboundAction::React { target, .. }
        | OutboundAction::Moderate { target, .. } => Some(target.platform()),
        OutboundAction::DeleteMessage { target, .. } => {
            target.as_ref().map(|target| target.platform())
        }
        OutboundAction::Query {
            query: QueryOperation::GroupMember { conversation, .. },
        } => Some(conversation.platform()),
        _ => None,
    };
    if target_platform.is_some_and(|platform| platform != action.bot.platform()) {
        return Err(ActionError::new(
            ActionErrorKind::Rejected,
            "动作目标平台与 bot 不一致",
        ));
    }
    action
        .action
        .validate()
        .map_err(|error| ActionError::new(ActionErrorKind::Rejected, error.to_string()))
}

pub async fn notify_action(
    transport: &dyn ActionTransport,
    action: TransportAction,
) -> Result<(), ActionError> {
    validate(&action)?;
    action.authority.authorize()?;
    transport.notify(action).await
}

pub async fn call_action(
    transport: &dyn ActionTransport,
    action: TransportAction,
) -> Result<ActionReceipt, ActionError> {
    validate(&action)?;
    action.authority.authorize()?;
    transport.call(action).await
}

/// 动作发起方的权限来源声明（由宿主在入站鉴权通过后构造）。
/// 注意：进程内插件与宿主共享信任域，此处的 `Plugin` 标识仅用于审计溯源而非安全沙箱隔离。
#[derive(Debug, Clone)]
pub enum ActionAuthority {
    Runtime {
        stage: String,
    },
    Dashboard(crate::auth::CallerIdentity),
    Plugin {
        name: String,
    },
    Agent {
        autonomy: crate::agent::AutonomyLevel,
        risk: crate::agent::tool::ToolRiskLevel,
    },
}
impl ActionAuthority {
    pub fn authorize(&self) -> Result<(), ActionError> {
        let allowed = match self {
            Self::Dashboard(caller) => caller.has(crate::auth::Capability::ManageRuntime),
            Self::Agent { autonomy, risk } => {
                risk.allowed_by(*autonomy)
                    && (*risk != crate::agent::tool::ToolRiskLevel::RequiresApproval
                        || *autonomy == crate::agent::AutonomyLevel::AdminApproved)
            }
            Self::Runtime { stage } => !stage.trim().is_empty(),
            Self::Plugin { name } => {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(ActionError::new(
                ActionErrorKind::Unauthorized,
                "动作权限被拒绝",
            ))
        }
    }
    pub fn actor(&self) -> String {
        match self {
            Self::Dashboard(c) => c.username.clone(),
            Self::Plugin { name } => format!("plugin:{name}"),
            Self::Runtime { stage } => format!("runtime:{stage}"),
            Self::Agent { .. } => "agent".into(),
        }
    }
}
#[async_trait::async_trait]
pub trait ActionAudit: Send + Sync {
    async fn record(
        &self,
        action: &TransportAction,
        authority: &ActionAuthority,
        phase: &str,
        error: Option<&ActionError>,
    ) -> Result<(), ActionError>;
}
/// 校验参数与权限，先记录开始审计日志，再执行一次动作并记录执行结果。即使结果审计写入失败，也绝不重复执行动作本身。
pub async fn execute_authorized<T, F: std::future::Future<Output = Result<T, ActionError>>>(
    authority: &ActionAuthority,
    action: &TransportAction,
    audit: Option<&dyn ActionAudit>,
    operation: F,
) -> Result<T, ActionError> {
    validate(action)?;
    authority.authorize()?;
    if let Some(audit) = audit {
        audit.record(action, authority, "started", None).await?;
    }
    let result = operation.await;
    if let Some(audit) = audit
        && let Err(error) = audit
            .record(
                action,
                authority,
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                result.as_ref().err(),
            )
            .await
    {
        tracing::warn!(%error,"动作结果审计失败；不得重试该动作");
    }
    result
}
