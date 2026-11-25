use super::*;
use sea_orm::ActiveModelTrait;
async fn fixture(db: &Database) -> ScheduledTask {
    crate::tasks::create(
        db,
        "task",
        "0 0 8 * * *",
        "send_message",
        Some(r#"{"target_type":"private","target_id":"1","message":"hello"}"#),
        Some("42"),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn concurrent_duplicate_requests_claim_once_and_edits_conflict() {
    let db = crate::test_support::test_db(4).await;
    let task = fixture(&db).await;
    let id = uuid::Uuid::new_v4().to_string();
    let (a, b) = tokio::join!(
        reserve(&db, &id, &task, "manual"),
        reserve(&db, &id, &task, "manual")
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.1, b.1);
    assert_eq!(a.0.id, b.0.id);
    let (a, b) = tokio::join!(claim(&db, &id), claim(&db, &id));
    assert_ne!(a.unwrap(), b.unwrap());
    finish(&db, &id, "succeeded", "sent").await.unwrap();
    assert!(!reserve(&db, &id, &task, "manual").await.unwrap().1);
    assert!(!claim(&db, &id).await.unwrap());
    let mut changed = task.clone();
    changed.action_params =
        Some(r#"{"target_type":"private","target_id":"2","message":"hello"}"#.into());
    assert!(matches!(
        reserve(&db, &id, &changed, "manual").await,
        Err(NuonoError::Conflict(_))
    ));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn restart_and_expired_history_never_replay_a_manual_request() {
    let db = crate::test_support::test_db(2).await;
    let task = fixture(&db).await;
    let id = uuid::Uuid::new_v4().to_string();
    reserve(&db, &id, &task, "manual").await.unwrap();
    claim(&db, &id).await.unwrap();
    interrupt_unfinished(&db).await.unwrap();
    assert_eq!(
        reserve(&db, &id, &task, "manual").await.unwrap().0.status,
        "unknown"
    );
    assert!(!claim(&db, &id).await.unwrap());
    let row = runs::Entity::find_by_id(&id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    let mut row: runs::ActiveModel = row.into();
    row.created_at = Set(crate::clock::store_time(
        crate::clock::db_now() - chrono::Duration::days(31),
    ));
    row.update(db.orm()).await.unwrap();
    reserve(&db, &uuid::Uuid::new_v4().to_string(), &task, "manual")
        .await
        .unwrap();
    let (old, inserted) = reserve(&db, &id, &task, "manual").await.unwrap();
    assert!(!inserted);
    assert_eq!(old.status, "unknown");
    assert!(old.detail.is_none());
    assert!(
        !list(&db, task.id)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == id)
    );
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn timezone_edit_is_versioned_and_preserves_disabled_state() {
    let db = crate::test_support::test_db(2).await;
    let task = fixture(&db).await;
    assert_eq!(task.timezone, "UTC");
    crate::tasks::toggle(&db, task.id, false).await.unwrap();
    let current = crate::tasks::get(&db, task.id).await.unwrap();
    let mut definition = current.definition();
    definition.timezone = "Asia/Shanghai".into();
    assert!(matches!(
        crate::tasks::update(&db, task.id, task.revision, &definition).await,
        Err(NuonoError::Conflict(_))
    ));
    let edited = crate::tasks::update(&db, task.id, current.revision, &definition)
        .await
        .unwrap();
    assert!(!edited.enabled);
    assert_eq!(edited.timezone, "Asia/Shanghai");
    assert_eq!(edited.revision, current.revision + 1);
    let mut invalid = definition.clone();
    invalid.timezone = "nonsense".into();
    assert!(
        crate::tasks::update(&db, task.id, edited.revision, &invalid)
            .await
            .is_err()
    );
    assert_eq!(
        crate::tasks::get(&db, task.id).await.unwrap().timezone,
        "Asia/Shanghai"
    );
    crate::tasks::toggle(&db, task.id, true).await.unwrap();
    let listed = crate::tasks::list(&db).await.unwrap().remove(0);
    let next = chrono::DateTime::from_timestamp_millis(
        listed.next_run_at.as_ref().unwrap().parse().unwrap(),
    )
    .unwrap();
    assert_eq!(
        next.with_timezone(&chrono::Utc).format("%H:%M").to_string(),
        "00:00"
    );
    db.shutdown().await.unwrap();
}
