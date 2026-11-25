use super::*;
use crate::{
    core::{Command, CommandHandler, MatchData, Plugins},
    plugin::{Event, LoadedPlugin, Plugin, PluginResponse},
};
use sea_orm::TransactionTrait;
use serde_json::json;
use std::sync::Mutex;

struct WaitCommand {
    db: Database,
    release: Arc<tokio::sync::Notify>,
    seen: Arc<Mutex<Vec<String>>>,
}
#[async_trait::async_trait]
impl CommandHandler for WaitCommand {
    async fn handle(
        &self,
        bot: &crate::api::Bot,
        _: &Event<'_>,
        data: MatchData,
    ) -> anyhow::Result<()> {
        if data.full_match == "ping" {
            self.seen.lock().unwrap().push("ping".into());
            bot.send("ping complete").await?;
        } else if data.full_match == "pool" {
            let transaction = self.db.orm().begin().await?;
            let receiver = bot.prepare_reply_wait(
                std::time::Duration::from_secs(5),
                nuono_plugin::ReplyWaitMode::Single,
            )?;
            bot.send("pool prompt").await?;
            self.release.notified().await;
            transaction.rollback().await?;
            let reply = receiver.receive(std::time::Duration::from_secs(5)).await?;
            self.seen
                .lock()
                .unwrap()
                .push(reply["raw_message"].as_str().unwrap().into());
            bot.send("pool complete").await?;
        } else {
            bot.send("prompt").await?;
            self.release.notified().await;
            let reply = bot
                .wait_for_reply_event(std::time::Duration::from_secs(5))
                .await?;
            self.seen.lock().unwrap().push(reply.event().text().into());
            bot.send("wait complete").await?;
        }
        Ok(())
    }
}
struct WaitPlugin {
    name: String,
    handler: Arc<WaitCommand>,
}
#[async_trait::async_trait]
impl Plugin for WaitPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "bounded dispatcher test"
    }
    fn service_config(&self) -> Plugins {
        Plugins::new(&self.name).with_prefix(vec!["t".into()])
    }
    fn commands(&self) -> Vec<Command> {
        vec![
            Command::new("wait", self.handler.clone()),
            Command::new("ping", self.handler.clone()),
            Command::new("pool", self.handler.clone()),
        ]
    }
    async fn on_event(&self, event: &Event) -> anyhow::Result<PluginResponse> {
        if event.command_text() == "async" {
            return Ok(PluginResponse::WaitReply {
                reply_message: json!({"action":"send_msg", "params":{"message":"async prompt"}}),
                session_id: crate::onebot::build_session_id(42, event.raw()),
                timeout_secs: 30,
                callback_id: "callback".into(),
                plugin_name: self.name.clone(),
                event_context: event.raw().clone(),
            });
        }
        Ok(PluginResponse::Continue)
    }
    async fn on_message(&self, _: &Event, _: &str, _: &str) -> anyhow::Result<PluginResponse> {
        self.handler.seen.lock().unwrap().push("async reply".into());
        Ok(PluginResponse::Continue)
    }
}
fn message(text: &str) -> InboundPostEvent {
    let event = json!({"post_type":"message", "message_type":"private", "sub_type":"friend",
        "time":1,"self_id":42,"user_id":7,"message_id":1,
        "sender":{"user_id":7,"nickname":"fixture"},
        "raw_message":text,"message":[{"type":"text","data":{"text":text}}]});
    InboundPostEvent {
        received_at: std::time::Instant::now(),
        receipt: None,
        bot_id: 42,
        text: event.to_string(),
        event,
        route: None,
    }
}

#[tokio::test]
async fn ordered_onebot_dispatch_delivers_early_queued_reply_without_running_business_concurrently()
{
    let root = std::env::temp_dir().join(format!("nuono-wait-dispatch-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = crate::test_support::test_db(1).await;
    let bus = Arc::new(EventBus::new());
    let permission = Arc::new(PermissionManager::new(db.clone()));
    let waits = Arc::new(WaitManager::new());
    let host = PluginHost::new(
        db.clone(),
        bus.clone(),
        permission.clone(),
        Paths::with_root(&root),
        waits.clone(),
        FrameworkControl::new(),
        Arc::new(crate::config::PluginConfigManager::with_database(
            root.join("plugin-config"),
            db.clone(),
        )),
    )
    .await
    .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let plugin_name = format!("wait-{}", uuid::Uuid::new_v4().simple());
    host.init_with_plugins(vec![LoadedPlugin {
        plugin: Box::new(WaitPlugin {
            name: plugin_name.clone(),
            handler: Arc::new(WaitCommand {
                db: db.clone(),
                release: release.clone(),
                seen: seen.clone(),
            }),
        }),
    }])
    .await
    .unwrap();
    // 该测试桩重点验证连续快速发送不同指令时的队列与语言偏好行为。
    // 触发准入限流逻辑由 trigger_control_host_tests 单独覆盖。
    host.set_unlimited_trigger_policy_for_test(&plugin_name);
    let mut handler = EventHandler::new(
        db.clone(),
        CommandDispatchServices {
            trigger_registry: host.trigger_registry(),
            cooldown: host.cooldown_tracker.clone(),
            stats: host.command_stats.clone(),
        },
        permission.clone(),
        bus.clone(),
        waits.clone(),
        Arc::new(ApiClient::new()),
    );
    handler.set_plugin_host(host.clone());
    let config = Arc::new(
        crate::config::ConfigManager::load(Some(root.join("nuono.toml")))
            .await
            .unwrap(),
    );
    let kernel = Arc::new(EventKernel::new(
        db.clone(),
        bus,
        permission.clone(),
        host.clone(),
        Arc::new(TaskExecutor::new(1)),
        config.clone(),
        EventKernelPipeline {
            filter_handler: Arc::new(FilterHandler::new(db.clone())),
            wait_manager: waits.clone(),
            event_handler: Arc::new(handler),
            matcher_registry: Arc::new(MatcherRegistry::new(Arc::new(SessionManager::new()))),
            adapter: Arc::new(nuono_adapter_fixture::FixtureAdapter),
            core_commands: Arc::new(crate::core_commands::CoreCommands::new(
                config,
                permission,
                crate::SystemMonitor::new(),
            )),
        },
    ));
    let (input, input_rx) = mpsc::channel(2);
    let (output, mut output_rx) = mpsc::channel(8);
    let task = tokio::spawn(run_ordered_events(kernel.clone(), input_rx, output));
    input.send(message("twait")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), output_rx.recv())
        .await
        .unwrap()
        .unwrap();
    // 两条事件都在当前回调注册 SDK 等待会话之前到达。
    // 指令必须留在队列中，且注册等待会话时必须唤醒派发器重新尝试匹配该回复。
    input.send(message("tping")).await.unwrap();
    input.send(message("answer")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while input.capacity() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(waits.wait_count(), 0);
    release.notify_one();
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(2), output_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(seen.lock().unwrap().as_slice(), &["answer", "ping"]);
    assert_eq!(waits.wait_count(), 0);

    // 准入检查需要获取数据库连接，而连接池中唯一的连接正被当前活跃回调占用。
    // 独立等待通道等待连接池时，当前回调必须能继续推进并释放连接，避免死锁。
    input.send(message("tpool")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), output_rx.recv())
        .await
        .unwrap()
        .unwrap();
    input.send(message("pool answer")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while input.capacity() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), output_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &["answer", "ping", "pool answer"]
    );

    // WaitReply 等待循环绑定在发起它的那条 OneBot 连接上。
    // 断开该接收端会自动取消等待注册，防止重连后的新消息被喂给旧连接的回调。
    input.send(message("tasync")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), output_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(waits.wait_count(), 1);
    drop(output_rx);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while waits.wait_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    let (reconnect, reconnect_rx) = mpsc::channel(2);
    let (output, mut replies) = mpsc::channel(2);
    let task = tokio::spawn(run_ordered_events(kernel, reconnect_rx, output));
    reconnect.send(message("tping")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &["answer", "ping", "pool answer", "ping"]
    );
    task.abort();
    let _ = task.await;
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(root).await;
}
