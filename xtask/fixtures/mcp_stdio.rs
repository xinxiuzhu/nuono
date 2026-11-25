//! 启动真实子进程验证 MCP stdio 通信，不依赖 Python、Node、数据库或外部模型。
use nuono_plugin::{
    PluginTaskScope, ToolRiskLevel,
    agent::{AgentContext, AgentToolContext},
    mcp::{McpOptions, McpTools},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    sync::Arc,
    time::Duration,
};

fn server(marker: &str) {
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        if request["method"] == "server/discover" {
            println!(
                "{}",
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"legacy server"}})
            );
            std::io::stdout().flush().unwrap();
            continue;
        }
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stdio-fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":[{"name":"echo","description":"Echo fixture input","inputSchema":{"type":"object"}}]})
            }
            "tools/call" => {
                json!({"content":[{"type":"text","text":"ok"}],"structuredContent":request["params"]["arguments"]})
            }
            other => panic!("unexpected method {other}"),
        };
        println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
        std::io::stdout().flush().unwrap();
    }
    std::fs::write(marker, b"closed").unwrap();
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--mcp-fixture") {
        server(&args[2]);
        return;
    }
    let marker = std::env::temp_dir().join(format!(
        "nuono-mcp-stdio-{}-{}.closed",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let scope = Arc::new(PluginTaskScope::new("stdio-fixture"));
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command.arg("--mcp-fixture").arg(&marker);
            let options = McpOptions::new("local", [("echo", ToolRiskLevel::ReadOnly)]);
            let source = McpTools::connect_stdio(scope.clone(), command, options)
                .await
                .unwrap();
            let ctx = AgentToolContext::new(
                AgentContext::from_runtime_event(1, &json!({"post_type":"message","user_id":42}))
                    .unwrap(),
            );
            let result = source.tools()[0]
                .call(ctx.clone(), json!({"query":"fixture"}))
                .await
                .unwrap();
            assert_eq!(result["structuredContent"], json!({"query":"fixture"}));
            tokio::time::timeout(Duration::from_secs(5), scope.shutdown())
                .await
                .unwrap();
            assert!(
                marker.exists(),
                "shutdown must wait for the child transport to exit"
            );
            assert!(source.tools()[0].call(ctx, json!({})).await.is_err());
        });
    std::fs::remove_file(marker).unwrap();
    println!("MCP stdio connection, call and shutdown passed");
}
