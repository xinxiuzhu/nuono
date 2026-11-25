use nuono_app::pipeline::{
    ProcessingState, RuntimeEventPipeline, RuntimeEventStage, StageContext, StageExecutor,
    StageFailure, process_event,
};
use std::sync::Mutex;

struct Context(ProcessingState);
impl StageContext for Context {
    fn processing_state(&self) -> ProcessingState {
        self.0
    }
}

#[derive(Default)]
struct Stages {
    calls: Mutex<Vec<RuntimeEventStage>>,
    consume_at: Option<RuntimeEventStage>,
    fail_at: Option<RuntimeEventStage>,
}

#[async_trait::async_trait]
impl StageExecutor<Context> for Stages {
    async fn execute_stage(
        &self,
        stage: RuntimeEventStage,
        ctx: &mut Context,
    ) -> Result<(), StageFailure> {
        self.calls.lock().unwrap().push(stage);
        if self.consume_at == Some(stage) {
            ctx.0.consumed = true;
        }
        if self.fail_at == Some(stage) {
            return Err(StageFailure::new("PluginExecution"));
        }
        Ok(())
    }
}

fn message() -> Context {
    Context(ProcessingState {
        is_message: true,
        ..Default::default()
    })
}

#[tokio::test]
async fn wait_session_consumes_before_plugins_and_agent() {
    let stages = Stages {
        consume_at: Some(RuntimeEventStage::WaitSession),
        ..Default::default()
    };
    let mut ctx = message();
    let result = process_event(
        &stages,
        &RuntimeEventPipeline::default(),
        &mut ctx,
        |_, _| {},
    )
    .await;
    assert!(result.blocked);
    assert_eq!(
        *stages.calls.lock().unwrap(),
        vec![RuntimeEventStage::WaitSession]
    );
}

#[tokio::test]
async fn failure_without_consumption_allows_agent_fallback() {
    let stages = Stages {
        fail_at: Some(RuntimeEventStage::PluginHost),
        ..Default::default()
    };
    let mut ctx = message();
    let mut failures = Vec::new();
    let result = process_event(
        &stages,
        &RuntimeEventPipeline::default(),
        &mut ctx,
        |stage, failure| {
            if let Some(err) = failure {
                failures.push((stage, err.category.clone()));
            }
        },
    )
    .await;
    assert!(!result.blocked);
    assert_eq!(stages.calls.lock().unwrap().len(), 8);
    assert_eq!(
        failures,
        vec![(RuntimeEventStage::PluginHost, "PluginExecution".to_string())]
    );
}

#[tokio::test]
async fn failure_after_consumption_does_not_release_the_event() {
    let stages = Stages {
        consume_at: Some(RuntimeEventStage::PluginHost),
        fail_at: Some(RuntimeEventStage::PluginHost),
        ..Default::default()
    };
    let mut ctx = message();
    let result = process_event(
        &stages,
        &RuntimeEventPipeline::default(),
        &mut ctx,
        |_, _| {},
    )
    .await;
    assert!(result.blocked);
    assert_eq!(stages.calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn non_message_events_only_enter_wait_session() {
    let stages = Stages::default();
    let mut ctx = Context(ProcessingState::default());
    let result = process_event(
        &stages,
        &RuntimeEventPipeline::default(),
        &mut ctx,
        |_, _| {},
    )
    .await;
    assert!(!result.is_message);
    assert_eq!(
        *stages.calls.lock().unwrap(),
        vec![RuntimeEventStage::WaitSession]
    );
}
