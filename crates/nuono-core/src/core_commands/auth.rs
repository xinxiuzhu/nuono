//! 系统核心指令的群/频道管理员权限核验。
//!
//! 机器人管理员及以上直接放行；普通群/频道管理员则通过平台原生群成员查询接口二次核实，
//! 不信任入站事件中未核验的发送者角色字段。

use std::time::Duration;

use serde_json::Value;

use super::{CoreCommands, localized};
use crate::core::event_kernel::RuntimeEventContext;
use crate::core::permission::PermissionLevel;
use crate::domain::{ConversationRef, DomainEvent, OutboundAction, QueryOperation};
use nuono_app::localization::Locale;

#[derive(Debug, Clone, Copy)]
pub(super) enum GroupAdminAuthError {
    Identity,
    GroupOnly,
    PermissionUnavailable,
    Denied,
    Timeout,
}

impl GroupAdminAuthError {
    pub(super) fn render(self, locale: Locale) -> String {
        match self {
            Self::Identity => localized(
                locale,
                "无法核实当前发送者和会话的原生身份，权限校验未通过。",
                "Sender and conversation identities could not be verified.",
                "送信者と会話の識別情報を確認できません。",
                "발신자와 대화의 식별 정보를 확인할 수 없습니다.",
            ),
            Self::GroupOnly => localized(
                locale,
                "该指令需要群组/频道管理员权限，请在群聊或频道中使用，或由机器人管理员调用。",
                "This command requires group/channel administrator permission. Use it in a group or channel, or ask a bot administrator.",
                "このコマンドはグループ/チャンネル管理者権限が必要です。グループまたはチャンネルで使用するか、ボット管理者に依頼してください。",
                "이 명령은 그룹/채널 관리자 권한이 필요합니다. 그룹 또는 채널에서 사용하거나 봇 관리자에게 요청하세요.",
            ),
            Self::PermissionUnavailable => localized(
                locale,
                "当前平台无法验证群/频道管理员身份；请由机器人管理员调用。",
                "This platform cannot verify group/channel administrators. Please ask a bot administrator.",
                "このプラットフォームではグループ/チャンネル管理者権限を確認できません。ボット管理者に依頼してください。",
                "이 플랫폼에서는 그룹/채널 관리자 권한을 확인할 수 없습니다. 봇 관리자에게 요청하세요.",
            ),
            Self::Denied => localized(
                locale,
                "该指令仅限当前群/频道管理员或机器人管理员使用。",
                "Only a verified administrator of this group/channel or a bot administrator can use this command.",
                "このグループまたはチャンネルの確認済み管理者、またはボット管理者のみ使用できます。",
                "현재 그룹/채널의 확인된 관리자 또는 봇 관리자만 이 명령을 사용할 수 있습니다.",
            ),
            Self::Timeout => localized(
                locale,
                "群/频道管理员权限验证超时，请稍后重试。",
                "Administrator verification timed out. Please try again later.",
                "管理者権限の確認がタイムアウトしました。しばらくしてから再試行してください。",
                "관리자 권한 확인 시간이 초과되었습니다. 잠시 후 다시 시도하세요.",
            ),
        }
    }
}

pub(super) async fn authorize_group_admin(
    commands: &CoreCommands,
    ctx: &RuntimeEventContext<'_>,
) -> std::result::Result<(), GroupAdminAuthError> {
    if commands
        .permission_manager
        .try_evaluate(&ctx.event)
        .await
        .map_err(|_| GroupAdminAuthError::PermissionUnavailable)?
        .has_permission(PermissionLevel::BotAdmin)
    {
        return Ok(());
    }
    let event = nuono_plugin::Event::with_domain(&ctx.event, ctx.domain_event.as_ref());
    let domain = event
        .native_domain_event()
        .map_err(|_| GroupAdminAuthError::Identity)?
        .ok_or(GroupAdminAuthError::Identity)?;
    let actor = domain.actor.as_ref().ok_or(GroupAdminAuthError::Identity)?;
    let projected = crate::core::runtime_outbound::runtime_identity(&ctx.event, "user_id");
    if projected.as_deref() != Some(actor.value().as_str()) {
        return Err(GroupAdminAuthError::Identity);
    }
    let conversation = domain
        .conversation
        .as_ref()
        .filter(|value| !matches!(value, ConversationRef::Direct { .. }))
        .ok_or(GroupAdminAuthError::GroupOnly)?
        .clone();
    let action = nuono_app::transport::TransportAction::new(
        domain.bot.clone(),
        OutboundAction::Query {
            query: QueryOperation::GroupMember {
                conversation: Box::new(conversation),
                actor: actor.clone(),
            },
        },
    );
    // OneBot 入口上下文的传输通道仅支持单向发通知；优先走受管路由查询以配对回执响应。
    let route = nuono_plugin::Event::new(&ctx.event).route();
    let query = async {
        if let (Some(router), Some(route)) = (&commands.action_router, route.as_ref()) {
            router
                .call_typed_routed_receipt(route, action)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "原生管理员路由查询失败");
                    GroupAdminAuthError::PermissionUnavailable
                })
        } else {
            let transport = ctx
                .action_transport
                .as_ref()
                .ok_or(GroupAdminAuthError::PermissionUnavailable)?;
            nuono_app::action::call_action(transport.as_ref(), action)
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "原生管理员查询失败");
                    GroupAdminAuthError::PermissionUnavailable
                })
        }
    };
    let receipt = tokio::time::timeout(Duration::from_secs(10), query)
        .await
        .map_err(|_| GroupAdminAuthError::Timeout)??;
    if receipt_is_admin(&receipt, &domain) {
        Ok(())
    } else {
        Err(GroupAdminAuthError::Denied)
    }
}

fn id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn receipt_is_admin(receipt: &crate::domain::ActionReceipt, domain: &DomainEvent) -> bool {
    let Some(actor) = domain.actor.as_ref() else {
        return false;
    };
    let data = receipt.raw.get("data").unwrap_or(&receipt.raw);
    if receipt
        .raw
        .get("retcode")
        .is_some_and(|code| code.as_i64() != Some(0))
        || matches!(
            receipt.raw.get("status").and_then(Value::as_str),
            Some("failed" | "error" | "async" | "pending")
        )
    {
        return false;
    }
    let returned_actor = data
        .get("user_id")
        .or_else(|| data.get("user").and_then(|user| user.get("id")))
        .and_then(id);
    let role = data
        .get("role")
        .or_else(|| data.get("status"))
        .and_then(Value::as_str);
    let matches_optional = |keys: &[&str], expected: Option<&str>| {
        keys.iter().all(|key| {
            data.get(*key)
                .is_none_or(|value| id(value).as_deref() == expected && expected.is_some())
        })
    };
    let same_conversation = match domain.conversation.as_ref() {
        Some(ConversationRef::Group { group }) => {
            matches_optional(&["group_id", "chat_id"], Some(group.value().as_str()))
        }
        Some(ConversationRef::Channel { channel, parent }) => {
            matches_optional(&["channel_id", "chat_id"], Some(channel.value().as_str()))
                && matches_optional(
                    &["guild_id", "group_id"],
                    parent.as_ref().map(|parent| parent.value().as_str()),
                )
        }
        _ => false,
    };
    receipt.status == crate::domain::ActionReceiptStatus::Completed
        && receipt.platform == *domain.bot.platform()
        && returned_actor.as_deref() == Some(actor.value().as_str())
        && same_conversation
        && matches!(role, Some("owner" | "admin" | "creator" | "administrator"))
}
