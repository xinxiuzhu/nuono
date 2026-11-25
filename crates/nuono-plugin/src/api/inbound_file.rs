//! 带大小上限的入站文件读取；平台特有资源标识的解析与下载由接收端适配器内部完成。
use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_app::media::{InboundFileRequest, InboundFileResponse, MAX_FILE_BYTES};
use serde_json::json;

use crate::{Bot, PluginHttpClient, RuntimeBotRoute};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InboundFile {
    Available {
        bytes: Vec<u8>,
        /// 平台返回的文件名（仅供参考）。当平台未返回时调用方应回退使用原始附件名，并自行校验文件内容。
        file_name: Option<String>,
        content_type: Option<String>,
    },
    Unsupported,
}

impl Bot {
    /// 按调用方指定的字节数上限读取入站 `File` 消息段的内容。
    /// HTTP(S) 链接与 `base64://` 数据直接按上限读取；平台内部资源句柄则通过精确的 Bot 路由交由适配器下载。
    /// 若有原始消息 ID 请一并传入（飞书下载消息资源时必需）。拒绝一切本机文件路径，且不对普通文件强制校验图片文件头或 MIME 类型。
    pub async fn read_inbound_file(
        &self,
        route: &RuntimeBotRoute,
        source: &str,
        message_id: Option<&str>,
        http: &PluginHttpClient,
        max_bytes: usize,
    ) -> Result<InboundFile> {
        ensure!(
            max_bytes > 0 && max_bytes <= MAX_FILE_BYTES,
            "入站文件大小上限无效"
        );
        ensure!(
            !route.platform.trim().is_empty()
                && !route.instance_id.trim().is_empty()
                && !route.bot_self_id.trim().is_empty(),
            "入站文件缺少机器人路由"
        );
        let source = source.trim();
        ensure!(!source.is_empty(), "入站文件来源为空");
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            if let Some(encoded) = source.strip_prefix("base64://") {
                return Ok(InboundFile::Available {
                    bytes: decode(encoded, max_bytes)?,
                    file_name: None,
                    content_type: None,
                });
            }
            if source.starts_with("https://") || source.starts_with("http://") {
                return download(source, http, max_bytes).await;
            }
            ensure!(
                !source.starts_with("file:")
                    && !source.starts_with('/')
                    && !source.starts_with('\\')
                    && !source.starts_with("./")
                    && !source.starts_with("../")
                    && !(source.as_bytes().get(1) == Some(&b':')
                        && source.as_bytes()[0].is_ascii_alphabetic()),
                "入站文件不接受本机文件路径"
            );
            let request = InboundFileRequest {
                source: source.to_owned(),
                message_id: message_id.map(str::to_owned),
                max_bytes,
            };
            request.validate()?;
            let response = self
                .call_routed(
                    route,
                    json!({"action":"read_inbound_file","params":request}),
                )
                .await?;
            let response: InboundFileResponse = serde_json::from_value(response)
                .map_err(|_| anyhow!("平台返回的入站文件响应无效"))?;
            match response {
                InboundFileResponse::Unsupported {} => Ok(InboundFile::Unsupported),
                InboundFileResponse::Available {
                    base64,
                    file_name,
                    content_type,
                } => {
                    validate_metadata(file_name.as_deref(), content_type.as_deref())?;
                    Ok(InboundFile::Available {
                        bytes: decode(&base64, max_bytes)?,
                        file_name,
                        content_type,
                    })
                }
            }
        })
        .await
        .map_err(|_| anyhow!("读取入站文件超时"))?
    }
}

fn decode(encoded: &str, max_bytes: usize) -> Result<Vec<u8>> {
    ensure!(
        encoded.len() <= max_bytes.div_ceil(3).saturating_mul(4),
        "文件超过大小上限"
    );
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow!("入站文件编码无效"))?;
    ensure!(bytes.len() <= max_bytes, "文件超过大小上限");
    Ok(bytes)
}

fn validate_metadata(file_name: Option<&str>, content_type: Option<&str>) -> Result<()> {
    ensure!(
        !file_name.is_some_and(|name| name.len() > 255
            || name.is_empty()
            || matches!(name, "." | "..")
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)),
        "平台返回的文件名无效"
    );
    ensure!(
        !content_type.is_some_and(|value| value.len() > 255
            || !value.is_ascii()
            || value.chars().any(char::is_control)),
        "平台返回的文件类型无效"
    );
    Ok(())
}

async fn download(source: &str, http: &PluginHttpClient, max_bytes: usize) -> Result<InboundFile> {
    let url = reqwest::Url::parse(source).map_err(|_| anyhow!("入站文件地址无效"))?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "入站文件地址不接受内嵌凭据"
    );
    let mut response = http
        .raw()
        .get(url)
        .send()
        .await
        .map_err(|error| anyhow!("下载入站文件失败: {}", error.without_url()))?;
    ensure!(
        response.status().is_success(),
        "下载入站文件失败，HTTP {}",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= max_bytes as u64),
        "文件超过大小上限"
    );
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    validate_metadata(None, content_type.as_deref())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("读取入站文件失败: {}", error.without_url()))?
    {
        ensure!(
            chunk.len() <= max_bytes.saturating_sub(bytes.len()),
            "文件超过大小上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(InboundFile::Available {
        bytes,
        file_name: None,
        content_type,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/api/inbound_file/tests.rs"]
mod tests;
