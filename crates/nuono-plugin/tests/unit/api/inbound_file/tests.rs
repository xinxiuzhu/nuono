use super::*;
use crate::{
    api::WaitManager,
    core::{RuntimeActionRouter, event_bus::EventBus, onebot_client::ApiClient},
};
use nuono_app::transport::RuntimeActionSink;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

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
fn bot(router: Arc<RuntimeActionRouter>) -> Bot {
    Bot::new(
        0,
        Arc::new(EventBus::new()),
        Arc::new(WaitManager::new()),
        Arc::new(ApiClient::new()),
        None,
        None,
    )
    .with_action_router(router)
}
fn fixture(response: serde_json::Value) -> (Bot, RuntimeBotRoute, Arc<Sink>) {
    let route = RuntimeBotRoute::new("feishu", "exact-instance", "native-bot");
    let sink = Arc::new(Sink {
        calls: Mutex::new(Vec::new()),
        response,
    });
    let router = Arc::new(RuntimeActionRouter::new());
    router.register(route.clone(), sink.clone());
    (bot(router), route, sink)
}

#[tokio::test]
async fn reads_json_native_bytes_with_exact_route_original_message_and_metadata() {
    let router = Arc::new(RuntimeActionRouter::new());
    let route = RuntimeBotRoute::new("feishu", "instance-two", "native-bot");
    let wanted = br#"{"list":[]}"#;
    let sink = Arc::new(Sink {
        calls: Mutex::new(Vec::new()),
        response: json!({
            "status":"available", "base64": STANDARD.encode(wanted),
            "file_name":"records.json", "content_type":"application/json"
        }),
    });
    let other = Arc::new(Sink {
        calls: Mutex::new(Vec::new()),
        response: json!({"status":"unsupported"}),
    });
    router.register(
        RuntimeBotRoute::new("feishu", "instance-one", "native-bot"),
        other.clone(),
    );
    router.register(route.clone(), sink.clone());
    let result = bot(router)
        .read_inbound_file(
            &route,
            "file_key:received",
            Some("om_001"),
            &PluginHttpClient::new(),
            16 * 1024 * 1024,
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        InboundFile::Available {
            bytes: wanted.to_vec(),
            file_name: Some("records.json".into()),
            content_type: Some("application/json".into())
        }
    );
    assert!(other.calls.lock().unwrap().is_empty());
    assert_eq!(
        sink.calls.lock().unwrap()[0],
        json!({"action":"read_inbound_file","params":{
            "source":"file_key:received","message_id":"om_001","max_bytes":16*1024*1024
        }})
    );
}

#[tokio::test]
async fn unsupported_malformed_oversize_and_local_sources_are_distinct() {
    let http = PluginHttpClient::new();
    let (bot, route, sink) = fixture(json!({"status":"unsupported"}));
    assert_eq!(
        bot.read_inbound_file(&route, "opaque-id", None, &http, 16)
            .await
            .unwrap(),
        InboundFile::Unsupported
    );
    for source in [
        "/etc/passwd",
        "file:///private/data",
        "../data",
        "./data",
        "C:\\private",
    ] {
        assert!(
            bot.read_inbound_file(&route, source, None, &http, 16)
                .await
                .is_err()
        );
    }
    for limit in [0, MAX_FILE_BYTES + 1] {
        assert!(
            bot.read_inbound_file(&route, "opaque-id", None, &http, limit)
                .await
                .is_err()
        );
    }
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
    for response in [
        json!({"status":"unsupported","base64":"unexpected"}),
        json!({"status":"available","base64":STANDARD.encode([0;17])}),
        json!({"status":"available","base64":"not-base64"}),
        json!({"status":"available","base64":"e30=","file_name":"../secret"}),
    ] {
        let (bot, route, _) = fixture(response);
        assert!(
            bot.read_inbound_file(&route, "opaque-id", None, &http, 16)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn files_can_exceed_image_limit_without_accepting_oversized_base64() {
    let (bot, route, sink) = fixture(json!({"status":"unsupported"}));
    let bytes = vec![b' '; crate::media::MAX_IMAGE_BYTES + 1];
    let source = format!("base64://{}", STANDARD.encode(&bytes));
    let result = bot
        .read_inbound_file(&route, &source, None, &PluginHttpClient::new(), bytes.len())
        .await
        .unwrap();
    assert_eq!(
        result,
        InboundFile::Available {
            bytes: bytes.clone(),
            file_name: None,
            content_type: None
        }
    );
    assert!(
        bot.read_inbound_file(
            &route,
            &source,
            None,
            &PluginHttpClient::new(),
            bytes.len() - 1
        )
        .await
        .is_err()
    );
    assert!(sink.calls.lock().unwrap().is_empty());
}

struct Server {
    url: String,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(response: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream.write_all(&response);
        });
        Self {
            url: format!("http://{address}"),
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[tokio::test]
async fn http_download_preserves_json_bytes_and_limits_chunked_content_without_url_leak() {
    let (bot, route, _) = fixture(json!({"status":"unsupported"}));
    let http = PluginHttpClient::new();
    let server = Server::new(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec());
    assert_eq!(
        bot.read_inbound_file(&route, &server.url, None, &http, 2)
            .await
            .unwrap(),
        InboundFile::Available {
            bytes: b"{}".to_vec(),
            file_name: None,
            content_type: Some("application/json".into())
        }
    );
    let server = Server::new(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n{}\r\n1\r\nx\r\n0\r\n\r\n".to_vec());
    assert!(
        bot.read_inbound_file(&route, &server.url, None, &http, 2)
            .await
            .is_err()
    );
    let server = Server::new(
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    );
    let error = bot
        .read_inbound_file(
            &route,
            &format!("{}/private?token=secret-fixture", server.url),
            None,
            &http,
            16,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("404"));
    assert!(!error.contains("secret-fixture"));
}
