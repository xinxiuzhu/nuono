use super::*;
use nuono_app::companion_data::{CompanionDeletion, DeletionKind};
use nuono_core::service::privacy_journal::PrivacyJournal;

#[tokio::test]
async fn managed_restore_replays_deletions_before_clearing_pending_marker() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path();
    std::fs::create_dir(data.join(BACKUP_DIR)).unwrap();
    let url = format!("sqlite://{}?mode=rwc", data.join("runtime.db").display());
    let config = nuono_core::config::DatabaseConfig {
        url: url.clone(),
        max_connections: 2,
    };
    let db = nuono_core::db::Database::from_config(&config)
        .await
        .unwrap();
    let user = db
        .create_user(nuono_core::db::NewUser {
            username: "privacy-restore".into(),
            email: None,
            password: "restore-test-password".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let scope = nuono_app::agent::assistant_chat_key(user.id);
    db.save_ai_conversation(&scope, "user", "content deleted after the backup")
        .await
        .unwrap();
    let dump = data.join(BACKUP_DIR).join("before-deletion.dump");
    dump_database(&url, &dump).await.unwrap();
    let journal = PrivacyJournal::new(data);
    journal
        .append(&CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user.id,
            scope_keys: vec![scope.clone()],
            through: chrono::Utc::now(),
            kind: DeletionKind::Conversation,
        })
        .await
        .unwrap();
    journal.replay(&db).await.unwrap();
    db.shutdown().await.unwrap();
    std::fs::write(
        data.join(RESTORE_MARKER),
        serde_json::json!({"source":dump,"actor":"test"}).to_string(),
    )
    .unwrap();
    assert!(apply_pending_restore(&url, data).await.unwrap().is_some());
    assert!(!data.join(RESTORE_MARKER).exists());
    let restored = nuono_core::db::Database::from_config(&config)
        .await
        .unwrap();
    assert!(
        restored
            .recent_ai_conversations(&scope, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        restored.user_count().await.unwrap() > 0,
        "the account survives conversation deletion"
    );
    restored.shutdown().await.unwrap();
    assert!(
        !journal.is_empty().await.unwrap(),
        "future older restores still need the journal"
    );
}

#[tokio::test]
async fn damaged_journal_never_reports_a_managed_restore_as_complete() {
    let root = tempfile::tempdir().unwrap();
    let journal = PrivacyJournal::new(root.path());
    journal
        .append(&CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: 7,
            scope_keys: vec![nuono_app::agent::assistant_chat_key(7)],
            through: chrono::Utc::now(),
            kind: DeletionKind::All,
        })
        .await
        .unwrap();
    let directory = root.path().join(".companion-deletions");
    let entry = std::fs::read_dir(directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(entry, b"corrupted journal").unwrap();
    // 隐私删除日志的格式校验发生在建立数据库连接之前，因此即便传入非法连接串也会先报日志损坏。
    assert!(
        replay_companion_deletions("invalid database", root.path())
            .await
            .unwrap_err()
            .contains("删除日志损坏")
    );
}
