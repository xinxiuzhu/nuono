use super::*;
use nuono_app::platform::UnifiedNotice;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[tokio::test]
async fn native_stop_interrupts_pending_work_and_prevents_a_new_attempt() {
    let ingress = RuntimeIngress::new();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let calls = Arc::new(Calls::default());
    let observed = calls.clone();
    let worker_ingress = ingress.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        worker_ingress
            .run_until_stopped(stop_rx, async move {
                observed.active.fetch_add(1, Ordering::SeqCst);
                let _active = Active(observed);
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .await;
    });
    started_rx.await.unwrap();
    stop_tx.send_replace(true);
    tokio::time::timeout(std::time::Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.active.load(Ordering::SeqCst), 0);

    let cancelled = RuntimeIngress::new();
    cancelled.cancel();
    let (_, stop_rx) = tokio::sync::watch::channel(false);
    cancelled
        .run_until_stopped(stop_rx, async {
            panic!("closed ingress cannot start network work");
        })
        .await;
}

#[tokio::test]
async fn cancelling_ingress_interrupts_supervisor_retry_delay() {
    let ingress = RuntimeIngress::new();
    let worker_ingress = ingress.clone();
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let started = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let session_attempts = attempts.clone();
    let session_started = started.clone();
    let worker = tokio::spawn(async move {
        worker_ingress
            .run_until_stopped(stop_rx, async {
                nuono_core::adapter::supervise(
                    "synthetic-discord",
                    nuono_core::adapter::BackoffPolicy {
                        initial: std::time::Duration::from_secs(600),
                        max: std::time::Duration::from_secs(600),
                        stable_after: std::time::Duration::from_secs(60),
                    },
                    move || {
                        session_attempts.fetch_add(1, Ordering::SeqCst);
                        session_started.notify_one();
                        async { Err(anyhow::anyhow!("synthetic gateway failure")) }
                    },
                )
                .await;
            })
            .await;
    });
    started.notified().await;
    ingress.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "shutdown cannot reconnect"
    );
}

#[derive(Default)]
struct Calls {
    started: AtomicUsize,
    active: AtomicUsize,
    finished: AtomicUsize,
    changed: Notify,
}
struct Active(Arc<Calls>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_one();
    }
}
struct Controlled {
    calls: Arc<Calls>,
    finish: Semaphore,
}
impl Controlled {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(Calls::default()),
            finish: Semaphore::new(0),
        })
    }
    async fn started(&self, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let changed = self.calls.changed.notified();
                if self.calls.started.load(Ordering::SeqCst) == count {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("business tasks started");
    }
}
#[async_trait]
impl Dispatch for Controlled {
    async fn deliver_waiting(&self, event: &UnifiedEvent) -> Result<bool> {
        let UnifiedEvent::Notice(notice) = event else {
            return Ok(false);
        };
        Ok(notice.raw["answer"] == true)
    }
    async fn process(&self, _: UnifiedEvent) -> Result<()> {
        self.calls.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.calls.clone());
        self.calls.started.fetch_add(1, Ordering::SeqCst);
        self.calls.changed.notify_one();
        self.finish.acquire().await.unwrap().forget();
        self.calls.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[derive(Default)]
struct Acks {
    calls: Mutex<Vec<Value>>,
    fail: bool,
}
#[async_trait]
impl RuntimeActionSink for Acks {
    async fn send_action(&self, action: Value) -> Result<()> {
        self.call_action(action).await.map(|_| ())
    }
    async fn call_action(&self, action: Value) -> Result<Value> {
        self.calls.lock().unwrap().push(action);
        if self.fail {
            return Err(NuonoError::Platform(
                "native-credential-must-not-log".into(),
            ));
        }
        Ok(json!({"acknowledged":true}))
    }
}
fn event(interaction: bool, answer: bool) -> UnifiedEvent {
    UnifiedEvent::Notice(UnifiedNotice {
        self_id: 9,
        notice_type: if interaction { "interaction" } else { "test" }.into(),
        sub_type: "button".into(),
        raw: json!({"post_type":if interaction {"interaction"} else {"notice"},
            "interaction_id":"111","interaction_token":"host-opaque-token",
            "interaction_context":{"token":"native-credential"},"answer":answer}),
    })
}
async fn fill(pool: &RuntimeIngress, dispatch: &Arc<Controlled>) {
    for _ in 0..MAX_IN_FLIGHT {
        assert_eq!(
            pool.submit_with("test", dispatch.clone(), event(false, false), None)
                .await
                .unwrap(),
            IngressOutcome::Accepted
        );
    }
    dispatch.started(MAX_IN_FLIGHT).await;
    assert_eq!(dispatch.calls.active.load(Ordering::SeqCst), MAX_IN_FLIGHT);
}

#[tokio::test]
async fn full_pool_still_acknowledges_click_promptly_and_rejects_extra_business() {
    let pool = RuntimeIngress::new();
    let dispatch = Controlled::new();
    let acknowledgements = Arc::new(Acks::default());
    fill(&pool, &dispatch).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        pool.submit_with(
            "discord",
            dispatch.clone(),
            event(true, false),
            Some(acknowledgements.clone()),
        ),
    )
    .await
    .expect("ACK must not wait for a blocked business task");
    assert!(matches!(result, Err(NuonoError::Conflict(_))));
    assert_eq!(dispatch.calls.started.load(Ordering::SeqCst), MAX_IN_FLIGHT);
    assert_eq!(dispatch.calls.finished.load(Ordering::SeqCst), 0);
    assert_eq!(
        *acknowledgements.calls.lock().unwrap(),
        vec![json!({"action":"ack_interaction","params":{
        "interaction_id":"111","interaction_context":{"token":"native-credential"}}})]
    );
    pool.shutdown().await;
    assert_eq!(dispatch.calls.active.load(Ordering::SeqCst), 0);
    assert_eq!(pool.inner.tracker.len(), 0);
}

#[tokio::test]
async fn ordinary_and_button_waiting_answers_bypass_all_busy_slots_without_extra_business() {
    let pool = RuntimeIngress::new();
    let dispatch = Controlled::new();
    let acknowledgements = Arc::new(Acks::default());
    fill(&pool, &dispatch).await;
    for interaction in [false, true] {
        assert_eq!(
            pool.submit_with(
                "test",
                dispatch.clone(),
                event(interaction, true),
                Some(acknowledgements.clone())
            )
            .await
            .unwrap(),
            IngressOutcome::WaitingDelivered
        );
    }
    assert_eq!(acknowledgements.calls.lock().unwrap().len(), 1);
    assert_eq!(dispatch.calls.started.load(Ordering::SeqCst), MAX_IN_FLIGHT);
    assert_eq!(pool.inner.tracker.len(), MAX_IN_FLIGHT);
    pool.shutdown().await;
    assert_eq!(dispatch.calls.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn aborted_consumer_cancels_and_joins_dispatches_and_closed_pool_never_acks_new_events() {
    let pool = RuntimeIngress::new();
    let dispatch = Controlled::new();
    pool.submit_with("test", dispatch.clone(), event(false, false), None)
        .await
        .unwrap();
    dispatch.started(1).await;
    let guard = pool.worker_guard();
    let consumer = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    consumer.abort();
    let _ = consumer.await;
    assert!(pool.inner.cancelled.is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), pool.inner.tracker.wait())
        .await
        .unwrap();
    assert_eq!(dispatch.calls.active.load(Ordering::SeqCst), 0);
    assert_eq!(pool.inner.tracker.len(), 0);
    let acknowledgements = Arc::new(Acks::default());
    assert!(
        pool.submit_with(
            "test",
            dispatch,
            event(true, false),
            Some(acknowledgements.clone())
        )
        .await
        .is_err()
    );
    assert!(acknowledgements.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn acknowledgement_error_is_attempted_once_and_does_not_resend_or_drop_accepted_business() {
    let pool = RuntimeIngress::new();
    let dispatch = Controlled::new();
    let acknowledgements = Arc::new(Acks {
        fail: true,
        ..Default::default()
    });
    assert_eq!(
        pool.submit_with(
            "test",
            dispatch.clone(),
            event(true, false),
            Some(acknowledgements.clone())
        )
        .await
        .unwrap(),
        IngressOutcome::Accepted
    );
    dispatch.started(1).await;
    assert_eq!(acknowledgements.calls.lock().unwrap().len(), 1);
    pool.shutdown().await;
    assert_eq!(dispatch.calls.active.load(Ordering::SeqCst), 0);
}
