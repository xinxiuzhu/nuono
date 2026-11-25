//! 统一聊天权限评估桥接。
use crate::error::{NuonoError, Result};
pub use nuono_app::permission::{
    ActorPermission, PermissionLevel, PermissionSettings, PermissionStore, resolve_chat_level,
};
use serde_json::Value;

pub struct PermissionManager {
    inner: std::sync::Arc<nuono_app::permission::PermissionService>,
    runtime: Option<crate::runtime::RuntimeExecutor>,
}

impl std::fmt::Debug for PermissionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionManager")
            .field("has_runtime", &self.runtime.is_some())
            .finish_non_exhaustive()
    }
}

impl PermissionManager {
    pub fn new(db: impl PermissionStore + 'static) -> Self {
        Self {
            inner: std::sync::Arc::new(nuono_app::permission::PermissionService::new(
                std::sync::Arc::new(db),
            )),
            runtime: crate::runtime::RuntimeExecutor::current(),
        }
    }

    /// 评估入站事件发送者的最终权限；存储或配置异常时严格 fail-closed 返回 [`ActorPermission::Banned`]。
    pub async fn evaluate(&self, event: &Value) -> ActorPermission {
        match self.try_evaluate(event).await {
            Ok(permission) => permission,
            Err(error) => {
                tracing::error!(error=%error, "Permission evaluation failed; denying access");
                ActorPermission::Banned
            }
        }
    }

    /// 可区分错误的权限评估入口。
    pub async fn try_evaluate(&self, event: &Value) -> Result<ActorPermission> {
        let user_id = identity_string(event, "user_id")?;
        let platform = extract_platform(event);
        let namespace = extract_namespace(event, platform.as_deref());
        let role = event
            .get("sender")
            .and_then(|v| v.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let inner = self.inner.clone();
        let evaluation = async move {
            inner
                .evaluate(
                    platform.as_deref(),
                    namespace.as_deref(),
                    &user_id,
                    role.as_deref(),
                )
                .await
                .map_err(|error| match error {
                    nuono_app::permission::PermissionError::Store(error) => {
                        NuonoError::DatabaseQuery(error)
                    }
                    nuono_app::permission::PermissionError::Configuration(error) => {
                        NuonoError::ConfigValidation(error)
                    }
                })
        };
        match &self.runtime {
            Some(runtime) => runtime.run(evaluation).await,
            None => evaluation.await,
        }
    }
}

fn extract_platform(event: &Value) -> Option<String> {
    if let Some(raw) = event
        .get("platform")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("route")
                .and_then(|r| r.get("platform"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let lower = raw.to_ascii_lowercase();
        return Some(match lower.as_str() {
            "qq" => "qq_official".to_owned(),
            "onebot_v11" => "onebot".to_owned(),
            _ => lower,
        });
    }
    // 原始 OneBot v11 WebSocket 入站报文不携带 platform 字段，由 post_type 与 self_id 识别为 onebot。
    if event.get("post_type").is_some() && event.get("self_id").is_some() {
        return Some("onebot".to_owned());
    }
    None
}

fn non_empty_value_str(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Some(Value::Number(num)) => {
            let text = num.to_string();
            (text != "0").then_some(text)
        }
        _ => None,
    }
}

/// 按平台身份约定提取发送者命名空间，确保官 Q 与飞书等按应用或群隔离的 `openid` 不会跨范围匹配。
fn extract_namespace(event: &Value, platform: Option<&str>) -> Option<String> {
    let platform = platform?;
    match platform {
        "onebot" | "telegram" | "discord" => Some(String::new()),
        "feishu" => non_empty_value_str(event.get("route").and_then(|r| r.get("bot_self_id")))
            .or_else(|| non_empty_value_str(event.get("raw").and_then(|r| r.get("app_id"))))
            .or_else(|| {
                non_empty_value_str(
                    event
                        .get("raw")
                        .and_then(|r| r.get("header"))
                        .and_then(|h| h.get("app_id")),
                )
            })
            .or_else(|| non_empty_value_str(event.get("self_id"))),
        "qq_official" => {
            let app_id = non_empty_value_str(event.get("route").and_then(|r| r.get("bot_self_id")))
                .or_else(|| non_empty_value_str(event.get("raw").and_then(|r| r.get("app_id"))))
                .or_else(|| non_empty_value_str(event.get("self_id")))?;
            let is_group = event
                .get("message_type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == "group")
                || event
                    .get("qq_target_type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "group");
            let group_openid = non_empty_value_str(event.get("group_openid"))
                .or_else(|| {
                    non_empty_value_str(event.get("raw").and_then(|r| r.get("group_openid")))
                })
                .or_else(|| {
                    non_empty_value_str(event.get("raw").and_then(|r| r.get("native_group_openid")))
                })
                .or_else(|| {
                    is_group
                        .then(|| non_empty_value_str(event.get("group_id")))
                        .flatten()
                });
            if is_group || group_openid.is_some() {
                group_openid.map(|group| format!("{app_id}/groups/{group}"))
            } else {
                Some(app_id)
            }
        }
        _ => Some(
            event
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned(),
        ),
    }
}

fn identity_string(event: &Value, key: &str) -> Result<String> {
    match event.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(Value::Number(value)) => Ok(value.to_string()),
        Some(Value::String(_)) | None | Some(Value::Null) => Err(NuonoError::PermissionValidation(
            format!("missing identity field {key}"),
        )),
        Some(_) => Err(NuonoError::PermissionValidation(format!(
            "identity field {key} must be a number or string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_identity_accepts_number_and_string() {
        assert_eq!(
            identity_string(&serde_json::json!({"user_id": 123}), "user_id").unwrap(),
            "123"
        );
        assert_eq!(
            identity_string(&serde_json::json!({"user_id": "ou_user"}), "user_id").unwrap(),
            "ou_user"
        );
        assert!(identity_string(&serde_json::json!({"user_id": 0}), "missing").is_err());
    }

    #[test]
    fn platform_and_namespace_extraction_matches_adapter_isolation_rules() {
        let onebot = serde_json::json!({"platform": "onebot", "user_id": 2609145334_i64});
        assert_eq!(extract_platform(&onebot).as_deref(), Some("onebot"));
        assert_eq!(
            extract_namespace(&onebot, Some("onebot")).as_deref(),
            Some("")
        );

        let qq_c2c = serde_json::json!({
            "platform": "qq_official",
            "message_type": "private",
            "route": {"platform": "qq_official", "bot_self_id": "1020001"},
            "user_id": "USER_OPENID"
        });
        assert_eq!(
            extract_namespace(&qq_c2c, Some("qq_official")).as_deref(),
            Some("1020001")
        );

        let qq_group = serde_json::json!({
            "platform": "qq",
            "message_type": "group",
            "route": {"platform": "qq_official", "bot_self_id": "1020001"},
            "group_openid": "GROUP_OPENID",
            "user_id": "MEMBER_OPENID"
        });
        assert_eq!(extract_platform(&qq_group).as_deref(), Some("qq_official"));
        assert_eq!(
            extract_namespace(&qq_group, Some("qq_official")).as_deref(),
            Some("1020001/groups/GROUP_OPENID")
        );

        let qq_missing_app = serde_json::json!({
            "platform": "qq_official",
            "message_type": "private",
            "user_id": "USER_OPENID"
        });
        assert_eq!(
            extract_namespace(&qq_missing_app, Some("qq_official")),
            None
        );

        let feishu = serde_json::json!({
            "platform": "feishu",
            "self_id": "cli_a123",
            "user_id": "ou_xyz"
        });
        assert_eq!(
            extract_namespace(&feishu, Some("feishu")).as_deref(),
            Some("cli_a123")
        );
    }
}
