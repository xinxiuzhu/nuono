use super::*;
use nuono_app::media::{FileSendResponse, NativeFileSendRequest};
use nuono_core::core::RuntimeActionSink;
use nuono_core::domain::{DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, Platform};

fn session(group: bool) -> Arc<Session> {
    Session::new(
        7,
        CreateSession {
            message_type: if group { "group" } else { "private" }.into(),
            sender_name: "测试用户".into(),
            group_role: "admin".into(),
        },
    )
    .unwrap()
}

fn actor(session: &Session) -> DeliveryTarget {
    DeliveryTarget::Actor {
        actor: IdentityRef::new(
            Platform::OneBot,
            IdentityKind::User,
            IdentityScheme::Native,
            session.user_id.to_string(),
            None::<String>,
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn account_conversations_hide_diagnostics_and_keep_media_in_the_owned_chat_route() {
    let chat = Session::with_mode(
        7,
        CreateSession {
            message_type: "private".into(),
            sender_name: "Member".into(),
            group_role: "member".into(),
        },
        SessionMode::Conversation,
    )
    .unwrap();
    chat.system("pipeline completed", None);
    assert!(chat.snapshot(0).unwrap().messages.is_empty());
    chat.system(
        "internal failure with private configuration",
        Some("secret details".into()),
    );
    let messages = chat.snapshot(0).unwrap().messages;
    assert_eq!(messages[0].error.as_deref(), Some("processing_failed"));
    assert!(!serde_json::to_string(&messages).unwrap().contains("secret"));
    let sink = sink::SandboxSink::new(Arc::downgrade(&chat));
    sink.call_native_file(NativeFileSendRequest {
        target: actor(&chat),
        base64: "aGVsbG8=".into(),
        file_name: "hello.txt".into(),
        content_type: Some("text/plain".into()),
    })
    .await
    .unwrap();
    let snapshot = chat.snapshot(0).unwrap();
    assert!(
        snapshot.messages.last().unwrap().segments[0]["data"]["url"]
            .as_str()
            .unwrap()
            .starts_with("/api/chat/sessions/")
    );
    assert!(!snapshot.test_mode);
}

#[tokio::test]
async fn routed_replies_never_leave_their_virtual_session() {
    let first = session(false);
    let second = session(false);
    assert!(first.bot_id < 0 && first.user_id < 0);
    assert_ne!(first.route, second.route);
    assert_ne!(first.wait_key(), second.wait_key());
    let router = RuntimeActionRouter::new();
    router.register(
        first.route.clone(),
        Arc::new(sink::SandboxSink::new(Arc::downgrade(&first))),
    );
    router.register(
        second.route.clone(),
        Arc::new(sink::SandboxSink::new(Arc::downgrade(&second))),
    );
    router
        .notify_action(
            &first.route,
            "send_private_msg",
            json!({"user_id":first.user_id,"message":"reply"}),
        )
        .await
        .unwrap();
    assert_eq!(first.snapshot(0).unwrap().messages.len(), 1);
    assert!(second.snapshot(0).unwrap().messages.is_empty());
    assert!(
        router
            .notify_action(
                &first.route,
                "send_private_msg",
                json!({"user_id":second.user_id,"message":"wrong target"})
            )
            .await
            .is_err()
    );
    assert!(second.snapshot(0).unwrap().messages.is_empty());
    let messages = first.snapshot(0).unwrap().messages;
    assert_eq!(messages.last().unwrap().direction, "system");
    assert!(messages.last().unwrap().error.is_some());
}

#[tokio::test]
async fn virtual_group_roles_and_mentions_reach_the_onebot_projection() {
    let group = session(true);
    let receipt = MessageReceipt {
        id: "-1".into(),
        seq: 1,
    };
    let event = group.event(
        &receipt,
        &SendMessage {
            text: "help".into(),
            at_bot: true,
        },
    );
    assert_eq!(event["raw_message"], "help");
    assert_eq!(event["message"][0]["type"], "at");
    let unified = group.unified_event(
        event.clone(),
        &SendMessage {
            text: "help".into(),
            at_bot: true,
        },
    );
    let projected = nuono_core::core::runtime_projection::unified_event_to_runtime_json_with_route(
        "onebot",
        &unified,
        Some(&group.route),
    );
    assert_eq!(projected["sender"]["role"], "admin");
    assert_eq!(projected["raw_message"], "help");
    let sink = sink::SandboxSink::new(Arc::downgrade(&group));
    let member = sink.call_action(json!({"action":"get_group_member_info","params":{"group_id":group.group_id.unwrap(),"user_id":group.user_id}})).await.unwrap();
    assert_eq!(member["role"], "admin");
    let private = session(false);
    assert_eq!(
        private.event(
            &receipt,
            &SendMessage {
                text: "help".into(),
                at_bot: false
            }
        )["sender"]["role"],
        "member"
    );
}

#[tokio::test]
async fn closing_cancels_waits_tasks_and_registered_routes() {
    let session = session(false);
    let sink = Arc::new(sink::SandboxSink::new(Arc::downgrade(&session)));
    let router = RuntimeActionRouter::new();
    let generation = router.register(session.route.clone(), sink.clone());
    session.generation.store(generation, Ordering::Release);
    let waits = WaitManager::new();
    let waiting = waits.register_wait(session.wait_key(), Duration::from_secs(60));
    let task = tokio::spawn(std::future::pending::<()>());
    session
        .state()
        .jobs
        .insert("-1".into(), task.abort_handle());
    sink.send_action(json!({"action":"send_private_msg","params":{"user_id":session.user_id,"message":"before close"}})).await.unwrap();
    session.close(&router, &waits);
    session.close(&router, &waits);
    assert!(!router.contains(&session.route));
    assert!(!waits.has_wait(&session.wait_key()));
    assert!(waiting.await.is_err());
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(session.snapshot(0).is_err());
    assert!(sink.send_action(json!({"action":"send_private_msg","params":{"user_id":session.user_id,"message":"after close"}})).await.is_err());
    assert!(session.state().jobs.is_empty());
}

#[tokio::test]
async fn captured_files_use_opaque_media_ids_and_expire_with_history() {
    let session = session(false);
    let sink = sink::SandboxSink::new(Arc::downgrade(&session));
    let response = sink
        .call_native_file(NativeFileSendRequest {
            target: actor(&session),
            base64: "aGVsbG8=".into(),
            file_name: "测试.txt".into(),
            content_type: Some("text/plain".into()),
        })
        .await
        .unwrap();
    assert!(matches!(response, FileSendResponse::Sent { .. }));
    let snapshot = session.snapshot(0).unwrap();
    let url = snapshot.messages[0].segments[0]["data"]["url"]
        .as_str()
        .unwrap();
    assert!(url.starts_with(&format!("/api/sandbox/sessions/{}/media/", session.id)));
    let media_id = url.rsplit('/').next().unwrap();
    let media = session.state().media.get(media_id).cloned().unwrap();
    assert_eq!(media.bytes.as_ref(), b"hello");
    assert!(!media.inline);
    assert_eq!(media.content_type, "application/octet-stream");
    for _ in 0..MAX_MESSAGES {
        session
            .push(
                "incoming",
                vec![json!({"type":"text","data":{"text":"next"}})],
                None,
                None,
                vec![],
            )
            .unwrap();
    }
    assert_eq!(session.snapshot(0).unwrap().messages.len(), MAX_MESSAGES);
    assert!(!session.state().media.contains_key(media_id));
    assert_eq!(session.state().media_bytes, 0);
    assert!(
        session
            .snapshot(snapshot.cursor)
            .unwrap()
            .messages
            .iter()
            .all(|message| message.seq > snapshot.cursor)
    );
    let compatibility = sink.call_action(json!({"action":"send_file","params":{"target":{"type":"private","user_id":session.user_id.to_string()},"base64":"aGVsbG8=","file_name":"legacy.txt"}})).await.unwrap();
    assert_eq!(compatibility["status"], "sent");
    assert_eq!(session.state().media_bytes, 5);
}

#[tokio::test]
async fn invalid_media_and_unmodeled_platform_actions_report_failure() {
    let session = session(false);
    let sink = sink::SandboxSink::new(Arc::downgrade(&session));
    // 复合消息中前一个媒体段解析成功、后一个消息段校验失败时，必须回滚清理已暂存的媒体字节，不能留下孤儿缓存。
    assert!(
        sink.call_action(
            json!({"action":"send_private_msg","params":{"user_id":session.user_id,"message":[
                {"type":"file","data":{"file":"base64://aGVsbG8=","name":"hello.txt"}},
                {"type":"node","data":1}
            ]}})
        )
        .await
        .is_err()
    );
    assert!(session.state().media.is_empty());
    assert!(
        sink.call_action(
            json!({"action":"set_group_kick","params":{"group_id":123,"user_id":123}})
        )
        .await
        .is_err()
    );
    let snapshot = session.snapshot(0).unwrap();
    assert_eq!(snapshot.messages.len(), 2);
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.direction == "system" && message.error.is_some())
    );
    assert_eq!(
        snapshot.messages[1].action.as_deref(),
        Some("set_group_kick")
    );
}
