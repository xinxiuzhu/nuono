//! 携带完整领域层 `DeliveryTarget` 的原生文件发送请求（保留完整命名空间与身份方案，直至适配器选定上传目标）。
use super::file::{FileSendRequest, FileTarget, validate_file_encoding, validate_file_metadata};
use nuono_domain::{
    ConversationRef, DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, Platform,
};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeFileSendRequest {
    pub target: DeliveryTarget,
    pub base64: String,
    pub file_name: String,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl NativeFileSendRequest {
    pub fn validate(&self) -> Result<()> {
        self.target
            .validate()
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        validate_file_metadata(&self.file_name, self.content_type.as_deref())?;
        validate_file_encoding(&self.base64)
    }

    /// 在进行目标降级投影、鉴权或上传前，先校验目标身份方案是否符合对应平台规范。
    pub fn validate_for_platform(&self, platform: &Platform) -> Result<()> {
        self.validate()?;
        for identity in self.target.identities() {
            let supported = match platform {
                Platform::QqOfficial => {
                    *identity.scheme() == IdentityScheme::OpenId
                        && identity.namespace().is_some()
                        && matches!(identity.kind(), IdentityKind::User | IdentityKind::Group)
                }
                Platform::Feishu => {
                    identity.namespace().is_some()
                        && match identity.kind() {
                            IdentityKind::User => matches!(
                                identity.scheme(),
                                IdentityScheme::OpenId
                                    | IdentityScheme::UserId
                                    | IdentityScheme::UnionId
                            ),
                            IdentityKind::Conversation
                            | IdentityKind::Group
                            | IdentityKind::Channel => *identity.scheme() == IdentityScheme::ChatId,
                            _ => false,
                        }
                }
                Platform::Telegram => {
                    *identity.scheme() == IdentityScheme::Numeric
                        && identity.namespace().is_none()
                        && identity.value().as_str().parse::<i64>().is_ok_and(|value| {
                            value != 0
                                && (!matches!(
                                    identity.kind(),
                                    IdentityKind::User | IdentityKind::Conversation
                                ) || value > 0)
                                && value.to_string() == identity.value().as_str()
                        })
                }
                Platform::Discord => {
                    *identity.scheme() == IdentityScheme::Snowflake
                        && identity.namespace().is_none()
                        && identity.value().as_str().parse::<u64>().is_ok_and(|value| {
                            value != 0 && value.to_string() == identity.value().as_str()
                        })
                }
                Platform::OneBot | Platform::Other(_) => {
                    *identity.scheme() == IdentityScheme::Native && identity.namespace().is_none()
                }
                _ => false,
            };
            if identity.platform() != platform || !supported {
                return Err(NuonoError::Validation(
                    "native file target has an invalid platform, identity scheme or namespace"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// 校验目标身份中的命名空间是否与当前适配器客户端的应用 ID（`app_id`）一致。
    pub fn validate_for_application(&self, platform: &Platform, app_id: &str) -> Result<()> {
        self.validate_for_platform(platform)?;
        if self
            .target
            .identities()
            .any(|identity| identity.namespace().as_ref().map(|id| id.as_str()) != Some(app_id))
        {
            return Err(NuonoError::Validation(
                "native file target belongs to another application".into(),
            ));
        }
        Ok(())
    }

    /// 转换为简化的 `FileSendRequest`（仅在已完成平台校验、且适配器无需区分应用级细分接收类型时使用；
    /// 若私聊已携带明确的会话 ID，则按会话 ID 投递而非按参与者用户 ID 投递）。
    pub fn compatibility_request(&self) -> Result<FileSendRequest> {
        self.validate()?;
        let channel_target =
            |channel: &IdentityRef, parent: Option<&IdentityRef>| FileTarget::Channel {
                channel_id: channel.value().as_str().to_owned(),
                parent: parent.map(|parent| parent.value().as_str().to_owned()),
            };
        let target = match &self.target {
            DeliveryTarget::Actor { actor } => FileTarget::Private {
                user_id: actor.value().as_str().to_owned(),
            },
            DeliveryTarget::Conversation { conversation } => match conversation {
                ConversationRef::Direct {
                    participant,
                    conversation: None,
                } => FileTarget::Private {
                    user_id: participant.value().as_str().to_owned(),
                },
                ConversationRef::Direct {
                    conversation: Some(conversation),
                    ..
                } => channel_target(conversation, None),
                ConversationRef::Group { group } => FileTarget::Group {
                    group_id: group.value().as_str().to_owned(),
                },
                ConversationRef::Channel { channel, parent } => {
                    channel_target(channel, parent.as_ref())
                }
                _ => {
                    return Err(NuonoError::Validation(
                        "unsupported native file conversation target".into(),
                    ));
                }
            },
            DeliveryTarget::Channel { channel, parent } => channel_target(channel, parent.as_ref()),
            _ => {
                return Err(NuonoError::Validation(
                    "unsupported native file delivery target".into(),
                ));
            }
        };
        Ok(FileSendRequest {
            target,
            base64: self.base64.clone(),
            file_name: self.file_name.clone(),
            content_type: self.content_type.clone(),
        })
    }
}
