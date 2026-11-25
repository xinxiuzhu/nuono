use super::*;
use crate::{
    RuntimeBotRoute,
    api::WaitManager,
    core::{RuntimeActionRouter, event_bus::EventBus, onebot_client::ApiClient},
    domain::*,
};
use nuono_app::{
    action::{ActionAudit, ActionAuthority},
    media::{FileSendResponse, NativeFileSendRequest},
    transport::{RuntimeActionSink, TransportAction},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct ScriptSink {
    calls: Mutex<Vec<Value>>,
    response: Option<Value>,
    app: Option<&'static str>,
}
#[async_trait::async_trait]
impl RuntimeActionSink for ScriptSink {
    async fn validate_native_target(&self, target: &DeliveryTarget) -> crate::NuonoResult<()> {
        if let Some(app) = self.app {
            NativeFileSendRequest {
                target: target.clone(),
                base64: String::new(),
                file_name: "check.json".into(),
                content_type: None,
            }
            .validate_for_application(target.platform(), app)?;
        }
        Ok(())
    }
    async fn call_native_file(
        &self,
        request: NativeFileSendRequest,
    ) -> crate::NuonoResult<FileSendResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(serde_json::to_value(request).unwrap());
        let Some(response) = self.response.clone() else {
            // 即便底层传输返回 NotImplemented 错误，也不能将其视为强类型文件接口明确返回的“无副作用 Unsupported 响应”而盲目降级重发。
            return Err(crate::NuonoError::NotImplemented(
                "synthetic upload error".into(),
            ));
        };
        serde_json::from_value(response)
            .map_err(|_| crate::NuonoError::Validation("synthetic malformed receipt".into()))
    }
    async fn send_action(&self, _: Value) -> crate::NuonoResult<()> {
        panic!("native files require the typed port")
    }
}
fn script(response: Option<Value>) -> Arc<ScriptSink> {
    Arc::new(ScriptSink {
        calls: Mutex::new(Vec::new()),
        response,
        app: None,
    })
}
fn sent() -> Value {
    json!({"status":"sent","message_ids":["000042","18446744073709551615"],"raw":{"id":"000042"}})
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
fn actor(
    platform: Platform,
    scheme: IdentityScheme,
    value: &str,
    namespace: Option<&str>,
) -> DeliveryTarget {
    DeliveryTarget::actor(
        IdentityRef::new(platform, IdentityKind::User, scheme, value, namespace).unwrap(),
    )
    .unwrap()
}
fn qq_actor(scope: &str) -> DeliveryTarget {
    actor(
        Platform::QqOfficial,
        IdentityScheme::OpenId,
        "000123",
        Some(scope),
    )
}

#[tokio::test]
async fn native_files_preserve_alias_route_current_generation_full_target_bytes_and_receipt() {
    let router = Arc::new(RuntimeActionRouter::new());
    let route = RuntimeBotRoute::new("qq", "same-instance", "app-a");
    let old = script(Some(sent()));
    let generation = router.register(route.clone(), old.clone());
    let current = script(Some(sent()));
    router.register(route.clone(), current.clone());
    assert!(!router.unregister(&route, generation));
    let alias = script(Some(sent()));
    router.register(
        RuntimeBotRoute::new("qq_official", "same-instance", "app-a"),
        alias.clone(),
    );
    let other = script(Some(sent()));
    router.register(
        RuntimeBotRoute::new("qq", "another-instance", "app-a"),
        other.clone(),
    );
    let target = qq_actor("app-a");
    let bytes = b"{\"list\":[]}\0\xff".to_vec();
    let result = bot(router)
        .send_native_file_routed(
            &route,
            &target,
            bytes.clone(),
            "records.json",
            Some("application/json"),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        FileDelivery::Sent {
            receipt: RuntimeActionReceipt {
                degradations: Vec::new(),
                message_ids: vec!["000042".into(), "18446744073709551615".into()],
                raw: json!({"id":"000042"}),
            }
        }
    );
    assert!(old.calls.lock().unwrap().is_empty());
    assert!(alias.calls.lock().unwrap().is_empty());
    assert!(other.calls.lock().unwrap().is_empty());
    assert_eq!(
        current.calls.lock().unwrap().as_slice(),
        &[json!({
            "target":target,"base64":STANDARD.encode(bytes),"file_name":"records.json","content_type":"application/json"
        })]
    );
}

#[tokio::test]
async fn rejected_targets_never_become_capability_fallbacks_or_reach_the_file_port() {
    let router = Arc::new(RuntimeActionRouter::new());
    let route = RuntimeBotRoute::new("qq", "instance", "app-a");
    let sink = script(Some(json!({"status":"unsupported"})));
    router.register(route.clone(), sink.clone());
    let bot = bot(router.clone());
    for target in [
        qq_actor("app-b"),
        qq_actor("app-a/groups/group-x"),
        actor(Platform::Telegram, IdentityScheme::Numeric, "123", None),
    ] {
        assert!(
            bot.send_native_file_routed(&route, &target, vec![1], "records.json", None)
                .await
                .is_err()
        );
    }
    assert!(sink.calls.lock().unwrap().is_empty());
    assert_eq!(
        bot.send_native_file_routed(&route, &qq_actor("app-a"), vec![1], "records.json", None)
            .await
            .unwrap(),
        FileDelivery::Unsupported
    );
    let lied_route = RuntimeBotRoute::new("feishu", "wrong-client", "app-a");
    let actual = Arc::new(ScriptSink {
        app: Some("actual-app"),
        calls: Mutex::new(Vec::new()),
        response: Some(sent()),
    });
    router.register(lied_route.clone(), actual.clone());
    let valid = actor(
        Platform::Feishu,
        IdentityScheme::UserId,
        "opaque",
        Some("app-a"),
    );
    assert!(
        bot.send_native_file_routed(&lied_route, &valid, vec![1], "records.json", None)
            .await
            .is_err()
    );
    let invalid_shape = DeliveryTarget::Actor {
        actor: IdentityRef::new(
            Platform::Feishu,
            IdentityKind::Group,
            IdentityScheme::ChatId,
            "opaque",
            Some("app-a"),
        )
        .unwrap(),
    };
    assert!(
        bot.send_native_file_routed(&lied_route, &invalid_shape, vec![1], "records.json", None)
            .await
            .is_err()
    );
    assert!(actual.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn only_explicit_native_unsupported_allows_fallback_and_malformed_or_failed_calls_are_not_retried()
 {
    let route = RuntimeBotRoute::new("feishu", "instance", "app-a");
    let target = actor(
        Platform::Feishu,
        IdentityScheme::UnionId,
        "opaque",
        Some("app-a"),
    );
    for response in [
        None,
        Some(json!({"status":"sent","message_ids":[],"raw":{}})),
        Some(json!({"status":"sent","message_ids":["\n"],"raw":{}})),
        Some(json!({"status":"unsupported","raw":{}})),
        Some(json!({"ok":true})),
    ] {
        let router = Arc::new(RuntimeActionRouter::new());
        let sink = script(response);
        router.register(route.clone(), sink.clone());
        assert!(
            bot(router)
                .send_native_file_routed(&route, &target, vec![1], "records.json", None)
                .await
                .is_err()
        );
        assert_eq!(sink.calls.lock().unwrap().len(), 1);
    }
    struct DefaultSink;
    #[async_trait::async_trait]
    impl RuntimeActionSink for DefaultSink {
        async fn send_action(&self, _: Value) -> crate::NuonoResult<()> {
            panic!("default capability must have no effects")
        }
    }
    let router = Arc::new(RuntimeActionRouter::new());
    router.register(route.clone(), Arc::new(DefaultSink));
    assert_eq!(
        bot(router)
            .send_native_file_routed(&route, &target, vec![1], "records.json", None)
            .await
            .unwrap(),
        FileDelivery::Unsupported
    );
}

#[tokio::test]
async fn native_files_validate_metadata_byte_budget_and_connection_before_effects() {
    let route = RuntimeBotRoute::new("qq", "instance", "app-a");
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = script(Some(sent()));
    router.register(route.clone(), sink.clone());
    let bot = bot(router);
    for (name, mime) in [
        ("../records.json", None),
        ("x\n", None),
        ("records.json", Some("application/json; x=1")),
    ] {
        assert!(
            bot.send_native_file_routed(&route, &qq_actor("app-a"), vec![], name, mime)
                .await
                .is_err()
        );
    }
    assert!(
        bot.send_native_file_routed(
            &route,
            &qq_actor("app-a"),
            vec![0; MAX_FILE_BYTES + 1],
            "records.json",
            None
        )
        .await
        .is_err()
    );
    assert!(
        bot.send_native_file_routed(
            &RuntimeBotRoute::new("qq", "missing", "app-a"),
            &qq_actor("app-a"),
            vec![],
            "records.json",
            None
        )
        .await
        .is_err()
    );
    assert!(sink.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn telegram_negative_users_and_direct_chats_never_reach_the_native_file_port() {
    let route = RuntimeBotRoute::new("telegram", "instance", "123");
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = script(Some(sent()));
    router.register(route.clone(), sink.clone());
    let bot = bot(router);
    let user = actor(Platform::Telegram, IdentityScheme::Numeric, "-100123", None);
    let direct = DeliveryTarget::conversation(
        ConversationRef::direct(
            IdentityRef::new(
                Platform::Telegram,
                IdentityKind::User,
                IdentityScheme::Numeric,
                "123",
                None::<String>,
            )
            .unwrap(),
            Some(
                IdentityRef::new(
                    Platform::Telegram,
                    IdentityKind::Conversation,
                    IdentityScheme::Numeric,
                    "-100123",
                    None::<String>,
                )
                .unwrap(),
            ),
        )
        .unwrap(),
    )
    .unwrap();
    for target in [user, direct] {
        assert!(
            bot.send_native_file_routed(&route, &target, vec![1], "records.json", None)
                .await
                .is_err()
        );
    }
    assert!(sink.calls.lock().unwrap().is_empty());
}

#[derive(Default)]
struct PendingSink {
    started: Notify,
    dropped: Notify,
    calls: AtomicUsize,
    owner: Mutex<Option<std::thread::ThreadId>>,
}
struct PendingGuard<'a>(&'a PendingSink);
impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.dropped.notify_one();
    }
}
#[async_trait::async_trait]
impl RuntimeActionSink for PendingSink {
    async fn call_native_file(
        &self,
        _: NativeFileSendRequest,
    ) -> crate::NuonoResult<FileSendResponse> {
        if let Some(owner) = *self.owner.lock().unwrap() {
            assert_eq!(owner, std::thread::current().id());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _guard = PendingGuard(self);
        self.started.notify_one();
        std::future::pending().await
    }
    async fn send_action(&self, _: Value) -> crate::NuonoResult<()> {
        panic!("native port required")
    }
}

#[test]
fn cancelling_a_native_file_on_another_runtime_drops_the_polled_host_upload_once() {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let route = RuntimeBotRoute::new("qq", "instance", "app-a");
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = Arc::new(PendingSink::default());
    let r = router.clone();
    let s = sink.clone();
    let host_route = route.clone();
    host.block_on(host.spawn(async move {
        *s.owner.lock().unwrap() = Some(std::thread::current().id());
        r.register(host_route, s);
    }))
    .unwrap();
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    caller.block_on(async {
        let task = tokio::spawn(async move {
            bot(router)
                .send_native_file_routed(&route, &qq_actor("app-a"), vec![1], "file.json", None)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), sink.started.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), sink.dropped.notified())
            .await
            .unwrap();
    });
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn native_file_budget_timeout_cancels_the_host_upload_without_fallback() {
    let route = RuntimeBotRoute::new("qq", "instance", "app-a");
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = Arc::new(PendingSink::default());
    router.register(route.clone(), sink.clone());
    let task = tokio::spawn(async move {
        bot(router)
            .send_native_file_routed(&route, &qq_actor("app-a"), vec![1], "file.json", None)
            .await
    });
    sink.started.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(46)).await;
    assert!(
        task.await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("未确认送达")
    );
    sink.dropped.notified().await;
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct Audit(Mutex<Vec<String>>);
#[async_trait::async_trait]
impl ActionAudit for Audit {
    async fn record(
        &self,
        action: &TransportAction,
        _: &ActionAuthority,
        phase: &str,
        _: Option<&ActionError>,
    ) -> std::result::Result<(), ActionError> {
        assert!(matches!(action.action, OutboundAction::SendFile { .. }));
        self.0.lock().unwrap().push(phase.to_owned());
        Ok(())
    }
}
#[tokio::test]
async fn native_file_authority_and_audit_precede_effects_and_receipt_validation_records_failure() {
    let audit = Arc::new(Audit::default());
    let router = Arc::new(RuntimeActionRouter::with_audit(audit.clone()));
    let route = RuntimeBotRoute::new("qq", "instance", "app-a");
    let sink = script(Some(sent()));
    router.register(route.clone(), sink.clone());
    let action = OutboundAction::SendFile {
        target: qq_actor("app-a"),
        source: FileSource::Base64("base64://e30=".into()),
        file_name: "records.json".into(),
        content_type: None,
    };
    let request = TransportAction::new(
        crate::core::runtime_transport::route_identity(&route).unwrap(),
        action,
    )
    .with_authority(ActionAuthority::Plugin { name: "".into() });
    assert!(router.call_typed_routed(&route, request).await.is_err());
    assert!(sink.calls.lock().unwrap().is_empty());
    assert!(audit.0.lock().unwrap().is_empty());
    bot(router.clone())
        .send_native_file_routed(&route, &qq_actor("app-a"), vec![1], "records.json", None)
        .await
        .unwrap();
    let malformed = script(Some(json!({"status":"sent","message_ids":[],"raw":{}})));
    router.register(route.clone(), malformed);
    assert!(
        bot(router)
            .send_native_file_routed(&route, &qq_actor("app-a"), vec![1], "records.json", None)
            .await
            .is_err()
    );
    assert_eq!(
        *audit.0.lock().unwrap(),
        ["started", "completed", "started", "failed"]
    );
}
