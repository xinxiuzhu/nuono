//! 覆盖 WaitReply 准入、提示发送与回复回调全生命周期的故障注入测试。
use super::*;
use crate::core::event_kernel::RuntimeEventContext;
use crate::core::runtime_stages::PluginHostRuntimeStage;
use nuono_app::transport::TransportFrame;
use nuono_plugin::api::waiter::{WaitRegistration, WaitScope};
use serde_json::json;
use std::sync::Mutex;
use tokio::sync::{Notify, mpsc};

const DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
enum Callback {
    Stop,
    Pending,
    Chain,
    FillAndChain,
}

struct Probe {
    callback: Callback,
    entered: Notify,
    dropped: AtomicUsize,
    waits: Arc<WaitManager>,
    held: Mutex<Vec<WaitRegistration<Value>>>,
}

struct DropCount(Arc<Probe>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct WaitingPlugin {
    name: String,
    probe: Arc<Probe>,
}

#[async_trait::async_trait]
impl Plugin for WaitingPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "WaitReply fault fixture"
    }
    async fn on_message(&self, _: &Event, _: &str, _: &str) -> anyhow::Result<PluginResponse> {
        let _resources = DropCount(self.probe.clone());
        self.probe.entered.notify_one();
        match self.probe.callback {
            Callback::Stop => Ok(PluginResponse::Stop),
            Callback::Pending => std::future::pending().await,
            Callback::Chain => Ok(response(&self.name, "next", 1)),
            Callback::FillAndChain => {
                let mut held = self.probe.held.lock().unwrap();
                for index in 0..1024 {
                    held.push(
                        self.probe
                            .waits
                            .try_register_wait(format!("full-{index}"), Duration::from_secs(60))
                            .unwrap(),
                    );
                }
                Ok(response(&self.name, "next", 1))
            }
        }
    }
}

fn event() -> Value {
    json!({"post_type":"message","message_type":"private","self_id":42,
        "user_id":7,"raw_message":"answer","message_id":1})
}

fn response(owner: &str, session: &str, timeout_secs: u32) -> PluginResponse {
    PluginResponse::WaitReply {
        reply_message: json!({"action":"send_msg","params":{"message":"prompt"}}),
        session_id: session.into(),
        timeout_secs,
        callback_id: "callback".into(),
        plugin_name: owner.into(),
        event_context: event(),
    }
}

fn context(sender: mpsc::Sender<TransportFrame>) -> RuntimeEventContext<'static> {
    RuntimeEventContext {
        locale: Default::default(),
        bot_id: 42,
        raw_text: "",
        event: event(),
        bot_sender: Some(sender),
        action_sink: None,
        action_transport: None,
        domain_event: None,
        unified_event: None,
        resolved_user_id: None,
        resolved_identity_id: None,
        web_chat_account_id: None,
        blocked: false,
        is_message: true,
        reply_attempts: AtomicUsize::new(0),
        reply_ok: AtomicUsize::new(0),
    }
}

struct Fixture {
    h: Harness,
    tasks: Arc<PluginTaskScope>,
    probe: Arc<Probe>,
}

impl Fixture {
    async fn new(callback: Callback) -> Self {
        let h = Harness::new("wait-reply-fault").await;
        let tasks = Arc::new(PluginTaskScope::new(&h.plugin));
        let probe = Arc::new(Probe {
            callback,
            entered: Notify::new(),
            dropped: AtomicUsize::new(0),
            waits: h.host.wait_manager.clone(),
            held: Mutex::new(Vec::new()),
        });
        h.host.plugins.write().await[0] = LoadedPlugin {
            plugin: Box::new(WaitingPlugin {
                name: h.plugin.clone(),
                probe: probe.clone(),
            }),
        }
        .into();
        h.host
            .task_scopes
            .write()
            .await
            .insert(h.plugin.clone(), tasks.clone());
        Self { h, tasks, probe }
    }

    fn stage(&self) -> PluginHostRuntimeStage {
        PluginHostRuntimeStage::new(
            self.h.host.clone(),
            self.probe.waits.clone(),
            self.h.host.permission_manager.clone(),
        )
    }

    async fn response(&self, timeout_secs: u32) -> PluginProcessResult {
        PluginProcessResult::new(
            Some(self.h.plugin.clone()),
            response(&self.h.plugin, "first", timeout_secs),
        )
        .with_wait_invocation(
            self.h
                .host
                .hosted_plugin(&self.h.plugin)
                .await
                .unwrap()
                .invocation(),
        )
    }

    async fn cleanup(self) {
        self.probe.held.lock().unwrap().clear();
        self.h.host.shutdown().await.unwrap();
        drop((self.tasks, self.probe));
        self.h.cleanup().await;
    }
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("WaitReply must release resources promptly")
}

async fn wait_count(waits: &WaitManager, count: usize) {
    bounded(async {
        while waits.wait_count() != count {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn assert_all_task_slots_returned(tasks: &PluginTaskScope) {
    bounded(async {
        for _ in 0..256 {
            while !tasks.try_spawn("capacity-returned", std::future::pending()) {
                tokio::task::yield_now().await;
            }
        }
    })
    .await;
    assert!(!tasks.try_spawn("full", std::future::pending()));
}

#[tokio::test]
async fn wait_reply_registry_full_rejects_before_prompt() {
    let f = Fixture::new(Callback::Stop).await;
    let held = (0..1024)
        .map(|index| {
            f.probe
                .waits
                .try_register_wait(format!("occupied-{index}"), Duration::from_secs(60))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let (sender, mut output) = mpsc::channel(1);
    let result = f
        .stage()
        .apply_response(&mut context(sender), f.response(60).await)
        .await;
    assert!(matches!(
        result,
        Err(crate::error::NuonoError::Overloaded(_))
    ));
    assert!(output.try_recv().is_err());
    assert_eq!(f.probe.waits.wait_count(), 1024);
    drop(held);
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_task_scope_full_rejects_before_prompt_and_drops_registration() {
    let f = Fixture::new(Callback::Stop).await;
    assert_all_task_slots_returned(&f.tasks).await;
    let (sender, mut output) = mpsc::channel(1);
    let result = f
        .stage()
        .apply_response(&mut context(sender), f.response(60).await)
        .await;
    assert!(matches!(
        result,
        Err(crate::error::NuonoError::Overloaded(_))
    ));
    assert!(output.try_recv().is_err());
    assert_eq!(f.probe.waits.wait_count(), 0);
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_pending_prompt_times_out_without_publishing() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, mut output) = mpsc::channel(1);
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let mut ctx = context(sender);
    bounded(f.stage().apply_response(&mut ctx, f.response(1).await))
        .await
        .unwrap();
    wait_count(&f.probe.waits, 0).await;
    assert_eq!(ctx.reply_ok.load(Ordering::SeqCst), 0);
    assert!(matches!(output.try_recv(), Ok(TransportFrame::Text(text)) if text == "occupied"));
    assert!(output.try_recv().is_err());
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_cancelled_prompt_caller_returns_all_capacity() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, _output) = mpsc::channel(1);
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let stage = f.stage();
    let response = f.response(60).await;
    let task =
        tokio::spawn(async move { stage.apply_response(&mut context(sender), response).await });
    wait_count(&f.probe.waits, 1).await;
    task.abort();
    let _ = task.await;
    wait_count(&f.probe.waits, 0).await;
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_replaced_registration_cancels_pending_prompt_without_removing_new_wait() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, mut output) = mpsc::channel(1);
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let stage = f.stage();
    let response = f.response(60).await;
    let task =
        tokio::spawn(async move { stage.apply_response(&mut context(sender), response).await });
    wait_count(&f.probe.waits, 1).await;
    let replacement = f
        .probe
        .waits
        .try_register_scoped_event_wait(
            "first".into(),
            Duration::from_secs(60),
            WaitScope::from_event(42, &event()),
        )
        .unwrap();
    bounded(task).await.unwrap().unwrap();
    assert!(replacement.is_active());
    assert_eq!(f.probe.waits.wait_count(), 1);
    assert!(matches!(output.try_recv(), Ok(TransportFrame::Text(text)) if text == "occupied"));
    assert!(output.try_recv().is_err());
    drop(replacement);
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_failed_prompt_send_releases_wait_and_task() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, output) = mpsc::channel(1);
    drop(output);
    bounded(
        f.stage()
            .apply_response(&mut context(sender), f.response(60).await),
    )
    .await
    .unwrap();
    wait_count(&f.probe.waits, 0).await;
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_invalid_prompt_releases_wait_without_sending() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, mut output) = mpsc::channel(1);
    let mut response = f.response(60).await;
    if let PluginResponse::WaitReply { reply_message, .. } = &mut response.response {
        reply_message["params"]["message"] =
            json!([{"type":"buttons","data":{"buttons":"invalid"}}]);
    }
    let result = bounded(f.stage().apply_response(&mut context(sender), response)).await;
    assert!(matches!(
        result,
        Err(crate::error::NuonoError::MessageFormat(_))
    ));
    wait_count(&f.probe.waits, 0).await;
    assert!(output.try_recv().is_err());
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_early_reply_survives_until_pending_prompt_is_sent() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, mut output) = mpsc::channel(1);
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let stage = f.stage();
    let response = f.response(60).await;
    let task =
        tokio::spawn(async move { stage.apply_response(&mut context(sender), response).await });
    wait_count(&f.probe.waits, 1).await;
    assert!(f.probe.waits.respond("first", event()));
    tokio::task::yield_now().await;
    assert_eq!(f.probe.dropped.load(Ordering::SeqCst), 0);
    output.recv().await.unwrap();
    bounded(task).await.unwrap().unwrap();
    output.recv().await.unwrap();
    bounded(f.probe.entered.notified()).await;
    wait_count(&f.probe.waits, 0).await;
    bounded(async {
        while f.probe.dropped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_plugin_disable_drops_pending_callback_before_returning() {
    let f = Fixture::new(Callback::Pending).await;
    let (sender, mut output) = mpsc::channel(1);
    bounded(
        f.stage()
            .apply_response(&mut context(sender), f.response(60).await),
    )
    .await
    .unwrap();
    output.recv().await.unwrap();
    assert!(f.probe.waits.respond("first", event()));
    bounded(f.probe.entered.notified()).await;
    bounded(f.h.host.toggle(&f.h.plugin, false)).await.unwrap();
    assert_eq!(f.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(f.probe.waits.wait_count(), 0);
    assert!(f.tasks.is_closed());
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_connection_close_drops_pending_callback() {
    let f = Fixture::new(Callback::Pending).await;
    let (sender, mut output) = mpsc::channel(1);
    bounded(
        f.stage()
            .apply_response(&mut context(sender), f.response(60).await),
    )
    .await
    .unwrap();
    output.recv().await.unwrap();
    assert!(f.probe.waits.respond("first", event()));
    bounded(f.probe.entered.notified()).await;
    drop(output);
    bounded(async {
        while f.probe.dropped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(f.probe.waits.wait_count(), 0);
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_plugin_disable_cancels_pending_prompt() {
    let f = Fixture::new(Callback::Stop).await;
    let (sender, mut output) = mpsc::channel(1);
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let stage = f.stage();
    let response = f.response(60).await;
    let task =
        tokio::spawn(async move { stage.apply_response(&mut context(sender), response).await });
    wait_count(&f.probe.waits, 1).await;
    bounded(f.h.host.toggle(&f.h.plugin, false)).await.unwrap();
    bounded(task).await.unwrap().unwrap();
    assert_eq!(f.probe.waits.wait_count(), 0);
    assert!(matches!(output.try_recv(), Ok(TransportFrame::Text(text)) if text == "occupied"));
    assert!(output.try_recv().is_err());
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_chain_rechecks_capacity_before_next_prompt() {
    let f = Fixture::new(Callback::FillAndChain).await;
    let (sender, mut output) = mpsc::channel(1);
    bounded(
        f.stage()
            .apply_response(&mut context(sender), f.response(60).await),
    )
    .await
    .unwrap();
    output.recv().await.unwrap();
    assert!(f.probe.waits.respond("first", event()));
    bounded(f.probe.entered.notified()).await;
    wait_count(&f.probe.waits, 1024).await;
    bounded(async {
        while f.probe.dropped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(output.try_recv().is_err());
    f.probe.held.lock().unwrap().clear();
    wait_count(&f.probe.waits, 0).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_chained_pending_prompt_times_out_and_releases_registration() {
    let f = Fixture::new(Callback::Chain).await;
    let (sender, mut output) = mpsc::channel(1);
    bounded(
        f.stage()
            .apply_response(&mut context(sender.clone()), f.response(60).await),
    )
    .await
    .unwrap();
    output.recv().await.unwrap();
    sender
        .send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    assert!(f.probe.waits.respond("first", event()));
    bounded(f.probe.entered.notified()).await;
    wait_count(&f.probe.waits, 1).await;
    wait_count(&f.probe.waits, 0).await;
    assert!(matches!(output.try_recv(), Ok(TransportFrame::Text(text)) if text == "occupied"));
    assert!(output.try_recv().is_err());
    assert_all_task_slots_returned(&f.tasks).await;
    f.cleanup().await;
}

#[tokio::test]
async fn wait_reply_old_response_cannot_use_replacement_scope() {
    let f = Fixture::new(Callback::Stop).await;
    let response = f.response(60).await;
    let hosted = f.h.host.hosted_plugin(&f.h.plugin).await.unwrap();
    hosted.retire();
    let replacement = Arc::new(PluginTaskScope::new(&f.h.plugin));
    f.h.host
        .task_scopes
        .write()
        .await
        .insert(f.h.plugin.clone(), replacement.clone());
    let (sender, mut output) = mpsc::channel(1);
    bounded(f.stage().apply_response(&mut context(sender), response))
        .await
        .unwrap();
    assert_eq!(f.probe.waits.wait_count(), 0);
    assert!(output.try_recv().is_err());
    assert_all_task_slots_returned(&replacement).await;
    f.tasks.shutdown().await;
    f.cleanup().await;
}
