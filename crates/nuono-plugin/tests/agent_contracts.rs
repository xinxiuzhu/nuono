//! 验证 SDK 门面类型与应用层扩展接口定义、历史执行路径（包括 `#[nuono_tool]` 宏生成代码）之间的兼容性。
use nuono_app::agent::contracts;
use nuono_plugin::{JsonSchema, ToolContext};
use nuono_plugin::{NuonoResult, Value, agent, nuono_tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static TYPED_CALLS: AtomicUsize = AtomicUsize::new(0);
type RawArguments = Value;

#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "nuono_plugin::schemars")]
#[serde(deny_unknown_fields)]
struct SearchInput {
    filters: Vec<SearchFilter>,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 20))]
    limit: u32,
}
fn default_limit() -> u32 {
    5
}
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "nuono_plugin::schemars")]
struct SearchFilter {
    kind: SearchKind,
}
#[derive(Deserialize, JsonSchema)]
#[schemars(crate = "nuono_plugin::schemars")]
#[serde(rename_all = "snake_case")]
enum SearchKind {
    Character,
    Weapon,
}
#[derive(Serialize)]
struct SearchResult {
    user: String,
    count: usize,
    limit: u32,
}

#[nuono_tool(name = "contract.search", description = "Search plugin data")]
async fn search(ctx: ToolContext, input: SearchInput) -> NuonoResult<SearchResult> {
    TYPED_CALLS.fetch_add(1, Ordering::SeqCst);
    let count = input
        .filters
        .iter()
        .filter(|filter| matches!(filter.kind, SearchKind::Character))
        .count();
    Ok(SearchResult {
        user: ctx.actor.value().to_string(),
        count,
        limit: input.limit,
    })
}

#[nuono_tool(
    name = "contract.echo",
    description = "Echo the tool context",
    risk = "low"
)]
async fn contract_echo(
    ctx: contracts::AgentToolContext,
    input: RawArguments,
) -> NuonoResult<Value> {
    Ok(json!({
        "user": ctx.agent.user_id,
        "input": input,
        "has_store": ctx.db.is_some(),
        "has_memory_policy": ctx.memory_policy.is_some(),
    }))
}

fn context() -> agent::AgentContext {
    agent::AgentContext::from_runtime_event(
        7,
        &json!({"post_type": "message", "user_id": 42, "raw_message": "hello"}),
    )
    .unwrap()
}

#[tokio::test]
async fn typed_tools_publish_full_schemas_and_reject_invalid_input_before_business_code() {
    let registry = agent::ToolRegistry::new();
    registry.register(SearchTool).unwrap();
    let schema = &registry.chat_tools(agent::AutonomyLevel::LowRiskTools)[0].parameters_schema;
    assert!(schema["$defs"].is_object());
    assert!(schema["properties"]["filters"]["items"]["$ref"].is_string());
    assert_eq!(schema["properties"]["limit"]["maximum"], 20);
    for input in [
        json!({}),
        json!({"filters":[{"kind":"unknown"}]}),
        json!({"filters":[],"limit":99}),
        json!({"filters":[],"extra":true}),
    ] {
        let result = registry
            .execute(
                "contract.search",
                agent::AutonomyLevel::LowRiskTools,
                agent::AgentToolContext::new(context()),
                input,
            )
            .await;
        assert_eq!(result.status, agent::ToolExecutionStatus::Rejected);
    }
    assert_eq!(TYPED_CALLS.load(Ordering::SeqCst), 0);
    let result = registry
        .execute(
            "contract.search",
            agent::AutonomyLevel::LowRiskTools,
            agent::AgentToolContext::new(context()),
            json!({"filters":[{"kind":"character"}]}),
        )
        .await;
    assert_eq!(result.status, agent::ToolExecutionStatus::Succeeded);
    assert_eq!(result.output, json!({"user":"42","count":1,"limit":5}));
    assert_eq!(TYPED_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn macro_tools_share_contracts_with_sdk_and_legacy_registry_paths() {
    let extension: Arc<dyn contracts::AgentTool> = Arc::new(ContractEchoTool);
    let sdk_tool: Arc<dyn nuono_plugin::AgentTool> = extension;
    let legacy_tool: Arc<dyn agent::tool::AgentTool> = sdk_tool;
    let registry: agent::ToolRegistry = agent::tool::ToolRegistry::new();
    registry.register_arc(legacy_tool).unwrap();

    let tool_context: nuono_plugin::AgentToolContext = contracts::AgentToolContext::new(context())
        .with_memory_policy(agent::memory::MemoryPolicy::default());
    let receipt: contracts::ToolExecution = registry
        .execute(
            "contract.echo",
            agent::AutonomyLevel::LowRiskTools,
            tool_context,
            json!({"message": "ping"}),
        )
        .await;
    let sdk_receipt: nuono_plugin::ToolExecution = receipt;
    assert_eq!(sdk_receipt.status, agent::ToolExecutionStatus::Succeeded);
    assert_eq!(
        sdk_receipt.output,
        json!({
            "user": 42,
            "input": {"message": "ping"},
            "has_store": false,
            "has_memory_policy": true,
        })
    );

    // 确保公开字段与 with_store 构造函数对现有工具保持向后兼容。
    let legacy_context = agent::tool::AgentToolContext {
        agent: context(),
        db: None,
        memory_policy: None,
    };
    let _: contracts::AgentToolContext = legacy_context;
    let _: fn(
        agent::AgentContext,
        Arc<dyn agent::store::AgentStore>,
    ) -> contracts::AgentToolContext = agent::AgentToolContext::with_store;
}

#[test]
fn execution_receipts_keep_their_persisted_shape_and_media_validation() {
    let legacy = json!({
        "tool_name": "contract.echo",
        "risk_level": "LowRisk",
        "status": "Succeeded",
        "input": {},
        "output": {"ok": true},
        "elapsed_ms": 3,
    });
    let receipt: agent::tool::ToolExecution = serde_json::from_value(legacy.clone()).unwrap();
    let current: contracts::ToolExecution = receipt;
    assert!(current.images.is_empty());
    assert_eq!(serde_json::to_value(current).unwrap(), legacy);

    let mut with_media = legacy;
    with_media["images"] = json!(["https://example.com/card.png"]);
    let receipt: nuono_plugin::ToolExecution = serde_json::from_value(with_media.clone()).unwrap();
    let image: contracts::AgentToolImage = receipt.images[0].clone();
    let output: agent::tool::AgentToolOutput = contracts::AgentToolOutput::new(json!({}))
        .with_images(vec![image])
        .unwrap();
    assert_eq!(output.images.len(), 1);

    with_media["images"] = json!(vec![
        "https://example.com/card.png";
        agent::MAX_TOOL_IMAGES + 1
    ]);
    assert!(serde_json::from_value::<contracts::ToolExecution>(with_media).is_err());
}
