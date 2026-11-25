//! 读取收到的图片，且不向插件暴露平台凭据。
use anyhow::{Result, anyhow, ensure};
use serde_json::json;

use crate::{Bot, PluginHttpClient, core::RuntimeBotRoute, media};

impl Bot {
    /// 读取入站图片或文件消息段中的图片内容。
    ///
    /// HTTP(S) 与 `base64://` 来源直接按字节上限读取；平台私有的资源标识则通过接收该消息的
    /// 具体适配器解析并返回图片字节流（绝不向插件暴露带平台凭据的下载链接）。
    /// 若有原始消息 ID 请一并传入（飞书等资源归属于特定消息的平台必需）。
    /// 出于安全考量，本接口拒绝读取任意本机文件路径；调用方在解码处理图片前还应自行限制图片像素尺寸。
    pub async fn read_inbound_image(
        &self,
        route: &RuntimeBotRoute,
        source: &str,
        message_id: Option<&str>,
        http: &PluginHttpClient,
        max_bytes: usize,
    ) -> Result<media::ImageUpload> {
        ensure!(
            max_bytes > 0 && max_bytes <= media::MAX_IMAGE_BYTES,
            "入站图片大小上限无效"
        );
        ensure!(
            !route.platform.trim().is_empty()
                && !route.instance_id.trim().is_empty()
                && !route.bot_self_id.trim().is_empty(),
            "入站图片缺少机器人路由"
        );
        let source = source.trim();
        ensure!(!source.is_empty(), "入站图片来源为空");
        if direct_source(source) {
            return media::resolve_image(source, http.raw(), max_bytes).await;
        }
        ensure!(
            !source.starts_with("file:")
                && !source.starts_with('/')
                && !source.starts_with('\\')
                && !source.starts_with("./")
                && !source.starts_with("../")
                && !(source.as_bytes().get(1) == Some(&b':')
                    && source.as_bytes()[0].is_ascii_alphabetic()),
            "入站图片不接受本机文件路径"
        );
        let onebot = route.platform.eq_ignore_ascii_case("onebot")
            || route.platform.eq_ignore_ascii_case("OneBot v11");
        let action = if onebot {
            json!({"action":"get_image", "params":{"file":source}})
        } else {
            json!({"action":"read_inbound_media", "params":{
                "source":source, "message_id":message_id, "max_bytes":max_bytes
            }})
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.call_routed(route, action),
        )
        .await
        .map_err(|_| anyhow!("读取入站图片超时"))??;

        if onebot {
            let data = result.get("data").unwrap_or(&result);
            let source = ["url", "file", "base64"]
                .into_iter()
                .filter_map(|key| data.get(key).and_then(|value| value.as_str()))
                .find(|value| direct_source(value))
                .ok_or_else(|| anyhow!("OneBot 未返回可下载的图片内容"))?;
            return media::resolve_image(source, http.raw(), max_bytes).await;
        }

        let encoded = result
            .get("base64")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("平台未返回入站图片内容"))?;
        ensure!(
            encoded.len() <= max_bytes.div_ceil(3).saturating_mul(4),
            "图片超过大小上限"
        );
        media::resolve_image(&format!("base64://{encoded}"), http.raw(), max_bytes).await
    }
}

fn direct_source(source: &str) -> bool {
    source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("base64://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::waiter::WaitManager;
    use crate::core::{RuntimeActionRouter, event_bus::EventBus, onebot_client::ApiClient};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use nuono_app::transport::RuntimeActionSink;
    use std::sync::{Arc, Mutex};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
    struct Sink {
        calls: Mutex<Vec<serde_json::Value>>,
        response: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl RuntimeActionSink for Sink {
        async fn send_action(&self, _: serde_json::Value) -> crate::NuonoResult<()> {
            Ok(())
        }
        async fn call_action(
            &self,
            action: serde_json::Value,
        ) -> crate::NuonoResult<serde_json::Value> {
            self.calls.lock().unwrap().push(action);
            Ok(self.response.clone())
        }
    }
    fn fixture(platform: &str, response: serde_json::Value) -> (Bot, RuntimeBotRoute, Arc<Sink>) {
        let route = RuntimeBotRoute::new(platform, "exact-instance", "exact-bot");
        let sink = Arc::new(Sink {
            calls: Mutex::new(Vec::new()),
            response,
        });
        let router = Arc::new(RuntimeActionRouter::new());
        router.register(route.clone(), sink.clone());
        let bot = Bot::new(
            0,
            Arc::new(EventBus::new()),
            Arc::new(WaitManager::new()),
            Arc::new(ApiClient::new()),
            None,
            None,
        )
        .with_action_router(router);
        (bot, route, sink)
    }

    #[tokio::test]
    async fn handles_use_exact_route_and_pass_original_message_context() {
        let (bot, route, sink) = fixture("feishu", json!({"base64":STANDARD.encode(PNG)}));
        let image = bot
            .read_inbound_image(
                &route,
                "image_key:img_fixture",
                Some("om_original"),
                &PluginHttpClient::new(),
                1024,
            )
            .await
            .unwrap();
        assert_eq!(image.bytes, PNG);
        assert_eq!(image.content_type, "image/png");
        assert_eq!(
            sink.calls.lock().unwrap()[0],
            json!({"action":"read_inbound_media","params":{
                "source":"image_key:img_fixture","message_id":"om_original","max_bytes":1024
            }})
        );
    }

    #[tokio::test]
    async fn rejects_local_sources_and_invalid_or_oversize_adapter_content() {
        let (bot, route, sink) = fixture("telegram", json!({"base64":STANDARD.encode(PNG)}));
        let http = PluginHttpClient::new();
        for source in [
            "file:///etc/passwd",
            "/etc/passwd",
            "../private",
            "C:\\private",
        ] {
            assert!(
                bot.read_inbound_image(&route, source, None, &http, 1024)
                    .await
                    .is_err()
            );
        }
        assert!(sink.calls.lock().unwrap().is_empty());
        assert!(
            bot.read_inbound_image(&route, "file_id:fixture", None, &http, 4)
                .await
                .is_err()
        );
        let (bot, route, _) = fixture(
            "telegram",
            json!({"base64":STANDARD.encode(b"not an image")}),
        );
        assert!(
            bot.read_inbound_image(&route, "file_id:fixture", None, &http, 1024)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn base64_bypasses_adapter_and_onebot_only_accepts_downloadable_result() {
        let (bot, route, sink) = fixture("onebot", json!({"data":{"file":"/remote/private"}}));
        let http = PluginHttpClient::new();
        let source = format!("base64://{}", STANDARD.encode(PNG));
        assert_eq!(
            bot.read_inbound_image(&route, &source, None, &http, 1024)
                .await
                .unwrap()
                .bytes,
            PNG
        );
        assert!(sink.calls.lock().unwrap().is_empty());
        assert!(
            bot.read_inbound_image(&route, "opaque-file", None, &http, 1024)
                .await
                .is_err()
        );
        assert_eq!(sink.calls.lock().unwrap()[0]["action"], "get_image");
    }

    #[tokio::test]
    async fn public_http_images_use_bounded_status_checked_reads() {
        let app = axum::Router::new()
            .route("/image", axum::routing::get(|| async { PNG }))
            .route(
                "/missing",
                axum::routing::get(|| async { (axum::http::StatusCode::NOT_FOUND, PNG) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (bot, route, sink) = fixture("discord", json!({}));
        let http = PluginHttpClient::new();
        let image = bot
            .read_inbound_image(&route, &format!("{base}/image"), None, &http, 1024)
            .await
            .unwrap();
        assert_eq!(image.bytes, PNG);
        assert!(
            bot.read_inbound_image(&route, &format!("{base}/image"), None, &http, 4)
                .await
                .is_err()
        );
        assert!(
            bot.read_inbound_image(&route, &format!("{base}/missing"), None, &http, 1024)
                .await
                .is_err()
        );
        assert!(sink.calls.lock().unwrap().is_empty());
        server.abort();
    }
}
