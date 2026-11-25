use super::*;
use crate::agent::{AgentContext, AutonomyLevel, ToolExecutionStatus, ToolRegistry};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn context() -> AgentToolContext {
    AgentToolContext::new(
        AgentContext::from_runtime_event(1, &json!({"post_type":"message","user_id":42})).unwrap(),
    )
}

#[tokio::test]
async fn mcp_tools_share_schema_risk_and_shutdown_gates() {
    let (client, server) = tokio::io::duplex(65536);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server_task = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            let Some(id) = request.get("id") else {
                continue;
            };
            let result = match request["method"].as_str().unwrap() {
                "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},
                    "serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" => {
                    let (name, next) = if request["params"]["cursor"] == "next" {
                        ("write", Value::Null)
                    } else {
                        ("echo", json!("next"))
                    };
                    let mut result = json!({"tools":[{"name":name,"description":"Fixture MCP tool",
                        "inputSchema":{"type":"object","required":["kind"],"additionalProperties":false,
                            "properties":{"kind":{"enum":["role","weapon"]}}},
                        "annotations":{"readOnlyHint":true}}]});
                    if !next.is_null() {
                        result["nextCursor"] = next;
                    }
                    result
                }
                "tools/call" => {
                    count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request["params"]["arguments"], json!({"kind":"role"}));
                    // 调用方身份与会话上下文绝不会发送给远端 MCP 服务器。
                    assert!(request["params"].get("context").is_none());
                    json!({"content":[{"type":"text","text":"role data"}],
                        "structuredContent":{"found":true},"isError":false})
                }
                method => panic!("unexpected MCP request: {method}"),
            };
            let mut response =
                serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap();
            response.push(b'\n');
            write.write_all(&response).await.unwrap();
        }
    });
    let scope = Arc::new(PluginTaskScope::new("fixture"));
    let mut options = McpOptions::new(
        "catalog",
        [
            ("echo", ToolRiskLevel::ReadOnly),
            ("write", ToolRiskLevel::RequiresApproval),
        ],
    );
    options.lifecycle = ClientLifecycleMode::Initialize;
    let source = McpTools::connect(scope.clone(), client, options)
        .await
        .unwrap();
    let registry = ToolRegistry::new();
    for tool in source.tools() {
        registry.register_arc(tool).unwrap();
    }
    let result = registry
        .execute(
            "mcp.catalog.echo",
            AutonomyLevel::LowRiskTools,
            context(),
            json!({"kind":"invalid"}),
        )
        .await;
    assert_eq!(result.status, ToolExecutionStatus::Rejected);
    let result = registry
        .execute(
            "mcp.catalog.write",
            AutonomyLevel::LowRiskTools,
            context(),
            json!({"kind":"role"}),
        )
        .await;
    assert_eq!(result.status, ToolExecutionStatus::Rejected);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = registry
        .execute(
            "mcp.catalog.echo",
            AutonomyLevel::LowRiskTools,
            context(),
            json!({"kind":"role"}),
        )
        .await;
    assert_eq!(result.status, ToolExecutionStatus::Succeeded);
    assert_eq!(result.output["structuredContent"]["found"], true);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // 即便外部仍持有工具句柄，在所属插件停用后也必须拒绝继续调用。
    tokio::time::timeout(Duration::from_secs(2), scope.shutdown())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap();
    let result = registry
        .execute(
            "mcp.catalog.echo",
            AutonomyLevel::LowRiskTools,
            context(),
            json!({"kind":"role"}),
        )
        .await;
    assert_eq!(result.status, ToolExecutionStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn mcp_errors_and_visuals_keep_their_semantics() {
    let failed = rmcp::model::CallToolResult::error(vec![ContentBlock::text("business failure")]);
    assert!(
        convert_result(failed)
            .unwrap_err()
            .to_string()
            .contains("business failure")
    );
    let output = convert_result(rmcp::model::CallToolResult::success(vec![
        ContentBlock::text("card"),
        ContentBlock::image("iVBORw0KGgo=", "image/png"),
    ]))
    .unwrap();
    assert_eq!(output.images.len(), 1);
    assert!(!output.value.to_string().contains("iVBOR"));
}

#[tokio::test]
async fn http_transport_uses_configured_credentials_and_preserves_tool_results() {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    async fn endpoint(headers: HeaderMap, Json(request): Json<Value>) -> axum::response::Response {
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer fixture-token"
        );
        let Some(id) = request.get("id") else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match request["method"].as_str().unwrap() {
            "server/discover" => serde_json::to_value(rmcp::model::DiscoverResult::new(
                vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                serde_json::from_value(json!({"tools":{}})).unwrap(),
            ))
            .unwrap(),
            "tools/list" => {
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"lookup","description":"Lookup a fixture",
                "inputSchema":{"type":"object","required":["id"],"properties":{"id":{"type":"integer"}}}}]})
            }
            "tools/call" => {
                json!({"resultType":"complete","content":[{"type":"text","text":"found"}],"structuredContent":request["params"]["arguments"]})
            }
            other => panic!("unexpected method {other}"),
        };
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/mcp", post(endpoint).delete(|| async { StatusCode::OK })),
        )
        .await
        .unwrap();
    });
    let scope = Arc::new(PluginTaskScope::new("http-fixture"));
    let options = McpOptions::new("http", [("lookup", ToolRiskLevel::ReadOnly)]);
    let source = McpTools::connect_http(
        scope.clone(),
        &format!("http://{address}/mcp"),
        Some("fixture-token".into()),
        options,
    )
    .await
    .unwrap();
    let output = source.tools()[0]
        .call(context(), json!({"id":7}))
        .await
        .unwrap();
    assert_eq!(output["structuredContent"], json!({"id":7}));
    scope.shutdown().await;
    server.abort();
    let _ = server.await;
}
