//! 生产环境插件 SDK 集成测试桩；仅用于测试，不会随框架默认安装。
use nuono_plugin::core::event_bus::{Event as BusEvent, EventHandler, EventResult, EventType};
use nuono_plugin::prelude::*;
use nuono_plugin::serde_json::json;
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Probe {
    state: Arc<State>,
}
#[derive(Default)]
struct State {
    context: Mutex<Option<PluginContext>>,
}
struct Receipt(std::path::PathBuf);
impl Drop for Receipt {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"stopped");
    }
}
struct BusHandler(PluginContext);
#[async_trait]
impl EventHandler for BusHandler {
    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::PluginEvent("probe".into())]
    }
    async fn handle(&self, _: &BusEvent) -> Result<EventResult> {
        self.0.db().kv_set("sdk-probe", "bus", "yes").await?;
        Ok(EventResult::Continue)
    }
}
#[nuono_command(pattern = "native")]
async fn native(bot: &Bot, event: &Event<'_>, _: MatchData, state: &State) -> Result<()> {
    bot.send(format!("native:{}", event.user_id_str().unwrap()))
        .await?;
    let ctx = state.context.lock().unwrap().clone().unwrap();
    ctx.db().kv_set("sdk-probe", "native", "yes").await?;
    Ok(())
}
#[nuono_command(pattern = "command")]
async fn command(bot: &Bot, _: &Message, _: MatchData) -> Result<()> {
    bot.send("command").await?;
    let _ = bot.get_group_list().await?;
    Ok(())
}
#[nuono_keyword(pattern = "keyword", block = false)]
async fn keyword(bot: &Bot, _: &Message, _: MatchData) -> Result<()> {
    bot.send("keyword").await?;
    Ok(())
}
struct ProbeTool;
#[async_trait]
impl AgentTool for ProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "Store probe"
    }
    async fn call(&self, ctx: AgentToolContext, _: Value) -> NuonoResult<Value> {
        let store = ctx.db.unwrap();
        store
            .save_ai_conversation("native-probe", "user", "hello")
            .await?;
        let memory = store.get_agent_memory(42).await?;
        let conversations = store.recent_ai_conversations("native-probe", 2).await?;
        Ok(json!({"memory":memory,"conversations":conversations.len()}))
    }
}
#[nuono_plugin(
    name = "sdk-probe",
    version = "0.0.1",
    description = "Unified SDK integration probe",
    state_fields = "state"
)]
impl Plugin for Probe {
    fn config_fields(&self) -> Vec<nuono_plugin::PluginConfigField> {
        vec![nuono_plugin::PluginConfigField::text(
            "greeting",
            "Greeting",
            "Probe response",
            "hello",
        )]
    }
    fn secret_fields(&self) -> Vec<PluginSecretField> {
        vec![PluginSecretField::new("token", "Token", "probe")]
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(ProbeTool)]
    }
    fn web_routes(&self) -> Vec<PluginWebRoute> {
        vec![PluginWebRoute::new(
            "POST".parse().unwrap(),
            "/echo",
            PluginWebAccessPolicy::Public,
        )]
    }
    async fn handle_web_request(&self, req: &PluginWebRequest) -> PluginWebResponse {
        PluginWebResponse::Json(
            json!({"body":req.body,"opaque":req.headers.get("x-opaque").map(|v|v.as_bytes()),"user":req.user}),
        )
    }
    fn migrations(&self) -> Vec<PluginMigrationForBackend> {
        let sql = "CREATE TABLE IF NOT EXISTS sdk_probe_values (value TEXT PRIMARY KEY)";
        vec![PluginMigrationForBackend {
            version: 1,
            description: "native probe".into(),
            pg_sql: sql.into(),
            sqlite_sql: format!("{sql} STRICT"),
        }]
    }

    async fn on_load(&mut self, ctx: &PluginContext) -> Result<()> {
        ctx.kv().set("loaded", "yes").await?;
        ctx.db()
            .execute(
                "INSERT INTO sdk_probe_values (value) VALUES ($1) ON CONFLICT (value) DO NOTHING",
                vec![PluginSqlValue::Text(r#"{"value":"sql-probe"}"#.into())],
            )
            .await?;
        anyhow::ensure!(
            !ctx.db()
                .query_json("SELECT value AS data FROM sdk_probe_values", vec![])
                .await?
                .is_empty(),
            "SQL lost"
        );
        ctx.secrets().set("token", "secret").await?;
        anyhow::ensure!(
            ctx.secrets().get("token").await?.as_deref() == Some("secret"),
            "secret lost"
        );
        let _ = ctx
            .permission_manager()
            .try_evaluate(&json!({"user_id":456,"self_id":123,"message_type":"private"}))
            .await?;
        ctx.command_stats().record_command(
            "discord",
            "bot:001",
            "probe",
            "user:001",
            Some("group:001"),
        );
        let _ = ctx.command_stats().get_summary("discord", "bot:001");
        anyhow::ensure!(
            !ctx.cooldown_tracker().is_user_on_cooldown("user:001", 1),
            "initial cooldown"
        );
        ctx.cooldown_tracker().reset_user("user:001");
        ctx.event_bus().subscribe(Arc::new(BusHandler(ctx.clone())));
        *self.state.context.lock().unwrap() = Some(ctx.clone());
        if ctx.config()["hang_load"] == true {
            ctx.kv().set("load-hanging", "yes").await?;
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn on_enable(&mut self) -> Result<()> {
        let ctx = self.state.context.lock().unwrap().clone().unwrap();
        let mut signals = ctx.framework_control().subscribe();
        let control = ctx.clone();
        ctx.spawn("control", async move {
            if let Ok(signal) = signals.recv().await {
                let _ = control
                    .db()
                    .kv_set("sdk-probe", "control", signal.reason())
                    .await;
            }
        });
        let task_ctx = ctx.clone();
        ctx.spawn("probe", async move {
            let _receipt = Receipt(task_ctx.data_dir().join("task-stopped"));
            // 在业务事件可正常接收并派发后，再写入任务启动回执。
            task_ctx
                .event_bus()
                .publish(BusEvent::new(
                    EventType::PluginEvent("probe".into()),
                    0,
                    Value::Null,
                ))
                .await;
            let _ = task_ctx
                .db()
                .kv_set("sdk-probe", "task-started", "yes")
                .await;
            std::future::pending::<()>().await;
        });
        Ok(())
    }
    async fn on_event(&self, event: &Event) -> Result<PluginResponse> {
        let ctx = self.state.context.lock().unwrap().clone().unwrap();
        if event["panic"] == true {
            panic!("intentional native callback panic");
        }
        if event["hang"] == true {
            let _receipt = Receipt(ctx.data_dir().join("request-stopped"));
            ctx.db()
                .kv_set("sdk-probe", "request-started", "yes")
                .await?;
            std::future::pending::<()>().await;
        }
        Ok(event.reply_text(ctx.current_config()["greeting"].as_str().unwrap_or("hello")))
    }
    async fn on_message(&self, event: &Event, _: &str, _: &str) -> Result<PluginResponse> {
        Ok(event.reply_text("wait-reply"))
    }
    async fn on_config_changed(&mut self, config: &Value) -> Result<()> {
        if config["reject"] == true {
            anyhow::bail!("rejected config");
        }
        Ok(())
    }
    async fn on_unload(&mut self) -> Result<()> {
        self.state.context.lock().unwrap().take();
        Ok(())
    }
}
register_plugin!(Probe);
