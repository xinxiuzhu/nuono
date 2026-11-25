//! 工具返回的图片/视觉内容数据结构。本层只定义类型，不负责打开本地文件或下载远程资源。
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const MAX_TOOL_IMAGES: usize = 4;
/// 单张图片 Base64 Data URI（含 MIME 头部）的最大字节数，而非解码后的原始图像大小。
pub const MAX_TOOL_IMAGE_BYTES: usize = 96 * 1024;
pub const MAX_TOOL_MEDIA_BYTES: usize = 256 * 1024;
const MAX_PUBLIC_URL_BYTES: usize = 8192;

/// 经过校验的公开 HTTPS 图片链接或受限的 Base64 Data URI。
/// 构造函数仅做格式与签名校验，不解析 DNS、不发起网络请求、不读取本地路径，也不接受带凭据的链接。
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AgentToolImage(String);

impl std::fmt::Debug for AgentToolImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentToolImage")
            .field("metadata", &self.metadata())
            .finish()
    }
}

impl<'de> Deserialize<'de> for AgentToolImage {
    fn deserialize<D: Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let source = String::deserialize(de)?;
        Self::from_source(source).map_err(serde::de::Error::custom)
    }
}

impl AgentToolImage {
    pub fn public_https(source: impl Into<String>) -> Result<Self> {
        let source = source.into();
        let parsed =
            url::Url::parse(&source).map_err(|_| invalid("工具图片需要有效的公开 HTTPS 地址"))?;
        let valid_host = parsed.host_str().is_some_and(|host| {
            let host = host.trim_end_matches('.');
            host.len() <= 253
                && host.contains('.')
                && !host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && ![
                    ".localhost",
                    ".local",
                    ".internal",
                    ".lan",
                    ".home",
                    ".arpa",
                ]
                .iter()
                .any(|suffix| host.ends_with(suffix))
                && host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        });
        if source.len() > MAX_PUBLIC_URL_BYTES
            || !source.starts_with("https://")
            || source.chars().any(|c| c.is_whitespace() || c.is_control())
            || source.contains('\\')
            || parsed.scheme() != "https"
            || !matches!(parsed.host(), Some(url::Host::Domain(_)))
            || !valid_host
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port_or_known_default() != Some(443)
        {
            return Err(invalid(
                "工具图片仅允许无登录凭据的公开 HTTPS 域名地址；不支持 IP、本地地址或文件路径",
            ));
        }
        Ok(Self(source))
    }

    pub fn inline_data_uri(source: impl Into<String>) -> Result<Self> {
        let source = source.into();
        if source.len() > MAX_TOOL_IMAGE_BYTES {
            return Err(invalid("单张工具内嵌图片超过 96 KiB"));
        }
        let (header, payload) = source
            .split_once(',')
            .ok_or_else(|| invalid("工具内嵌图片格式无效"))?;
        let mime = header
            .strip_prefix("data:")
            .and_then(|header| header.strip_suffix(";base64"))
            .ok_or_else(|| invalid("工具图片仅支持图片 base64 data URI"))?;
        let bytes = STANDARD
            .decode(payload)
            .map_err(|_| invalid("工具图片 base64 无效"))?;
        if !valid_image_signature(mime, &bytes) {
            return Err(invalid(
                "工具图片须为 MIME 与内容一致的 PNG、JPEG、WebP 或 GIF",
            ));
        }
        Ok(Self(source))
    }

    /// 将已读取的图像字节编码为 Data URI。宿主不会替工具自动缩放或下载图片，由工具自行控制在大小上限内。
    pub fn inline_bytes(mime: &str, bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_TOOL_IMAGE_BYTES * 3 / 4 {
            return Err(invalid("单张工具内嵌图片超过 96 KiB 编码预算"));
        }
        Self::inline_data_uri(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
    }

    pub fn from_source(source: impl Into<String>) -> Result<Self> {
        let source = source.into();
        if source.starts_with("data:") {
            Self::inline_data_uri(source)
        } else {
            Self::public_https(source)
        }
    }

    pub fn source(&self) -> &str {
        &self.0
    }

    /// 审计日志与 JSON 摘要信息，特意省略原始 URL 和编码后的图像内容。
    pub fn metadata(&self) -> Value {
        json!({
            "kind": if self.0.starts_with("data:") { "inline_image" } else { "public_https" },
            "media_type": self.0.strip_prefix("data:").and_then(|source| source.split(';').next()),
            "source_bytes": self.0.len(),
            "source_sha256": hex::encode(Sha256::digest(self.0.as_bytes())),
        })
    }
}

fn valid_image_signature(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        _ => false,
    }
}

fn invalid(text: &str) -> NuonoError {
    NuonoError::Validation(text.into())
}

pub fn validate_images(images: &[AgentToolImage]) -> Result<()> {
    if images.len() > MAX_TOOL_IMAGES
        || images
            .iter()
            .map(|image| image.source().len())
            .sum::<usize>()
            > MAX_TOOL_MEDIA_BYTES
    {
        return Err(invalid("每次工具结果最多 4 张图片、合计 256 KiB"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialization_enforces_the_same_visual_contract_as_construction() {
        for source in [
            "https://127.0.0.1/image.png",
            "https://user:password@example.com/image.png",
            "file:///tmp/image.png",
            "data:image/png;base64,SGVsbG8=",
        ] {
            assert!(serde_json::from_value::<AgentToolImage>(json!(source)).is_err());
        }
        let image = AgentToolImage::inline_bytes("image/png", b"\x89PNG\r\n\x1a\n").unwrap();
        let restored: AgentToolImage =
            serde_json::from_value(serde_json::to_value(&image).unwrap()).unwrap();
        assert_eq!(restored, image);
        let audit = image.metadata().to_string();
        assert!(!audit.contains(image.source()));
        assert!(!format!("{image:?}").contains(image.source()));
        assert!(validate_images(&vec![image; MAX_TOOL_IMAGES + 1]).is_err());
    }
}
