//! Dashboard 手动发送消息与定时推送任务共用的消息结构与校验逻辑。
use crate::transport::RuntimeBotRoute;
use nuono_domain::{DeliveryTarget, MessageSegment, OutboundAction};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    #[default]
    Text,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedMessage {
    #[schema(value_type = Object)]
    pub route: RuntimeBotRoute,
    #[schema(value_type = Object)]
    pub target: DeliveryTarget,
    pub message: String,
    #[serde(default)]
    pub message_type: MessageType,
}

pub fn validate_text(message: &str) -> Result<()> {
    if message.trim().is_empty() {
        return Err(NuonoError::Validation("消息内容不能为空".into()));
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(NuonoError::Validation(format!(
            "消息内容不能超过 {MAX_MESSAGE_CHARS} 个字符"
        )));
    }
    Ok(())
}

impl ManagedMessage {
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.message)?;
        for value in [
            &self.route.platform,
            &self.route.instance_id,
            &self.route.bot_self_id,
        ] {
            if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err(NuonoError::Validation(
                    "必须指定完整有效的平台、实例与机器人身份".into(),
                ));
            }
        }
        self.target
            .validate()
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        let platform = match self.route.platform.as_str() {
            "qq" => "qq_official",
            "OneBot v11" => "onebot",
            value => value,
        };
        if self.target.platform().as_str() != platform {
            return Err(NuonoError::Validation("消息目标与机器人平台不一致".into()));
        }
        Ok(())
    }

    pub fn action(&self) -> Result<OutboundAction> {
        self.validate()?;
        let segment = match self.message_type {
            MessageType::Text => MessageSegment::text(&self.message),
            MessageType::Markdown => MessageSegment::Markdown {
                content: self.message.clone(),
            },
        };
        Ok(OutboundAction::SendMessage {
            target: self.target.clone(),
            segments: vec![segment],
            reply_to: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_contract_preserves_native_targets_and_markdown() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/managed_messages.json")).unwrap();
        for case in cases {
            let request: ManagedMessage = serde_json::from_value(case["request"].clone()).unwrap();
            request.validate().unwrap();
            let OutboundAction::SendMessage {
                target, segments, ..
            } = request.action().unwrap()
            else {
                panic!("send")
            };
            assert_eq!(target, request.target);
            assert!(
                matches!(&segments[0], MessageSegment::Markdown {content} if content=="**hello**")
            );
        }
    }
    #[test]
    fn mismatched_platform_and_empty_route_are_rejected() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/managed_messages.json")).unwrap();
        let mut request: ManagedMessage =
            serde_json::from_value(cases[0]["request"].clone()).unwrap();
        request.route.platform = "discord".into();
        assert!(request.validate().is_err());
        request.route.platform = "onebot".into();
        request.route.instance_id.clear();
        assert!(request.validate().is_err());
    }
}
