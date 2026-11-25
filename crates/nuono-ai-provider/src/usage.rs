//! 在 HTTP 传输层边界记录每次模型调用的 Token 用量，不影响原有的 Agent 上下文预算统计。
use nuono_ai::usage::{
    ReportedUsage, UsageContext, UsageEvent, UsageObserver, UsageOperation, UsageOutcome,
};
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct UsageTracking {
    pub observer: Arc<dyn UsageObserver>,
    pub context: UsageContext,
}

pub(crate) struct Attempt {
    tracking: Option<UsageTracking>,
    event: UsageEvent,
    started: Instant,
}
impl Attempt {
    pub(crate) fn new(
        tracking: Option<UsageTracking>,
        model: &str,
        operation: UsageOperation,
    ) -> Self {
        let context = tracking
            .as_ref()
            .map(|t| t.context.clone())
            .unwrap_or_default();
        Self {
            tracking,
            started: Instant::now(),
            event: UsageEvent {
                request_id: uuid::Uuid::new_v4().to_string(),
                started_at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
                elapsed_ms: 0,
                context,
                model: model.into(),
                operation,
                outcome: UsageOutcome::Cancelled,
                usage: ReportedUsage::default(),
            },
        }
    }
    pub(crate) fn report(&mut self, usage: ReportedUsage) {
        // 流式响应中的用量数据是累计快照，而非增量值，直接覆盖最新值即可。
        if usage.input_tokens.is_some() {
            self.event.usage.input_tokens = usage.input_tokens;
        }
        if usage.output_tokens.is_some() {
            self.event.usage.output_tokens = usage.output_tokens;
        }
        if usage.total_tokens.is_some() {
            self.event.usage.total_tokens = usage.total_tokens;
        }
        if usage.cached_input_tokens.is_some() {
            self.event.usage.cached_input_tokens = usage.cached_input_tokens;
        }
        if usage.reasoning_output_tokens.is_some() {
            self.event.usage.reasoning_output_tokens = usage.reasoning_output_tokens;
        }
    }
    pub(crate) fn finish(&mut self, outcome: UsageOutcome) {
        self.emit(outcome);
    }
    fn emit(&mut self, outcome: UsageOutcome) {
        if let Some(tracking) = self.tracking.take() {
            self.event.outcome = outcome;
            self.event.elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            tracking.observer.observe(self.event.clone());
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.emit(UsageOutcome::Cancelled);
    }
}

fn count(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= i64::MAX as u64)
}
pub(crate) fn chat_usage(value: &Value) -> ReportedUsage {
    let usage = &value["usage"];
    ReportedUsage {
        input_tokens: count(&usage["prompt_tokens"]),
        output_tokens: count(&usage["completion_tokens"]),
        total_tokens: count(&usage["total_tokens"]),
        cached_input_tokens: count(&usage["prompt_tokens_details"]["cached_tokens"])
            .or_else(|| count(&usage["prompt_cache_hit_tokens"])),
        reasoning_output_tokens: count(&usage["completion_tokens_details"]["reasoning_tokens"]),
    }
}
pub(crate) fn responses_usage(value: &Value) -> ReportedUsage {
    let usage = &value["usage"];
    ReportedUsage {
        input_tokens: count(&usage["input_tokens"]),
        output_tokens: count(&usage["output_tokens"]),
        total_tokens: count(&usage["total_tokens"]),
        cached_input_tokens: count(&usage["input_tokens_details"]["cached_tokens"]),
        reasoning_output_tokens: count(&usage["output_tokens_details"]["reasoning_tokens"]),
    }
}
pub(crate) fn embedding_usage(value: &Value) -> ReportedUsage {
    let usage = &value["usage"];
    ReportedUsage {
        input_tokens: count(&usage["prompt_tokens"]).or_else(|| count(&usage["input_tokens"])),
        total_tokens: count(&usage["total_tokens"]),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Capture(Mutex<Vec<UsageEvent>>);
    impl UsageObserver for Capture {
        fn observe(&self, event: UsageEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    #[test]
    fn cumulative_reports_merge_without_adding_and_finish_or_drop_emits_once() {
        let capture = Arc::new(Capture::default());
        let tracking = || {
            Some(UsageTracking {
                observer: capture.clone(),
                context: UsageContext::default(),
            })
        };
        let mut attempt = Attempt::new(tracking(), "model", UsageOperation::ChatStream);
        attempt.report(ReportedUsage {
            input_tokens: Some(10),
            total_tokens: Some(12),
            ..Default::default()
        });
        attempt.report(ReportedUsage {
            output_tokens: Some(3),
            total_tokens: Some(13),
            ..Default::default()
        });
        attempt.finish(UsageOutcome::Succeeded);
        drop(Attempt::new(tracking(), "model", UsageOperation::Chat));
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].usage.input_tokens, Some(10));
        assert_eq!(events[0].usage.output_tokens, Some(3));
        assert_eq!(events[0].usage.total_tokens, Some(13));
        assert_eq!(events[0].outcome, UsageOutcome::Succeeded);
        assert_eq!(events[1].outcome, UsageOutcome::Cancelled);
        assert!(!events[1].usage.is_reported());
        assert_ne!(events[0].request_id, events[1].request_id);
    }
    #[test]
    fn malformed_and_oversized_reports_are_unknown_while_real_zero_is_known() {
        let usage = chat_usage(
            &json!({"usage":{"prompt_tokens":-1,"completion_tokens":"4","total_tokens":u64::MAX}}),
        );
        assert!(!usage.is_reported());
        let zero = embedding_usage(&json!({"usage":{"prompt_tokens":0,"total_tokens":0}}));
        assert!(zero.is_reported());
        assert_eq!(zero.input_tokens, Some(0));
        assert_eq!(zero.output_tokens, None);
        let cached = chat_usage(&json!({"usage":{"prompt_cache_hit_tokens":20}}));
        assert_eq!(cached.cached_input_tokens, Some(20));
        assert_eq!(cached.input_tokens, None);
    }
}
