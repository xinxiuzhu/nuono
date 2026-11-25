//! 普通文件（非图片）收发请求与响应的序列化定义（不含任何平台鉴权凭据）。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// 文件发送目标（保留平台原生字符串标识，由具体平台适配器负责解析）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileTarget {
    Private {
        user_id: String,
    },
    Group {
        group_id: String,
    },
    Channel {
        channel_id: String,
        #[serde(default)]
        parent: Option<String>,
    },
}

impl FileTarget {
    pub fn id(&self) -> &str {
        match self {
            Self::Private { user_id } => user_id,
            Self::Group { group_id } => group_id,
            Self::Channel { channel_id, .. } => channel_id,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let valid = |value: &str| {
            !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        if !valid(self.id())
            || matches!(self, Self::Channel { parent: Some(parent), .. } if !valid(parent))
        {
            return Err(NuonoError::Validation("文件发送目标无效".into()));
        }
        Ok(())
    }
}

pub fn validate_file_metadata(file_name: &str, content_type: Option<&str>) -> Result<()> {
    if file_name.trim().is_empty()
        || file_name.len() > 255
        || matches!(file_name, "." | "..")
        || file_name.contains(['/', '\\', ':', '"'])
        || file_name.chars().any(char::is_control)
    {
        return Err(NuonoError::Validation(
            "文件名必须为有效文件名，不能包含路径".into(),
        ));
    }
    if let Some(value) = content_type {
        // 仅允许不带附加参数的纯净 MIME 类型（type/subtype），防止在构造 multipart 请求头时发生注入或歧义。
        let valid = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
        };
        if value.len() > 127
            || !value
                .split_once('/')
                .is_some_and(|(kind, subtype)| valid(kind) && valid(subtype))
        {
            return Err(NuonoError::Validation(
                "文件 MIME 类型无效，应为 type/subtype".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSendRequest {
    pub target: FileTarget,
    pub base64: String,
    pub file_name: String,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl FileSendRequest {
    /// 在交由平台适配器解码或发起 HTTP 上传前，先校验目标、文件名及 Base64 编码长度上限。
    pub fn validate(&self) -> Result<()> {
        self.target.validate()?;
        validate_file_metadata(&self.file_name, self.content_type.as_deref())?;
        validate_file_encoding(&self.base64)
    }
}

pub(super) fn validate_file_encoding(base64: &str) -> Result<()> {
    let encoded = base64.as_bytes();
    let padding = encoded.iter().rev().take_while(|&&b| b == b'=').count();
    if encoded.len() > MAX_FILE_BYTES.div_ceil(3) * 4
        || !encoded.len().is_multiple_of(4)
        || padding > 2
        || !encoded[..encoded.len() - padding]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(b))
        || encoded.len() / 4 * 3 - padding > MAX_FILE_BYTES
    {
        return Err(NuonoError::Validation(
            "文件内容编码无效或超过 32 MiB".into(),
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileSendResponse {
    Sent {
        message_ids: Vec<String>,
        raw: serde_json::Value,
    },
    Unsupported {},
}

impl FileSendResponse {
    pub fn validate(&self) -> Result<()> {
        if let Self::Sent { message_ids, .. } = self
            && (message_ids.is_empty()
                || message_ids.len() > 16
                || message_ids.iter().any(|id| {
                    id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control)
                }))
        {
            return Err(NuonoError::Validation(
                "平台未提供有效的文件消息回执".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundFileRequest {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub max_bytes: usize,
}

impl InboundFileRequest {
    pub fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 || self.max_bytes > MAX_FILE_BYTES {
            return Err(NuonoError::Validation("入站文件大小上限无效".into()));
        }
        if self.source.trim().is_empty()
            || self.source.len() > 4096
            || self.source.chars().any(char::is_control)
            || self.message_id.as_ref().is_some_and(|id| {
                id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control)
            })
        {
            return Err(NuonoError::Validation("入站文件标识无效".into()));
        }
        Ok(())
    }
}

/// 入站文件下载响应（文件名与 MIME 类型仅供参考，调用方仍需自行校验文件内容；
/// 严禁在响应中返回携带平台鉴权凭据的下载链接）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum InboundFileResponse {
    Available {
        base64: String,
        #[serde(default)]
        file_name: Option<String>,
        #[serde(default)]
        content_type: Option<String>,
    },
    Unsupported {},
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_send_rejects_paths_header_injection_invalid_encoding_and_ambiguous_wire() {
        let mut request = FileSendRequest {
            target: FileTarget::Private {
                user_id: "ou_001".into(),
            },
            base64: "e30=".into(),
            file_name: "records.json".into(),
            content_type: Some("application/json".into()),
        };
        assert!(request.validate().is_ok());
        for name in ["", ".", "..", "../x", "/tmp/x", "C:\\x", "x\n", "a\".json"] {
            assert!(validate_file_metadata(name, None).is_err());
        }
        for encoded in ["=", "====", "a===", "e3=0", "a?==", "e30"] {
            request.base64 = encoded.into();
            assert!(request.validate().is_err());
        }
        assert!(
            serde_json::from_value::<FileSendResponse>(
                serde_json::json!({"status":"unsupported","raw":{}})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<FileSendRequest>(serde_json::json!({"target":{"type":"private","user_id":123},"base64":"e30=","file_name":"records.json"})).is_err());
    }

    #[test]
    fn file_limit_is_independent_of_image_limit_and_wire_is_strict() {
        let mut request = InboundFileRequest {
            source: "file_id:fixture".into(),
            message_id: Some("original".into()),
            max_bytes: MAX_FILE_BYTES,
        };
        assert!(request.validate().is_ok());
        for limit in [0, MAX_FILE_BYTES + 1] {
            request.max_bytes = limit;
            assert!(request.validate().is_err());
        }
        assert!(
            serde_json::from_value::<InboundFileRequest>(
                serde_json::json!({"source":"file_id:x","max_bytes":1,"token":"not-allowed"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<InboundFileResponse>(
                serde_json::json!({"status":"unsupported","base64":"leaked"})
            )
            .is_err()
        );
    }
}
