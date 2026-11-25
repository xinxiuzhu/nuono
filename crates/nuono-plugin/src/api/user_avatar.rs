//! 通过精确连接的平台适配器获取用户头像，避免在插件代码中直接硬编码各平台的头像 URL。
use anyhow::{Result, anyhow, ensure};
use nuono_app::media::{UserAvatarRequest, UserAvatarResponse};
use nuono_domain::Platform;
use serde_json::json;

use crate::{Bot, PluginHttpClient, RuntimeBotRoute, media};

#[non_exhaustive]
pub enum UserAvatar {
    Available {
        image: media::ImageUpload,
        version: Option<String>,
    },
    Absent,
    Unsupported,
}

impl std::fmt::Debug for UserAvatar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Available { image, version } => f
                .debug_struct("Available")
                .field("bytes", &image.bytes.len())
                .field("content_type", &image.content_type)
                .field("has_version", &version.is_some())
                .finish(),
            Self::Absent => f.write_str("Absent"),
            Self::Unsupported => f.write_str("Unsupported"),
        }
    }
}

impl Bot {
    /// 读取指定用户的头像。若传入 `source_hint`，该提示必须来自已收到的入站消息发送者资料。
    /// 头像地址解析、平台鉴权、提示校验与字节下载均由对应的平台适配器负责。
    /// “用户无头像（`Absent`）”与“平台不支持（`Unsupported`）”会显式区分；网络抖动等临时故障仍作为错误返回，以便上层缓存保留上一次成功获取的头像。
    /// 调用方在解码图片像素前仍应自行限制最大宽高尺寸。
    pub async fn read_user_avatar(
        &self,
        route: &RuntimeBotRoute,
        request: &UserAvatarRequest,
        http: &PluginHttpClient,
    ) -> Result<UserAvatar> {
        request.validate()?;
        ensure!(
            !route.instance_id.trim().is_empty() && !route.bot_self_id.trim().is_empty(),
            "头像读取缺少机器人路由"
        );
        ensure!(
            platform_matches(&route.platform, request.identity.platform()),
            "头像用户身份与机器人平台不一致"
        );
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.call_routed(route, json!({"action":"read_user_avatar","params":request})),
        )
        .await
        .map_err(|_| anyhow!("读取用户头像超时"))??;
        // 在反序列化为结构体前先按引用检查 base64 字符串长度，避免为超限数据额外分配一份 String 拷贝。
        if let Some(encoded) = response.get("base64").and_then(|value| value.as_str()) {
            ensure!(
                encoded.len() <= request.max_bytes.div_ceil(3).saturating_mul(4),
                "头像超过大小上限"
            );
        }
        let response: UserAvatarResponse =
            serde_json::from_value(response).map_err(|_| anyhow!("平台头像响应无效"))?;
        match response {
            UserAvatarResponse::Absent {} => Ok(UserAvatar::Absent),
            UserAvatarResponse::Unsupported {} => Ok(UserAvatar::Unsupported),
            UserAvatarResponse::Available { base64, version } => {
                ensure!(
                    version.as_ref().is_none_or(|value| !value.is_empty()
                        && value.len() <= 256
                        && value.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.'))),
                    "平台头像版本标识无效"
                );
                let image = media::resolve_image(
                    &format!("base64://{base64}"),
                    http.raw(),
                    request.max_bytes,
                )
                .await?;
                Ok(UserAvatar::Available { image, version })
            }
        }
    }
}

fn platform_matches(route: &str, identity: &Platform) -> bool {
    let route = route.to_ascii_lowercase();
    match identity {
        Platform::OneBot => matches!(route.as_str(), "onebot" | "onebot v11"),
        Platform::Telegram => route == "telegram",
        Platform::Discord => route == "discord",
        Platform::Feishu => route == "feishu",
        Platform::QqOfficial => matches!(route.as_str(), "qq" | "qq_official" | "qq official"),
        Platform::Other(name) => route == name.to_ascii_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::WaitManager,
        core::{RuntimeActionRouter, event_bus::EventBus, onebot_client::ApiClient},
    };
    use async_trait::async_trait;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use nuono_app::transport::RuntimeActionSink;
    use nuono_domain::{IdentityKind, IdentityRef, IdentityScheme};
    use std::sync::{Arc, Mutex};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
    struct Sink {
        calls: Mutex<Vec<serde_json::Value>>,
        response: Mutex<serde_json::Value>,
    }
    #[async_trait]
    impl RuntimeActionSink for Sink {
        async fn send_action(&self, _: serde_json::Value) -> crate::NuonoResult<()> {
            Ok(())
        }
        async fn call_action(
            &self,
            action: serde_json::Value,
        ) -> crate::NuonoResult<serde_json::Value> {
            self.calls.lock().unwrap().push(action);
            Ok(self.response.lock().unwrap().clone())
        }
    }
    fn fixture() -> (Bot, RuntimeBotRoute, Arc<Sink>, Arc<Sink>) {
        let route = RuntimeBotRoute::new("QQ", "native-instance", "native-bot");
        let sink = Arc::new(Sink {
            calls: Mutex::default(),
            response: Mutex::new(json!({
                "status":"available","base64":STANDARD.encode(PNG),"version":"content_v1"
            })),
        });
        let other = Arc::new(Sink {
            calls: Mutex::default(),
            response: Mutex::new(json!({"status":"absent"})),
        });
        let router = Arc::new(RuntimeActionRouter::new());
        router.register(route.clone(), sink.clone());
        router.register(
            RuntimeBotRoute::new("QQ", "other-instance", "native-bot"),
            other.clone(),
        );
        let bot = Bot::new(
            0,
            Arc::new(EventBus::new()),
            Arc::new(WaitManager::new()),
            Arc::new(ApiClient::new()),
            None,
            None,
        )
        .with_action_router(router);
        (bot, route, sink, other)
    }
    fn request() -> UserAvatarRequest {
        UserAvatarRequest {
            identity: IdentityRef::new(
                Platform::QqOfficial,
                IdentityKind::User,
                IdentityScheme::OpenId,
                "same-openid",
                Some("app/groups/group-a"),
            )
            .unwrap(),
            size: 256,
            max_bytes: 1024,
            source_hint: None,
        }
    }
    #[tokio::test]
    async fn native_bytes_keep_exact_route_and_full_identity_scope() {
        let (bot, route, sink, other) = fixture();
        let request = request();
        let UserAvatar::Available { image, version } = bot
            .read_user_avatar(&route, &request, &PluginHttpClient::new())
            .await
            .unwrap()
        else {
            panic!("the routed adapter supplied image bytes");
        };
        assert_eq!(image.bytes, PNG);
        assert_eq!(version.as_deref(), Some("content_v1"));
        assert_eq!(
            sink.calls.lock().unwrap()[0],
            json!({"action":"read_user_avatar","params":request})
        );
        assert!(other.calls.lock().unwrap().is_empty());
        let mut second = request.clone();
        second.identity = IdentityRef::new(
            Platform::QqOfficial,
            IdentityKind::User,
            IdentityScheme::OpenId,
            "same-openid",
            Some("app/groups/group-b"),
        )
        .unwrap();
        bot.read_user_avatar(&route, &second, &PluginHttpClient::new())
            .await
            .unwrap();
        assert_eq!(
            sink.calls.lock().unwrap()[1]["params"]["identity"]["namespace"],
            "app/groups/group-b"
        );
    }
    #[tokio::test]
    async fn absence_and_unsupported_are_explicit_and_offline_is_an_error() {
        let (bot, route, sink, _) = fixture();
        let http = PluginHttpClient::new();
        *sink.response.lock().unwrap() = json!({"status":"absent"});
        assert!(matches!(
            bot.read_user_avatar(&route, &request(), &http)
                .await
                .unwrap(),
            UserAvatar::Absent
        ));
        *sink.response.lock().unwrap() = json!({"status":"unsupported"});
        assert!(matches!(
            bot.read_user_avatar(&route, &request(), &http)
                .await
                .unwrap(),
            UserAvatar::Unsupported
        ));
        assert!(
            bot.read_user_avatar(
                &RuntimeBotRoute::new("QQ", "missing-instance", "native-bot"),
                &request(),
                &http
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn invalid_requests_never_route_and_untrusted_responses_are_bounded() {
        let (bot, route, sink, _) = fixture();
        let http = PluginHttpClient::new();
        let mut invalid = request();
        invalid.max_bytes = 0;
        assert!(bot.read_user_avatar(&route, &invalid, &http).await.is_err());
        assert!(
            bot.read_user_avatar(
                &RuntimeBotRoute::new("telegram", "native-instance", "native-bot"),
                &request(),
                &http
            )
            .await
            .is_err()
        );
        assert!(sink.calls.lock().unwrap().is_empty());
        for response in [
            json!({"status":"available","base64":STANDARD.encode(vec![0;1025])}),
            json!({"status":"available","base64":"broken!"}),
            json!({"status":"available","base64":STANDARD.encode(b"not an image")}),
            json!({"status":"available","base64":STANDARD.encode(PNG),"version":"https://secret.invalid/token"}),
            json!({"status":"absent","url":"https://credential.invalid"}),
        ] {
            *sink.response.lock().unwrap() = response;
            assert!(
                bot.read_user_avatar(&route, &request(), &http)
                    .await
                    .is_err()
            );
        }
    }
}
