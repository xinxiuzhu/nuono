use super::*;
use serde_json::json;

struct StructuredTool;
#[async_trait::async_trait]
impl AgentTool for StructuredTool {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Look up a scoped item"
    }
    fn input_schema(&self) -> Option<Value> {
        Some(json!({"type":"object","required":["kind"],
            "properties":{"kind":{"enum":["role","weapon"]}}}))
    }
    async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
        Ok(input)
    }
}

#[tokio::test]
async fn plugin_namespace_and_diagnostics_wrappers_preserve_complete_schema() {
    let registry = ToolRegistry::new();
    let metrics = super::super::diagnostics::DiagnosticsRegistry::default().for_plugin("fixture");
    let scoped = scoped_tools("fixture", vec![Arc::new(StructuredTool)], vec![]).unwrap();
    for tool in scoped {
        registry
            .register_arc(super::super::diagnostics::observe_tool(
                tool,
                metrics.clone(),
                None,
            ))
            .unwrap();
    }
    let tools = registry.chat_tools(crate::agent::AutonomyLevel::LowRiskTools);
    assert_eq!(tools[0].name, "plugin.fixture.lookup");
    assert_eq!(
        tools[0].parameters_schema["properties"]["kind"]["enum"],
        json!(["role", "weapon"])
    );
    let ctx = AgentToolContext::new(
        AgentContext::from_runtime_event(1, &json!({"post_type":"message","user_id":42})).unwrap(),
    );
    let receipt = registry
        .execute(
            "plugin.fixture.lookup",
            crate::agent::AutonomyLevel::LowRiskTools,
            ctx,
            json!({"kind":"other"}),
        )
        .await;
    assert_eq!(receipt.status, crate::agent::ToolExecutionStatus::Rejected);
}
