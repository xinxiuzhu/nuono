use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct Output {
    texts: Mutex<Vec<String>>,
    controls: Mutex<Vec<Value>>,
    reject_open: bool,
    fail_update: bool,
    slow_update: bool,
}
#[async_trait::async_trait]
impl ReplyOutput for Output {
    async fn text(&self, text: String) -> bool {
        self.texts.lock().unwrap().push(text);
        true
    }
    fn native_request(&self) -> Option<Value> {
        Some(json!({"target":"synthetic"}))
    }
    async fn control(&self, params: Value) -> Result<Value> {
        self.controls.lock().unwrap().push(params.clone());
        if self.slow_update && params["operation"] == "update" {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        if (self.reject_open && params["operation"] == "open")
            || (self.fail_update && params["operation"] == "update")
        {
            return Err(NuonoError::MessageSend(
                "synthetic transport failure".into(),
            ));
        }
        Ok(json!({"protocol":1,"stream_id":params["stream_id"]}))
    }
}
fn task(
    tx: watch::Sender<OutputProgress>,
    text: &str,
    delay: Duration,
    complete: bool,
) -> AgentRunTask {
    let text = text.to_owned();
    Box::pin(async move {
        tx.send_replace(OutputProgress {
            content: text.clone(),
            complete,
        });
        tokio::time::sleep(delay).await;
        Ok(nuono_app::agent::run::AgentRunReply {
            run_id: "run".into(),
            context: nuono_app::agent::AgentContext::from_runtime_event(
                1,
                &json!({"post_type":"message","user_id":2,"message":"hi"}),
            )
            .unwrap(),
            content: if complete {
                text
            } else {
                "模型调用中断".into()
            },
        })
    })
}
fn policy(mode: OutputMode) -> OutputPolicy {
    OutputPolicy {
        mode,
        interval_ms: 300,
        segment_chars: 80,
    }
}

#[tokio::test]
async fn whitespace_only_prefix_waits_for_visible_text_before_opening_a_native_stream() {
    let output = Output::default();
    let mut delivery = Delivery::new(&output, policy(OutputMode::Native));
    delivery.flush(" \n", false).await.unwrap();
    assert!(output.controls.lock().unwrap().is_empty());
    delivery.flush(" \n答案", false).await.unwrap();
    assert_eq!(
        output.controls.lock().unwrap().last().unwrap()["content"],
        " \n答案"
    );
}

#[tokio::test(start_paused = true)]
async fn segmented_output_does_not_send_blank_prefix_or_trailing_whitespace_messages() {
    let output = Output::default();
    let (tx, rx) = watch::channel(Default::default());
    let text = format!("{}{} \n", " ".repeat(80), "诺".repeat(80));
    deliver_to(
        &output,
        task(tx, &text, Duration::from_secs(1), true),
        rx,
        policy(OutputMode::Segmented),
    )
    .await;
    assert_eq!(*output.texts.lock().unwrap(), ["诺".repeat(80)]);
}

#[tokio::test(start_paused = true)]
async fn unsupported_native_open_buffers_once_and_never_sends_updates() {
    let output = Output {
        reject_open: true,
        ..Default::default()
    };
    let (tx, rx) = watch::channel(Default::default());
    deliver_to(
        &output,
        task(tx, "最终答案", Duration::from_secs(1), true),
        rx,
        policy(OutputMode::Native),
    )
    .await;
    assert_eq!(*output.texts.lock().unwrap(), ["最终答案"]);
    assert!(
        output
            .controls
            .lock()
            .unwrap()
            .iter()
            .all(|value| value["operation"] != "update")
    );
}

#[tokio::test(start_paused = true)]
async fn native_completion_uses_one_handle_and_does_not_repeat_the_body_as_text() {
    let output = Output::default();
    let (tx, rx) = watch::channel(Default::default());
    deliver_to(
        &output,
        task(tx, "最终答案", Duration::from_secs(1), true),
        rx,
        policy(OutputMode::Native),
    )
    .await;
    assert!(output.texts.lock().unwrap().is_empty());
    let calls = output.controls.lock().unwrap();
    assert_eq!(calls.iter().filter(|v| v["operation"] == "open").count(), 1);
    assert_eq!(calls.last().unwrap()["operation"], "complete");
    assert!(
        calls
            .iter()
            .all(|v| v["stream_id"] == calls[0]["stream_id"])
    );
}

#[tokio::test(start_paused = true)]
async fn uncertain_stream_send_does_not_fall_back_to_replaying_the_answer() {
    let output = Output {
        fail_update: true,
        ..Default::default()
    };
    let (tx, rx) = watch::channel(Default::default());
    deliver_to(
        &output,
        task(tx, "不能重复的答案", Duration::from_secs(1), true),
        rx,
        policy(OutputMode::Native),
    )
    .await;
    assert_eq!(
        *output.texts.lock().unwrap(),
        ["AI 回复投递中断，正文未自动重发。"]
    );
    assert!(
        output
            .controls
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["operation"] != "complete")
    );
}

#[tokio::test(start_paused = true)]
async fn segmented_delivery_preserves_unicode_without_replaying_prefixes() {
    let output = Output::default();
    let (tx, rx) = watch::channel(Default::default());
    let text = "诺🙂".repeat(90);
    deliver_to(
        &output,
        task(tx, &text, Duration::from_secs(1), true),
        rx,
        policy(OutputMode::Segmented),
    )
    .await;
    let texts = output.texts.lock().unwrap();
    assert_eq!(texts.concat(), text);
    assert_eq!(texts.len(), 3);
    assert!(texts.iter().all(|text| text.chars().count() <= 80));
    assert!(output.controls.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn truncated_model_output_has_no_successful_done_frame() {
    let output = Output::default();
    let (tx, rx) = watch::channel(Default::default());
    deliver_to(
        &output,
        task(tx, "部分内容", Duration::from_secs(1), false),
        rx,
        policy(OutputMode::Native),
    )
    .await;
    assert_eq!(*output.texts.lock().unwrap(), ["模型调用中断"]);
    assert!(
        output
            .controls
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["operation"] != "complete")
    );
}

#[tokio::test(start_paused = true)]
async fn failed_run_interrupts_a_slow_platform_wait_without_completing_its_partial_text() {
    let output = Output {
        slow_update: true,
        ..Default::default()
    };
    let (tx, rx) = watch::channel(Default::default());
    let started = Instant::now();
    deliver_to(
        &output,
        task(tx, "部分内容", Duration::from_secs(1), false),
        rx,
        policy(OutputMode::Native),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(*output.texts.lock().unwrap(), ["模型调用中断"]);
    assert!(
        output
            .controls
            .lock()
            .unwrap()
            .iter()
            .all(|value| value["operation"] != "complete")
    );
}
