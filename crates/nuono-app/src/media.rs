//! 宿主媒体文件传输与带大小上限的图片解析接口定义。
//!
//! 具体平台的上传参数与 HTTP/文件系统读写由外层适配器实现，本模块仅定义应用层数据契约。

use nuono_error::{NuonoError, Result};
use serde_json::Value;

mod avatar;
pub use avatar::{UserAvatarRequest, UserAvatarResponse};
mod file;
mod native_file;
pub use file::{
    FileSendRequest, FileSendResponse, FileTarget, InboundFileRequest, InboundFileResponse,
    MAX_FILE_BYTES, validate_file_metadata,
};
pub use native_file::NativeFileSendRequest;

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub struct ImageUpload {
    pub bytes: Vec<u8>,
    pub file_name: String,
    pub content_type: &'static str,
}

#[async_trait::async_trait]
pub trait ImageResolver: Send + Sync {
    async fn resolve_image(&self, source: &str, max_bytes: usize) -> Result<ImageUpload>;
}

/// 从兼容格式的 JSON 图片消息段中提取图片源地址（优先选取带协议前缀的 `file`，其次为 `url`）。
pub fn image_source(segment: &Value) -> Result<&str> {
    let data = segment
        .get("data")
        .ok_or_else(|| NuonoError::MessageSend("图片段缺少 data".into()))?;
    let file = data
        .get("file")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let url = data
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    file.filter(|s| {
        s.starts_with("base64://")
            || s.starts_with("file://")
            || s.starts_with("http://")
            || s.starts_with("https://")
    })
    .or(url)
    .or(file)
    .ok_or_else(|| NuonoError::MessageSend("图片段缺少 file/url".into()))
}
