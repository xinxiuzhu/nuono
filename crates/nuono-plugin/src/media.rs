//! 图片来源与有大小限制的读取。插件传字节、文件或 URL，平台上传由宿主完成。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::io::AsyncReadExt;

use crate::MessageSegment;

pub use crate::api::{FileDelivery, InboundFile};
/// SDK 图片读取的默认上限；平台仍可能施加更小的限制。
pub use nuono_app::media::{MAX_FILE_BYTES, MAX_IMAGE_BYTES};

/// 要发送的图片。文件在发送前由宿主进程读取，远程 OneBot 也能收到内容。
#[derive(Clone)]
#[non_exhaustive]
pub enum ImageSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
    Url(String),
    /// 平台已上传文件的标识，只能在对应平台使用。
    FileId(String),
}

impl std::fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
            Self::Path(path) => f.debug_tuple("Path").field(path).finish(),
            Self::Url(url) => f.debug_tuple("Url").field(url).finish(),
            Self::FileId(id) => f.debug_tuple("FileId").field(id).finish(),
        }
    }
}

impl ImageSource {
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(bytes.into())
    }

    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    pub fn url(url: impl Into<String>) -> Self {
        Self::Url(url.into())
    }

    pub fn file_id(id: impl Into<String>) -> Self {
        Self::FileId(id.into())
    }

    /// 转成可与文本一起发送的图片段。内部编码兼容现有 OneBot/QQ 消息通道。
    pub async fn into_segment(self) -> Result<MessageSegment> {
        Ok(MessageSegment::image(self.into_source().await?))
    }

    /// 生成可与原生富消息组合复用的图片消息段。
    /// QQ 官方 Markdown 消息会将此类图片作为 Bot 级持久素材上传复用；普通图片发送及其他平台仍保持原有的单次上传行为。
    ///
    /// # Errors
    /// 当 URL 无效、图片格式不受支持或文件体积超过上限时返回错误。
    pub async fn into_reusable_segment(self) -> Result<MessageSegment> {
        Ok(MessageSegment::reusable_image(self.into_source().await?))
    }

    /// 转换为供原生领域目标发送使用的强类型图片消息段（[`crate::domain::MessageSegment`]），
    /// 复用与兼容层 API 完全一致的文件大小限制、URL 校验和图片文件头检查。
    pub async fn into_native_segment(self) -> Result<crate::domain::MessageSegment> {
        Ok(crate::domain::MessageSegment::Image {
            source: crate::domain::FileSource::from_wire(self.into_source().await?),
            id: None,
        })
    }

    async fn into_source(self) -> Result<String> {
        let bytes = match self {
            Self::Bytes(bytes) => bytes,
            Self::Path(path) => read_file_limited(&path, MAX_IMAGE_BYTES).await?,
            Self::Url(url) => {
                validate_http_url(&url)?;
                return Ok(url);
            }
            Self::FileId(id) => {
                ensure!(!id.trim().is_empty(), "图片文件标识不能为空");
                return Ok(id);
            }
        };
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "图片超过 SDK 大小上限");
        image_format(&bytes)?;
        Ok(format!("base64://{}", STANDARD.encode(bytes)))
    }
}

#[cfg(test)]
#[path = "../tests/unit/media.rs"]
mod tests;

impl From<Vec<u8>> for ImageSource {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}

impl From<PathBuf> for ImageSource {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

/// 平台适配层得到的上传内容，不带聊天平台的具体参数。
pub use nuono_app::media::ImageUpload;

/// 将图片段中的本地文件、base64 或 HTTP URL 解析为上传内容。
/// 平台 file_id 不在这里下载；使用它的平台应直接调用原生接口。
pub async fn resolve_image(
    source: &str,
    http: &reqwest::Client,
    max_bytes: usize,
) -> Result<ImageUpload> {
    ensure!(max_bytes > 0, "图片大小上限必须大于 0");
    let bytes = if let Some(encoded) = source.strip_prefix("base64://") {
        ensure!(
            encoded.len() <= max_bytes.div_ceil(3).saturating_mul(4),
            "图片超过上传大小上限"
        );
        let bytes = STANDARD.decode(encoded).context("图片 base64 内容无效")?;
        ensure!(bytes.len() <= max_bytes, "图片超过上传大小上限");
        bytes
    } else if source.starts_with("file://") {
        let url = url::Url::parse(source).context("图片文件 URL 无效")?;
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("图片文件 URL 必须指向本机文件"))?;
        read_file_limited(&path, max_bytes).await?
    } else {
        download_limited(http, source, max_bytes).await?
    };
    let (extension, content_type) = image_format(&bytes)?;
    Ok(ImageUpload {
        bytes,
        file_name: format!("image.{extension}"),
        content_type,
    })
}

fn image_format(bytes: &[u8]) -> Result<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(("png", "image/png"))
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok(("jpg", "image/jpeg"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok(("webp", "image/webp"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok(("gif", "image/gif"))
    } else {
        bail!("图片为空或格式不支持；请使用 PNG、JPEG、WebP 或 GIF")
    }
}

pub(crate) fn validate_http_url(value: &str) -> Result<()> {
    let url = url::Url::parse(value).context("素材 URL 无效")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "素材 URL 必须使用 HTTP 或 HTTPS"
    );
    Ok(())
}

pub(crate) async fn download_limited(
    http: &reqwest::Client,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    validate_http_url(url)?;
    let mut response = http
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("下载素材失败: {}", e.without_url()))?;
    response
        .error_for_status_ref()
        .map_err(|e| anyhow::anyhow!("下载素材失败: {}", e.without_url()))?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= max_bytes as u64),
        "素材超过大小上限"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("读取素材失败: {}", e.without_url()))?
    {
        ensure!(
            chunk.len() <= max_bytes.saturating_sub(bytes.len()),
            "素材超过大小上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    ensure!(!bytes.is_empty(), "素材内容为空");
    Ok(bytes)
}

pub(crate) async fn read_file_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let file = tokio::fs::File::open(path)
        .await
        .context("打开素材文件失败")?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "素材路径不是普通文件");
    ensure!(metadata.len() <= max_bytes as u64, "素材超过大小上限");
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(!bytes.is_empty(), "素材内容为空");
    ensure!(bytes.len() <= max_bytes, "素材超过大小上限");
    Ok(bytes)
}
