//! 测试指令等待 OneBot API 回执期间真实 WebSocket 读取协程的并发处理行为。
//! 所有帧均在内存中传输，test_db 与配置路径均为相互隔离的测试桩。

use nuono_adapter_fixture::FixtureAdapter as OneBotAdapter;
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use futures::{Sink, Stream, StreamExt};
use nuono_app::transport::TransportFrame;
use nuono_core::{
    ConfigManager, Database,
    api::Bot,
    config::PluginConfigManager,
    core::{Command, CommandHandler, ConnectionManager, MatchData, Paths},
    plugin::Event,
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};

const BOT_ID: i64 = 900000001;
const DEADLINE: Duration = Duration::from_secs(3);

struct MemorySocket {
    incoming: mpsc::Receiver<TransportFrame>,
    outgoing: futures::channel::mpsc::Sender<TransportFrame>,
}

impl Stream for MemorySocket {
    type Item = Result<TransportFrame, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.incoming.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

fn disconnected(_: futures::channel::mpsc::SendError) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "memory peer disconnected")
}

impl Sink<TransportFrame> for MemorySocket {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.outgoing)
            .poll_ready(cx)
            .map(|result| result.map_err(disconnected))
    }

    fn start_send(mut self: Pin<&mut Self>, frame: TransportFrame) -> Result<(), Self::Error> {
        Pin::new(&mut self.outgoing)
            .start_send(frame)
            .map_err(disconnected)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.outgoing)
            .poll_flush(cx)
            .map(|result| result.map_err(disconnected))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.outgoing)
            .poll_close(cx)
            .map(|result| result.map_err(disconnected))
    }
}

struct MemoryPeer {
    incoming: mpsc::Sender<TransportFrame>,
    outgoing: futures::channel::mpsc::Receiver<TransportFrame>,
}

impl MemoryPeer {
    fn pair() -> (MemorySocket, Self) {
        let (incoming_tx, incoming_rx) = mpsc::channel(512);
        let (outgoing_tx, outgoing_rx) = futures::channel::mpsc::channel(16);
        (
            MemorySocket {
                incoming: incoming_rx,
                outgoing: outgoing_tx,
            },
            Self {
                incoming: incoming_tx,
                outgoing: outgoing_rx,
            },
        )
    }

    async fn event(&self, event: Value) {
        self.incoming
            .send(TransportFrame::Text(event.to_string()))
            .await
            .expect("fixture socket remains connected");
    }

    async fn action(&mut self, segment: &str, file: &str) -> Value {
        let action = loop {
            let frame = timeout(DEADLINE, self.outgoing.next())
                .await
                .expect("command must emit the next action within seconds")
                .expect("socket writer must remain connected");
            let TransportFrame::Text(text) = frame else {
                panic!("expected OneBot action text frame");
            };
            let action: Value = serde_json::from_str(&text).unwrap();
            let data = match action["action"].as_str().unwrap() {
                "get_status" => json!({"online":true,"good":true}),
                "get_login_info" => json!({"user_id":BOT_ID,"nickname":"fixture bot"}),
                "get_group_list" => json!([]),
                _ => break action,
            };
            self.event(json!({"status":"ok","retcode":0,"echo":action["echo"],"data":data}))
                .await;
        };
        assert_eq!(action["action"], "send_msg");
        assert_eq!(action["params"]["message"][0]["type"], segment);
        assert_eq!(action["params"]["message"][0]["data"]["file"], file);
        assert!(action["echo"].as_str().is_some(), "call must carry an echo");
        action
    }

    async fn reply(&self, action: &Value, message_id: &str) {
        self.event(json!({
            "status": "ok",
            "retcode": 0,
            "data": {"message_id": message_id},
            "echo": action["echo"],
        }))
        .await;
    }

    async fn assert_closed(&mut self) {
        assert!(
            timeout(DEADLINE, self.outgoing.next())
                .await
                .expect("all connection writers must be cancelled")
                .is_none(),
            "no later media action may survive disconnect"
        );
    }
}

struct ConnectionTask(JoinHandle<()>);

impl ConnectionTask {
    async fn finished(&mut self) {
        timeout(DEADLINE, &mut self.0)
            .await
            .expect("socket close or overload must stop all connection tasks promptly")
            .expect("connection task must not panic");
    }
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
struct CommandState {
    trace: Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
}

impl CommandState {
    fn record(&self, kind: &str, label: &str) {
        self.trace.lock().unwrap().push(format!("{kind}:{label}"));
        self.changed.notify_one();
    }

    fn snapshot(&self) -> Vec<String> {
        self.trace.lock().unwrap().clone()
    }

    async fn wait_for(&self, step: &str) {
        timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                if self.trace.lock().unwrap().iter().any(|entry| entry == step) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("command must finish promptly after its echo");
    }
}

struct CancelMarker {
    state: Arc<CommandState>,
    label: &'static str,
    completed: bool,
}

impl Drop for CancelMarker {
    fn drop(&mut self) {
        if !self.completed {
            self.state.record("cancel", self.label);
        }
    }
}

struct ReceiptCommand {
    label: &'static str,
    media: &'static str,
    state: Arc<CommandState>,
}

#[async_trait]
impl CommandHandler for ReceiptCommand {
    async fn handle(&self, bot: &Bot, event: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        let mut cancellation = CancelMarker {
            state: self.state.clone(),
            label: self.label,
            completed: false,
        };
        self.state.record("start", self.label);
        let route = event.route().expect("socket supplies an exact bot route");
        for (kind, segment) in [("card", "image"), ("media", self.media)] {
            let receipt = bot
                .call_routed_receipt(
                    &route,
                    json!({
                        "action": "send_msg",
                        "params": {
                            "message_type": "private",
                            "user_id": 900000002,
                            "message": [{"type": segment, "data": {
                                "file": format!("fixture-{}-{kind}", self.label),
                            }}],
                        },
                    }),
                )
                .await?;
            assert_eq!(receipt.message_ids, [format!("{}-{kind}", self.label)]);
            self.state.record(kind, self.label);
        }
        cancellation.completed = true;
        self.state.record("done", self.label);
        Ok(())
    }
}

struct Fixture {
    manager: Arc<ConnectionManager>,
    state: Arc<CommandState>,
    db: Database,
    root: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("nuono-ws-receipts-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let db = nuono_core::test_support::test_db(2).await;
        let config = Arc::new(
            ConfigManager::load(Some(root.join("runtime.toml")))
                .await
                .unwrap(),
        );
        let manager = ConnectionManager::new(
            db.clone(),
            Paths::with_root(&root),
            Arc::new(OneBotAdapter),
            config,
            Arc::new(PluginConfigManager::new(root.join("plugin-config"))),
        )
        .await
        .unwrap();
        let state = Arc::new(CommandState::default());
        for (label, media) in [("record", "record"), ("video", "video")] {
            let command = Command::new(
                format!("receipt-{label}"),
                Arc::new(ReceiptCommand {
                    label,
                    media,
                    state: state.clone(),
                }),
            );
            for trigger in command.to_triggers() {
                manager.plugin_host.trigger_registry().register(trigger);
            }
        }
        Self {
            manager,
            state,
            db,
            root,
        }
    }

    fn connect(&self) -> (ConnectionTask, MemoryPeer) {
        let (socket, peer) = MemoryPeer::pair();
        let manager = self.manager.clone();
        (
            ConnectionTask(tokio::spawn(async move {
                manager
                    .handle_inbound_ws(socket, "in-memory-fixture".into())
                    .await;
            })),
            peer,
        )
    }

    async fn cleanup(self) {
        self.manager.scheduler.shutdown().await.unwrap();
        self.manager.plugin_host.shutdown().await.unwrap();
        drop(self.manager);
        self.db.shutdown().await.unwrap();
        tokio::fs::remove_dir_all(self.root).await.unwrap();
    }
}

fn message(command: &str, message_id: i64) -> Value {
    json!({
        "time": 1,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "self_id": BOT_ID,
        "user_id": 900000002,
        "message_id": message_id,
        "raw_message": command,
        "message": [{"type": "text", "data": {"text": command}}],
        "sender": {"user_id": 900000002, "nickname": "socket fixture"},
    })
}

#[tokio::test]
async fn connection_queries_account_status_and_nickname_without_a_chat_message() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    peer.event(json!({"post_type":"meta_event", "meta_event_type":"lifecycle", "sub_type":"connect", "self_id":BOT_ID})).await;
    let mut actions = Vec::new();
    for _ in 0..2 {
        let frame = timeout(DEADLINE, peer.outgoing.next())
            .await
            .expect("connection must query status and nickname automatically")
            .expect("socket remains connected");
        let TransportFrame::Text(text) = frame else {
            panic!("expected action text");
        };
        let action: Value = serde_json::from_str(&text).unwrap();
        let data = match action["action"].as_str().unwrap() {
            "get_status" => json!({"online":true,"good":false}),
            "get_login_info" => json!({"user_id":BOT_ID.to_string(),"nickname":"测试昵称"}),
            other => panic!("unexpected automatic action: {other}"),
        };
        actions.push(action["action"].as_str().unwrap().to_owned());
        peer.event(json!({"status":"ok","retcode":0,"echo":action["echo"],"data":data}))
            .await;
    }
    actions.sort();
    assert_eq!(actions, ["get_login_info", "get_status"]);
    let group_frame = timeout(DEADLINE, peer.outgoing.next())
        .await
        .expect("connection must automatically sync OneBot groups once online")
        .expect("socket remains connected");
    let TransportFrame::Text(group_text) = group_frame else {
        panic!("expected action text");
    };
    let group_action: Value = serde_json::from_str(&group_text).unwrap();
    assert_eq!(group_action["action"], "get_group_list");
    peer.event(json!({
        "status": "ok",
        "retcode": 0,
        "echo": group_action["echo"],
        "data": [{"group_id": 100001, "group_name": "自动同步群"}]
    }))
    .await;
    timeout(DEADLINE, async {
        loop {
            if fixture
                .manager
                .bot_states
                .snapshot(BOT_ID)
                .is_some_and(|bot| {
                    bot.nickname.as_deref() == Some("测试昵称")
                        && bot.status == "online"
                        && bot.status_reason.as_deref() == Some("platform_unhealthy")
                })
                && fixture
                    .db
                    .group_for_onebot("100001")
                    .await
                    .unwrap()
                    .is_some_and(|group| group.display_name == "自动同步群")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("status, nickname and groups must publish without a business event");
    peer.event(json!({"post_type":"meta_event","meta_event_type":"heartbeat","self_id":BOT_ID,"status":{"online":true,"good":true}})).await;
    timeout(DEADLINE, async {
        while fixture
            .manager
            .bot_states
            .snapshot(BOT_ID)
            .unwrap()
            .status_reason
            .as_deref()
            != Some("status_confirmed")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("healthy heartbeat must recover automatically");
    peer.incoming
        .send(TransportFrame::Close(None))
        .await
        .unwrap();
    connection.finished().await;
    peer.assert_closed().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn socket_reader_delivers_echo_while_command_waits_and_preserves_event_order() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    peer.event(message("receipt-record", 1)).await;
    let record_card = peer.action("image", "fixture-record-card").await;

    // 在第一个回调等待图片发送回执期间，再入队第二个业务事件。
    // 读取协程必须能越过该业务事件，继续读到排在后面的 API 回执。
    peer.event(message("receipt-video", 2)).await;
    peer.event(json!({
        "time": 2,
        "post_type": "meta_event",
        "meta_event_type": "heartbeat",
        "self_id": BOT_ID,
        "interval": 60_000,
        "status": {"online": true, "good": true},
    }))
    .await;
    assert_eq!(fixture.state.snapshot(), ["start:record"]);
    peer.reply(&record_card, "record-card").await;
    let record = peer.action("record", "fixture-record-media").await;
    let status = fixture.manager.bot_states.snapshot(BOT_ID).unwrap();
    assert!(
        !status.last_heartbeat.is_empty(),
        "reader must observe heartbeat before the following echo unblocks the command"
    );
    assert_eq!(fixture.state.snapshot(), ["start:record", "card:record"]);
    peer.reply(&record, "record-media").await;

    let video_card = peer.action("image", "fixture-video-card").await;
    assert_eq!(
        fixture.state.snapshot(),
        [
            "start:record",
            "card:record",
            "media:record",
            "done:record",
            "start:video"
        ]
    );
    peer.reply(&video_card, "video-card").await;
    let video = peer.action("video", "fixture-video-media").await;
    peer.reply(&video, "video-media").await;
    fixture.state.wait_for("done:video").await;
    assert_eq!(
        fixture.state.snapshot(),
        [
            "start:record",
            "card:record",
            "media:record",
            "done:record",
            "start:video",
            "card:video",
            "media:video",
            "done:video",
        ]
    );

    peer.incoming
        .send(TransportFrame::Close(None))
        .await
        .unwrap();
    connection.finished().await;
    peer.assert_closed().await;
    assert!(fixture.manager.bot_txs.is_empty());
    assert!(fixture.manager.event_kernel.action_router().is_empty());
    fixture.cleanup().await;
}

#[tokio::test]
async fn notice_events_do_not_close_the_transport_or_stop_later_commands() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    for (index, notice) in [
        json!({"notice_type": "notify", "sub_type": "poke", "user_id": 7, "target_id": BOT_ID}),
        json!({"notice_type": "group_recall", "group_id": 8, "user_id": 7, "message_id": 9}),
        json!({"notice_type": "group_decrease", "sub_type": "kick_me", "group_id": 8, "user_id": BOT_ID}),
        json!({"notice_type": "bot_offline", "tag": "fixture", "message": "offline notice"}),
        json!({}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut event = notice;
        event["post_type"] = json!("notice");
        event["self_id"] = json!(BOT_ID);
        event["time"] = json!(1);
        peer.event(event).await;

        // 业务派发器必须先处理完通知事件，再开始执行这条指令。
        // 两个回执都能顺利完成，证明连接的读写半部与工作协程均未提前退出。
        peer.event(message("receipt-record", index as i64 + 1)).await;
        let card = peer.action("image", "fixture-record-card").await;
        peer.reply(&card, "record-card").await;
        let media = peer.action("record", "fixture-record-media").await;
        peer.reply(&media, "record-media").await;
        assert!(!connection.0.is_finished());
    }

    peer.incoming
        .send(TransportFrame::Close(None))
        .await
        .unwrap();
    connection.finished().await;
    peer.assert_closed().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn socket_close_cancels_callback_waiting_for_echo_and_discards_queued_events() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    peer.event(message("receipt-record", 1)).await;
    let _unanswered_card = peer.action("image", "fixture-record-card").await;
    peer.event(message("receipt-video", 2)).await;
    peer.incoming
        .send(TransportFrame::Close(None))
        .await
        .unwrap();

    connection.finished().await;
    fixture.state.wait_for("cancel:record").await;
    assert_eq!(fixture.state.snapshot(), ["start:record", "cancel:record"]);
    peer.assert_closed().await;
    assert!(fixture.manager.bot_txs.is_empty());
    assert!(fixture.manager.event_kernel.action_router().is_empty());
    fixture.cleanup().await;
}

#[tokio::test]
async fn socket_writer_failure_cancels_callback_and_removes_the_route() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    peer.event(message("receipt-record", 1)).await;
    let _unanswered_card = peer.action("image", "fixture-record-card").await;
    let MemoryPeer { incoming, outgoing } = peer;
    // 等回调进入等待状态后再断开写入端；否则自动账号查询可能会在业务派发器启动该指令前就先行失败。
    drop(outgoing);
    fixture
        .manager
        .send_to_bot(BOT_ID, "{}".into())
        .await
        .unwrap();

    connection.finished().await;
    fixture.state.wait_for("cancel:record").await;
    assert_eq!(fixture.state.snapshot(), ["start:record", "cancel:record"]);
    assert!(
        incoming.is_closed(),
        "failed writer must also stop the reader"
    );
    assert!(fixture.manager.bot_txs.is_empty());
    assert!(fixture.manager.event_kernel.action_router().is_empty());
    fixture.cleanup().await;
}

#[tokio::test]
async fn saturated_event_queue_disconnects_without_blocking_the_socket_reader() {
    let fixture = Fixture::new().await;
    let (mut connection, mut peer) = fixture.connect();
    peer.event(message("receipt-record", 1)).await;
    let _unanswered_card = peer.action("image", "fixture-record-card").await;

    // 连接的事件积压上限为 100 条。由于当前回调一直阻塞等待回执，
    // 连续涌入的事件必然触发过载保护并取消连接。
    for id in 2..=257 {
        if peer
            .incoming
            .send(TransportFrame::Text(
                message("receipt-video", id).to_string(),
            ))
            .await
            .is_err()
        {
            break;
        }
    }
    connection.finished().await;
    fixture.state.wait_for("cancel:record").await;
    assert_eq!(fixture.state.snapshot(), ["start:record", "cancel:record"]);
    peer.assert_closed().await;
    assert!(fixture.manager.bot_txs.is_empty());
    assert!(fixture.manager.event_kernel.action_router().is_empty());
    fixture.cleanup().await;
}
