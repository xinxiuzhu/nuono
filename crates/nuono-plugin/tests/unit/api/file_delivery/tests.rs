use super::*;
use crate::{
    RuntimeBotRoute,
    api::WaitManager,
    core::{RuntimeActionRouter, event_bus::EventBus, onebot_client::ApiClient},
};
use nuono_app::transport::RuntimeActionSink;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

struct Sink {
    calls: Mutex<Vec<Value>>,
    response: Option<Value>,
}
#[async_trait::async_trait]
impl RuntimeActionSink for Sink {
    async fn send_action(&self, _: Value) -> crate::NuonoResult<()> {
        panic!("files require a receipt-bearing call")
    }
    async fn call_action(&self, action: Value) -> crate::NuonoResult<Value> {
        self.calls.lock().unwrap().push(action);
        self.response
            .clone()
            .ok_or_else(|| crate::NuonoError::MessageSend("synthetic upload failure".into()))
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
fn fixture(response: Option<Value>) -> (Bot, RuntimeMessageTarget, Arc<Sink>) {
    let route = RuntimeBotRoute::new("feishu", "exact-instance", "native-bot");
    let sink = Arc::new(Sink {
        calls: Mutex::new(vec![]),
        response,
    });
    let router = Arc::new(RuntimeActionRouter::new());
    router.register(route.clone(), sink.clone());
    (
        bot(router),
        RuntimeMessageTarget::private(route, "ou_001"),
        sink,
    )
}

#[tokio::test]
async fn native_file_bytes_target_and_receipt_preserve_exact_instance_route() {
    let router = Arc::new(RuntimeActionRouter::new());
    let route = RuntimeBotRoute::new("discord", "second-instance", "native-bot");
    let sink = Arc::new(Sink {
        calls: Mutex::new(vec![]),
        response: Some(
            json!({"status":"sent","message_ids":["001-native"],"raw":{"id":"001-native"}}),
        ),
    });
    let other = Arc::new(Sink {
        calls: Mutex::new(vec![]),
        response: None,
    });
    router.register(
        RuntimeBotRoute::new("discord", "first-instance", "native-bot"),
        other.clone(),
    );
    router.register(route.clone(), sink.clone());
    let target =
        RuntimeMessageTarget::channel(route, "18446744073709551615", Some("parent-native"));
    let bytes = b"{\"list\":[]}\0\xff".to_vec();
    let result = bot(router)
        .send_file_routed(
            &target,
            bytes.clone(),
            "export.json",
            Some("application/json"),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        FileDelivery::Sent {
            receipt: RuntimeActionReceipt {
                degradations: Vec::new(),
                message_ids: vec!["001-native".into()],
                raw: json!({"id":"001-native"})
            }
        }
    );
    assert!(other.calls.lock().unwrap().is_empty());
    assert_eq!(
        sink.calls.lock().unwrap().as_slice(),
        &[json!({"action":"send_file","params":{
            "target":{"type":"channel","channel_id":"18446744073709551615","parent":"parent-native"},
            "base64":STANDARD.encode(bytes),"file_name":"export.json","content_type":"application/json"
        }})]
    );
}

#[tokio::test]
async fn only_explicit_unsupported_allows_fallback_and_failures_are_never_retried() {
    let (bot, target, sink) = fixture(Some(json!({"status":"unsupported"})));
    assert_eq!(
        bot.send_file_routed(&target, vec![], "empty.json", None)
            .await
            .unwrap(),
        FileDelivery::Unsupported
    );
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
    for response in [
        None,
        Some(json!({"status":"unsupported","message_ids":["fake"]})),
        Some(json!({"status":"sent","message_ids":[],"raw":{}})),
        Some(json!({"status":"sent","message_ids":["\n"],"raw":{}})),
        Some(json!({"ok":true})),
    ] {
        let (bot, target, sink) = fixture(response);
        assert!(
            bot.send_file_routed(&target, vec![0], "export.json", None)
                .await
                .is_err()
        );
        assert_eq!(sink.calls.lock().unwrap().len(), 1);
    }
    let missing = RuntimeMessageTarget::private(
        RuntimeBotRoute::new("feishu", "missing-instance", "native-bot"),
        "ou_001",
    );
    assert!(
        bot.send_file_routed(&missing, vec![], "empty.json", None)
            .await
            .is_err()
    );
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn validates_byte_limit_paths_and_mime_before_dispatch() {
    let (bot, target, sink) = fixture(Some(json!({"status":"unsupported"})));
    for name in [
        "",
        "../export.json",
        "/tmp/export.json",
        "C:\\export.json",
        "x\r\nX-Injected: yes",
        "a\".json",
    ] {
        assert!(
            bot.send_file_routed(&target, vec![], name, None)
                .await
                .is_err()
        );
    }
    for mime in [
        "json",
        "text/",
        "*/json",
        "text/json; charset=utf-8",
        "text/json\r\nX:1",
    ] {
        assert!(
            bot.send_file_routed(&target, vec![], "export.json", Some(mime))
                .await
                .is_err()
        );
    }
    assert!(
        bot.send_file_routed(&target, vec![0; MAX_FILE_BYTES + 1], "export.json", None)
            .await
            .is_err()
    );
    assert!(sink.calls.lock().unwrap().is_empty());
    assert_eq!(
        bot.send_file_routed(
            &target,
            vec![0; MAX_FILE_BYTES],
            "导出.json",
            Some("application/json")
        )
        .await
        .unwrap(),
        FileDelivery::Unsupported
    );
    assert_eq!(sink.calls.lock().unwrap().len(), 1);
}

struct HangingSink {
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
}
struct CancelGuard(Arc<Notify>);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
#[async_trait::async_trait]
impl RuntimeActionSink for HangingSink {
    async fn send_action(&self, _: Value) -> crate::NuonoResult<()> {
        panic!("receipt required")
    }
    async fn call_action(&self, _: Value) -> crate::NuonoResult<Value> {
        let _guard = CancelGuard(self.cancelled.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelling_file_sender_aborts_the_registered_host_upload_future() {
    let route = RuntimeBotRoute::new("telegram", "instance", "bot");
    let router = Arc::new(RuntimeActionRouter::new());
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    router.register(
        route.clone(),
        Arc::new(HangingSink {
            started: started.clone(),
            cancelled: cancelled.clone(),
        }),
    );
    let target = RuntimeMessageTarget::private(route, "123");
    let task = tokio::spawn(async move {
        bot(router)
            .send_file_routed(&target, vec![1], "file.json", None)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), cancelled.notified())
        .await
        .unwrap();
}
