use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 生成的测试工程位于框架 workspace 之外的临时目录中，因此必须向 Cargo 传递 target 目录的绝对路径。
pub(crate) fn cargo_target(root: &Path) -> Result<PathBuf> {
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    fs::create_dir_all(&target)?;
    target
        .canonicalize()
        .with_context(|| format!("resolve Cargo target directory {}", target.display()))
}

pub(crate) fn copy_tree(source: &Path, destination: &Path, ignored: &[&str]) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if ignored
            .iter()
            .any(|name| entry.file_name() == OsStr::new(name))
        {
            continue;
        }
        let path = entry.path();
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target, ignored)?;
        } else {
            fs::copy(&path, &target)
                .with_context(|| format!("copy {} to {}", path.display(), target.display()))?;
        }
    }
    Ok(())
}

fn render_templates(
    source: &Path,
    destination: &Path,
    substitutions: &[(&str, String)],
) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let path = entry?.path();
        let target = destination.join(path.file_name().context("template filename missing")?);
        if path.is_dir() {
            render_templates(&path, &target, substitutions)?;
        } else if path.extension() == Some(OsStr::new("template")) {
            let mut content = fs::read_to_string(&path)?;
            for (placeholder, replacement) in substitutions {
                content = content.replace(placeholder, replacement);
            }
            fs::write(target.with_extension(""), content)?;
        }
    }
    Ok(())
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn offline(command: &mut Command) {
    if env::var_os("NUONO_CARGO_OFFLINE").is_some_and(|value| !value.is_empty()) {
        command.arg("--offline");
    }
}

fn cargo(root: &Path, target: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(root).env("CARGO_TARGET_DIR", target);
    if env::var_os("CARGO_BUILD_JOBS").is_none() {
        command.env("CARGO_BUILD_JOBS", "1");
    }
    command
}

fn execute(command: &mut Command, capture: bool) -> Result<String> {
    println!("+ {command:?}");
    if capture {
        let output = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("execute {command:?}"))?;
        ensure!(
            output.status.success(),
            "command failed ({})",
            output.status
        );
        String::from_utf8(output.stdout).context("Cargo output is not UTF-8")
    } else {
        let status = command
            .status()
            .with_context(|| format!("execute {command:?}"))?;
        ensure!(status.success(), "command failed ({status})");
        Ok(String::new())
    }
}

fn forbidden_dependencies(tree: &str, forbid_test_sdk: bool) -> BTreeSet<&str> {
    tree.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| {
            matches!(
                *name,
                "nuono-core"
                    | "nuono-server"
                    | "nuono-tooling"
                    | "nuono-storage"
                    | "nuono-ai-provider"
                    | "sea-query-sqlx"
                    | "sqlx"
                    | "axum"
                    | "libloading"
            ) || name.starts_with("sqlx-")
                || name.starts_with("axum-")
                || (forbid_test_sdk && *name == "nuono-plugin-test")
        })
        .collect()
}

fn browser_backend_dependencies(tree: &str) -> BTreeSet<&str> {
    tree.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| matches!(*name, "obscura-browser" | "obscura-js" | "v8"))
        .collect()
}

pub fn templates(root: &Path) -> Result<()> {
    let target = cargo_target(root)?;
    let directory = tempfile::Builder::new()
        .prefix("nuono-source-templates-")
        .tempdir()?;
    for name in ["verified-plugin", "other-plugin"] {
        let plugin = directory.path().join(name);
        render_templates(
            &root.join("crates/nuono-tooling/templates/plugin"),
            &plugin,
            &[
                ("{{package_name}}", name.into()),
                (
                    "../../crates/nuono-plugin-test",
                    portable_path(&root.join("crates/nuono-plugin-test")),
                ),
                (
                    "../../crates/nuono-plugin",
                    portable_path(&root.join("crates/nuono-plugin")),
                ),
            ],
        )?;
        fs::copy(root.join("Cargo.lock"), plugin.join("Cargo.lock"))?;
        // 在独立工作区中直接使用 SDK 重导出的类型（包括旧版 embedding 路径），无需直接添加 AI/RAG 依赖。
        fs::create_dir_all(plugin.join("tests"))?;
        fs::write(
            plugin.join("tests/lifecycle_contracts.rs"),
            r#"use nuono_plugin::prelude::*;
use std::sync::Arc;

#[derive(Default)]
struct LifecyclePlugin { runtime: Arc<PluginRuntime> }

#[nuono_plugin(name = "lifecycle-contract", state_fields = "runtime")]
impl Plugin for LifecyclePlugin {
    fn migrations(&self) -> Vec<PluginMigrationForBackend> {
        vec![PluginMigrationForBackend {
            version: 1, description: "notes".into(),
            pg_sql: "CREATE TABLE lifecycle_contract_notes (id TEXT PRIMARY KEY)".into(),
            sqlite_sql: "CREATE TABLE lifecycle_contract_notes (id TEXT PRIMARY KEY) STRICT".into(),
        }]
    }
    async fn on_load(&mut self, ctx: &PluginContext) -> Result<()> { self.runtime.bind(ctx) }
    async fn on_enable(&mut self) -> Result<()> {
        let reason: PluginActivationReason = self.runtime.context()?.activation_reason();
        let _ = reason;
        Ok(())
    }
}

#[tokio::test]
async fn macro_preserves_pure_migrations_and_lifecycle_without_host_dependencies() {
    let mut plugin = LifecyclePlugin::default();
    let migrations = plugin.migrations();
    assert_eq!(migrations.len(), 1);
    PluginMigrationForBackend::validate_all(&migrations).unwrap();
    plugin.storage_declaration().unwrap().validate().unwrap();
    assert!(plugin.on_enable().await.is_err(), "no bound lifecycle context yet");
}
"#,
        )?;
        fs::write(
            plugin.join("tests/shared_ai_rag.rs"),
            r#"use nuono_plugin::{ai, embedding, rag};

#[test]
fn shared_ai_contracts_and_bounded_rag_work_without_a_host() {
    let model = ai::embedding::EmbeddingModel {
        id: 7,
        model: "standalone-model".into(),
        revision: "standalone-revision".into(),
    };
    let legacy_model: embedding::PluginEmbeddingModel = model;
    let batch = embedding::PluginEmbeddingBatch {
        model: legacy_model,
        vectors: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
    };
    let shared_batch: &ai::embedding::EmbeddingBatch = &batch;
    ai::embedding::validate_vectors(&shared_batch.vectors, 2).unwrap();
    let budget = rag::IndexBudget::default();
    let digest = rag::vector_digest(&shared_batch.vectors);
    rag::validate_vector_manifest(
        &shared_batch.vectors,
        2,
        rag::VectorManifest {
            dimensions: 2,
            vector_bytes: budget.vector_bytes(2, 2).unwrap(),
            vectors_sha256: &digest,
        },
        &budget,
    ).unwrap();
    let identity = rag::IndexIdentity {
        format_version: 1,
        rules_version: "standalone-rules",
        corpus_sha256: "standalone-corpus",
        model: &shared_batch.model,
    };
    rag::validate_identity(identity, identity).unwrap();
}
"#,
        )?;
        // 宏展开与旧版 Agent 兼容路径必须仅靠 SDK 即可正常工作，无需框架 workspace 直接提供 nuono-app 依赖。
        fs::write(
            plugin.join("tests/agent_contracts.rs"),
            r#"use nuono_plugin::{agent, nuono_tool, NuonoResult, Value};
use nuono_plugin::serde_json::json;
use std::sync::Arc;

#[nuono_tool(name = "fixture.echo", description = "Echo an independent tool call", risk = "low")]
async fn echo(ctx: agent::tool::AgentToolContext, input: Value) -> NuonoResult<Value> {
    Ok(json!({"user": ctx.agent.user_id, "input": input}))
}

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::JsonSchema)]
#[serde(crate = "nuono_plugin::serde", deny_unknown_fields)]
#[schemars(crate = "nuono_plugin::schemars")]
struct SearchInput {
    query: String,
    #[schemars(range(min = 1, max = 10))]
    limit: u32,
}

#[nuono_tool(name = "fixture.typed", description = "A typed SDK-only tool")]
async fn typed(ctx: nuono_plugin::ToolContext, input: SearchInput) -> NuonoResult<Value> {
    Ok(json!({"user":ctx.actor.value(), "query":input.query, "limit":input.limit}))
}

#[tokio::test]
async fn typed_tool_schema_and_context_work_without_direct_schema_or_app_dependencies() {
    let registry = agent::ToolRegistry::new();
    registry.register(TypedTool).unwrap();
    let context = agent::AgentToolContext::new(agent::AgentContext::from_runtime_event(
        7, &json!({"post_type":"message","user_id":42})
    ).unwrap());
    let schema = &registry.chat_tools(agent::AutonomyLevel::LowRiskTools)[0].parameters_schema;
    assert_eq!(schema["properties"]["limit"]["maximum"], 10);
    let rejected = registry.execute("fixture.typed", agent::AutonomyLevel::LowRiskTools,
        context.clone(), json!({"query":"role","limit":11})).await;
    assert_eq!(rejected.status, agent::ToolExecutionStatus::Rejected);
    let result = registry.execute("fixture.typed", agent::AutonomyLevel::LowRiskTools,
        context, json!({"query":"role","limit":2})).await;
    assert_eq!(result.output, json!({"user":"42","query":"role","limit":2}));
}

#[tokio::test]
async fn macro_tools_execute_through_shared_sdk_and_legacy_agent_contracts() {
    let sdk_tool: Arc<dyn nuono_plugin::AgentTool> = Arc::new(EchoTool);
    let legacy_tool: Arc<dyn agent::tool::AgentTool> = sdk_tool;
    let registry: agent::ToolRegistry = agent::tool::ToolRegistry::new();
    registry.register_arc(legacy_tool).unwrap();
    let context = agent::AgentContext::from_runtime_event(
        7,
        &json!({"post_type": "message", "user_id": 42, "raw_message": "hello"}),
    ).unwrap();
    let sdk_context: nuono_plugin::AgentToolContext = agent::AgentToolContext::new(context);
    let receipt: nuono_plugin::ToolExecution = registry.execute(
        "fixture.echo",
        agent::AutonomyLevel::LowRiskTools,
        sdk_context,
        json!({"message": "ping"}),
    ).await;
    let legacy_receipt: agent::tool::ToolExecution = receipt;
    assert_eq!(legacy_receipt.status, agent::ToolExecutionStatus::Succeeded);
    assert_eq!(legacy_receipt.risk_level, Some(nuono_plugin::ToolRiskLevel::LowRisk));
    assert_eq!(legacy_receipt.output, json!({"user": 42, "input": {"message": "ping"}}));
}
"#,
        )?;
        // 未启用原生浏览器引擎时，浏览器接口类型依然可用；此测试桩仅验证异步 API 可正常编译，不实际打开页面。
        fs::write(
            plugin.join("tests/browser_api.rs"),
            r#"use std::time::Duration;
use nuono_plugin::{PluginContext, Result};
use nuono_plugin::browser::{
    BrowserContent, BrowserPage, BrowserRequest, BrowserWaitUntil, PluginBrowser,
    initialize, is_available,
};

pub fn browser_from_context(context: &PluginContext) -> PluginBrowser {
    context.browser()
}

pub async fn fetch_page(browser: &PluginBrowser, request: BrowserRequest) -> Result<BrowserPage> {
    browser.fetch(request).await
}

#[test]
fn browser_contracts_work_without_the_optional_engine() {
    initialize().expect("initialization without the browser engine is a no-op");
    assert!(!is_available());
    let mut request = BrowserRequest::new("https://example.com");
    assert_eq!(request.timeout, Duration::from_secs(30));
    assert_eq!(request.max_output_bytes, 2 * 1024 * 1024);
    request.evaluation = Some("document.title".to_owned());
    for content in [BrowserContent::Html, BrowserContent::Text, BrowserContent::None] {
        request.content = content;
    }
    for wait_until in [
        BrowserWaitUntil::Load,
        BrowserWaitUntil::DomContentLoaded,
        BrowserWaitUntil::NetworkIdle0,
    ] {
        request.wait_until = wait_until;
    }
}
"#,
        )?;
        let manifest = plugin.join("Cargo.toml");
        let template_cargo = || {
            let mut command = cargo(root, &target);
            command
                .env("CARGO_BUILD_JOBS", "1")
                .env("CARGO_INCREMENTAL", "0")
                .env("CARGO_PROFILE_DEV_DEBUG", "0");
            command
        };
        let mut tree = template_cargo();
        tree.args(["tree", "--manifest-path"]).arg(&manifest).args([
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ]);
        offline(&mut tree);
        let output = execute(&mut tree, true)?;
        let forbidden = forbidden_dependencies(&output, false);
        ensure!(
            forbidden.is_empty(),
            "Standalone plugin pulled in host/database dependencies: {forbidden:?}"
        );
        let browser_backends = browser_backend_dependencies(&output);
        ensure!(
            browser_backends.is_empty(),
            "Default standalone plugin pulled in optional browser engines: {browser_backends:?}"
        );
        let mut test = template_cargo();
        test.args(["test", "--manifest-path"]).arg(&manifest);
        offline(&mut test);
        execute(&mut test, false)?;
    }
    println!(
        "Independent source template dependency boundaries and unit tests passed with incremental compilation disabled."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_tree_rejects_driver_families_and_test_sdk() {
        let tree = "nuono-plugin v0.0.1\nnuono-ai v0.0.1\nnuono-rag v0.0.1\nnuono-ai-provider v0.0.1\nsea-orm v2.0.4\nsqlx-core v0.9.0 (*)\naxum-core v0.5.0\nnuono-plugin-test v0.0.1\n";
        assert_eq!(
            forbidden_dependencies(tree, true),
            BTreeSet::from([
                "sqlx-core",
                "axum-core",
                "nuono-plugin-test",
                "nuono-ai-provider",
            ])
        );
        assert_eq!(
            forbidden_dependencies(tree, false),
            BTreeSet::from(["sqlx-core", "axum-core", "nuono-ai-provider"])
        );
    }

    #[test]
    fn production_tree_allows_shared_ai_rag_without_provider_or_host() {
        let tree = "verified-plugin v0.1.0\nnuono-plugin v0.0.1\nnuono-app v0.0.1\nnuono-ai v0.0.1\nnuono-rag v0.0.1\nnuono-error v0.0.1\nsea-orm v2.0.4\nreqwest v0.13.5\n";
        assert!(forbidden_dependencies(tree, true).is_empty());
        assert!(browser_backend_dependencies(tree).is_empty());
    }

    #[test]
    fn default_template_tree_detects_optional_browser_engines() {
        let tree = "verified-plugin v0.1.0\nnuono-plugin v0.0.1\nobscura-browser v0.1.0\nobscura-js v0.1.0\nv8 v149.0.0\nv8 v149.0.0 (*)\n";
        assert_eq!(
            browser_backend_dependencies(tree),
            BTreeSet::from(["obscura-browser", "obscura-js", "v8"])
        );
        // 插件显式启用可选浏览器引擎时是合法的；仅要求默认脚手架模板不主动引入它。
        assert!(forbidden_dependencies(tree, true).is_empty());
    }
}
