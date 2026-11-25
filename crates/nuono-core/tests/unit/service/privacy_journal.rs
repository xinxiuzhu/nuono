use super::*;
use nuono_app::companion_data::DeletionKind;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("nuono-privacy-journal-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn entry() -> CompanionDeletion {
    CompanionDeletion {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: 7,
        scope_keys: vec![nuono_app::agent::assistant_chat_key(7)],
        through: crate::clock::load_time(crate::clock::store_time(chrono::Utc::now())),
        kind: DeletionKind::Conversation,
    }
}

#[tokio::test]
async fn durable_deletions_survive_new_instance_and_retries_cannot_change_identity() {
    let temp = Temp::new();
    let journal = PrivacyJournal::new(&temp.0);
    let original = entry();
    journal.append(&original).await.unwrap();
    journal.append(&original).await.unwrap();
    let reloaded = PrivacyJournal::new(&temp.0).entries().await.unwrap();
    assert_eq!(reloaded, vec![original.clone()]);
    let mut collision = original.clone();
    collision.kind = DeletionKind::All;
    assert!(journal.append(&collision).await.is_err());
    assert_eq!(journal.entries().await.unwrap(), vec![original]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&journal.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn corrupt_or_misnamed_durable_entries_block_recovery() {
    let temp = Temp::new();
    let journal = PrivacyJournal::new(&temp.0);
    assert!(journal.is_empty().await.unwrap());
    let original = entry();
    journal.append(&original).await.unwrap();
    let path = journal.directory.join(format!("{}.json", original.id));
    fs::write(&path, b"partial-json").unwrap();
    assert!(journal.is_empty().await.is_err());
    fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    fs::rename(&path, journal.directory.join("wrong.json")).unwrap();
    assert!(journal.entries().await.is_err());
}

#[tokio::test]
async fn incomplete_unpublished_writes_do_not_become_deletions() {
    let temp = Temp::new();
    let journal = PrivacyJournal::new(&temp.0);
    fs::create_dir(&journal.directory).unwrap();
    fs::write(journal.directory.join(".pending-interrupted"), b"partial").unwrap();
    assert!(journal.is_empty().await.unwrap());
    let original = entry();
    journal.append(&original).await.unwrap();
    assert_eq!(journal.entries().await.unwrap(), vec![original]);
}

#[tokio::test]
async fn a_restored_conversation_is_deleted_again_but_newer_messages_survive() {
    let db = crate::test_support::test_db(2).await;
    let temp = Temp::new();
    let journal = PrivacyJournal::new(&temp.0);
    let user = db
        .create_user(crate::db::NewUser {
            username: "privacy-replay".into(),
            email: None,
            password: "journal-test-password".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let mut deletion = entry();
    deletion.user_id = user.id;
    deletion.scope_keys = vec![nuono_app::agent::assistant_chat_key(user.id)];
    let scope = &deletion.scope_keys[0];
    db.save_ai_conversation(scope, "user", "restored personal history")
        .await
        .unwrap();
    deletion.through = crate::clock::load_time(crate::clock::store_time(chrono::Utc::now()))
        + chrono::Duration::seconds(1);
    journal.append(&deletion).await.unwrap();
    journal.replay(&db).await.unwrap();
    assert!(
        db.recent_ai_conversations(scope, 20)
            .await
            .unwrap()
            .is_empty()
    );
    // 模拟从旧备份快照中恢复出了早于已持久化清除时间线（cutoff）的历史记录。
    db.save_ai_conversation(scope, "user", "old backup content")
        .await
        .unwrap();
    journal.replay(&db).await.unwrap();
    assert!(
        db.recent_ai_conversations(scope, 20)
            .await
            .unwrap()
            .is_empty()
    );
    let conv = crate::conversation_store::ensure_session_conversation_on(db.orm(), scope)
        .await
        .unwrap();
    crate::conversation_store::append_message_on(
        db.orm(),
        crate::conversation_store::NewMessage {
            conversation_id: Some(conv.id),
            platform: conv.platform,
            bot_instance_id: None,
            native_message_id: None,
            reply_to_native_message_id: None,
            sender_user_id: conv.owner_user_id,
            sender_identity_id: None,
            native_sender_id: None,
            native_group_id: None,
            sender_nickname: None,
            direction: crate::conversation_store::DIRECTION_INBOUND.into(),
            role: crate::conversation_store::ROLE_USER.into(),
            text_content: "new conversation".into(),
            content_blocks_json: "[]".into(),
            raw_payload_json: None,
            occurred_at_ms: (deletion.through + chrono::Duration::seconds(1)).timestamp_millis(),
        },
    )
    .await
    .unwrap();
    journal.replay(&db).await.unwrap();
    let surviving = db.recent_ai_conversations(scope, 20).await.unwrap();
    assert_eq!(surviving.len(), 1);
    assert_eq!(surviving[0].content, "new conversation");
    db.shutdown().await.unwrap();
}
