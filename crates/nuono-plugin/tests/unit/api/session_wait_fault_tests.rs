use super::*;
use crate::api::bot::MessageContext;
use crate::api::waiter::WaitManager;
use crate::core::event_bus::EventBus;
use crate::core::event_kernel::RuntimeActionSink;
use crate::core::onebot_client::ApiClient;
use crate::error::NuonoError;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

const KEY: &str = "42:private:7";

#[derive(Clone, Copy)]
enum Behavior {
    Pending,
    ReplyThenPending,
    Reply,
}

struct PendingDrop(Arc<AtomicUsize>);
impl Drop for PendingDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct PromptSink {
    waits: Arc<WaitManager>,
    behavior: Behavior,
    started: Notify,
    calls: AtomicUsize,
    dropped: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RuntimeActionSink for PromptSink {
    async fn send_action(&self, _: Value) -> Result<()> {
        let _resource = PendingDrop(self.dropped.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.waits.has_wait(KEY),
            "admission must precede publication"
        );
        if matches!(self.behavior, Behavior::Reply | Behavior::ReplyThenPending) {
            assert!(self.waits.respond(KEY, json!({"raw_message":"early"})));
        }
        self.started.notify_one();
        if matches!(
            self.behavior,
            Behavior::Pending | Behavior::ReplyThenPending
        ) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn bot(waits: Arc<WaitManager>, sink: Option<Arc<dyn RuntimeActionSink>>) -> Bot {
    let mut bot = Bot::new(
        42,
        Arc::new(EventBus::new()),
        waits,
        Arc::new(ApiClient::new()),
        None,
        sink,
    )
    .with_wait_owner("fixture", None);
    bot.set_current_message(
        MessageContext::from_onebot_json(&json!({
            "post_type":"message", "message_type":"private", "self_id":42, "user_id":7
        }))
        .unwrap(),
    );
    bot
}

struct Fixture {
    waits: Arc<WaitManager>,
    sessions: Arc<SessionManager>,
    session: Arc<InteractiveSession>,
    sink: Arc<PromptSink>,
}

fn fixture(behavior: Behavior, multiply: bool, capacity: usize) -> Fixture {
    let waits = Arc::new(WaitManager::with_capacity(capacity));
    let sink = Arc::new(PromptSink {
        waits: waits.clone(),
        behavior,
        started: Notify::new(),
        calls: AtomicUsize::new(0),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let sessions = Arc::new(SessionManager::new());
    let session = Arc::new(InteractiveSession::new(
        KEY.into(),
        bot(waits.clone(), Some(sink.clone())),
        sessions.clone(),
        multiply,
    ));
    Fixture {
        waits,
        sessions,
        session,
        sink,
    }
}

async fn assert_released(f: &Fixture) {
    assert_eq!(f.waits.wait_count(), 0);
    assert_eq!(f.sessions.instance_count().await, (0, 0));
    assert_eq!(f.sink.dropped.load(Ordering::SeqCst), 1);
    let probe = f
        .waits
        .try_register_wait("probe".into(), Duration::from_secs(30))
        .unwrap();
    assert!(probe.is_active(), "capacity must be reusable");
}

#[tokio::test]
async fn bot_wait_entry_points_preserve_overload_and_disabled_errors() {
    let waits = Arc::new(WaitManager::with_capacity(1));
    let occupied = waits
        .try_register_wait("other".into(), Duration::from_secs(30))
        .unwrap();
    let bot = bot(waits.clone(), None);
    let timeout = Duration::from_secs(5);
    assert!(matches!(
        bot.prepare_reply_wait(timeout, crate::api::waiter::ReplyWaitMode::Single),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        bot.prepare_reply_event_wait(timeout, crate::api::waiter::ReplyWaitMode::Single),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        bot.wait_for_reply(timeout).await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        bot.wait_for_reply_event(timeout).await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(occupied.is_active());
    drop(occupied);
    waits.cancel_plugin("fixture");
    assert!(matches!(
        bot.prepare_reply_wait(timeout, crate::api::waiter::ReplyWaitMode::Single),
        Err(NuonoError::Conflict(_))
    ));
    assert!(matches!(
        bot.prepare_reply_event_wait(timeout, crate::api::waiter::ReplyWaitMode::Single),
        Err(NuonoError::Conflict(_))
    ));
    assert!(matches!(
        bot.wait_for_reply(timeout).await,
        Err(NuonoError::Conflict(_))
    ));
    assert!(matches!(
        bot.wait_for_reply_event(timeout).await,
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(waits.wait_count(), 0);
}

#[tokio::test]
async fn full_registry_rejects_before_prompt_and_compatibility_index_registration() {
    for multiply in [false, true] {
        let f = fixture(Behavior::Pending, multiply, 1);
        let occupied = f
            .waits
            .try_register_wait("other".into(), Duration::from_secs(30))
            .unwrap();
        assert!(matches!(
            f.session.send_and_wait("prompt", None, 30).await,
            Err(NuonoError::Overloaded(_))
        ));
        assert_eq!(f.sink.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.sessions.instance_count().await, (0, 0));
        assert!(occupied.is_active());
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_prompt_deadline_releases_wait_and_both_compatibility_indices() {
    for multiply in [false, true] {
        let f = fixture(Behavior::Pending, multiply, 1);
        let session = f.session.clone();
        let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 1).await });
        f.sink.started.notified().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(task.await.unwrap(), Err(NuonoError::Timeout(_))));
        assert_released(&f).await;
    }
}

#[tokio::test]
async fn cancelling_or_disabling_a_wait_interrupts_stalled_prompt_publication() {
    for disable in [false, true] {
        let f = fixture(Behavior::Pending, true, 1);
        let session = f.session.clone();
        let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 3600).await });
        f.sink.started.notified().await;
        if disable {
            f.waits.cancel_plugin("fixture");
        } else {
            assert!(f.waits.cancel(KEY));
        }
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(NuonoError::Conflict(_))));
        assert_released(&f).await;
    }
}

#[tokio::test]
async fn replacement_cancels_stalled_prompt_without_removing_new_generation() {
    let f = fixture(Behavior::Pending, true, 1);
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 3600).await });
    f.sink.started.notified().await;
    let replacement = f
        .waits
        .try_register_wait(KEY.into(), Duration::from_secs(30))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(NuonoError::Conflict(_))));
    assert!(replacement.is_active());
    assert_eq!(f.sessions.instance_count().await, (0, 0));
    assert_eq!(f.sink.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn aborting_stalled_prompt_drops_transport_future_and_compatibility_indices() {
    let f = fixture(Behavior::Pending, true, 1);
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 3600).await });
    f.sink.started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_released(&f).await;
}

#[tokio::test]
async fn immediate_reply_does_not_cancel_successful_prompt() {
    let f = fixture(Behavior::Reply, false, 1);
    let reply = f
        .session
        .send_and_wait("prompt", None, 30)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply["raw_message"], "early");
    assert_released(&f).await;
}

#[tokio::test(start_paused = true)]
async fn early_reply_does_not_extend_stalled_prompt_deadline() {
    let f = fixture(Behavior::ReplyThenPending, true, 1);
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.send_and_wait("prompt", None, 1).await });
    f.sink.started.notified().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(task.await.unwrap(), Err(NuonoError::Timeout(_))));
    assert_released(&f).await;
}
