//! 群角色（Persona）与个人伴侣角色卡共用的媒体文件（头像、立绘、参考音频）上传校验与转码逻辑。
use super::ApiResponse;
use axum::{
    extract::Multipart,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use image::{ImageFormat, imageops::FilterType};
use nuono_app::persona::PersonaAssetKind;
use nuono_core::NuonoError;
use std::io::Cursor;
use tokio::io::AsyncWriteExt;

pub(super) const MAX_REQUEST_BYTES: usize = 21 * 1024 * 1024;
pub(super) const MAX_AUDIO_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct HttpError(pub StatusCode, pub String);

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        crate::api::ApiReply::<()>::error(self.0, self.1).into_response()
    }
}

impl From<NuonoError> for HttpError {
    fn from(error: NuonoError) -> Self {
        let reply = ApiResponse::<()>::from_err(&error);
        Self(reply.status, reply.body.0.error.unwrap_or_default())
    }
}

pub(super) async fn read_upload(
    mut multipart: Multipart,
    kind: PersonaAssetKind,
) -> Result<(Vec<u8>, String, i64), HttpError> {
    let mut upload = None;
    let mut revision = None;
    let max = if kind == PersonaAssetKind::ReferenceAudio {
        MAX_AUDIO_BYTES
    } else {
        MAX_IMAGE_BYTES
    };
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| HttpError(error.status(), "上传表单损坏或超过请求大小限制".into()))?
    {
        match field.name() {
            Some("file") => {
                if upload.is_some() {
                    return Err(HttpError(
                        StatusCode::BAD_REQUEST,
                        "每次只能上传一个文件".into(),
                    ));
                }
                let mime = field.content_type().unwrap_or_default().to_string();
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|error| {
                    HttpError(error.status(), "读取上传文件失败或超过请求大小限制".into())
                })? {
                    if chunk.len() > max.saturating_sub(bytes.len()) {
                        return Err(HttpError(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!("素材不能超过 {} MiB", max / 1024 / 1024),
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                if bytes.is_empty() {
                    return Err(HttpError(StatusCode::BAD_REQUEST, "上传文件为空".into()));
                }
                upload = Some((bytes, mime));
            }
            Some("revision") => {
                if revision.is_some() {
                    return Err(HttpError(
                        StatusCode::BAD_REQUEST,
                        "revision 不能重复".into(),
                    ));
                }
                let value = field
                    .text()
                    .await
                    .map_err(|error| HttpError(error.status(), "revision 无效".into()))?;
                revision = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| HttpError(StatusCode::BAD_REQUEST, "revision 无效".into()))?,
                );
            }
            _ => {
                return Err(HttpError(
                    StatusCode::BAD_REQUEST,
                    "上传表单包含未知字段".into(),
                ));
            }
        }
    }
    let (bytes, mime) =
        upload.ok_or_else(|| HttpError(StatusCode::BAD_REQUEST, "上传表单缺少 file".into()))?;
    let revision = revision.filter(|revision| *revision > 0).ok_or_else(|| {
        HttpError(
            StatusCode::BAD_REQUEST,
            "上传素材必须携带有效 revision".into(),
        )
    })?;
    Ok((bytes, mime, revision))
}

pub(super) async fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), HttpError> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "创建角色素材失败".into()))?;
    file.write_all(bytes)
        .await
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "写入角色素材失败".into()))?;
    file.sync_all()
        .await
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "提交角色素材失败".into()))?;
    Ok(())
}

pub(super) fn normalize_image(
    bytes: &[u8],
    mime: &str,
    kind: PersonaAssetKind,
) -> Result<Vec<u8>, HttpError> {
    let declared = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => {
            return Err(HttpError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "图片仅支持 PNG、JPEG、WebP".into(),
            ));
        }
    };
    let actual = image::guess_format(bytes)
        .map_err(|_| HttpError(StatusCode::BAD_REQUEST, "无法识别图片".into()))?;
    if declared != actual {
        return Err(HttpError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "图片内容与 MIME 类型不符".into(),
        ));
    }
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), actual)
        .into_dimensions()
        .map_err(|_| HttpError(StatusCode::BAD_REQUEST, "无法读取图片尺寸".into()))?;
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || u64::from(width) * u64::from(height) > 25_000_000
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "图片不能超过 8192 像素单边或 2500 万总像素".into(),
        ));
    }
    let image = image::load_from_memory_with_format(bytes, actual)
        .map_err(|_| HttpError(StatusCode::BAD_REQUEST, "图片已损坏".into()))?;
    let normalized = if kind == PersonaAssetKind::Avatar {
        let side = width.min(height);
        image
            .crop_imm((width - side) / 2, (height - side) / 2, side, side)
            .resize_exact(512, 512, FilterType::Lanczos3)
    } else if width > 2048 || height > 2048 {
        image.resize(2048, 2048, FilterType::Lanczos3)
    } else {
        image
    };
    let mut output = Vec::new();
    normalized
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "图片转码失败".into()))?;
    Ok(output)
}

pub(super) fn content_type(filename: &str) -> Option<&'static str> {
    match filename.rsplit_once('.')?.1 {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mpeg"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        _ => None,
    }
}

/// 仅校验音频文件头与基本容器格式；具体时长、编码参数与采样率由下游 TTS 服务负责校验。
pub(super) fn audio_extension(bytes: &[u8], mime: &str) -> Result<&'static str, HttpError> {
    // 部分浏览器上传本地 FLAC/WAV 文件时不会带具体 MIME 类型；
    // 仅在 MIME 为空或通用二进制流时按文件头自动嗅探，若声明的 MIME 与实际容器矛盾则直接拒绝。
    if matches!(mime, "" | "application/octet-stream") {
        for declared in ["audio/wav", "audio/ogg", "audio/flac", "audio/mpeg"] {
            if let Ok(extension) = audio_extension(bytes, declared) {
                return Ok(extension);
            }
        }
        return Err(HttpError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "无法识别参考音频格式".into(),
        ));
    }
    let extension = match mime {
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" if valid_wav(bytes) => "wav",
        "audio/mpeg" | "audio/mp3" if valid_mp3(bytes) => "mp3",
        "audio/ogg" | "application/ogg" if bytes.len() >= 28 && bytes.starts_with(b"OggS\0") => {
            "ogg"
        }
        "audio/flac" | "audio/x-flac" if bytes.len() >= 42 && bytes.starts_with(b"fLaC") => "flac",
        _ => {
            return Err(HttpError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "参考音频仅支持内容有效的 WAV、MP3、OGG、FLAC".into(),
            ));
        }
    };
    Ok(extension)
}

fn valid_wav(bytes: &[u8]) -> bool {
    if bytes.len() < 44 || !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return false;
    }
    let mut offset = 12usize;
    let mut format = false;
    let mut data = false;
    while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let Some(end) = offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(length))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        if &header[..4] == b"fmt " {
            format |= length >= 16;
        }
        if &header[..4] == b"data" {
            data |= length > 0;
        }
        offset = end.saturating_add(length % 2);
    }
    format && data
}

fn valid_mp3(bytes: &[u8]) -> bool {
    let offset = if bytes.starts_with(b"ID3") {
        if bytes.len() < 10 || bytes[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return false;
        }
        10 + bytes[6..10]
            .iter()
            .fold(0usize, |size, byte| (size << 7) | usize::from(*byte))
    } else {
        0
    };
    bytes.get(offset..).is_some_and(|audio| {
        audio.windows(4).take(4096).any(|frame| {
            frame[0] == 0xff
                && frame[1] & 0xe0 == 0xe0
                && frame[1] & 0x06 != 0
                && frame[1] & 0x18 != 0x08
                && frame[2] & 0xf0 != 0
                && frame[2] & 0xf0 != 0xf0
                && frame[2] & 0x0c != 0x0c
        })
    })
}

#[cfg(test)]
#[path = "../../tests/unit/api/role_media.rs"]
mod tests;
