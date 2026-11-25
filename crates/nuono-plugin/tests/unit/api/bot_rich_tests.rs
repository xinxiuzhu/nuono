use super::*;
use crate::api::interaction::PluginInteractionPort;
use nuono_app::message_interactions::MessagePolicy;
use std::sync::Mutex;

#[derive(Default)]
struct Capture(Mutex<Vec<Value>>);
#[async_trait::async_trait]
impl RuntimeActionSink for Capture {
    async fn send_action(&self, action: Value) -> Result<()> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }
    async fn call_action(&self, action: Value) -> Result<Value> {
        self.send_action(action).await?;
        Ok(json!({"message_id":"0009"}))
    }
}
struct Port {
    policy: MessagePolicy,
    requests: Mutex<Vec<InteractionRegistration>>,
    reads: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl PluginInteractionPort for Port {
    async fn register(&self, request: InteractionRegistration) -> Result<String> {
        let mut requests = self.requests.lock().unwrap();
        let token = format!("host-token-{}", requests.len());
        requests.push(request);
        Ok(token)
    }
    async fn resolve_command(&self, plugin: &str, command: &str) -> Result<String> {
        assert_eq!(plugin, "Wuwa");
        Ok(if command.starts_with("ww") {
            command.to_owned()
        } else {
            format!("ww{command}")
        })
    }
    async fn message_policy(&self, _: &RuntimeBotRoute) -> Result<MessagePolicy> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.policy.clone())
    }
}
fn fixture(
    platform: &str,
    policy: MessagePolicy,
) -> (Bot, Arc<Capture>, Arc<Port>, RuntimeMessageTarget) {
    let sink = Arc::new(Capture::default());
    let port = Arc::new(Port {
        policy,
        requests: Mutex::new(Vec::new()),
        reads: Default::default(),
    });
    let route = RuntimeBotRoute::new(platform, "app", "123");
    let router = Arc::new(RuntimeActionRouter::new());
    router.set_interaction_port(port.clone());
    router.register(route.clone(), sink.clone());
    let mut bot = Bot::new(
        123,
        Arc::new(EventBus::new()),
        Arc::new(WaitManager::new()),
        Arc::new(ApiClient::new()),
        None,
        Some(sink.clone()),
    )
    .with_wait_owner("Wuwa", None);
    bot.action_router = Some(router);
    bot.current_message = MessageContext::from_onebot_json(
        &json!({"post_type":"message","message_type":"group","self_id":123,"user_id":"0007","group_id":"0010","message_id":"m1","route":route}),
    );
    (bot, sink, port, RuntimeMessageTarget::group(route, "0010"))
}

#[tokio::test]
async fn qq_inline_reusable_images_keep_their_order_and_host_button_registration() {
    let policy = MessagePolicy {
        buttons: nuono_app::message_interactions::ButtonMode::Native,
        fallback: nuono_app::message_interactions::FallbackMode::Error,
        ..Default::default()
    };
    let (bot, sink, port, _) = fixture("qq_official", policy);
    let image = crate::media::ImageSource::bytes(b"\x89PNG\r\n\x1a\n".to_vec())
        .into_reusable_segment()
        .await
        .unwrap();
    bot.send(vec![
        crate::api::MessageSegment::markdown("前言"),
        image,
        crate::api::MessageSegment::markdown("结尾"),
        crate::api::MessageSegment::buttons(vec![vec![ButtonOne::command("帮助", "帮助")]]),
    ])
    .await
    .unwrap();
    let calls = sink.0.lock().unwrap();
    let parts = calls[0]["params"]["message"].as_array().unwrap();
    assert_eq!(
        parts
            .iter()
            .map(|part| part["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["markdown", "image", "markdown", "buttons"]
    );
    assert_eq!(parts[1]["data"]["reuse"], "persistent");
    assert_eq!(parts[3]["data"]["buttons"][0][0]["token"], "host-token-0");
    assert!(
        matches!(&port.requests.lock().unwrap()[0].action,InteractionAction::Command {command} if command=="ww帮助")
    );
}
#[tokio::test]
async fn single_markdown_is_segment_array_and_plain_chat_never_reads_policy() {
    let (bot, sink, port, _) = fixture("telegram", Default::default());
    bot.send("plain").await.unwrap();
    assert_eq!(port.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    bot.send_markdown("*bold*").await.unwrap();
    let calls = sink.0.lock().unwrap();
    assert_eq!(calls[1]["params"]["message"][0]["type"], "markdown");
    assert_eq!(
        calls[1]["params"]["message"][0]["data"]["content"],
        "*bold*"
    );
    assert!(calls[1]["params"].get("route").is_none());
}
#[tokio::test]
async fn commands_resolve_prefix_and_callbacks_receive_host_tokens_instead_of_caller_tokens() {
    let (bot, sink, port, _) = fixture("telegram", Default::default());
    let mut command = ButtonOne::command("帮助", "帮助");
    command.token = Some("forged".into());
    bot.send_options(
        "选择",
        vec![
            command,
            ButtonOne::callback_json("确认", "confirm", json!({"account":"opaque"})),
        ],
    )
    .await
    .unwrap();
    let requests = port.requests.lock().unwrap();
    assert!(matches!(&requests[0].action,InteractionAction::Command{command} if command=="ww帮助"));
    assert_eq!(requests[0].actor_id, "0007");
    assert_eq!(requests[0].target.route.instance_id, "app");
    assert!(
        matches!(&requests[1].action,InteractionAction::Callback{callback_id,data} if callback_id=="confirm" && data["account"]=="opaque")
    );
    let calls = sink.0.lock().unwrap();
    let rows = &calls[0]["params"]["message"][1]["data"]["buttons"];
    assert_eq!(rows[0][0]["token"], "host-token-0");
    assert!(!calls[0].to_string().contains("forged"));
}
#[tokio::test]
async fn unsupported_buttons_fall_back_with_current_prefix_without_exposing_callback_state() {
    let (bot, sink, port, _) = fixture("onebot", Default::default());
    bot.send_options(
        "选择",
        vec![
            ButtonOne::command("帮助", "帮助"),
            ButtonOne::callback_json("确认", "confirm", json!({"secret":"never-in-text"})),
        ],
    )
    .await
    .unwrap();
    assert!(port.requests.lock().unwrap().is_empty());
    let calls = sink.0.lock().unwrap();
    let text = calls[0].to_string();
    assert!(text.contains("ww帮助"));
    assert!(!text.contains("never-in-text"));
    assert!(!text.contains("host-token"));
}
#[tokio::test]
async fn routed_and_native_sends_report_degradation_and_preserve_exact_targets() {
    let (bot, sink, _, target) = fixture("onebot", Default::default());
    let receipt = bot
        .send_routed(&target, crate::api::MessageSegment::markdown("**hello**"))
        .await
        .unwrap();
    assert_eq!(receipt.degradations.len(), 1);
    let route = &target.route;
    let group = crate::domain::IdentityRef::new(
        crate::domain::Platform::OneBot,
        crate::domain::IdentityKind::Group,
        crate::domain::IdentityScheme::Native,
        "0010",
        None::<String>,
    )
    .unwrap();
    let target = crate::domain::DeliveryTarget::conversation(
        crate::domain::ConversationRef::group(group).unwrap(),
    )
    .unwrap();
    let receipt = bot
        .send_native_routed(
            route,
            &target,
            NativeMessage::from(vec![crate::domain::MessageSegment::Markdown {
                content: "**native**".into(),
            }]),
        )
        .await
        .unwrap();
    assert_eq!(receipt.degradations.len(), 1);
    assert_eq!(sink.0.lock().unwrap()[0]["params"]["group_id"], "0010");
}
#[tokio::test]
async fn configured_static_template_is_explicit_and_error_policy_never_sends() {
    let (bot, sink, _, _) = fixture(
        "qq_official",
        MessagePolicy {
            button_template_id: "approved".into(),
            ..Default::default()
        },
    );
    bot.send_button_template("固定入口").await.unwrap();
    assert_eq!(
        sink.0.lock().unwrap()[0]["params"]["message"][1]["data"]["id"],
        "approved"
    );
    let (bot, sink, _, _) = fixture(
        "onebot",
        MessagePolicy {
            fallback: nuono_app::message_interactions::FallbackMode::Error,
            ..Default::default()
        },
    );
    assert!(bot.send_markdown("x").await.is_err());
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_direct_buttons_bind_the_participant_and_keep_the_original_delivery_target() {
    use crate::domain::{
        ConversationRef, DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, MessageSegment,
        OutboundAction, Platform,
    };
    for (platform, native_platform, actor_id, chat_id, actor_scheme, chat_scheme) in [
        (
            "discord",
            Platform::Discord,
            "444",
            "222",
            IdentityScheme::Snowflake,
            IdentityScheme::Snowflake,
        ),
        (
            "feishu",
            Platform::Feishu,
            "ou_target",
            "oc_target",
            IdentityScheme::OpenId,
            IdentityScheme::ChatId,
        ),
    ] {
        let (bot, _sink, port, legacy) = fixture(platform, Default::default());
        let namespace = (native_platform == Platform::Feishu).then_some("123");
        let actor = IdentityRef::new(
            native_platform.clone(),
            IdentityKind::User,
            actor_scheme,
            actor_id,
            namespace,
        )
        .unwrap();
        let chat = IdentityRef::new(
            native_platform,
            IdentityKind::Conversation,
            chat_scheme,
            chat_id,
            namespace,
        )
        .unwrap();
        let target =
            DeliveryTarget::conversation(ConversationRef::direct(actor, Some(chat)).unwrap())
                .unwrap();
        let action = OutboundAction::SendMessage {
            target: target.clone(),
            segments: vec![MessageSegment::Buttons {
                buttons: vec![vec![ButtonOne::callback_json(
                    "确认",
                    "confirm",
                    json!({"value":"stored"}),
                )]],
            }],
            reply_to: None,
        };
        let (prepared, _) = bot
            .prepare_native_rich(&legacy.route, action)
            .await
            .unwrap();
        let OutboundAction::SendMessage {
            target: prepared_target,
            ..
        } = prepared
        else {
            panic!("message expected");
        };
        assert_eq!(
            prepared_target, target,
            "the actual native delivery still uses its original DM conversation"
        );
        let registrations = port.requests.lock().unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].actor_id, actor_id);
        assert_eq!(
            registrations[0].target.kind,
            RuntimeTargetKind::Private {
                user_id: actor_id.into()
            }
        );
        assert_eq!(registrations[0].origin_chat_id.as_deref(), Some(chat_id));
    }
}

#[tokio::test]
async fn cross_target_sends_never_inherit_the_current_actor_or_chat() {
    let (mut bot, _sink, port, legacy) = fixture("feishu", Default::default());
    bot.current_message.as_mut().unwrap().reply_context["chat_id"] = json!("oc_current");
    let target = RuntimeMessageTarget::group(legacy.route, "oc_other");
    bot.send_routed(
        &target,
        crate::api::MessageSegment::buttons(vec![vec![ButtonOne::callback_json(
            "确认",
            "confirm",
            json!({"value":"stored"}),
        )]]),
    )
    .await
    .unwrap();
    let registrations = port.requests.lock().unwrap();
    assert_eq!(registrations[0].target, target);
    assert_eq!(registrations[0].origin_chat_id, None);
    assert_eq!(registrations[0].actor_id, "");
}

#[tokio::test]
async fn specified_user_defaults_are_shared_by_host_and_native_button_wire() {
    let policy = MessagePolicy {
        buttons: nuono_app::message_interactions::ButtonMode::Native,
        ..Default::default()
    };
    let (bot, sink, port, _) = fixture("qq_official", policy.clone());
    bot.send_options(
        "选择",
        vec![ButtonOne::command("帮助", "帮助").with_permission(ButtonPermission::SpecifiedUser)],
    )
    .await
    .unwrap();
    {
        let registrations = port.requests.lock().unwrap();
        assert_eq!(registrations[0].allowed_user_ids, ["0007"]);
        let sent = sink.0.lock().unwrap();
        assert_eq!(
            sent[0]["params"]["message"][1]["data"]["buttons"][0][0]["specify_user_ids"],
            json!(["0007"])
        );
    }

    let (mut bot, sink, port, target) = fixture("qq_official", policy);
    bot.current_message = None;
    let error = bot
        .send_routed(
            &target,
            crate::api::MessageSegment::buttons(vec![vec![
                ButtonOne::command("帮助", "帮助").with_permission(ButtonPermission::SpecifiedUser),
            ]]),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("指定用户按钮"));
    assert!(port.requests.lock().unwrap().is_empty());
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn typed_router_itself_applies_live_message_policy_without_sdk_bot_preparation() {
    let (bot, sink, _, target) = fixture(
        "telegram",
        MessagePolicy {
            markdown: nuono_app::message_interactions::MarkdownMode::Plain,
            ..Default::default()
        },
    );
    let route = &target.route;
    let participant = crate::domain::IdentityRef::new(
        crate::domain::Platform::Telegram,
        crate::domain::IdentityKind::User,
        crate::domain::IdentityScheme::Numeric,
        "7",
        None::<String>,
    )
    .unwrap();
    let action = crate::domain::OutboundAction::SendMessage {
        target: crate::domain::DeliveryTarget::actor(participant).unwrap(),
        segments: vec![crate::domain::MessageSegment::Markdown {
            content: "**plain**".into(),
        }],
        reply_to: Some(crate::domain::NativeId::new("42").unwrap()),
    };
    bot.action_router
        .as_ref()
        .unwrap()
        .call_typed_routed(
            route,
            nuono_app::transport::TransportAction::new(
                crate::core::runtime_transport::route_identity(route).unwrap(),
                action,
            ),
        )
        .await
        .unwrap();
    let calls = sink.0.lock().unwrap();
    assert_eq!(calls[0]["params"]["reply_to"], 42);
    assert_eq!(calls[0]["params"]["message_id"], 42);
    assert_eq!(calls[0]["params"]["message"].as_array().unwrap().len(), 1);
    assert_eq!(calls[0]["params"]["message"][0]["type"], "text");
    assert_eq!(
        calls[0]["params"]["message"][0]["data"]["text"],
        "**plain**"
    );
    assert!(calls[0].get("_nuono_rich_degradations").is_none());
}
