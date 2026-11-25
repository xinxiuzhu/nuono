use super::*;
use crate::config::NuonoConfig;
use crate::core::event_kernel::RuntimeActionSink;
use nuono_app::permission::{PermissionSettings, PermissionStore};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct TestPermissions {
    fail: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PermissionStore for TestPermissions {
    async fn permission_settings(&self) -> std::result::Result<PermissionSettings, String> {
        if self.fail.load(Ordering::Relaxed) {
            return Err("permission store unavailable".into());
        }
        Ok(PermissionSettings {
            banned_users: Some(json!(["banned"])),
        })
    }

    async fn bound_account_role(
        &self,
        _platform: Option<&str>,
        _namespace: Option<&str>,
        user_id: &str,
    ) -> std::result::Result<Option<nuono_app::auth::Role>, String> {
        Ok(match user_id {
            "master" => Some(nuono_app::auth::Role::Owner),
            "superuser" => Some(nuono_app::auth::Role::Admin),
            _ => None,
        })
    }
}

#[derive(Default)]
struct Replies(Mutex<Vec<Value>>);

#[async_trait::async_trait]
impl RuntimeActionSink for Replies {
    async fn send_action(&self, action: Value) -> Result<()> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }
}

impl Replies {
    fn last_text(&self) -> String {
        self.0.lock().unwrap().last().unwrap()["params"]["message"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[derive(Default)]
struct TestRestart {
    calls: AtomicUsize,
    fail: AtomicBool,
    fail_commit: AtomicBool,
    targets: Mutex<Vec<Option<RestartReplyTarget>>>,
}

#[async_trait::async_trait]
impl RestartService for TestRestart {
    async fn prepare_restart(
        &self,
        _: &str,
        reply: Option<RestartReplyTarget>,
    ) -> std::result::Result<PreparedRestart, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.targets.lock().unwrap().push(reply);
        if self.fail.load(Ordering::Relaxed) {
            Err("源码构建进行中".into())
        } else {
            let fail = self.fail_commit.load(Ordering::Relaxed);
            Ok(PreparedRestart::new(move || async move {
                if fail {
                    Err("保存重启完成通知失败".into())
                } else {
                    Ok(())
                }
            }))
        }
    }
}

struct Fixture {
    commands: CoreCommands,
    config: Arc<ConfigManager>,
    permission_failure: Arc<AtomicBool>,
    restart: Arc<TestRestart>,
    replies: Arc<Replies>,
}

impl Fixture {
    fn new() -> Self {
        let config = Arc::new(ConfigManager::for_test(
            NuonoConfig::default(),
            "unused-core-command-test.toml".into(),
        ));
        let permission_failure = Arc::new(AtomicBool::new(false));
        let permissions = Arc::new(PermissionManager::new(TestPermissions {
            fail: permission_failure.clone(),
        }));
        let commands = CoreCommands::new(config.clone(), permissions, SystemMonitor::new());
        let restart = Arc::new(TestRestart::default());
        commands.set_restart_service(restart.clone());
        Self {
            commands,
            config,
            permission_failure,
            restart,
            replies: Arc::new(Replies::default()),
        }
    }

    fn context(&self, user: &str, text: &str) -> RuntimeEventContext<'static> {
        RuntimeEventContext {
            locale: nuono_app::localization::LocaleContext::default(),
            bot_id: 1,
            // 入站 raw_text 是原始协议报文 JSON，而非归一化后的纯文本指令。
            raw_text: r#"{"raw_message":"nuono重启"}"#,
            event: json!({
                "post_type": "message",
                "message_type": "group",
                "raw_message": text,
                "user_id": user,
                "group_id": "oc_group",
                "message_id": "message-string-id",
                "sender": {"role": "member"}
            }),
            bot_sender: None,
            action_sink: Some(self.replies.clone()),
            action_transport: None,
            domain_event: None,
            unified_event: None,
            resolved_user_id: None,
            resolved_identity_id: None,
            blocked: false,
            is_message: true,
            reply_attempts: AtomicUsize::new(0),
            reply_ok: AtomicUsize::new(0),
        }
    }
}

#[tokio::test]
async fn normalized_text_controls_dispatch_and_reply_keeps_string_identity() {
    let fixture = Fixture::new();
    let mut unrelated = fixture.context("master", "普通消息");
    fixture.commands.handle(&mut unrelated).await.unwrap();
    assert!(!unrelated.blocked);
    assert!(fixture.replies.0.lock().unwrap().is_empty());
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 0);

    let mut status = fixture.context("member", "  nuono状态\n");
    fixture.commands.handle(&mut status).await.unwrap();
    assert!(status.blocked);
    let replies = fixture.replies.0.lock().unwrap();
    let source = replies[0]["params"]["message"][0]["data"]["file"]
        .as_str()
        .unwrap();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.strip_prefix("base64://").unwrap())
        .unwrap();
    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!(image.width(), 1000);
    assert!(image.height() > 1000);
    assert_eq!(replies[0]["params"]["message"][0]["type"], "image");
    assert_eq!(replies[0]["params"]["group_id"], "oc_group");
    assert_eq!(replies[0]["params"]["message_id"], "message-string-id");
    assert_eq!(status.reply_ok.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn casual_chat_and_removed_language_commands_are_not_intercepted() {
    let fixture = Fixture::new();
    for text in [
        "nuono",
        "nuono今天怎么这么慢",
        "nuono未知",
        "nuono重启现在",
        "nuono重启 extra",
        "nuono帮助看看",
        "nuono语言",
        "nuono语言 en",
        "nuono群语言",
        "nuono group language en",
        "请发送nuono重启",
    ] {
        assert!(!is_core_command(text), "unexpected claim: {text}");
        let mut ctx = fixture.context("master", text);
        fixture.commands.handle(&mut ctx).await.unwrap();
        assert!(!ctx.blocked, "unexpected block: {text}");
    }
    assert!(fixture.replies.0.lock().unwrap().is_empty());
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn custom_prefix_aliases_and_permission_floor_work() {
    let fixture = Fixture::new();
    let mut config = fixture.config.get().await;
    config.core_commands = CoreCommandsConfig {
        prefix: "小助手".into(),
        policies: vec![
            CoreCommandPolicy {
                id: "help".into(),
                enabled: true,
                permission: Some(CoreCommandPermission::Everyone),
                aliases: vec!["菜单".into()],
            },
            CoreCommandPolicy {
                id: "status".into(),
                enabled: true,
                permission: Some(CoreCommandPermission::BotAdmin),
                aliases: vec!["/status".into()],
            },
            CoreCommandPolicy {
                id: "restart".into(),
                enabled: true,
                permission: Some(CoreCommandPermission::BotAdmin),
                aliases: vec![],
            },
        ],
    };
    fixture.config.replace_runtime(config).await.unwrap();

    // 旧前缀不再生效，不拦截消息。
    let mut old_prefix = fixture.context("member", "nuono帮助");
    fixture.commands.handle(&mut old_prefix).await.unwrap();
    assert!(!old_prefix.blocked);

    // 新前缀 + 中文/英文/自定义别名均可正常命中。
    for trigger in ["小助手 帮助", "小助手帮助", "/小助手 help", "菜单"] {
        let mut ctx = fixture.context("member", trigger);
        fixture.commands.handle(&mut ctx).await.unwrap();
        assert!(ctx.blocked, "expected match for {trigger}");
        let reply = fixture.replies.last_text();
        assert!(reply.contains("小助手 帮助"), "{trigger}: {reply}");
        assert!(reply.contains("菜单"), "{trigger}: {reply}");
    }

    // 上调 status 权限为 BotAdmin 后，普通成员被拦截。
    let mut denied_status = fixture.context("member", "/status");
    fixture.commands.handle(&mut denied_status).await.unwrap();
    assert!(denied_status.blocked);
    assert!(fixture.replies.last_text().contains("仅限机器人管理员"));

    // 重启权限不可低于 BotAdmin 安全下限。
    let invalid = CoreCommandsConfig {
        prefix: "nuono".into(),
        policies: vec![CoreCommandPolicy {
            id: "restart".into(),
            enabled: true,
            permission: Some(CoreCommandPermission::Everyone),
            aliases: vec![],
        }],
    };
    assert!(invalid.validate().is_err());
}

#[test]
fn binding_tokens_bypass_plugin_waits_without_claiming_ordinary_text() {
    let token = "0123456789abcdef0123456789abcdef";
    let command = format!(" 绑定 {token}\n");
    assert!(is_core_command(&command));
    assert_eq!(binding_code(&command), Some(token));
    for text in [
        "绑定",
        "绑定 小明",
        "绑定 012345",
        "绑定 0123456789abcdef0123456789abcdeg",
        "请绑定 0123456789abcdef0123456789abcdef",
    ] {
        assert!(!is_core_command(text), "ordinary text was reserved: {text}");
        assert!(binding_code(text).is_none());
    }
}

#[tokio::test]
async fn restart_allows_master_and_superuser_but_not_members_or_banned() {
    let fixture = Fixture::new();
    for (user, expected) in [
        ("member", "仅限机器人管理员及以上"),
        ("banned", "当前账号无权使用"),
    ] {
        let mut ctx = fixture.context(user, "nuono重启");
        ctx.event["sender"]["role"] = json!("owner");
        fixture.commands.handle(&mut ctx).await.unwrap();
        assert!(ctx.blocked);
        let reply = fixture.replies.last_text();
        assert!(reply.contains(expected), "{user}: {reply}");
    }
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 0);

    for user in ["master", "superuser"] {
        let mut ctx = fixture.context(user, "nuono重启");
        fixture.commands.handle(&mut ctx).await.unwrap();
        assert!(ctx.blocked);
        assert_eq!(fixture.replies.last_text(), "已安排 Nuono 重启。");
    }
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn disabled_command_uses_latest_snapshot_and_never_calls_handler() {
    let fixture = Fixture::new();
    let mut config = fixture.config.get().await;
    config.core_commands.policies = vec![CoreCommandPolicy {
        id: "restart".into(),
        enabled: false,
        permission: Some(CoreCommandPermission::BotAdmin),
        aliases: vec!["紧急重启".into()],
    }];
    fixture.config.replace_runtime(config).await.unwrap();

    // 显式系统前缀触发禁用指令：拦截并提示已禁用。
    let mut ctx = fixture.context("master", "nuono重启");
    fixture.commands.handle(&mut ctx).await.unwrap();
    assert!(ctx.blocked);
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.replies.last_text(), "nuono 重启 已禁用。");

    // 禁用指令的自定义别名不再劫持消息，留给插件或普通聊天处理。
    let mut alias_ctx = fixture.context("master", "紧急重启");
    fixture.commands.handle(&mut alias_ctx).await.unwrap();
    assert!(!alias_ctx.blocked);

    let mut help = fixture.context("member", "nuono帮助");
    fixture.commands.handle(&mut help).await.unwrap();
    assert!(
        fixture
            .replies
            .last_text()
            .contains("安全重启 Nuono 服务进程（机器人管理员）")
    );
    assert!(fixture.replies.last_text().contains("［已禁用］"));

    let mut config = fixture.config.get().await;
    config.core_commands.policies.clear();
    fixture.config.replace_runtime(config).await.unwrap();
    let mut ctx = fixture.context("master", "nuono重启");
    fixture.commands.handle(&mut ctx).await.unwrap();
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn permission_and_restart_failures_remain_consumed() {
    let fixture = Fixture::new();
    fixture.permission_failure.store(true, Ordering::Relaxed);
    let mut denied = fixture.context("master", "nuono重启");
    assert!(fixture.commands.handle(&mut denied).await.is_err());
    assert!(denied.blocked);
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 0);
    assert!(fixture.replies.last_text().contains("无法验证权限"));

    fixture.permission_failure.store(false, Ordering::Relaxed);
    fixture.restart.fail.store(true, Ordering::Relaxed);
    let mut failed = fixture.context("master", "nuono重启");
    assert!(fixture.commands.handle(&mut failed).await.is_err());
    assert!(failed.blocked);
    assert_eq!(fixture.restart.calls.load(Ordering::Relaxed), 1);
    assert!(fixture.replies.last_text().contains("源码构建进行中"));
    assert!(!fixture.replies.last_text().contains("已安排"));
}

#[tokio::test]
async fn completion_target_preserves_native_route_and_channel_and_commit_failure_is_reported() {
    use crate::domain::{
        BotIdentity, ConversationRef, DomainEvent, DomainEventKind, DomainMessage, IdentityKind,
        IdentityRef, IdentityScheme, MessageSegment, Platform,
    };
    let fixture = Fixture::new();
    let identity = |kind, value: &str| {
        IdentityRef::new(
            Platform::Discord,
            kind,
            IdentityScheme::Native,
            value,
            None::<String>,
        )
        .unwrap()
    };
    let bot = BotIdentity::new(
        "exact-instance",
        identity(IdentityKind::Bot, "123456789012345678901"),
        None,
    )
    .unwrap();
    let conversation = ConversationRef::channel(
        identity(IdentityKind::Channel, "123456789012345678902"),
        Some(identity(
            IdentityKind::Organization,
            "123456789012345678903",
        )),
    )
    .unwrap();
    let actor = identity(IdentityKind::User, "master");
    let mut ctx = fixture.context("master", "nuono重启");
    let route =
        crate::core::RuntimeBotRoute::new("discord", "exact-instance", "123456789012345678901");
    ctx.event["route"] = json!(route);
    ctx.domain_event = Some(
        DomainEvent::new(
            "event",
            chrono::Utc::now(),
            chrono::Utc::now(),
            bot.clone(),
            Some(actor.clone()),
            Some(conversation.clone()),
            DomainEventKind::Message(
                DomainMessage::new(
                    "message",
                    vec![MessageSegment::text("nuono重启")],
                    actor,
                    Value::Null,
                )
                .unwrap(),
            ),
            Value::Null,
        )
        .unwrap(),
    );
    fixture.commands.handle(&mut ctx).await.unwrap();
    let reply = fixture.restart.targets.lock().unwrap()[0].clone().unwrap();
    assert_eq!(reply.route, route);
    assert_eq!(reply.bot, bot);
    assert_eq!(reply.conversation, conversation);

    ctx.event["route"]["instance_id"] = json!("wrong-instance");
    ctx.blocked = false;
    fixture.restart.fail_commit.store(true, Ordering::Relaxed);
    assert!(fixture.commands.handle(&mut ctx).await.is_err());
    assert!(fixture.replies.last_text().contains("重启未执行"));
    assert!(fixture.restart.targets.lock().unwrap()[1].is_none());
}

struct GatedReply {
    entered: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Notify>,
    completed: Arc<AtomicBool>,
    fail: bool,
}

#[async_trait::async_trait]
impl RuntimeActionSink for GatedReply {
    async fn send_action(&self, _: Value) -> Result<()> {
        self.entered.notify_one();
        if self.fail {
            return Err(NuonoError::Internal("transport unavailable".into()));
        }
        self.released.notified().await;
        self.completed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct ObservedRestart {
    commits: Arc<AtomicUsize>,
    reply_completed: Arc<AtomicBool>,
    committed_after_reply: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RestartService for ObservedRestart {
    async fn prepare_restart(
        &self,
        _: &str,
        _: Option<RestartReplyTarget>,
    ) -> std::result::Result<PreparedRestart, String> {
        let commits = self.commits.clone();
        let reply_completed = self.reply_completed.clone();
        let committed_after_reply = self.committed_after_reply.clone();
        Ok(PreparedRestart::new(move || async move {
            committed_after_reply.store(reply_completed.load(Ordering::SeqCst), Ordering::SeqCst);
            commits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
    }
}

fn observed_restart(fixture: &Fixture, fail: bool) -> (Arc<ObservedRestart>, Arc<GatedReply>) {
    let reply = Arc::new(GatedReply {
        entered: Arc::new(tokio::sync::Notify::new()),
        released: Arc::new(tokio::sync::Notify::new()),
        completed: Arc::new(AtomicBool::new(false)),
        fail,
    });
    let service = Arc::new(ObservedRestart {
        commits: Arc::new(AtomicUsize::new(0)),
        reply_completed: reply.completed.clone(),
        committed_after_reply: Arc::new(AtomicBool::new(false)),
    });
    fixture.commands.set_restart_service(service.clone());
    (service, reply)
}

#[tokio::test]
async fn restart_waits_for_acknowledgement_and_cancelled_handler_does_not_commit() {
    for cancel in [false, true] {
        let fixture = Fixture::new();
        let (service, reply) = observed_restart(&fixture, false);
        let mut ctx = fixture.context("master", "nuono重启");
        ctx.action_sink = Some(reply.clone());
        let mut handling = Box::pin(fixture.commands.handle(&mut ctx));
        tokio::select! {
            _ = reply.entered.notified() => {},
            result = &mut handling => panic!("handler finished before the reply: {result:?}"),
        }
        assert_eq!(service.commits.load(Ordering::SeqCst), 0);
        if cancel {
            drop(handling);
            reply.released.notify_one();
            assert_eq!(service.commits.load(Ordering::SeqCst), 0);
        } else {
            reply.released.notify_one();
            handling.await.unwrap();
            assert_eq!(service.commits.load(Ordering::SeqCst), 1);
            assert!(service.committed_after_reply.load(Ordering::SeqCst));
        }
    }
}

#[tokio::test]
async fn failed_or_stuck_acknowledgement_cannot_prevent_an_accepted_restart() {
    for fail in [true, false] {
        let fixture = Fixture::new();
        let (service, reply) = observed_restart(&fixture, fail);
        let mut ctx = fixture.context("master", "nuono重启");
        ctx.action_sink = Some(reply);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            fixture.commands.handle(&mut ctx),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(service.commits.load(Ordering::SeqCst), 1);
        assert!(!service.committed_after_reply.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn failed_persistence_does_not_publish_a_disabled_snapshot() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .commands
            .set_enabled("restart", false)
            .await
            .is_err()
    );
    let (_, resolved) = fixture.commands.resolved().await;
    assert!(
        resolved
            .iter()
            .find(|item| item.definition.key == "restart")
            .unwrap()
            .enabled
    );
    assert!(matches!(
        fixture.commands.set_enabled("unknown", false).await,
        Err(NuonoError::Validation(_))
    ));
}

#[test]
fn status_does_not_report_unavailable_metrics_as_zero() {
    assert!(status::render(&Default::default()).contains("采集中"));
    let status = crate::core::system_monitor::SystemStatus {
        cpu_percent: 12.5,
        total_memory_mb: 4096,
        used_memory_mb: 1024,
        process_memory_mb: 64,
        process_physical_memory_mb: None,
        timestamp_secs: 1,
    };
    let rendered = status::render(&status);
    assert!(rendered.contains("CPU：12.5%"));
    assert!(rendered.contains("1024 / 4096 MiB"));
    assert!(rendered.contains("进程内存：64 MiB"));
}

#[tokio::test]
async fn status_image_uses_current_jpeg_policy_and_preserves_private_identity() {
    let fixture = Fixture::new();
    let mut config = fixture.config.get().await;
    config.image_output.format = nuono_app::image_output::ImageOutputFormat::Jpeg;
    config.image_output.jpeg_quality = 70;
    fixture.config.replace_runtime(config).await.unwrap();
    let mut ctx = fixture.context("opaque-private-user", "nuono状态");
    ctx.event["message_type"] = json!("private");
    fixture.commands.handle(&mut ctx).await.unwrap();
    let replies = fixture.replies.0.lock().unwrap();
    assert_eq!(replies[0]["params"]["user_id"], "opaque-private-user");
    let source = replies[0]["params"]["message"][0]["data"]["file"]
        .as_str()
        .unwrap();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.strip_prefix("base64://").unwrap())
        .unwrap();
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        image::ImageFormat::Jpeg
    );
    assert!(ctx.blocked);
    assert_eq!(ctx.reply_ok.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn status_image_failure_falls_back_to_text_and_keeps_message_consumed() {
    #[derive(Default)]
    struct RejectImages(Mutex<Vec<Value>>);
    #[async_trait::async_trait]
    impl RuntimeActionSink for RejectImages {
        async fn send_action(&self, action: Value) -> Result<()> {
            if action["params"]["message"].is_array() {
                return Err(NuonoError::Platform("fixture rejects images".into()));
            }
            self.0.lock().unwrap().push(action);
            Ok(())
        }
    }
    let fixture = Fixture::new();
    let sink = Arc::new(RejectImages::default());
    let mut ctx = fixture.context("member", "nuono状态");
    ctx.action_sink = Some(sink.clone());
    fixture.commands.handle(&mut ctx).await.unwrap();
    let replies = sink.0.lock().unwrap();
    let text = replies[0]["params"]["message"].as_str().unwrap();
    assert!(text.starts_with("Nuono 状态"));
    assert!(text.contains("回退文字"));
    assert!(ctx.blocked);
    assert_eq!(ctx.reply_attempts.load(Ordering::Relaxed), 2);
    assert_eq!(ctx.reply_ok.load(Ordering::Relaxed), 1);
}
