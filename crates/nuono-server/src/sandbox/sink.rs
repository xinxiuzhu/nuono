use super::{CapturedMedia, MAX_MEDIA_BYTES, MAX_MEDIA_ITEMS, Session, validation};
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_app::media::{FileSendRequest, FileSendResponse, FileTarget, NativeFileSendRequest};
use nuono_core::core::RuntimeActionSink;
use nuono_core::domain::{ConversationRef, DeliveryTarget, IdentityRef};
use nuono_core::error::{NuonoError, Result};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use tokio::io::AsyncReadExt;

pub(super) struct SandboxSink {
    session: Weak<Session>,
}

impl SandboxSink {
    pub(super) fn new(session: Weak<Session>) -> Self {
        Self { session }
    }

    fn session(&self) -> Result<Arc<Session>> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| validation("测试会话已关闭"))?;
        session.ensure_open()?;
        Ok(session)
    }

    fn target_id(session: &Session, value: Option<&Value>, expected: i64) -> Result<()> {
        let id = value.and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|id| id.to_string()))
        });
        if id.as_deref() != Some(expected.to_string().as_str()) {
            return Err(validation(format!(
                "测试发送只能使用当前虚拟会话的目标（{}）",
                session.id
            )));
        }
        Ok(())
    }

    fn raw_target(session: &Session, action: &str, params: &Value) -> Result<()> {
        let group = action.contains("group")
            || params.get("message_type").and_then(Value::as_str) == Some("group")
            || params.get("group_id").is_some();
        if group {
            let group_id = session
                .group_id
                .ok_or_else(|| validation("当前测试会话没有虚拟群"))?;
            Self::target_id(session, params.get("group_id"), group_id)
        } else {
            Self::target_id(session, params.get("user_id"), session.user_id)
        }
    }

    fn identity(session: &Session, id: &IdentityRef) -> Result<()> {
        use nuono_core::domain::{IdentityKind, IdentityScheme, Platform};
        let expected = match id.kind() {
            IdentityKind::User => Some(session.user_id),
            IdentityKind::Group | IdentityKind::Conversation => session.group_id,
            _ => None,
        };
        if *id.platform() != Platform::OneBot
            || *id.scheme() != IdentityScheme::Native
            || id.namespace().is_some()
            || expected.map(|id| id.to_string()).as_deref() != Some(id.value().as_str())
        {
            return Err(validation("原生发送目标不属于当前虚拟会话"));
        }
        Ok(())
    }

    fn discard_media(session: &Session, media_ids: &[String]) {
        let mut state = session.state();
        for id in media_ids {
            if let Some(media) = state.media.remove(id) {
                state.media_bytes -= media.bytes.len();
            }
        }
    }

    fn capture_media(session: &Session, media: CapturedMedia) -> Result<(String, String)> {
        let mut state = session.state();
        session.ensure_open()?;
        if state.media.len() >= MAX_MEDIA_ITEMS
            || media.bytes.len() > MAX_MEDIA_BYTES.saturating_sub(state.media_bytes)
        {
            return Err(validation("测试会话媒体缓存已满，请重新创建会话后继续"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        state.media_bytes += media.bytes.len();
        state.media.insert(id.clone(), media);
        Ok((
            format!(
                "/api/{}/sessions/{}/media/{id}",
                if session.mode == super::SessionMode::Test {
                    "sandbox"
                } else {
                    "chat"
                },
                session.id
            ),
            id,
        ))
    }

    async fn media(
        session: &Session,
        source: &str,
        kind: &str,
        file_name: Option<&str>,
    ) -> Result<Option<(String, String)>> {
        if source.starts_with("https://") || source.starts_with("http://") {
            // 远程 HTTP(S) 链接直接交由浏览器加载展示，沙盒服务端绝不代为发起远程抓取（防止 SSRF）。
            let url = url::Url::parse(source).map_err(|_| validation("测试媒体 URL 无效"))?;
            if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
                return Err(validation("测试媒体 URL 无效"));
            }
            return Ok(None);
        }
        let max_bytes = if kind == "image" {
            nuono_app::media::MAX_IMAGE_BYTES
        } else {
            MAX_MEDIA_BYTES
        };
        let bytes = if let Some(encoded) = source.strip_prefix("base64://") {
            if encoded.len() > max_bytes.div_ceil(3) * 4 {
                return Err(validation("测试媒体超过大小上限"));
            }
            STANDARD
                .decode(encoded)
                .map_err(|_| validation("测试媒体 base64 内容无效"))?
        } else if source.starts_with("file://") || std::path::Path::new(source).is_absolute() {
            // 只有服务端插件主动发出的消息动作才会走到此分支读取本机文件；
            // 对外暴露的媒体 HTTP 接口只接受不透明的随机 media_id，不接受任何文件路径或 URL 参数。
            let path = if source.starts_with("file://") {
                url::Url::parse(source)
                    .map_err(|_| validation("测试媒体文件 URL 无效"))?
                    .to_file_path()
                    .map_err(|_| validation("测试媒体必须是本机文件"))?
            } else {
                std::path::PathBuf::from(source)
            };
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|_| validation("无法读取插件输出的测试媒体文件"))?;
            let metadata = file
                .metadata()
                .await
                .map_err(|_| validation("无法读取测试媒体元数据"))?;
            if !metadata.is_file() || metadata.len() > max_bytes as u64 {
                return Err(validation("测试媒体不是普通文件或超过大小上限"));
            }
            let mut bytes = Vec::new();
            file.take(max_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| validation("读取测试媒体失败"))?;
            bytes
        } else {
            return Err(NuonoError::NotImplemented(
                "测试会话不能解析 QQ 已上传文件 ID，请使用 URL、base64 或本机文件".into(),
            ));
        };
        if bytes.is_empty() || bytes.len() > max_bytes {
            return Err(validation("测试媒体为空或超过大小上限"));
        }
        let (content_type, fallback) = match kind {
            "image" => match image::guess_format(&bytes)
                .map_err(|_| validation("测试图片格式无效"))?
            {
                image::ImageFormat::Png => ("image/png", "image.png"),
                image::ImageFormat::Jpeg => ("image/jpeg", "image.jpg"),
                image::ImageFormat::Gif => ("image/gif", "image.gif"),
                image::ImageFormat::WebP => ("image/webp", "image.webp"),
                _ => return Err(validation("测试图片只支持 PNG、JPEG、GIF 或 WebP")),
            },
            "record" if bytes.starts_with(b"OggS") => ("audio/ogg", "audio.ogg"),
            "record" if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") => {
                ("audio/wav", "audio.wav")
            }
            "record" if bytes.starts_with(b"ID3") || bytes.starts_with(b"\xff\xfb") => {
                ("audio/mpeg", "audio.mp3")
            }
            "video" if bytes.get(4..8) == Some(b"ftyp") => ("video/mp4", "video.mp4"),
            _ => ("application/octet-stream", "file.bin"),
        };
        let file_name = file_name
            .filter(|name| {
                name.len() <= 255
                    && !name
                        .chars()
                        .any(|c| c.is_control() || c == '/' || c == '\\')
            })
            .unwrap_or(fallback)
            .to_owned();
        Self::capture_media(
            session,
            CapturedMedia {
                bytes: Arc::<[u8]>::from(bytes),
                content_type: content_type.into(),
                file_name,
                inline: kind != "file" && content_type != "application/octet-stream",
            },
        )
        .map(Some)
    }

    fn normalize<'a>(
        session: &'a Session,
        input: Value,
        depth: usize,
        media_ids: &'a mut Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Value>>> + Send + 'a>> {
        Box::pin(async move {
            if depth > 6 {
                return Err(validation("测试转发消息嵌套过深"));
            }
            let mut segments = match input {
                Value::String(text) => vec![json!({"type":"text","data":{"text":text}})],
                Value::Array(segments) => segments,
                _ => return Err(validation("测试回复必须是文本或 OneBot 消息段数组")),
            };
            if segments.len() > 100 {
                return Err(validation("测试回复消息段过多"));
            }
            for segment in &mut segments {
                if !segment.is_object() || !segment.get("data").is_some_and(Value::is_object) {
                    return Err(validation("测试回复消息段必须包含对象类型的 data"));
                }
                let kind = segment
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if kind == "node" {
                    let content = segment["data"]["content"].take();
                    segment["data"]["content"] =
                        json!(Self::normalize(session, content, depth + 1, media_ids).await?);
                } else if matches!(kind.as_str(), "image" | "file" | "record" | "video") {
                    let source = segment
                        .get("data")
                        .and_then(|d| {
                            d.get("file")
                                .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                                .or_else(|| d.get("url"))
                        })
                        .and_then(Value::as_str)
                        .ok_or_else(|| validation("测试媒体段缺少来源"))?
                        .to_owned();
                    let name = segment["data"]["name"].as_str().map(str::to_owned);
                    if let Some((url, id)) =
                        Self::media(session, &source, &kind, name.as_deref()).await?
                    {
                        segment["data"]["file"] = json!(url);
                        segment["data"]["url"] = json!(url);
                        media_ids.push(id);
                    }
                } else if segment.to_string().len() > 64 * 1024 {
                    return Err(validation("测试回复消息段超过大小上限"));
                }
            }
            Ok(segments)
        })
    }

    async fn call(&self, session: &Session, action: &str, params: Value) -> Result<Value> {
        match action {
            "send_file" => {
                let request: FileSendRequest = serde_json::from_value(params)
                    .map_err(|_| validation("测试文件请求格式无效"))?;
                request.validate()?;
                match &request.target {
                    FileTarget::Private { user_id } => {
                        Self::target_id(session, Some(&json!(user_id)), session.user_id)?
                    }
                    FileTarget::Group { group_id } => Self::target_id(
                        session,
                        Some(&json!(group_id)),
                        session
                            .group_id
                            .ok_or_else(|| validation("当前测试会话没有虚拟群"))?,
                    )?,
                    FileTarget::Channel { .. } => return Err(validation("测试会话未模拟频道")),
                }
                let receipt = Self::file(
                    session,
                    &format!("base64://{}", request.base64),
                    &request.file_name,
                    action,
                )
                .await?;
                Ok(
                    json!({"status":"sent","message_ids":[receipt.id],"raw":{"message_id":receipt.id}}),
                )
            }
            "upload_private_file" | "upload_group_file" => {
                Self::raw_target(session, action, &params)?;
                let source = params
                    .get("file")
                    .and_then(Value::as_str)
                    .ok_or_else(|| validation("测试文件缺少来源"))?;
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| validation("测试文件缺少名称"))?;
                nuono_app::media::validate_file_metadata(name, None)?;
                let receipt = Self::file(session, source, name, action).await?;
                Ok(json!({"message_id":receipt.id}))
            }
            "send_msg"
            | "send_private_msg"
            | "send_group_msg"
            | "send_private_forward_msg"
            | "send_group_forward_msg" => {
                Self::raw_target(session, action, &params)?;
                let input = if action.contains("forward") {
                    params.get("messages")
                } else {
                    params.get("message")
                }
                .cloned()
                .ok_or_else(|| validation("测试发送缺少消息内容"))?;
                let mut media_ids = Vec::new();
                let segments = match Self::normalize(session, input, 0, &mut media_ids).await {
                    Ok(segments) => segments,
                    Err(error) => {
                        Self::discard_media(session, &media_ids);
                        return Err(error);
                    }
                };
                if serde_json::to_vec(&segments).map_or(true, |v| v.len() > 256 * 1024) {
                    Self::discard_media(session, &media_ids);
                    return Err(validation("测试回复超过大小上限"));
                }
                let receipt =
                    session.push("outgoing", segments, Some(action.into()), None, media_ids)?;
                Ok(json!({"message_id":receipt.id.parse::<i64>().unwrap_or(0)}))
            }
            "get_login_info" => Ok(json!({"user_id":session.bot_id,"nickname":"Nuono"})),
            "can_send_image" | "can_send_record" => Ok(json!({"yes":true})),
            "read_user_avatar" => {
                let request: nuono_app::media::UserAvatarRequest =
                    serde_json::from_value(params)
                        .map_err(|_| validation("测试头像请求格式无效"))?;
                request.validate()?;
                Self::identity(session, &request.identity)?;
                // 虚拟用户没有 QQ 头像；插件可以使用它们的正常备用头像。
                Ok(json!({"status":"absent"}))
            }
            "get_status" => Ok(
                json!({"online":true,"good":true,"sandbox":session.mode == super::SessionMode::Test}),
            ),
            "get_version_info" => Ok(
                json!({"app_name":"Nuono Sandbox","app_version":env!("CARGO_PKG_VERSION"),"protocol_version":"v11"}),
            ),
            "get_friend_list" => Ok(
                json!([{"user_id":session.user_id,"nickname":session.options.sender_name,"remark":"测试用户"}]),
            ),
            "get_group_list" => Ok(match session.group_id {
                Some(id) => {
                    json!([{"group_id":id,"group_name":"Nuono 测试群","member_count":2,"max_member_count":2}])
                }
                None => json!([]),
            }),
            "get_group_info" | "get_group_member_list" | "get_group_member_info" => {
                let group_id = session
                    .group_id
                    .ok_or_else(|| validation("当前测试会话没有虚拟群"))?;
                Self::target_id(session, params.get("group_id"), group_id)?;
                let member = |bot: bool| json!({"group_id":group_id,"user_id":if bot {session.bot_id} else {session.user_id},"nickname":if bot {"Nuono 测试机器人"} else {&session.options.sender_name},"card":"","role":if bot {"owner"} else {&session.options.group_role},"title":"","level":"1","sex":"unknown","age":0});
                match action {
                    "get_group_info" => Ok(
                        json!({"group_id":group_id,"group_name":"Nuono 测试群","member_count":2,"max_member_count":2}),
                    ),
                    "get_group_member_list" => Ok(json!([member(false), member(true)])),
                    _ => {
                        let bot = params.get("user_id").and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        }) == Some(session.bot_id);
                        Self::target_id(
                            session,
                            params.get("user_id"),
                            if bot { session.bot_id } else { session.user_id },
                        )?;
                        Ok(member(bot))
                    }
                }
            }
            "get_stranger_info" => {
                Self::target_id(session, params.get("user_id"), session.user_id)?;
                Ok(
                    json!({"user_id":session.user_id,"nickname":session.options.sender_name,"sex":"unknown","age":0}),
                )
            }
            "get_msg" => {
                let id = params
                    .get("message_id")
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .or_else(|| v.as_i64().map(|id| id.to_string()))
                    })
                    .ok_or_else(|| validation("测试消息 ID 无效"))?;
                let state = session.state();
                let message = state
                    .history
                    .iter()
                    .find(|e| e.message.id == id)
                    .ok_or_else(|| validation("测试消息不存在或已清理"))?;
                Ok(
                    json!({"message_id":id,"message":message.message.segments,"message_type":session.options.message_type,"sender":{"user_id":if message.message.direction == "outgoing" {session.bot_id} else {session.user_id}},"real_id":id,"time":chrono::Utc::now().timestamp()}),
                )
            }
            _ => Err(NuonoError::NotImplemented(format!(
                "测试会话未模拟 QQ 操作：{action}"
            ))),
        }
    }

    async fn file(
        session: &Session,
        source: &str,
        name: &str,
        action: &str,
    ) -> Result<super::MessageReceipt> {
        let (url, media) = match Self::media(session, source, "file", Some(name)).await? {
            Some((url, id)) => (url, vec![id]),
            None => (source.to_owned(), vec![]),
        };
        session.push(
            "outgoing",
            vec![json!({"type":"file","data":{"file":url,"url":url,"name":name}})],
            Some(action.into()),
            None,
            media,
        )
    }
}

#[async_trait::async_trait]
impl RuntimeActionSink for SandboxSink {
    async fn validate_native_target(&self, target: &DeliveryTarget) -> Result<()> {
        let session = self.session()?;
        match target {
            DeliveryTarget::Actor { actor } => Self::identity(&session, actor),
            DeliveryTarget::Conversation { conversation } => match conversation {
                ConversationRef::Direct {
                    participant,
                    conversation: None,
                } => Self::identity(&session, participant),
                ConversationRef::Group { group } => Self::identity(&session, group),
                _ => Err(validation("测试会话未模拟频道或独立私聊会话身份")),
            },
            DeliveryTarget::Channel { .. } => Err(validation("测试会话未模拟频道")),
            _ => Err(validation("测试会话不支持该投递目标")),
        }
    }

    async fn call_native_file(&self, request: NativeFileSendRequest) -> Result<FileSendResponse> {
        request.validate()?;
        self.validate_native_target(&request.target).await?;
        let session = self.session()?;
        let receipt = Self::file(
            &session,
            &format!("base64://{}", request.base64),
            &request.file_name,
            "send_file",
        )
        .await?;
        Ok(FileSendResponse::Sent {
            message_ids: vec![receipt.id.clone()],
            raw: json!({"message_id":receipt.id}),
        })
    }

    async fn send_action(&self, action: Value) -> Result<()> {
        self.call_action(action).await.map(|_| ())
    }

    async fn call_action(&self, packet: Value) -> Result<Value> {
        let session = self.session()?;
        let action = packet
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| validation("测试发送缺少 action"))?;
        let params = packet.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = self.call(&session, action, params).await;
        if let Err(error) = &result {
            let mut text = format!("{action}：{error}");
            text.truncate(text.floor_char_boundary(2048));
            let _ = session.push(
                "system",
                vec![json!({"type":"text","data":{"text":text}})],
                Some(action.into()),
                Some(error.kind().into()),
                vec![],
            );
        }
        result
    }
}
