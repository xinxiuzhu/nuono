//! 宿主侧 Agent 回复投递实现；平台网络 I/O 期间不阻塞模型推理任务。
use crate::{
    core::event_kernel::RuntimeEventContext,
    error::{NuonoError, Result},
};
use nuono_app::agent::{
    AgentContext,
    output::{OutputMode, OutputPolicy, OutputProgress},
    run::AgentRunTask,
};
use serde_json::{Value, json};
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

const MAX_SEGMENTS: usize = 32;

pub(crate) fn supports_native(ctx: &AgentContext) -> bool {
    ctx.native.as_ref().is_some_and(|native| {
        native.bot.platform() == &nuono_domain::Platform::QqOfficial
            && matches!(
                native.conversation,
                Some(nuono_domain::ConversationRef::Direct { .. })
            )
    }) && ctx.message_id.as_ref().is_some_and(|id| !id.is_empty())
}

#[async_trait::async_trait]
trait ReplyOutput: Sync {
    async fn text(&self, text: String) -> bool;
    async fn control(&self, params: Value) -> Result<Value>;
    fn native_request(&self) -> Option<Value>;
}

#[async_trait::async_trait]
impl ReplyOutput for RuntimeEventContext<'_> {
    async fn text(&self, text: String) -> bool {
        self.send_reply_text(text, "AgentRuntime").await
    }
    async fn control(&self, params: Value) -> Result<Value> {
        if let (Some(transport), Some(event)) = (&self.action_transport, &self.domain_event) {
            return transport
                .call_raw("text_stream", params, Some(&event.bot))
                .await
                .map_err(|error| NuonoError::MessageSend(error.to_string()));
        }
        if let Some(sink) = &self.action_sink {
            return sink
                .call_action(json!({"action":"text_stream","params":params}))
                .await;
        }
        Err(NuonoError::NotImplemented(
            "当前连接不支持原生流式回复".into(),
        ))
    }
    fn native_request(&self) -> Option<Value> {
        let event = self.domain_event.as_ref()?;
        let ctx = AgentContext::from_domain_event(event)?;
        if !supports_native(&ctx) {
            return None;
        }
        Some(
            json!({"target":{"type":"conversation","conversation":ctx.native?.conversation?},"message_id":ctx.message_id?}),
        )
    }
}

struct Delivery<'a, T> {
    output: &'a T,
    policy: OutputPolicy,
    native: Option<Value>,
    opened: bool,
    failed: bool,
    sent: String,
    segments: usize,
    next_send: Instant,
}
impl<'a, T: ReplyOutput> Delivery<'a, T> {
    fn new(output: &'a T, mut policy: OutputPolicy) -> Self {
        let native = output.native_request().map(|mut value| {
            value["stream_id"] = json!(uuid::Uuid::new_v4().simple().to_string());
            value["interval_ms"] = json!(policy.interval_ms);
            value["protocol"] = json!(1);
            value
        });
        if policy.mode == OutputMode::Native && native.is_none() {
            policy.mode = OutputMode::Complete;
        }
        Self {
            output,
            policy,
            native,
            opened: false,
            failed: false,
            sent: String::new(),
            segments: 0,
            next_send: Instant::now(),
        }
    }
    async fn command(&self, operation: &str, content: Option<&str>) -> Result<Value> {
        let mut params = self
            .native
            .clone()
            .ok_or_else(|| NuonoError::NotImplemented("原生流式不可用".into()))?;
        params["operation"] = json!(operation);
        if let Some(content) = content {
            params["content"] = json!(content);
        }
        self.output.control(params).await
    }
    async fn open(&mut self) -> bool {
        if self.opened {
            return true;
        }
        // open 指令仅用于本地探测适配器是否支持流式输出，不会实际向平台发消息。
        match self.command("open", None).await {
            Ok(value)
                if value["protocol"] == 1
                    && self
                        .native
                        .as_ref()
                        .is_some_and(|params| value["stream_id"] == params["stream_id"]) =>
            {
                self.opened = true;
                true
            }
            result => {
                tracing::warn!(error=?result.err(), "原生流式不可用，使用完整回复");
                let _ = self.command("cancel", None).await;
                self.policy.mode = OutputMode::Complete;
                false
            }
        }
    }
    async fn flush(&mut self, content: &str, final_segment: bool) -> Result<()> {
        if content.trim().is_empty() || self.failed {
            return Ok(());
        }
        if !content.starts_with(&self.sent) {
            return Err(NuonoError::MessageSend(
                "流式输出不能修改已发送的前缀".into(),
            ));
        }
        match self.policy.mode {
            OutputMode::Complete => {}
            OutputMode::Native => {
                if content != self.sent && self.open().await {
                    self.command("update", Some(content)).await?;
                    self.sent = content.into();
                }
            }
            OutputMode::Segmented => {
                let remaining = &content[self.sent.len()..];
                if remaining.trim().is_empty() {
                    if final_segment {
                        self.sent.push_str(remaining);
                    }
                    return Ok(());
                }
                let end = remaining
                    .char_indices()
                    .nth(self.policy.segment_chars)
                    .map(|(index, _)| index);
                let end = match end {
                    Some(end) => end,
                    None if final_segment => remaining.len(),
                    _ => return Ok(()),
                };
                if end == 0 {
                    return Ok(());
                }
                // 纯空白片段不单独发往平台，仅推进源文本游标并继续等待下一段可见内容。
                if remaining[..end].trim().is_empty() {
                    self.sent.push_str(&remaining[..end]);
                    return Ok(());
                }
                if self.segments >= MAX_SEGMENTS {
                    return Err(NuonoError::MessageSend("AI 回复超过 32 段投递上限".into()));
                }
                tokio::time::sleep_until(self.next_send).await;
                if !self.output.text(remaining[..end].into()).await {
                    return Err(NuonoError::MessageSend("AI 分段回复投递失败".into()));
                }
                self.sent.push_str(&remaining[..end]);
                self.segments += 1;
                self.next_send = Instant::now() + Duration::from_millis(self.policy.interval_ms);
            }
        }
        Ok(())
    }
    async fn interrupted(&mut self, error: &NuonoError) {
        self.failed = true;
        tracing::warn!(%error, "AI 输出投递中断，不自动重放正文");
        if self.opened {
            let _ = self.command("cancel", None).await;
        }
    }
}

pub(crate) async fn deliver(
    context: RuntimeEventContext<'_>,
    task: AgentRunTask,
    progress: watch::Receiver<OutputProgress>,
    policy: OutputPolicy,
) {
    if tokio::time::timeout(
        Duration::from_secs(300),
        deliver_to(&context, task, progress, policy),
    )
    .await
    .is_err()
    {
        tracing::warn!("AI 回复生成与投递总时限已到，不自动重放正文");
        context
            .send_reply_text("AI 回复超时，正文未自动重发。".into(), "AgentRuntime")
            .await;
    }
}

async fn deliver_to<T: ReplyOutput>(
    output: &T,
    task: AgentRunTask,
    progress: watch::Receiver<OutputProgress>,
    policy: OutputPolicy,
) {
    let mut delivery = Delivery::new(output, policy);
    let (done, mut finished) = watch::channel(None);
    let compute = async {
        let result = task.await;
        let keep_stream = result.as_ref().is_ok_and(|reply| {
            let snapshot = progress.borrow();
            snapshot.complete && snapshot.content == reply.content
        });
        done.send_replace(Some(keep_stream));
        result
    };
    let consume = async {
        let mut interval =
            tokio::time::interval(Duration::from_millis(delivery.policy.interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if finished.borrow().is_some() {
                break;
            }
            tokio::select! {
                biased;
                _ = finished.changed() => break,
                _ = interval.tick() => {
                    let snapshot = progress.borrow().clone();
                    // 推理被取消或失败时不再等待缓慢的平台发送调用；
                    // 若推理正常完成，则仍需等待在途消息发送确认（ACK）。
                    let result=tokio::select! {
                        biased;
                        _=finished.wait_for(|done|*done==Some(false))=>None,
                        result=delivery.flush(&snapshot.content, false)=>Some(result),
                    };
                    match result {
                        None=>break,
                        Some(Err(error))=>delivery.interrupted(&error).await,
                        Some(Ok(()))=>{},
                    }
                }
            }
        }
    };
    let (result, ()) = tokio::join!(compute, consume);
    let snapshot = progress.borrow().clone();
    let final_content = match result {
        Ok(reply) => reply.content,
        Err(error) => {
            tracing::warn!(%error, "Background Agent run failed");
            format!("Agent 任务失败：{error}")
        }
    };
    if delivery.failed {
        output
            .text("AI 回复投递中断，正文未自动重发。".into())
            .await;
        return;
    }
    if snapshot.complete
        && final_content == snapshot.content
        && delivery.policy.mode != OutputMode::Complete
    {
        let result = match delivery.policy.mode {
            OutputMode::Native => {
                if delivery.open().await {
                    delivery
                        .command("complete", Some(&final_content))
                        .await
                        .map(|_| ())
                } else {
                    output.text(final_content).await;
                    return;
                }
            }
            OutputMode::Segmented => {
                async {
                    while delivery.sent.len() < final_content.len() {
                        delivery.flush(&final_content, true).await?;
                    }
                    Ok(())
                }
                .await
            }
            OutputMode::Complete => unreachable!("complete delivery handled below"),
        };
        if let Err(error) = result {
            delivery.interrupted(&error).await;
            output
                .text("AI 回复投递中断，正文未自动重发。".into())
                .await;
        }
    } else {
        // 首轮推理被取消、失败或进入后续工具轮次时，
        // 绝不能重复发送首轮流式输出中已经发出的内容。
        if delivery.opened {
            let _ = delivery.command("cancel", None).await;
        }
        if delivery.sent != final_content {
            output.text(final_content).await;
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/agent/output.rs"]
mod tests;
