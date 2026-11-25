//! 用户头像获取请求与响应的序列化定义（具体头像地址拼接与平台凭据由外层适配器负责）。
use nuono_domain::{IdentityKind, IdentityRef, IdentityScheme, Platform};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAvatarRequest {
    #[serde(deserialize_with = "deserialize_identity")]
    pub identity: IdentityRef,
    /// 期望的头像像素尺寸；各平台适配器会选取最接近的原生规格。
    pub size: u16,
    pub max_bytes: usize,
    /// 入站事件携带的头像 URL 提示（各平台适配器在下载前仍需自行校验域名与身份合法性）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AvatarIdentityWire {
    platform: Platform,
    kind: IdentityKind,
    scheme: IdentityScheme,
    value: String,
    #[serde(default)]
    namespace: Option<String>,
}

fn deserialize_identity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<IdentityRef, D::Error> {
    let wire = AvatarIdentityWire::deserialize(deserializer)?;
    IdentityRef::new(
        wire.platform,
        wire.kind,
        wire.scheme,
        wire.value,
        wire.namespace,
    )
    .map_err(serde::de::Error::custom)
}

impl UserAvatarRequest {
    pub fn validate(&self) -> Result<()> {
        if *self.identity.kind() != IdentityKind::User {
            return Err(NuonoError::Validation("头像需要原生用户身份".into()));
        }
        if !(16..=1024).contains(&self.size) {
            return Err(NuonoError::Validation("头像尺寸需在 16–1024 之间".into()));
        }
        if self.max_bytes == 0 || self.max_bytes > super::MAX_IMAGE_BYTES {
            return Err(NuonoError::Validation("头像大小上限无效".into()));
        }
        if self.source_hint.as_ref().is_some_and(|hint| {
            hint.len() > 4096
                || hint.bytes().any(|byte| byte.is_ascii_control())
                || !(hint.starts_with("https://") || hint.starts_with("http://"))
        }) {
            return Err(NuonoError::Validation("头像来源提示无效".into()));
        }
        Ok(())
    }
}

/// 头像图片直接以 Base64 字节通过内部动作通道返回，绝不向外暴露带平台鉴权信息的原始下载链接。
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserAvatarResponse {
    Available {
        base64: String,
        #[serde(default)]
        version: Option<String>,
    },
    Absent {},
    Unsupported {},
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_domain::{IdentityScheme, Platform};

    fn request() -> UserAvatarRequest {
        UserAvatarRequest {
            identity: IdentityRef::new(
                Platform::QqOfficial,
                IdentityKind::User,
                IdentityScheme::OpenId,
                "native-member",
                Some("app/groups/native-group"),
            )
            .unwrap(),
            size: 256,
            max_bytes: 1024,
            source_hint: None,
        }
    }

    #[test]
    fn exact_scoped_identity_roundtrips_and_invalid_requests_are_rejected() {
        let request = request();
        request.validate().unwrap();
        let decoded: UserAvatarRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(decoded.identity, request.identity);
        for size in [0, 15, 1025, u16::MAX] {
            let mut invalid = request.clone();
            invalid.size = size;
            assert!(invalid.validate().is_err());
        }
        for limit in [0, super::super::MAX_IMAGE_BYTES + 1] {
            let mut invalid = request.clone();
            invalid.max_bytes = limit;
            assert!(invalid.validate().is_err());
        }
        let mut invalid = request.clone();
        invalid.identity = IdentityRef::new(
            Platform::QqOfficial,
            IdentityKind::Group,
            IdentityScheme::OpenId,
            "native-group",
            Some("app/groups/native-group"),
        )
        .unwrap();
        assert!(invalid.validate().is_err());
        assert!(
            IdentityRef::new(
                Platform::QqOfficial,
                IdentityKind::User,
                IdentityScheme::OpenId,
                "native-member",
                None::<&str>,
            )
            .is_err()
        );
        for hint in [
            "file:///private/data",
            "https://example.invalid/\ncredential",
        ] {
            let mut invalid = request.clone();
            invalid.source_hint = Some(hint.into());
            assert!(invalid.validate().is_err());
        }
        let mut wire = serde_json::to_value(&request).unwrap();
        wire["identity"]["extra_scope"] = serde_json::json!("ignored-scope-is-unsafe");
        assert!(serde_json::from_value::<UserAvatarRequest>(wire).is_err());
        let mut wire = serde_json::to_value(&request).unwrap();
        wire["token"] = serde_json::json!("must-not-enter-contract");
        assert!(serde_json::from_value::<UserAvatarRequest>(wire).is_err());
    }
}
