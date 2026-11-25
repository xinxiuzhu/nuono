use super::*;
use nuono_app::action::{ActionAudit, execute_authorized};
use nuono_app::transport::TransportAction;
use nuono_domain::{
    ActionError, ActionErrorKind, BotIdentity, ConversationRef, DeliveryTarget, FileSource,
    IdentityKind, IdentityRef, IdentityScheme, MessageSegment, NativeId, OutboundAction, Platform,
    QueryOperation,
};
use sea_orm::{ConnectionTrait, EntityTrait, QueryOrder};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const PRIVATE_PAYLOAD: &str = "PRIVATE-MESSAGE-DO-NOT-PERSIST";
const PRIVATE_BASE64: &str = "base64://UFJJVkFURS1JTUFHRS1ETy1OT1QtUEVSU0lTVA==";

fn identity(kind: IdentityKind, value: &str) -> IdentityRef {
    IdentityRef::new(
        Platform::Feishu,
        kind,
        IdentityScheme::Native,
        value,
        None::<String>,
    )
    .unwrap()
}

fn bot() -> BotIdentity {
    BotIdentity::new(
        "runtime-instance",
        identity(IdentityKind::Bot, "opaque-bot.+001"),
        None,
    )
    .unwrap()
}

fn send(target: DeliveryTarget) -> TransportAction {
    TransportAction::new(
        bot(),
        OutboundAction::SendMessage {
            target,
            segments: vec![
                MessageSegment::text(PRIVATE_PAYLOAD),
                MessageSegment::Image {
                    source: FileSource::Base64(PRIVATE_BASE64.into()),
                    id: None,
                },
            ],
            reply_to: None,
        },
    )
}

fn direct_send() -> TransportAction {
    send(DeliveryTarget::actor(identity(IdentityKind::User, "User:AbC-0001")).unwrap())
}

fn raw(action: &str, params: Value) -> TransportAction {
    TransportAction::new(
        bot(),
        OutboundAction::RawPlatform {
            action: action.into(),
            params,
        },
    )
}

fn raw_for_platform(platform: Platform, action: &str, params: Value) -> TransportAction {
    let bot = BotIdentity::new(
        "native-runtime-instance",
        IdentityRef::new(
            platform,
            IdentityKind::Bot,
            IdentityScheme::Native,
            "opaque-bot.+001",
            None::<String>,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    TransportAction::new(
        bot,
        OutboundAction::RawPlatform {
            action: action.into(),
            params,
        },
    )
}

async fn history(db: &Database, minimum: usize) -> Vec<crate::entity::messages::Model> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let rows = crate::entity::messages::Entity::find()
                .order_by_asc(crate::entity::messages::Column::Id)
                .all(db.orm())
                .await
                .unwrap();
            if rows.len() >= minimum {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("batched outgoing history should be flushed")
}

#[tokio::test]
async fn outbound_stats_count_success_once_across_audit_phases_without_payloads() {
    let db = crate::test_support::test_db(2).await;
    let action = direct_send();
    let executions = AtomicUsize::new(0);
    execute_authorized(&action.authority, &action, Some(&db), async {
        executions.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ActionError>(())
    })
    .await
    .unwrap();
    let failed = execute_authorized(&action.authority, &action, Some(&db), async {
        executions.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(ActionError::new(ActionErrorKind::Rejected, PRIVATE_PAYLOAD))
    })
    .await;
    assert!(failed.is_err());
    db.record(
        &action,
        &action.authority,
        "completed",
        Some(&ActionError::new(
            ActionErrorKind::Rejected,
            PRIVATE_PAYLOAD,
        )),
    )
    .await
    .unwrap();
    let rows = history(&db, 1).await;
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        rows.len(),
        1,
        "started/failed/error phases never count a send"
    );
    assert_eq!(rows[0].bot_instance_id.as_deref(), Some("runtime-instance"));
    assert_eq!(rows[0].platform, "feishu");
    assert_eq!(rows[0].native_sender_id.as_deref(), Some("User:AbC-0001"));
    assert_eq!(rows[0].direction, "outbound");
    assert_eq!(rows[0].text_content, "");
    assert_eq!(rows[0].raw_payload_json, None);
    let events = get_event_logs(&db, 10).await.unwrap();
    assert_eq!(events.len(), 5);
    for event in events {
        let data = event.data.unwrap();
        assert!(!data.contains(PRIVATE_PAYLOAD));
        assert!(!data.contains(PRIVATE_BASE64));
        assert!(!data.contains("User:AbC-0001"));
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbound_stats_recognize_only_known_send_actions_and_preserve_native_destinations() {
    let db = crate::test_support::test_db(2).await;
    let actions = [
        raw(
            "send_msg",
            json!({"message_type":"private", "user_id":"u.native", "message":PRIVATE_PAYLOAD, "token":PRIVATE_BASE64}),
        ),
        raw(
            "send_group_msg",
            json!({"group_id":"00170", "message":PRIVATE_BASE64}),
        ),
        raw(
            "send_private_msg",
            json!({"user_id":42, "message":PRIVATE_PAYLOAD}),
        ),
        raw(
            "send_guild_channel_msg",
            json!({"channel_id":"opaque.channel", "guild_id":"private-parent", "message":PRIVATE_BASE64}),
        ),
        raw(
            "get_group_list",
            json!({"group_id":"should-not-count", "message":PRIVATE_PAYLOAD}),
        ),
        raw(
            "edit_message",
            json!({"user_id":"should-not-count", "message":PRIVATE_PAYLOAD}),
        ),
        raw("delete_msg", json!({"message_id":12})),
        raw(
            "send_message",
            json!({"user_id":"unknown-operation", "message":PRIVATE_PAYLOAD}),
        ),
        raw(
            "send_msg",
            json!({"message_type":"unknown", "user_id":"should-not-count"}),
        ),
        TransportAction::new(
            bot(),
            OutboundAction::Query {
                query: QueryOperation::GroupList,
            },
        ),
        TransportAction::new(
            bot(),
            OutboundAction::DeleteMessage {
                target: None,
                message_id: NativeId::new("message-id").unwrap(),
            },
        ),
        TransportAction::new(
            bot(),
            OutboundAction::EditMessage {
                target: DeliveryTarget::actor(identity(IdentityKind::User, "edited-user")).unwrap(),
                message_id: NativeId::new("message-id").unwrap(),
                segments: vec![MessageSegment::text(PRIVATE_PAYLOAD)],
            },
        ),
    ];
    for action in &actions {
        db.record(action, &action.authority, "started", None)
            .await
            .unwrap();
        db.record(action, &action.authority, "completed", None)
            .await
            .unwrap();
    }
    let rows = history(&db, 4).await;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].native_sender_id.as_deref(), Some("u.native"));
    assert_eq!(rows[1].native_group_id.as_deref(), Some("00170"));
    assert_eq!(rows[2].native_sender_id.as_deref(), Some("42"));
    assert_eq!(rows[3].native_group_id.as_deref(), Some("opaque.channel"));
    assert!(
        rows.iter()
            .all(|row| row.bot_instance_id.as_deref() == Some("runtime-instance"))
    );
    assert!(
        rows.iter()
            .all(|row| row.text_content.is_empty() && row.raw_payload_json.is_none())
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn outbound_stats_count_native_routed_send_msg_shapes_with_platform_scoped_aliases() {
    let db = crate::test_support::test_db(2).await;
    // 与 nuono_plugin::api::routed::build_send_msg_action 生成的参数结构完全对齐
    // （存储层刻意不依赖插件 SDK，哪怕在测试中也直接构造等价 JSON 进行验证）。
    let cases = [
        (
            Platform::Feishu,
            json!({"message_type":"private", "chat_id":"oc.Private.+001", "message":PRIVATE_BASE64}),
            "user_id",
            "oc.Private.+001",
        ),
        (
            Platform::Feishu,
            json!({"message_type":"group", "chat_id":"oc.Group.+002", "message":PRIVATE_PAYLOAD}),
            "group_id",
            "oc.Group.+002",
        ),
        (
            Platform::Discord,
            json!({"message_type":"group", "channel_id":"18446744073709551615", "message":PRIVATE_BASE64}),
            "group_id",
            "18446744073709551615",
        ),
        (
            Platform::QqOfficial,
            json!({"message_type":"private", "openid":"User.OpenId.+003", "user_id":"User.OpenId.+003", "qq_target_type":"c2c", "message":PRIVATE_PAYLOAD}),
            "user_id",
            "User.OpenId.+003",
        ),
        (
            Platform::QqOfficial,
            json!({"message_type":"group", "group_openid":"Group.OpenId.+004", "group_id":"Group.OpenId.+004", "qq_target_type":"group", "message":PRIVATE_BASE64}),
            "group_id",
            "Group.OpenId.+004",
        ),
        (
            Platform::Telegram,
            json!({"message_type":"channel", "chat_id":-1001234567890_i64, "message":PRIVATE_PAYLOAD}),
            "group_id",
            "-1001234567890",
        ),
    ];
    for (platform, params, _, _) in &cases {
        let action = raw_for_platform(platform.clone(), "send_msg", params.clone());
        db.record(&action, &action.authority, "completed", None)
            .await
            .unwrap();
    }
    let rows = history(&db, cases.len()).await;
    assert_eq!(rows.len(), cases.len());
    for (row, (_, _, key, expected)) in rows.iter().zip(cases) {
        let value = if key == "user_id" {
            row.native_sender_id.as_deref()
        } else {
            row.native_group_id.as_deref()
        };
        assert_eq!(value, Some(expected));
        assert_eq!(
            row.bot_instance_id.as_deref(),
            Some("native-runtime-instance")
        );
        assert_eq!(row.text_content, "");
        assert_eq!(row.raw_payload_json, None);
    }
    db.shutdown().await.unwrap();
}

#[test]
fn outbound_stats_do_not_interpret_aliases_for_unrelated_platforms_or_actions() {
    let cases = [
        (
            Platform::OneBot,
            "send_msg",
            json!({"message_type":"private", "chat_id":"not-onebot-user"}),
        ),
        (
            Platform::OneBot,
            "send_msg",
            json!({"message_type":"group", "channel_id":"not-onebot-group"}),
        ),
        (
            Platform::Discord,
            "send_msg",
            json!({"message_type":"private", "openid":"not-discord-user"}),
        ),
        (
            Platform::Feishu,
            "send_msg",
            json!({"message_type":"group", "group_openid":"not-feishu-group"}),
        ),
        (
            Platform::Other("feishu".into()),
            "send_msg",
            json!({"message_type":"private", "chat_id":"unknown-platform"}),
        ),
        (
            Platform::QqOfficial,
            "get_group_list",
            json!({"message_type":"group", "group_openid":"query-only"}),
        ),
        (
            Platform::Feishu,
            "send_unknown",
            json!({"message_type":"group", "chat_id":"unknown-action"}),
        ),
        (
            Platform::Feishu,
            "send_msg",
            json!({"chat_id":"ambiguous-message-kind"}),
        ),
    ];
    for (platform, operation, params) in cases {
        let action = raw_for_platform(platform, operation, params);
        assert!(outbound_message_metadata(&action.action, action.bot.platform()).is_none());
    }
    for (platform, operation, params, expected) in [
        (
            Platform::QqOfficial,
            "send_msg",
            json!({"message_type":"private", "openid":"native-primary", "user_id":"legacy-shadow"}),
            "native-primary",
        ),
        (
            Platform::QqOfficial,
            "send_private_msg",
            json!({"openid":"fallback", "user_id":"native-primary"}),
            "native-primary",
        ),
        (
            Platform::Feishu,
            "send_msg",
            json!({"message_type":"private", "chat_id":"native-primary", "user_id":"legacy-shadow"}),
            "native-primary",
        ),
    ] {
        let action = raw_for_platform(platform, operation, params);
        let metadata = outbound_message_metadata(&action.action, action.bot.platform()).unwrap();
        assert_eq!(metadata["user_id"], expected);
    }
}

#[test]
fn outbound_stats_project_all_typed_destinations_without_serializing_segments() {
    let targets = [
        DeliveryTarget::actor(identity(IdentityKind::User, "User.Actor")).unwrap(),
        DeliveryTarget::conversation(
            ConversationRef::direct(
                identity(IdentityKind::User, "User.Direct"),
                Some(identity(IdentityKind::Conversation, "private-chat")),
            )
            .unwrap(),
        )
        .unwrap(),
        DeliveryTarget::conversation(
            ConversationRef::group(identity(IdentityKind::Group, "Group.+001")).unwrap(),
        )
        .unwrap(),
        DeliveryTarget::conversation(
            ConversationRef::channel(identity(IdentityKind::Channel, "Channel.+002"), None)
                .unwrap(),
        )
        .unwrap(),
        DeliveryTarget::channel(identity(IdentityKind::Channel, "Channel.+003"), None).unwrap(),
    ];
    let expected = [
        ("user_id", "User.Actor"),
        ("user_id", "User.Direct"),
        ("group_id", "Group.+001"),
        ("group_id", "Channel.+002"),
        ("group_id", "Channel.+003"),
    ];
    for (target, (key, value)) in targets.into_iter().zip(expected) {
        let action = send(target);
        let metadata = outbound_message_metadata(&action.action, action.bot.platform()).unwrap();
        assert_eq!(metadata[key], value);
        assert_eq!(metadata["message"], "");
        assert_eq!(metadata["raw_message"], "");
        let encoded = metadata.to_string();
        assert!(!encoded.contains(PRIVATE_PAYLOAD));
        assert!(!encoded.contains(PRIVATE_BASE64));
        assert!(!encoded.contains("private-chat"));
    }
}

#[tokio::test]
async fn outbound_stats_write_failure_never_retries_or_fails_a_successful_send() {
    let db = crate::test_support::test_db(2).await;
    // 删除 nuono_messages 及其下游引用表以模拟异步消息历史落盘失败，此时动作发送与审计事件记录仍须正常完成。
    db.orm()
        .execute_unprepared("DROP TABLE nuono_ai_runs")
        .await
        .unwrap();
    db.orm()
        .execute_unprepared("DROP TABLE nuono_messages")
        .await
        .unwrap();
    let action = direct_send();
    let executions = AtomicUsize::new(0);
    execute_authorized(&action.authority, &action, Some(&db), async {
        executions.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ActionError>("delivered")
    })
    .await
    .expect("statistics must not change successful transport outcome");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(get_event_logs(&db, 10).await.unwrap().len(), 2);
    db.shutdown().await.unwrap();
}
