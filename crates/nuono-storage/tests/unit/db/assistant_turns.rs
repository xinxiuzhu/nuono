use super::*;
use nuono_app::{
    agent::store::AgentRunRecord,
    assistant::{TurnDelivery, TurnStatus},
};
use sea_orm::ConnectionTrait;

async fn account(db: &Database, name: &str) -> i64 {
    db.create_user(crate::db::NewUser {
        username: name.into(),
        email: None,
        password: "test-password-with-length".into(),
        role: "user".into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap();
    db.find_user_by_username(name).await.unwrap().unwrap().id
}

fn settings() -> AssistantSettings {
    AssistantSettings {
        enabled: true,
        daily_limit: 2,
        ..Default::default()
    }
}

async fn start(db: &Database, user: i64, key: &str, retry: Option<&str>) -> AssistantTurnAdmission {
    db.start_assistant_turn(
        user,
        &settings(),
        AssistantTurnRequest {
            request_key: key,
            text: retry.is_none().then_some("question"),
            retry_of: retry,
            sender: None,
        },
    )
    .await
    .unwrap()
}

fn accepted(result: AssistantTurnAdmission) -> AssistantTurn {
    match result {
        AssistantTurnAdmission::Accepted(turn) => turn,
        _ => panic!("turn was not admitted"),
    }
}

async fn completed(db: &Database, user: i64, turn: &AssistantTurn) {
    db.create_agent_run(AgentRunRecord {
        id: assistant_run_id(user, &turn.id),
        parent_id: None,
        session_id: history_key(db, user).await,
        status: "completed".into(),
        state_json: serde_json::json!({"reply":"saved answer"}).to_string(),
        approval_id: None,
        revision: 1,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn assistant_failed_turn_refunds_once_and_concurrent_retries_create_one_charge() {
    let db = crate::test_support::test_db(4).await;
    let user = account(&db, "turn-user").await;
    let other = account(&db, "turn-other").await;
    let turn = accepted(start(&db, user, "first", None).await);
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        1
    );
    assert!(matches!(
        start(&db, user, "first", None).await,
        AssistantTurnAdmission::Duplicate
    ));
    assert!(
        db.assistant_request_replayed(user, "first", Some("different"), None)
            .await
            .is_err()
    );
    let failed = db
        .settle_assistant_turn(user, &turn.id, TurnFailure::ModelFailed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, TurnStatus::Failed);
    assert!(failed.retryable);
    db.settle_assistant_turn(user, &turn.id, TurnFailure::Interrupted)
        .await
        .unwrap();
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        2
    );
    assert!(
        db.recent_ai_conversations(&history_key(&db, user).await, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.start_assistant_turn(
            other,
            &settings(),
            AssistantTurnRequest {
                request_key: "steal",
                text: None,
                retry_of: Some(&turn.id),
                sender: None
            }
        )
        .await
        .is_err()
    );
    assert!(db.assistant_turns(other).await.unwrap().is_empty());
    let (a, b) = tokio::join!(
        start(&db, user, "retry-a", Some(&turn.id)),
        start(&db, user, "retry-b", Some(&turn.id))
    );
    let retry = match (a, b) {
        (AssistantTurnAdmission::Accepted(turn), AssistantTurnAdmission::Duplicate)
        | (AssistantTurnAdmission::Duplicate, AssistantTurnAdmission::Accepted(turn)) => turn,
        _ => panic!("concurrent retries must execute exactly once"),
    };
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        1
    );
    assert!(!db.assistant_turns(user).await.unwrap()[0].retryable);
    completed(&db, user, &retry).await;
    for _ in 0..2 {
        db.settle_assistant_turn(user, &retry.id, TurnFailure::Interrupted)
            .await
            .unwrap();
    }
    let history = db
        .recent_ai_conversations(&history_key(&db, user).await, 20)
        .await
        .unwrap();
    assert_eq!(
        history.len(),
        2,
        "failed attempts must not pollute model context"
    );
    assert_eq!(history[0].content, "question");
    assert_eq!(history[1].content, "saved answer");
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        1
    );
    assert!(
        db.begin_assistant_delivery(user, &retry.id, "qq-replay")
            .await
            .unwrap()
    );
    assert!(
        !db.begin_assistant_delivery(user, &retry.id, "qq-replay")
            .await
            .unwrap()
    );
    assert_eq!(
        db.assistant_turns(user).await.unwrap()[1].delivery,
        TurnDelivery::Unknown
    );
    db.confirm_assistant_delivery(user, &retry.id, "qq-replay")
        .await
        .unwrap();
    assert_eq!(
        db.assistant_turns(user).await.unwrap()[1].delivery,
        TurnDelivery::Sent
    );
    assert!(matches!(
        start(&db, user, "first", None).await,
        AssistantTurnAdmission::Duplicate
    ));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn assistant_restart_recovers_success_and_refunds_interrupted_turns_without_replay() {
    let db = crate::test_support::test_db(4).await;
    let interrupted = account(&db, "interrupted-user").await;
    let success = account(&db, "successful-user").await;
    let pending = accepted(start(&db, interrupted, "pending", None).await);
    let done = accepted(start(&db, success, "completed-before-crash", None).await);
    completed(&db, success, &done).await;
    assert_eq!(
        db.reconcile_assistant_turns_before_start().await.unwrap(),
        2
    );
    assert_eq!(
        db.reconcile_assistant_turns_before_start().await.unwrap(),
        0
    );
    let receipt = db.assistant_turns(interrupted).await.unwrap().remove(0);
    assert_eq!(receipt.status, TurnStatus::Interrupted);
    assert_eq!(receipt.failure, Some(TurnFailure::Interrupted));
    assert_eq!(
        db.assistant_account(interrupted, &settings())
            .await
            .unwrap()
            .remaining,
        2
    );
    assert_eq!(
        db.assistant_account(success, &settings())
            .await
            .unwrap()
            .remaining,
        1
    );
    assert_eq!(
        db.assistant_turns(success).await.unwrap()[0]
            .reply
            .as_deref(),
        Some("saved answer")
    );
    assert!(matches!(
        start(&db, interrupted, "pending", None).await,
        AssistantTurnAdmission::Duplicate
    ));
    assert_eq!(
        accepted(start(&db, interrupted, "explicit-retry", Some(&pending.id)).await).text,
        "question"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn assistant_settlement_is_atomic_and_old_day_refunds_do_not_touch_new_usage() {
    let db = crate::test_support::test_db(4).await;
    let user = account(&db, "atomic-user").await;
    let turn = accepted(start(&db, user, "atomic", None).await);
    completed(&db, user, &turn).await;
    db.orm()
        .execute_unprepared("ALTER TABLE nuono_messages RENAME TO fixture_saved_messages")
        .await
        .unwrap();
    assert!(
        db.settle_assistant_turn(user, &turn.id, TurnFailure::ModelFailed)
            .await
            .is_err()
    );
    assert_eq!(
        db.assistant_turns(user).await.unwrap()[0].status,
        TurnStatus::Running
    );
    assert_eq!(
        db.assistant_account(user, &settings())
            .await
            .unwrap()
            .remaining,
        1
    );
    db.orm()
        .execute_unprepared("ALTER TABLE fixture_saved_messages RENAME TO nuono_messages")
        .await
        .unwrap();
    db.settle_assistant_turn(user, &turn.id, TurnFailure::ModelFailed)
        .await
        .unwrap();
    assert_eq!(
        db.recent_ai_conversations(&history_key(&db, user).await, 20)
            .await
            .unwrap()
            .len(),
        2
    );
    let failure = accepted(start(&db, user, "previous-day", None).await);
    turns::ActiveModel {
        id: Set(failure.id.clone()),
        quota_day: Set(Utc::now().timestamp().div_euclid(DAY_SECONDS) - 1),
        ..Default::default()
    }
    .update(db.orm())
    .await
    .unwrap();
    accounts::ActiveModel {
        user_id: Set(user),
        request_ids: Set("[\"today-charge\"]".into()),
        ..Default::default()
    }
    .update(db.orm())
    .await
    .unwrap();
    users::ActiveModel {
        id: Set(user),
        status: Set("disabled".into()),
        ..Default::default()
    }
    .update(db.orm())
    .await
    .unwrap();
    db.settle_assistant_turn(user, &failure.id, TurnFailure::TimedOut)
        .await
        .unwrap();
    assert_eq!(
        db.assistant_turns(user).await.unwrap()[1].failure,
        Some(TurnFailure::TimedOut)
    );
    assert_eq!(
        accounts::Entity::find_by_id(user)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap()
            .request_ids,
        "[\"today-charge\"]"
    );
    db.shutdown().await.unwrap();
}

async fn history_key(db: &Database, user: i64) -> String {
    let card = db.ensure_default_companion_card(user).await.unwrap();
    let scope = companion_scope_key(user, &card.id);
    companion_history_key(&scope, db.companion_epoch(&scope).await.unwrap())
}
