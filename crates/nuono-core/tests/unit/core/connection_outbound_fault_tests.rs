use super::*;
use std::time::Duration;

struct TestSink(mpsc::Sender<TransportFrame>);

#[async_trait::async_trait]
impl RuntimeActionSink for TestSink {
    async fn send_action(&self, action: serde_json::Value) -> Result<()> {
        self.0
            .send(TransportFrame::Text(action.to_string()))
            .await
            .map_err(|_| crate::error::NuonoError::Connection("test transport closed".into()))
    }
}

struct Registries {
    sessions: Arc<BotSessionManager>,
    senders: Arc<DashMap<i64, mpsc::Sender<TransportFrame>>>,
    states: Arc<crate::core::bot_state::BotStateRegistry>,
    router: Arc<crate::core::RuntimeActionRouter>,
}

impl Registries {
    fn new() -> Self {
        Self {
            sessions: Arc::new(BotSessionManager::new()),
            senders: Arc::new(DashMap::new()),
            states: Arc::new(crate::core::bot_state::BotStateRegistry::new()),
            router: Arc::new(crate::core::RuntimeActionRouter::new()),
        }
    }

    fn guard(&self) -> ConnectionSessionGuard {
        ConnectionSessionGuard::new(
            self.sessions.clone(),
            self.senders.clone(),
            self.states.clone(),
            self.router.clone(),
        )
    }

    fn assert_empty(&self) {
        assert!(self.senders.is_empty());
        assert!(self.states.snapshots().is_empty());
        assert!(self.router.is_empty());
        assert_eq!(self.sessions.active_count(), 0);
    }
}

fn pending_session() -> (
    PendingConnectionSession,
    Arc<SendActor>,
    mpsc::Receiver<TransportFrame>,
) {
    let (sender, receiver) = mpsc::channel(1);
    let actor = Arc::new(SendActor::new(42, sender.clone()));
    (
        PendingConnectionSession {
            actor: actor.clone(),
            sender: sender.clone(),
            route: RuntimeBotRoute::new("one_bot", "42", "42"),
            sink: Arc::new(TestSink(sender)),
            ip: "127.0.0.1".into(),
            heartbeat_timeout: Duration::from_secs(30),
        },
        actor,
        receiver,
    )
}

#[tokio::test]
async fn cancelled_connection_driver_cleans_all_registries_and_releases_transport() {
    let registries = Registries::new();
    let guard = registries.guard();
    let reader_registration = guard.registration.clone();
    let (pending, actor, mut transport) = pending_session();
    assert!(reader_registration.register(pending).is_some());
    assert_eq!(registries.senders.len(), 1);
    assert_eq!(registries.states.snapshots().len(), 1);
    assert_eq!(registries.router.len(), 1);
    assert_eq!(registries.sessions.active_count(), 1);
    let (started, ready) = tokio::sync::oneshot::channel();
    let driver = tokio::spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        futures::future::pending::<()>().await;
    });
    ready.await.unwrap();
    driver.abort();
    assert!(driver.await.unwrap_err().is_cancelled());
    registries.assert_empty();
    assert!(reader_registration.route().is_none());
    assert!(!reader_registration.cleanup(), "cleanup must be idempotent");
    assert!(actor.is_closed());
    assert!(
        send_to_registered_bot(&registries.senders, 42, "late".into())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), transport.recv())
            .await
            .unwrap()
            .is_none(),
        "a registry or worker retained its transport sender"
    );
}

#[tokio::test]
async fn old_connection_guard_cannot_remove_any_replacement_registration() {
    let registries = Registries::new();
    let old_guard = registries.guard();
    let (pending, old_actor, mut old_transport) = pending_session();
    assert!(old_guard.registration.register(pending).is_some());
    let (route, old_generation) = old_guard.registration.route().unwrap();
    let new_guard = registries.guard();
    let (pending, new_actor, mut new_transport) = pending_session();
    assert!(new_guard.registration.register(pending).is_some());
    let (_, new_generation) = new_guard.registration.route().unwrap();
    assert_ne!(old_generation, new_generation);
    drop(old_guard);

    assert!(old_actor.is_closed());
    assert!(Arc::ptr_eq(
        &registries.sessions.get(42).unwrap(),
        &new_actor
    ));
    assert!(!new_actor.is_closed());
    assert_eq!(registries.states.connection(42).unwrap().0, new_generation);
    assert!(registries.router.contains(&route));
    assert!(!registries.router.unregister(&route, old_generation));
    assert_eq!(registries.senders.len(), 1);
    assert!(
        registries.senders.get(&42).unwrap().same_channel(
            &new_guard
                .registration
                .state
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .sender
        )
    );
    send_to_registered_bot(&registries.senders, 42, "replacement".into())
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), new_transport.recv()).await.unwrap(),
        Some(TransportFrame::Text(text)) if text == "replacement"
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), old_transport.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(new_guard.registration.cleanup());
    registries.assert_empty();
}

#[tokio::test]
async fn late_reader_cannot_republish_any_registration_after_driver_cleanup() {
    let registries = Registries::new();
    let guard = registries.guard();
    let reader_registration = guard.registration.clone();
    drop(guard);
    let (pending, actor, mut transport) = pending_session();
    assert!(reader_registration.register(pending).is_none());
    registries.assert_empty();
    assert!(reader_registration.route().is_none());
    assert!(actor.is_closed());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), transport.recv())
            .await
            .unwrap()
            .is_none()
    );
}

fn inbound_event(bot_id: i64, route: Option<RuntimeBotRoute>) -> InboundPostEvent {
    InboundPostEvent {
        received_at: std::time::Instant::now(),
        receipt: None,
        bot_id,
        text: "{\"post_type\":\"message\"}".into(),
        event: serde_json::json!({"post_type": "message"}),
        route,
    }
}

#[tokio::test]
async fn cleanup_rejects_new_business_events_with_missing_or_stale_route() {
    let registries = Registries::new();
    let guard = registries.guard();
    let (pending, _actor, _transport) = pending_session();
    assert!(guard.registration.register(pending).is_some());
    let (saved_route, _) = guard.registration.route().unwrap();
    let (sender, mut receiver) = mpsc::channel(1);
    assert!(guard.registration.cleanup());

    for route in [None, Some(saved_route)] {
        assert!(matches!(
            guard
                .registration
                .enqueue_event(&sender, inbound_event(42, route)),
            Err(InboundAdmissionError::Closed)
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    assert!(matches!(
        guard
            .registration
            .enqueue_event(&sender, inbound_event(0, None)),
        Err(InboundAdmissionError::Closed)
    ));
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn open_admission_preserves_unknown_bot_and_uses_current_registered_route() {
    let registries = Registries::new();
    let guard = registries.guard();
    let (sender, mut receiver) = mpsc::channel(1);
    guard
        .registration
        .enqueue_event(&sender, inbound_event(0, None))
        .unwrap();
    assert!(receiver.recv().await.unwrap().route.is_none());

    let (pending, _actor, _transport) = pending_session();
    assert!(guard.registration.register(pending).is_some());
    let (current_route, _) = guard.registration.route().unwrap();
    let stale_route = RuntimeBotRoute::new("one_bot", "stale", "999");
    guard
        .registration
        .enqueue_event(&sender, inbound_event(42, Some(stale_route)))
        .unwrap();
    assert_eq!(receiver.recv().await.unwrap().route, Some(current_route));
}
