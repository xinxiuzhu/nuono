use super::*;

async fn fixture() -> (Database, CompanionRetentionScope) {
    let db = crate::test_support::test_db(2).await;
    let user = db
        .create_user(crate::db::NewUser {
            username: "retention-user".into(),
            email: None,
            password: "test-password-with-length".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let card = db.ensure_default_companion_card(user.id).await.unwrap();
    let scope = CompanionRetentionScope {
        user_id: user.id,
        scope_key: nuono_app::agent::companion_scope_key(user.id, &card.id),
        card_id: card.id,
    };
    (db, scope)
}

async fn turn(db: &Database, scope: &CompanionRetentionScope, age_days: i64) -> turns::Model {
    let id = uuid::Uuid::new_v4().to_string();
    let history_key = nuono_app::agent::companion_history_key(&scope.scope_key, 0);
    let conv = crate::conversation_store::ensure_session_conversation_on(db.orm(), &history_key)
        .await
        .unwrap();
    let row = turns::ActiveModel {
        id: Set(id.clone()),
        user_id: Set(scope.user_id),
        conversation_id: Set(Some(conv.id.clone())),
        card_id: Set(scope.card_id.clone()),
        scope_key: Set(scope.scope_key.clone()),
        context_epoch: Set(0),
        prompt_snapshot: Set("private character prompt".into()),
        redacted: Set(false),
        control: Set(false),
        compacted: Set(false),
        request_key: Set(format!("retention:{id}")),
        text: Set("我今天去公园散步，看到了很多花，心情很轻松。".repeat(10)),
        status: Set("succeeded".into()),
        reply: Set(Some(
            "听起来是愉快的经历，之后还可以继续去散步。".repeat(10),
        )),
        quota_day: Set(0),
        delivery: Set("none".into()),
        created_at: Set(crate::clock::store_time(
            crate::clock::db_now() - Duration::days(age_days),
        )),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    let msg_id = crate::conversation_store::append_message_on(
        db.orm(),
        crate::conversation_store::NewMessage {
            conversation_id: Some(conv.id.clone()),
            platform: conv.platform.clone(),
            bot_instance_id: None,
            native_message_id: None,
            reply_to_native_message_id: None,
            sender_user_id: Some(scope.user_id),
            sender_identity_id: None,
            native_sender_id: None,
            native_group_id: None,
            sender_nickname: None,
            direction: crate::conversation_store::DIRECTION_INBOUND.into(),
            role: crate::conversation_store::ROLE_USER.into(),
            text_content: row.text.clone(),
            content_blocks_json: "[]".into(),
            raw_payload_json: None,
            occurred_at_ms: row.created_at.timestamp_millis(),
        },
    )
    .await
    .unwrap();
    crate::conversation_store::record_completed_ai_run_on(
        db.orm(),
        crate::conversation_store::CompletedAiRun {
            id: Some(id.clone()),
            conversation_id: conv.id.clone(),
            trigger_message_id: msg_id,
            reply_message_id: None,
            user_id: Some(scope.user_id),
            provider: String::new(),
            model: String::new(),
            status: crate::conversation_store::AI_RUN_STATUS_SUCCEEDED.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            latency_ms: 0,
            steps_json: "[]".into(),
            error_message: None,
            created_at_ms: row.created_at.timestamp_millis(),
            finished_at_ms: Some(row.created_at.timestamp_millis()),
        },
    )
    .await
    .unwrap();
    agent_runs::ActiveModel {
        id: Set(nuono_app::assistant::assistant_run_id(scope.user_id, &id)),
        conversation_id: Set(Some(conv.id)),
        session_id: Set(history_key),
        status: Set("completed".into()),
        state_json: Set(format!("{{\"history\":{:?}}}", row.text)),
        revision: Set(1),
        updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap();
    row
}

#[tokio::test]
async fn verified_summary_reclaims_old_raw_copies_but_preserves_recent_turns() {
    let (db, scope) = fixture().await;
    let oldest = turn(&db, &scope, 10).await;
    let old = turn(&db, &scope, 9).await;
    let recent = turn(&db, &scope, 6).await;
    db.save_companion_memories(
        scope.user_id,
        &oldest.id,
        0,
        &[nuono_app::agent::companion_memory::MemoryUpsert {
            key: "walk.experience".into(),
            content: "用户分享了公园散步经历".into(),
            evidence: "公园散步".into(),
            importance: 0.0,
        }],
    )
    .await
    .unwrap();
    let batch = db.companion_retention_batch(&scope, 7).await.unwrap();
    assert_eq!(batch.turns.len(), 2);
    let summary = CompanionSummary {
        source_turn_ids: vec![oldest.id.clone(), old.id.clone()],
        summary: "用户分享了在公园散步的愉快经历。".into(),
    };
    assert_eq!(
        db.commit_companion_retention(&batch, &summary, 7)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        db.commit_companion_retention(&batch, &summary, 7)
            .await
            .unwrap(),
        0
    );
    let kept = turns::Entity::find_by_id(&oldest.id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.text, "旧对话摘要");
    assert_eq!(kept.reply, Some(summary.summary));
    assert!(kept.compacted && !kept.redacted && kept.prompt_snapshot.is_empty());
    let hidden = turns::Entity::find_by_id(&old.id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert!(
        hidden.redacted && hidden.compacted && hidden.text.is_empty() && hidden.reply.is_none()
    );
    assert_eq!(
        turns::Entity::find_by_id(&recent.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap(),
        recent
    );
    assert_eq!(
        messages::Entity::find().all(db.orm()).await.unwrap().len(),
        1
    );
    assert!(
        agent_runs::Entity::find()
            .all(db.orm())
            .await
            .unwrap()
            .is_empty(),
        "later checkpoint copies of reclaimed history must also be removed"
    );
    let evidence = sources::Entity::find().all(db.orm()).await.unwrap();
    assert_eq!(evidence.len(), 3);
    assert!(
        evidence
            .iter()
            .all(|row| row.detail == "summary" && row.original == "reclaimed")
    );
    let memory_ids = evidence
        .iter()
        .map(|row| row.memory_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let source_map = db.companion_memory_sources(&memory_ids).await.unwrap();
    let expected_scope = nuono_app::agent::memory::MemoryScope {
        scope_type: "user".into(),
        scope_key: scope.scope_key.clone(),
    };
    assert!(
        source_map
            .values()
            .flatten()
            .all(|source| source.validate(&expected_scope).is_ok()),
        "reclaimed fact sources must remain valid summary evidence"
    );
    assert_eq!(
        evidence
            .iter()
            .map(|row| row.source_id.clone())
            .collect::<HashSet<_>>(),
        [format!("turn:{}", oldest.id), format!("turn:{}", old.id)]
            .into_iter()
            .collect()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn model_failure_unknown_sources_or_new_deletion_epoch_never_reclaim_originals() {
    let (db, scope) = fixture().await;
    let original = turn(&db, &scope, 10).await;
    let batch = db.companion_retention_batch(&scope, 7).await.unwrap();
    let invalid = CompanionSummary {
        source_turn_ids: vec!["unknown".into()],
        summary: "摘要".into(),
    };
    assert!(
        db.commit_companion_retention(&batch, &invalid, 7)
            .await
            .is_err()
    );
    assert_eq!(
        turns::Entity::find_by_id(&original.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap(),
        original
    );
    let epochs = serde_json::json!({ &scope.scope_key: 1 }).to_string();
    crate::entity::account_assistant::Entity::insert(
        crate::entity::account_assistant::ActiveModel {
            user_id: Set(scope.user_id),
            context_epochs_json: Set(epochs),
            ..Default::default()
        },
    )
    .on_conflict(
        sea_orm::sea_query::OnConflict::column(crate::entity::account_assistant::Column::UserId)
            .update_column(crate::entity::account_assistant::Column::ContextEpochsJson)
            .to_owned(),
    )
    .exec(db.orm())
    .await
    .unwrap();
    let summary = CompanionSummary {
        source_turn_ids: vec![original.id.clone()],
        summary: "用户分享散步经历。".into(),
    };
    assert_eq!(
        db.commit_companion_retention(&batch, &summary, 7)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        turns::Entity::find_by_id(&original.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert!(
        db.companion_retention_batch(&scope, 7)
            .await
            .unwrap()
            .turns
            .is_empty(),
        "forgotten contexts must not be sent back to a model"
    );
    assert!(
        memories::Entity::find()
            .all(db.orm())
            .await
            .unwrap()
            .is_empty()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn importance_and_recall_delay_reclamation_without_changing_the_minimum_window() {
    let (db, scope) = fixture().await;
    let mut original = turn(&db, &scope, 8).await;
    let now = crate::clock::db_now();
    assert!(eligible(&original, now, 7, 0.0, None));
    assert!(!eligible(&original, now, 7, 0.5, None));
    assert!(!eligible(
        &original,
        now,
        7,
        0.0,
        Some((now - Duration::days(1)).into())
    ));
    original.created_at = now - Duration::days(6);
    assert!(!eligible(&original, now, 7, 0.0, None));
    original.created_at = now - Duration::days(29);
    assert!(eligible(&original, now, 7, 1.0, Some(now.into())));
    db.shutdown().await.unwrap();
}
