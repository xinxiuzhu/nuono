//! 带回执的原生文件发送接口（明确区分“平台不支持该能力”与“网络/传输失败”）。
use crate::{Bot, RuntimeActionReceipt, RuntimeMessageTarget, RuntimeTargetKind};
use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_app::media::{
    FileSendRequest, FileSendResponse, FileTarget, MAX_FILE_BYTES, validate_file_metadata,
};

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FileDelivery {
    Sent { receipt: RuntimeActionReceipt },
    Unsupported,
}

impl Bot {
    /// 按大小上限校验并发送文件字节流，完整保留接收方的原生身份类型与作用域。
    /// 仅当适配器明确返回无副作用的“不支持此能力”时才返回 `Unsupported` 以供调用方降级。
    pub async fn send_native_file_routed(
        &self,
        route: &crate::RuntimeBotRoute,
        target: &crate::domain::DeliveryTarget,
        bytes: Vec<u8>,
        file_name: &str,
        content_type: Option<&str>,
    ) -> Result<FileDelivery> {
        target.validate()?;
        ensure!(bytes.len() <= MAX_FILE_BYTES, "文件超过 32 MiB 大小上限");
        validate_file_metadata(file_name, content_type)?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            self.call_native_routed(
                route,
                crate::domain::OutboundAction::SendFile {
                    target: target.clone(),
                    source: crate::domain::FileSource::Base64(format!(
                        "base64://{}",
                        STANDARD.encode(bytes)
                    )),
                    file_name: file_name.to_owned(),
                    content_type: content_type.map(str::to_owned),
                },
            ),
        )
        .await
        .map_err(|_| anyhow!("文件发送超时，未确认送达"))?;
        match response {
            Ok(receipt) => Ok(FileDelivery::Sent { receipt }),
            Err(crate::NuonoError::NotImplemented(_)) => Ok(FileDelivery::Unsupported),
            Err(error) => Err(error.into()),
        }
    }

    /// 通过精确的目标路由上传文件字节流并等待平台原生消息回执。
    /// 仅当适配器明确返回 `Unsupported` 时才允许调用方降级；网络故障、权限不足、回执格式错误
    /// 及超时均原样报错返回，避免对已经产生副作用的上传请求盲目重发。
    pub async fn send_file_routed(
        &self,
        target: &RuntimeMessageTarget,
        bytes: Vec<u8>,
        file_name: &str,
        content_type: Option<&str>,
    ) -> Result<FileDelivery> {
        target.validate()?;
        ensure!(bytes.len() <= MAX_FILE_BYTES, "文件超过 32 MiB 大小上限");
        validate_file_metadata(file_name, content_type)?;
        let native_target = match &target.kind {
            RuntimeTargetKind::Private { user_id } => FileTarget::Private {
                user_id: user_id.clone(),
            },
            RuntimeTargetKind::Group { group_id } => FileTarget::Group {
                group_id: group_id.clone(),
            },
            RuntimeTargetKind::Channel { channel_id, parent } => FileTarget::Channel {
                channel_id: channel_id.clone(),
                parent: parent.clone(),
            },
        };
        let request = FileSendRequest {
            target: native_target,
            base64: STANDARD.encode(bytes),
            file_name: file_name.to_owned(),
            content_type: content_type.map(str::to_owned),
        };
        request.validate()?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            self.call_routed(
                &target.route,
                serde_json::json!({"action":"send_file","params":request}),
            ),
        )
        .await
        .map_err(|_| anyhow!("文件发送超时，未确认送达"))??;
        match serde_json::from_value::<FileSendResponse>(response)
            .map_err(|_| anyhow!("平台文件发送回执无效"))?
        {
            FileSendResponse::Unsupported {} => Ok(FileDelivery::Unsupported),
            FileSendResponse::Sent { message_ids, raw } => {
                ensure!(
                    !message_ids.is_empty()
                        && message_ids.len() <= 16
                        && message_ids.iter().all(|id| !id.trim().is_empty()
                            && id.len() <= 512
                            && !id.chars().any(char::is_control)),
                    "平台未提供有效的文件消息回执"
                );
                Ok(FileDelivery::Sent {
                    receipt: RuntimeActionReceipt {
                        message_ids,
                        degradations: Vec::new(),
                        raw,
                    },
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/file_delivery/native_tests.rs"]
mod native_tests;
#[cfg(test)]
#[path = "../../tests/unit/api/file_delivery/tests.rs"]
mod tests;
