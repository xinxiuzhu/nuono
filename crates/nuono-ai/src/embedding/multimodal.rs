//! 多模态向量化的受限输入定义。本模块仅做数据结构与配额校验，不负责打开本地路径、下载 URL 或校验素材权限。
use std::collections::BTreeSet;

use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{EmbeddingBatch, EmbeddingModel, EmbeddingPort, validate_vectors};

pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_MEDIA_BATCH_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_AUDIO_DURATION_MS: u32 = 300_000;
pub const MAX_INPUTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModality {
    Text,
    Image,
    Audio,
}

/// 对应一次明确模型选择的能力快照；不从模型名称推断能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingCapabilities {
    pub model: EmbeddingModel,
    pub modalities: BTreeSet<EmbeddingModality>,
}

impl EmbeddingCapabilities {
    pub fn text_only(model: EmbeddingModel) -> Self {
        Self {
            model,
            modalities: BTreeSet::from([EmbeddingModality::Text]),
        }
    }
}

/// 由宿主登记的媒体标识，不是文件名、URL 或下载指令。
///
/// asset_id 必须为 `asset_` + 32 位小写十六进制。宿主生成随机 ID 并验证
/// 账号/角色作用域后从自身素材仓储取出字节。ID、MIME 和摘要均不授予访问权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingAssetRef {
    pub asset_id: String,
    pub mime: String,
    pub sha256: String,
    pub byte_length: u64,
}

impl EmbeddingAssetRef {
    fn validate(&self) -> Result<()> {
        let valid_id = self.asset_id.strip_prefix("asset_").is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if !valid_id
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.byte_length == 0
            || self.byte_length > MAX_IMAGE_BYTES as u64
            || !matches!(
                self.mime.as_str(),
                "image/png"
                    | "image/jpeg"
                    | "image/webp"
                    | "audio/wav"
                    | "audio/mpeg"
                    | "audio/ogg"
                    | "audio/flac"
                    | "audio/webm"
            )
        {
            return Err(invalid(
                "Embedding media reference is invalid or exceeds its bound",
            ));
        }
        Ok(())
    }
}

/// 已验证长度与摘要的授权素材字节。不提供路径或 URL 构造器，也不在 Debug 中打印载荷。
///
/// 这里只验证声明和摘要；宿主负责解码媒体并核实 MIME、像素/时长与来源授权。
#[derive(Clone, PartialEq, Eq)]
pub struct EmbeddingMedia {
    reference: EmbeddingAssetRef,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for EmbeddingMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingMedia")
            .field("reference", &self.reference)
            .finish_non_exhaustive()
    }
}

impl EmbeddingMedia {
    /// 接收已由宿主授权读取的字节，验证元数据、实际长度与 SHA-256。
    ///
    /// # Errors
    /// 标识、MIME、长度、摘要无效或字节与声明不符时拒绝输入。
    pub fn new(mut reference: EmbeddingAssetRef, bytes: Vec<u8>) -> Result<Self> {
        reference.validate()?;
        if reference.byte_length != bytes.len() as u64
            || !reference
                .sha256
                .eq_ignore_ascii_case(&hex::encode(Sha256::digest(&bytes)))
        {
            return Err(invalid(
                "Embedding media bytes do not match their length or SHA-256",
            ));
        }
        reference.sha256.make_ascii_lowercase();
        Ok(Self { reference, bytes })
    }

    pub fn reference(&self) -> &EmbeddingAssetRef {
        &self.reference
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 每个输入产生一个向量；不隐式把图/音转成文件路径、链接或文字占位符。
#[derive(Clone, PartialEq, Eq)]
pub enum EmbeddingInput {
    Text(String),
    Image(EmbeddingMedia),
    Audio {
        media: EmbeddingMedia,
        duration_ms: u32,
    },
}

impl std::fmt::Debug for EmbeddingInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_struct("Text").field("bytes", &text.len()).finish(),
            Self::Image(media) => f.debug_tuple("Image").field(media).finish(),
            Self::Audio { media, duration_ms } => f
                .debug_struct("Audio")
                .field("media", media)
                .field("duration_ms", duration_ms)
                .finish(),
        }
    }
}

impl EmbeddingInput {
    pub fn modality(&self) -> EmbeddingModality {
        match self {
            Self::Text(_) => EmbeddingModality::Text,
            Self::Image(_) => EmbeddingModality::Image,
            Self::Audio { .. } => EmbeddingModality::Audio,
        }
    }
}

/// 在调用 provider 前校验模型能力及资源预算，不读取外部资源。
///
/// 文本保留既有 8192 字节/项、128 KiB/批规则；图/音合计不超过 64 MiB。
/// 音频时长采用有界正整数毫秒，类型上不接受 NaN/无穷。
///
/// # Errors
/// 模型不匹配、模态不受支持、MIME 不符或任一预算超限时返回错误。
pub fn validate_multimodal_input(
    model_id: i64,
    inputs: &[EmbeddingInput],
    capabilities: &EmbeddingCapabilities,
) -> Result<()> {
    if model_id <= 0
        || capabilities.model.id != model_id
        || capabilities.modalities.is_empty()
        || inputs.is_empty()
        || inputs.len() > MAX_INPUTS
    {
        return Err(invalid(
            "Embedding input model, capabilities or batch size is invalid",
        ));
    }
    let mut text_bytes = 0usize;
    let mut media_bytes = 0usize;
    for input in inputs {
        if !capabilities.modalities.contains(&input.modality()) {
            return Err(invalid(
                "Embedding input modality is unsupported by the selected provider",
            ));
        }
        match input {
            EmbeddingInput::Text(text) => {
                if text.trim().is_empty() || text.len() > 8192 || text.contains('\0') {
                    return Err(invalid("Embedding text exceeds its bounded input contract"));
                }
                text_bytes += text.len();
            }
            EmbeddingInput::Image(media) => {
                if !matches!(
                    media.reference.mime.as_str(),
                    "image/png" | "image/jpeg" | "image/webp"
                ) || media.bytes.len() > MAX_IMAGE_BYTES
                {
                    return Err(invalid("Embedding image MIME or byte size is invalid"));
                }
                media_bytes += media.bytes.len();
            }
            EmbeddingInput::Audio { media, duration_ms } => {
                if !matches!(
                    media.reference.mime.as_str(),
                    "audio/wav" | "audio/mpeg" | "audio/ogg" | "audio/flac" | "audio/webm"
                ) || media.bytes.len() > MAX_AUDIO_BYTES
                    || !(1..=MAX_AUDIO_DURATION_MS).contains(duration_ms)
                {
                    return Err(invalid(
                        "Embedding audio MIME, byte size or duration is invalid",
                    ));
                }
                media_bytes += media.bytes.len();
            }
        }
    }
    if text_bytes > 128 * 1024 || media_bytes > MAX_MEDIA_BATCH_BYTES {
        return Err(invalid(
            "Embedding inputs exceed the aggregate batch budget",
        ));
    }
    Ok(())
}

/// 消费者入口：按能力快照先校验输入，再验证返回模型身份和有界向量。
///
/// provider 可替换，当前文字 HTTP 实现不会因此自动获得图片或声音支持。
///
/// # Errors
/// 能力查询、输入验证、provider 调用失败或返回模型/向量无效时返回错误。
pub async fn embed_multimodal(
    port: &dyn EmbeddingPort,
    model_id: i64,
    inputs: Vec<EmbeddingInput>,
) -> Result<EmbeddingBatch> {
    let capabilities = port.capabilities(model_id).await?;
    validate_multimodal_input(model_id, &inputs, &capabilities)?;
    let rows = inputs.len();
    let batch = port.embed_inputs(model_id, inputs).await?;
    if batch.model != capabilities.model {
        return Err(invalid(
            "Embedding model identity changed during the request",
        ));
    }
    validate_vectors(&batch.vectors, rows)?;
    Ok(batch)
}

fn invalid(message: &str) -> NuonoError {
    NuonoError::Validation(message.into())
}
