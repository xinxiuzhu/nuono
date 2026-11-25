use super::{DomainModelError, validate_text};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 平台原生标识符，按原始字节原样保留（包括大小写与首尾空白字符）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct NativeId(String);

impl NativeId {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainModelError> {
        let value = value.into();
        validate_text(&value, "native id")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for NativeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for NativeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for NativeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NativeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Platform {
    OneBot,
    Telegram,
    Discord,
    Feishu,
    QqOfficial,
    Other(String),
}

impl Platform {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OneBot => "onebot",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Feishu => "feishu",
            Self::QqOfficial => "qq_official",
            Self::Other(value) => value.as_str(),
        }
    }

    fn validate(&self) -> Result<(), DomainModelError> {
        if let Self::Other(name) = self {
            validate_text(name, "platform")?;
        }
        Ok(())
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IdentityKind {
    User,
    Bot,
    Group,
    Channel,
    Conversation,
    Tenant,
    Organization,
    Message,
    Other(String),
}

impl IdentityKind {
    fn validate(&self) -> Result<(), DomainModelError> {
        if let Self::Other(name) = self {
            validate_text(name, "identity kind")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IdentityScheme {
    Native,
    Numeric,
    Snowflake,
    OpenId,
    UserId,
    UnionId,
    ChatId,
    AppId,
    Other(String),
}

impl IdentityScheme {
    fn validate(&self) -> Result<(), DomainModelError> {
        if let Self::Other(name) = self {
            validate_text(name, "identity scheme")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct IdentityRef {
    platform: Platform,
    kind: IdentityKind,
    scheme: IdentityScheme,
    value: NativeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<NativeId>,
}

#[derive(Deserialize)]
struct IdentityRefWire {
    platform: Platform,
    kind: IdentityKind,
    scheme: IdentityScheme,
    value: NativeId,
    #[serde(default)]
    namespace: Option<NativeId>,
}

impl IdentityRef {
    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    pub fn kind(&self) -> &IdentityKind {
        &self.kind
    }

    pub fn scheme(&self) -> &IdentityScheme {
        &self.scheme
    }

    pub fn value(&self) -> &NativeId {
        &self.value
    }

    pub fn namespace(&self) -> Option<&NativeId> {
        self.namespace.as_ref()
    }

    pub fn new(
        platform: Platform,
        kind: IdentityKind,
        scheme: IdentityScheme,
        value: impl Into<String>,
        namespace: Option<impl Into<String>>,
    ) -> Result<Self, DomainModelError> {
        let value = NativeId::new(value)?;
        let namespace = namespace
            .map(|value| NativeId::new(value.into()))
            .transpose()?;
        Self::from_parts(platform, kind, scheme, value, namespace)
    }

    fn from_parts(
        platform: Platform,
        kind: IdentityKind,
        scheme: IdentityScheme,
        value: NativeId,
        namespace: Option<NativeId>,
    ) -> Result<Self, DomainModelError> {
        platform.validate()?;
        kind.validate()?;
        scheme.validate()?;

        match (&platform, &scheme) {
            (Platform::Telegram, IdentityScheme::Numeric) => {
                value
                    .as_str()
                    .parse::<i64>()
                    .map_err(|_| DomainModelError::InvalidValue {
                        field: "value",
                        expected: "a signed 64-bit Telegram identifier",
                    })?;
            }
            (Platform::Discord, IdentityScheme::Snowflake) => {
                value
                    .as_str()
                    .parse::<u64>()
                    .map_err(|_| DomainModelError::InvalidValue {
                        field: "value",
                        expected: "an unsigned 64-bit Discord snowflake",
                    })?;
            }
            (_, IdentityScheme::Numeric) => {
                value
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| DomainModelError::InvalidValue {
                        field: "value",
                        expected: "a signed decimal integer",
                    })?;
            }
            _ => {}
        }

        let scoped_open_id = matches!(scheme, IdentityScheme::OpenId);
        let feishu_only_scheme = matches!(
            scheme,
            IdentityScheme::UserId | IdentityScheme::UnionId | IdentityScheme::ChatId
        );
        let app_id_scheme = matches!(scheme, IdentityScheme::AppId);
        if matches!(platform, Platform::Feishu | Platform::QqOfficial)
            && scoped_open_id
            && namespace.is_none()
        {
            return Err(DomainModelError::MissingNamespace {
                field: "namespace",
                platform: match platform {
                    Platform::Feishu => "feishu",
                    Platform::QqOfficial => "qq_official",
                    _ => unreachable!(),
                },
            });
        }
        if feishu_only_scheme && platform != Platform::Feishu {
            return Err(DomainModelError::InvalidValue {
                field: "scheme",
                expected: "a Feishu platform identity",
            });
        }
        if app_id_scheme && !matches!(platform, Platform::Feishu | Platform::QqOfficial) {
            return Err(DomainModelError::InvalidValue {
                field: "scheme",
                expected: "a Feishu or QQ Official application identity",
            });
        }
        if scoped_open_id && !matches!(platform, Platform::Feishu | Platform::QqOfficial) {
            return Err(DomainModelError::InvalidValue {
                field: "scheme",
                expected: "a Feishu or QQ Official open identity",
            });
        }
        if scheme == IdentityScheme::Snowflake && platform != Platform::Discord {
            return Err(DomainModelError::InvalidValue {
                field: "scheme",
                expected: "a Discord platform identity",
            });
        }

        Ok(Self {
            platform,
            kind,
            scheme,
            value,
            namespace,
        })
    }
}

impl<'de> Deserialize<'de> for IdentityRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = IdentityRefWire::deserialize(deserializer)?;
        Self::from_parts(
            wire.platform,
            wire.kind,
            wire.scheme,
            wire.value,
            wire.namespace,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct BotIdentity {
    pub instance_id: NativeId,
    pub account_id: IdentityRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<IdentityRef>,
}

#[derive(Deserialize)]
struct BotIdentityWire {
    instance_id: NativeId,
    account_id: IdentityRef,
    #[serde(default)]
    tenant: Option<IdentityRef>,
}

impl BotIdentity {
    pub fn new(
        instance_id: impl Into<String>,
        account_id: IdentityRef,
        tenant: Option<IdentityRef>,
    ) -> Result<Self, DomainModelError> {
        Self::from_parts(NativeId::new(instance_id)?, account_id, tenant)
    }

    fn from_parts(
        instance_id: NativeId,
        account_id: IdentityRef,
        tenant: Option<IdentityRef>,
    ) -> Result<Self, DomainModelError> {
        if account_id.kind != IdentityKind::Bot {
            return Err(DomainModelError::InvalidValue {
                field: "account_id",
                expected: "a bot identity",
            });
        }
        if let Some(tenant) = &tenant {
            if tenant.platform != account_id.platform {
                return Err(DomainModelError::PlatformMismatch { field: "tenant" });
            }
            if tenant.kind != IdentityKind::Tenant {
                return Err(DomainModelError::InvalidValue {
                    field: "tenant",
                    expected: "a tenant identity",
                });
            }
        }
        Ok(Self {
            instance_id,
            account_id,
            tenant,
        })
    }

    pub fn platform(&self) -> &Platform {
        &self.account_id.platform
    }
}

impl<'de> Deserialize<'de> for BotIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BotIdentityWire::deserialize(deserializer)?;
        Self::from_parts(wire.instance_id, wire.account_id, wire.tenant)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConversationRef {
    Direct {
        participant: IdentityRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<IdentityRef>,
    },
    Group {
        group: IdentityRef,
    },
    Channel {
        channel: IdentityRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<IdentityRef>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ConversationRefWire {
    Direct {
        participant: IdentityRef,
        #[serde(default)]
        conversation: Option<IdentityRef>,
    },
    Group {
        group: IdentityRef,
    },
    Channel {
        channel: IdentityRef,
        #[serde(default)]
        parent: Option<IdentityRef>,
    },
}

impl ConversationRef {
    pub fn direct(
        participant: IdentityRef,
        conversation: Option<IdentityRef>,
    ) -> Result<Self, DomainModelError> {
        Self::ensure_kind(&participant, IdentityKind::User, "participant")?;
        if let Some(conversation) = &conversation {
            Self::ensure_kind(conversation, IdentityKind::Conversation, "conversation")?;
            Self::ensure_same_platform(&participant, conversation, "conversation")?;
        }
        Ok(Self::Direct {
            participant,
            conversation,
        })
    }

    pub fn group(group: IdentityRef) -> Result<Self, DomainModelError> {
        Self::ensure_kind(&group, IdentityKind::Group, "group")?;
        Ok(Self::Group { group })
    }

    pub fn channel(
        channel: IdentityRef,
        parent: Option<IdentityRef>,
    ) -> Result<Self, DomainModelError> {
        Self::ensure_kind(&channel, IdentityKind::Channel, "channel")?;
        if let Some(parent) = &parent {
            if !matches!(
                parent.kind,
                IdentityKind::Group | IdentityKind::Organization
            ) {
                return Err(DomainModelError::InvalidValue {
                    field: "parent",
                    expected: "a group or organization identity",
                });
            }
            Self::ensure_same_platform(&channel, parent, "parent")?;
        }
        Ok(Self::Channel { channel, parent })
    }

    fn ensure_kind(
        identity: &IdentityRef,
        expected: IdentityKind,
        field: &'static str,
    ) -> Result<(), DomainModelError> {
        if identity.kind != expected {
            return Err(DomainModelError::InvalidValue {
                field,
                expected: "identity kind matching the conversation type",
            });
        }
        Ok(())
    }

    fn ensure_same_platform(
        left: &IdentityRef,
        right: &IdentityRef,
        field: &'static str,
    ) -> Result<(), DomainModelError> {
        if left.platform != right.platform {
            return Err(DomainModelError::PlatformMismatch { field });
        }
        Ok(())
    }

    pub fn platform(&self) -> &Platform {
        match self {
            Self::Direct { participant, .. } => &participant.platform,
            Self::Group { group } => &group.platform,
            Self::Channel { channel, .. } => &channel.platform,
        }
    }

    pub fn identity(&self) -> &IdentityRef {
        match self {
            Self::Direct {
                participant,
                conversation,
            } => conversation.as_ref().unwrap_or(participant),
            Self::Group { group } => group,
            Self::Channel { channel, .. } => channel,
        }
    }

    pub fn validate(&self) -> Result<(), DomainModelError> {
        match self {
            Self::Direct {
                participant,
                conversation,
            } => Self::direct(participant.clone(), conversation.clone()).map(|_| ()),
            Self::Group { group } => Self::group(group.clone()).map(|_| ()),
            Self::Channel { channel, parent } => {
                Self::channel(channel.clone(), parent.clone()).map(|_| ())
            }
        }
    }
}

impl<'de> Deserialize<'de> for ConversationRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ConversationRefWire::deserialize(deserializer)?;
        match wire {
            ConversationRefWire::Direct {
                participant,
                conversation,
            } => Self::direct(participant, conversation),
            ConversationRefWire::Group { group } => Self::group(group),
            ConversationRefWire::Channel { channel, parent } => Self::channel(channel, parent),
        }
        .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_kind_and_namespace_are_part_of_identity() {
        let user = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::User,
            IdentityScheme::Numeric,
            "-42",
            None::<String>,
        )
        .unwrap();
        let bot = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::Bot,
            IdentityScheme::Numeric,
            "-42",
            None::<String>,
        )
        .unwrap();
        let discord_user = IdentityRef::new(
            Platform::Discord,
            IdentityKind::User,
            IdentityScheme::Snowflake,
            "42",
            None::<String>,
        )
        .unwrap();
        let namespaced_user = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::User,
            IdentityScheme::Numeric,
            "-42",
            Some("tenant-a"),
        )
        .unwrap();
        assert_ne!(user, bot);
        assert_ne!(user, discord_user);
        assert_ne!(user, namespaced_user);
        assert!(ConversationRef::group(user).is_err());
    }

    #[test]
    fn telegram_negative_ids_and_discord_large_snowflakes_are_preserved() {
        let telegram = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::Group,
            IdentityScheme::Numeric,
            "-1001234567890",
            None::<String>,
        )
        .unwrap();
        assert_eq!(telegram.value.as_str(), "-1001234567890");

        let discord = IdentityRef::new(
            Platform::Discord,
            IdentityKind::User,
            IdentityScheme::Snowflake,
            "18446744073709551615",
            None::<String>,
        )
        .unwrap();
        assert_eq!(discord.value.as_str(), "18446744073709551615");
        assert!(
            IdentityRef::new(
                Platform::Discord,
                IdentityKind::User,
                IdentityScheme::Snowflake,
                "18446744073709551616",
                None::<String>,
            )
            .is_err()
        );
    }

    #[test]
    fn feishu_requires_namespace_on_construction_and_deserialization() {
        assert!(
            IdentityRef::new(
                Platform::Feishu,
                IdentityKind::User,
                IdentityScheme::OpenId,
                "ou_abc",
                None::<String>,
            )
            .is_err()
        );
        let invalid = serde_json::json!({
            "platform": "feishu",
            "kind": "user",
            "scheme": "open_id",
            "value": "ou_abc"
        });
        assert!(serde_json::from_value::<IdentityRef>(invalid).is_err());
    }

    #[test]
    fn direct_and_channel_keep_platform_scope() {
        let participant = IdentityRef::new(
            Platform::Discord,
            IdentityKind::User,
            IdentityScheme::Snowflake,
            "1",
            None::<String>,
        )
        .unwrap();
        let channel = IdentityRef::new(
            Platform::Discord,
            IdentityKind::Conversation,
            IdentityScheme::Snowflake,
            "2",
            None::<String>,
        )
        .unwrap();
        assert!(ConversationRef::direct(participant, Some(channel)).is_ok());

        let mismatched = IdentityRef::new(
            Platform::Telegram,
            IdentityKind::Conversation,
            IdentityScheme::Numeric,
            "2",
            None::<String>,
        )
        .unwrap();
        let discord_user = IdentityRef::new(
            Platform::Discord,
            IdentityKind::User,
            IdentityScheme::Snowflake,
            "1",
            None::<String>,
        )
        .unwrap();
        assert!(ConversationRef::direct(discord_user, Some(mismatched)).is_err());
    }
}
