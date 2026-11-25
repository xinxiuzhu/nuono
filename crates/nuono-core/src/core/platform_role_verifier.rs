//! 通过向 OneBot 协议端发起实时查询来获取短期有效的群角色凭证，绝不信任聊天消息自带的文本声明。
use crate::{
    core::onebot_client::ApiClient,
    error::{NuonoError, Result},
};
use dashmap::DashMap;
use nuono_app::configuration::{PlatformRoleSubject, PlatformRoleVerifier, VerifiedGroupRole};
use std::sync::Arc;
pub(crate) struct OneBotRoleVerifier {
    pub senders: Arc<DashMap<i64, tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>>,
    pub api: Arc<ApiClient>,
}
#[async_trait::async_trait]
impl PlatformRoleVerifier for OneBotRoleVerifier {
    fn provider_id(&self) -> &str {
        "onebot.live_group_member"
    }
    async fn role(&self, subject: &PlatformRoleSubject) -> Result<VerifiedGroupRole> {
        if subject.platform != "onebot"
            || !subject.namespace.is_empty()
            || subject.conversation_kind != "group:native"
            || !subject.native_parent_id.is_empty()
        {
            return Err(NuonoError::NotImplemented(
                "此适配器无法证明该平台作用域的群角色".into(),
            ));
        }
        let group = subject
            .native_group_id
            .parse::<i64>()
            .ok()
            .filter(|id| id.to_string() == subject.native_group_id);
        let user = subject
            .native_user_id
            .parse::<i64>()
            .ok()
            .filter(|id| id.to_string() == subject.native_user_id);
        let (Some(group), Some(user)) = (group, user) else {
            return Err(NuonoError::Validation("平台数字身份非规范形式".into()));
        };
        let mut routes = self
            .senders
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect::<Vec<_>>();
        routes.sort_by_key(|(id, _)| *id);
        for (_, sender) in routes {
            if let Ok(reply) = self
                .api
                .call(
                    "get_group_member_info",
                    serde_json::json!({"group_id":group,"user_id":user,"no_cache":true}),
                    &sender,
                )
                .await
            {
                if reply.get("group_id").and_then(serde_json::Value::as_i64) != Some(group)
                    || reply.get("user_id").and_then(serde_json::Value::as_i64) != Some(user)
                {
                    continue;
                }
                return match reply.get("role").and_then(serde_json::Value::as_str) {
                    Some("owner") => Ok(VerifiedGroupRole::Owner),
                    Some("admin") => Ok(VerifiedGroupRole::Administrator),
                    Some("member") => Ok(VerifiedGroupRole::Member),
                    _ => Err(NuonoError::PermissionDenied(
                        "平台未返回可验证的群角色".into(),
                    )),
                };
            }
        }
        Err(NuonoError::PermissionDenied(
            "暂无在线适配器可核验群角色".into(),
        ))
    }
}
