use super::*;
use crate::agent::{AutonomyLevel, ToolExecutionStatus};
use crate::config::{ConfigManager, NuonoConfig};

struct RuntimeToolPlugin;

#[async_trait::async_trait]
impl Plugin for RuntimeToolPlugin {
    fn name(&self) -> &str {
        "RuntimeTools"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "shared runtime tools fixture"
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(EchoTool)]
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Return the supplied value"
    }
    async fn call(&self, _: AgentToolContext, input: Value) -> Result<Value> {
        Ok(input)
    }
}

#[tokio::test]
async fn event_and_approval_runtimes_follow_plugin_tool_registration_and_removal() {
    let h = Harness::new("runtime-tools").await;
    let mut settings = NuonoConfig::default();
    settings.ai.enabled = true;
    settings.ai.reply_enabled = true;
    settings.ai.agent_autonomy = "low_risk_tools".into();
    let config = Arc::new(ConfigManager::for_test(settings, h.root.join("nuono.toml")));
    let service = h.host.tool_execution_service(config.clone());
    let runtime = h.host.bind_runtime(h.db.clone(), config.clone());
    let tool_name = "plugin.RuntimeTools.echo";
    assert!(runtime.tool_registry().get(tool_name).is_none());

    // 在两个消费方都已创建后再注册工具，模拟运行期动态启用插件的场景。
    let plugin = LoadedPlugin {
        plugin: Box::new(RuntimeToolPlugin),
    };
    h.host.register_scoped_plugin_tools(&plugin).await.unwrap();
    assert!(runtime.tool_registry().get(tool_name).is_some());
    let rebound = h.host.bind_runtime(h.db.clone(), config);
    assert!(rebound.tool_registry().get(tool_name).is_some());

    let context = AgentContext::from_runtime_event(
        1,
        &serde_json::json!({"post_type":"message", "user_id":42}),
    )
    .unwrap();
    let input = serde_json::json!({"value":"shared registration"});
    let result = service
        .execute(
            context.clone(),
            AutonomyLevel::LowRiskTools,
            tool_name,
            input.clone(),
            "runtime assembly regression",
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolExecutionStatus::Succeeded);
    assert_eq!(result.output, input);

    assert_eq!(h.host.unregister_plugin_tools(plugin.name()), 1);
    assert!(runtime.tool_registry().get(tool_name).is_none());
    assert!(rebound.tool_registry().get(tool_name).is_none());
    let removed = service
        .execute(
            context,
            AutonomyLevel::LowRiskTools,
            tool_name,
            input,
            "removed plugin must not execute",
        )
        .await
        .unwrap();
    assert_eq!(removed.status, ToolExecutionStatus::Failed);
    assert_eq!(removed.output["error"], "tool not found");
    drop((service, runtime, rebound));
    h.cleanup().await;
}
