use crate::db::{AssistantTurnAdmission, AssistantTurnRequest, Database, NewUser};
use crate::entity::{agent_memory_graphs, agent_runs, assistant_turns, companion_memory_sources};
use chrono::Utc;
use nuono_app::agent::{companion_history_key, companion_memory::MemoryUpsert};
use nuono_app::assistant::{AssistantSettings, AssistantTurn, TurnFailure, assistant_run_id};
use nuono_app::companion::CompanionInput;
use nuono_app::companion_data::{CompanionDeletion, DeletionKind};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

fn settings() -> AssistantSettings {
    AssistantSettings {
        enabled: true,
        daily_limit: 30,
        ..Default::default()
    }
}
async fn account(db: &Database) -> i64 {
    db.create_user(NewUser {
        username: "privacy-owner".into(),
        email: None,
        password: "privacy-test-password".into(),
        role: "user".into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap()
    .id
}
async fn start(db: &Database, user: i64, text: &str) -> AssistantTurn {
    let key = uuid::Uuid::new_v4().to_string();
    match db
        .start_assistant_turn(
            user,
            &settings(),
            AssistantTurnRequest {
                request_key: &key,
                text: Some(text),
                retry_of: None,
                sender: None,
            },
        )
        .await
        .unwrap()
    {
        AssistantTurnAdmission::Accepted(turn) => turn,
        other => panic!("expected admission, got {other:?}"),
    }
}
async fn successful(db: &Database, user: i64, text: &str) -> AssistantTurn {
    let turn = start(db, user, text).await;
    agent_runs::ActiveModel {
        id: Set(assistant_run_id(user, &turn.id)),
        conversation_id: Set(None),
        parent_id: Set(None),
        session_id: Set(companion_history_key(&turn.scope_key, turn.context_epoch)),
        status: Set("completed".into()),
        state_json: Set(serde_json::json!({"reply":"我记得了。"}).to_string()),
        approval_id: Set(None),
        revision: Set(0),
        updated_at: Set(crate::clock::store_time(Utc::now())),
    }
    .insert(db.orm())
    .await
    .unwrap();
    db.settle_assistant_turn(user, &turn.id, TurnFailure::Interrupted)
        .await
        .unwrap()
        .unwrap()
}
async fn remember(db: &Database, user: i64, turn: &AssistantTurn, key: &str) -> i64 {
    assert!(
        db.save_companion_memories(
            user,
            &turn.id,
            turn.context_epoch,
            &[MemoryUpsert {
                key: key.into(),
                content: turn.text.clone(),
                evidence: turn.text.clone(),
                importance: 0.5,
            }]
        )
        .await
        .unwrap()
    );
    db.list_agent_memories(Some("user"), Some(&turn.scope_key), None, false, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == turn.text)
        .unwrap()
        .id
}
fn deletion(user: i64, scopes: Vec<String>, kind: DeletionKind) -> CompanionDeletion {
    CompanionDeletion {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user,
        scope_keys: scopes,
        through: Utc::now(),
        kind,
    }
}

#[tokio::test]
async fn clearing_one_character_preserves_its_memory_other_character_and_paid_usage() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let first = successful(&db, user, "我喜欢红茶").await;
    let memory = remember(&db, user, &first, "tea").await;
    let second_card = db
        .create_companion_card(
            user,
            CompanionInput {
                name: "第二人".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    db.activate_companion_card(user, &second_card.id)
        .await
        .unwrap();
    let second = successful(&db, user, "我只告诉第二人的事情").await;
    let before = db
        .assistant_account(user, &settings())
        .await
        .unwrap()
        .remaining;
    let request = deletion(
        user,
        vec![first.scope_key.clone()],
        DeletionKind::Conversation,
    );
    db.apply_companion_deletion(&request).await.unwrap();
    db.apply_companion_deletion(&request).await.unwrap();
    assert!(
        db.assistant_turns_for_card(user, &first.card_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.recent_ai_conversations(
            &companion_history_key(&first.scope_key, first.context_epoch),
            20
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(db.get_agent_memory(memory).await.unwrap().is_some());
    assert_eq!(
        db.assistant_turns_for_card(user, &second.card_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.recent_ai_conversations(
            &companion_history_key(&second.scope_key, second.context_epoch),
            20
        )
        .await
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        before
    );
    assert!(db.companion_epoch(&first.scope_key).await.unwrap() > first.context_epoch);
    assert_eq!(
        db.companion_epoch(&second.scope_key).await.unwrap(),
        second.context_epoch
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn forgetting_removes_shared_source_derivatives_and_model_context_but_keeps_archive() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let turn = successful(&db, user, "一件不想再提起的经历").await;
    let fact = remember(&db, user, &turn, "experience").await;
    let (summary, _) = db
        .upsert_agent_memory(crate::db::AgentMemoryRecord {
            scope_type: "user".into(),
            scope_key: turn.scope_key.clone(),
            memory_key: "episode:old".into(),
            content: "摘要也包含那件经历".into(),
            tags_json: None,
            enabled: true,
            expires_at: None,
            actor: None,
        })
        .await
        .unwrap();
    companion_memory_sources::ActiveModel {
        memory_id: Set(summary.id),
        source_id: Set(format!("turn:{}", turn.id)),
        scope_key: Set(turn.scope_key.clone()),
        detail: Set("summary".into()),
        original: Set("available".into()),
        importance: Set(0.5),
        observed_at: Set(crate::clock::store_time(turn.created_at)),
    }
    .insert(db.orm())
    .await
    .unwrap();
    agent_memory_graphs::ActiveModel {
        scope_type: Set("user".into()),
        scope_key: Set(turn.scope_key.clone()),
        source_fingerprint: Set("old".into()),
        source_limit: Set(64),
        graph_json: Set("{\"old_text\":\"那件经历\"}".into()),
        updated_at: Set(crate::clock::store_time(Utc::now())),
    }
    .insert(db.orm())
    .await
    .unwrap();
    db.apply_companion_deletion(&deletion(
        user,
        vec![turn.scope_key.clone()],
        DeletionKind::Forget {
            memory_ids: vec![fact],
        },
    ))
    .await
    .unwrap();
    assert!(db.get_agent_memory(fact).await.unwrap().is_none());
    assert!(db.get_agent_memory(summary.id).await.unwrap().is_none());
    assert!(
        agent_memory_graphs::Entity::find_by_id(("user".to_string(), turn.scope_key.clone()))
            .one(db.orm())
            .await
            .unwrap()
            .is_none()
    );
    let epoch = db.companion_epoch(&turn.scope_key).await.unwrap();
    assert!(
        db.recent_ai_conversations(&companion_history_key(&turn.scope_key, epoch), 20)
            .await
            .unwrap()
            .is_empty()
    );
    let archive = db
        .assistant_turns_for_card(user, &turn.card_id)
        .await
        .unwrap();
    assert_eq!(archive.len(), 1);
    assert_eq!(archive[0].text, turn.text);
    assert!(!archive[0].retryable);
    assert!(
        agent_runs::Entity::find_by_id(assistant_run_id(user, &turn.id))
            .one(db.orm())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !db.save_companion_memories(
            user,
            &turn.id,
            turn.context_epoch,
            &[MemoryUpsert {
                key: "resurrection".into(),
                content: turn.text.clone(),
                evidence: turn.text.clone(),
                importance: 0.5,
            }]
        )
        .await
        .unwrap()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_clear_erases_all_characters_and_replay_preserves_later_turns() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let first = successful(&db, user, "第一人的旧经历").await;
    remember(&db, user, &first, "first").await;
    let second_card = db
        .create_companion_card(user, CompanionInput::default())
        .await
        .unwrap();
    db.activate_companion_card(user, &second_card.id)
        .await
        .unwrap();
    let second = successful(&db, user, "第二人的旧经历").await;
    remember(&db, user, &second, "second").await;
    let request = deletion(
        user,
        vec![first.scope_key.clone(), second.scope_key.clone()],
        DeletionKind::All,
    );
    db.apply_companion_deletion(&request).await.unwrap();
    for turn in [&first, &second] {
        assert!(
            db.assistant_turns_for_card(user, &turn.card_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.list_agent_memories(Some("user"), Some(&turn.scope_key), None, false, 100)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let new = successful(&db, user, "清除后新建立的经历").await;
    let memory = remember(&db, user, &new, "new").await;
    let remaining = db
        .assistant_account(user, &settings())
        .await
        .unwrap()
        .remaining;
    db.apply_companion_deletion(&request).await.unwrap();
    assert_eq!(
        db.assistant_turns_for_card(user, &new.card_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(db.get_agent_memory(memory).await.unwrap().is_some());
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        remaining
    );
    assert_eq!(db.list_companion_cards(user).await.unwrap().len(), 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn erasing_a_running_turn_refunds_once_and_late_checkpoint_cannot_restore_it() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let completed = successful(&db, user, "已经结算的问候").await;
    let pending = start(&db, user, "尚未完成的秘密").await;
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        28
    );
    let request = deletion(
        user,
        vec![pending.scope_key.clone()],
        DeletionKind::Conversation,
    );
    db.apply_companion_deletion(&request).await.unwrap();
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        29
    );
    agent_runs::ActiveModel {
        id: Set(assistant_run_id(user, &pending.id)),
        conversation_id: Set(None),
        parent_id: Set(None),
        session_id: Set(companion_history_key(
            &pending.scope_key,
            pending.context_epoch,
        )),
        status: Set("completed".into()),
        state_json: Set(serde_json::json!({"reply":"迟到的旧回复"}).to_string()),
        approval_id: Set(None),
        revision: Set(0),
        updated_at: Set(crate::clock::store_time(Utc::now())),
    }
    .insert(db.orm())
    .await
    .unwrap();
    db.settle_assistant_turn(user, &pending.id, TurnFailure::Interrupted)
        .await
        .unwrap();
    db.apply_companion_deletion(&request).await.unwrap();
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        29
    );
    let row = assistant_turns::Entity::find_by_id(&pending.id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert!(row.redacted);
    assert!(row.text.is_empty());
    assert!(row.reply.is_none());
    assert!(
        db.recent_ai_conversations(
            &companion_history_key(&completed.scope_key, completed.context_epoch),
            20
        )
        .await
        .unwrap()
        .is_empty()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn forgotten_archive_cannot_be_reintroduced_by_late_or_future_compression() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let turn = successful(
        &db,
        user,
        "这是一段曾经发生过但现在不想再提起的个人经历和细节。",
    )
    .await;
    let fact = remember(&db, user, &turn, "experience").await;
    assistant_turns::ActiveModel {
        id: Set(turn.id.clone()),
        created_at: Set(crate::clock::store_time(
            Utc::now() - chrono::Duration::days(90),
        )),
        ..Default::default()
    }
    .update(db.orm())
    .await
    .unwrap();
    let scope = crate::db::CompanionRetentionScope {
        user_id: user,
        card_id: turn.card_id.clone(),
        scope_key: turn.scope_key.clone(),
    };
    let batch = db.companion_retention_batch(&scope, 7).await.unwrap();
    assert_eq!(batch.turns.len(), 1);
    db.apply_companion_deletion(&deletion(
        user,
        vec![turn.scope_key.clone()],
        DeletionKind::Forget {
            memory_ids: vec![fact],
        },
    ))
    .await
    .unwrap();
    assert!(
        db.companion_retention_batch(&scope, 7)
            .await
            .unwrap()
            .turns
            .is_empty()
    );
    let summary = nuono_app::agent::companion_memory::CompanionSummary {
        source_turn_ids: vec![turn.id.clone()],
        summary: "旧经历。".into(),
    };
    assert_eq!(
        db.commit_companion_retention(&batch, &summary, 7)
            .await
            .unwrap(),
        0
    );
    assert!(
        db.list_agent_memories(Some("user"), Some(&turn.scope_key), None, false, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.assistant_turns_for_card(user, &turn.card_id)
            .await
            .unwrap()
            .len(),
        1
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn correction_key_collision_rolls_back_all_new_facts_and_control_receipt() {
    let db = crate::test_support::test_db(2).await;
    let user = account(&db).await;
    let previous = successful(&db, user, "我喜欢红茶").await;
    let memory = remember(&db, user, &previous, "tea").await;
    assert_eq!(
        db.get_agent_memory(memory)
            .await
            .unwrap()
            .unwrap()
            .memory_key,
        "tea"
    );
    let before = db
        .assistant_account(user, &settings())
        .await
        .unwrap()
        .remaining;
    let control = start(&db, user, "你记错了，我喜欢咖啡").await;
    assert!(control.control);
    let update = |key: &str| MemoryUpsert {
        key: key.into(),
        content: "我喜欢咖啡".into(),
        evidence: "我喜欢咖啡".into(),
        importance: 0.5,
    };
    assert!(
        db.complete_companion_control(
            user,
            &control.id,
            control.context_epoch,
            "已纠正",
            &[update("new_fact"), update("tea")]
        )
        .await
        .is_err()
    );
    let memories = db
        .list_agent_memories(Some("user"), Some(&control.scope_key), None, false, 100)
        .await
        .unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, previous.text);
    let row = assistant_turns::Entity::find_by_id(&control.id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.text, control.text);
    db.complete_companion_control(
        user,
        &control.id,
        control.context_epoch,
        "请明确要纠正的记忆",
        &[],
    )
    .await
    .unwrap();
    let row = assistant_turns::Entity::find_by_id(&control.id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.text, "记忆调整");
    assert!(row.prompt_snapshot.is_empty());
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        before
    );
    db.shutdown().await.unwrap();
}
