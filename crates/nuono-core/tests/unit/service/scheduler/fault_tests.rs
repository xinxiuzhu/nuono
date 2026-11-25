use super::*;
use sea_orm::{ConnectionTrait, TransactionTrait};

async fn definition(db: &Database, name: &str) -> ScheduledTask {
    super::super::tasks::create(
        db,
        name,
        "* * * * * *",
        "send_message",
        Some(r#"{"target_type":"private","target_id":"1","message":"fixture"}"#),
        Some("11"),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn failed_database_reload_preserves_the_running_snapshot_and_recovers() {
    let db = crate::test_support::test_db(1).await;
    let first = definition(&db, "first").await;
    let (scheduler, mut events) = Scheduler::new(db.clone());
    let scheduler = Arc::new(scheduler);
    scheduler.start().await.unwrap();
    db.orm()
        .execute_unprepared("ALTER TABLE nuono_scheduled_tasks RENAME TO unavailable_tasks")
        .await
        .unwrap();
    assert!(scheduler.reload().await.is_err());
    assert_eq!(scheduler.job_count(), 1);
    scheduler.snapshot.lock().unwrap().trigger_due(
        Utc::now() + chrono::Duration::seconds(2),
        &scheduler.event_tx,
    );
    assert_eq!(events.recv().await.unwrap().task_id, first.id);
    db.orm()
        .execute_unprepared("ALTER TABLE unavailable_tasks RENAME TO nuono_scheduled_tasks")
        .await
        .unwrap();
    definition(&db, "second").await;
    scheduler.reload().await.unwrap();
    assert_eq!(scheduler.job_count(), 2);
    scheduler.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_candidate_and_cancelled_database_read_leave_old_jobs_intact() {
    let db = crate::test_support::test_db(1).await;
    let first = definition(&db, "first").await;
    let (scheduler, _) = Scheduler::new(db.clone());
    let scheduler = Arc::new(scheduler);
    scheduler.start().await.unwrap();
    db.orm()
        .execute_unprepared("UPDATE nuono_scheduled_tasks SET cron_expr = 'invalid cron'")
        .await
        .unwrap();
    assert!(matches!(
        scheduler.reload().await,
        Err(NuonoError::Validation(_))
    ));
    assert_eq!(scheduler.job_count(), 1);
    assert_eq!(
        scheduler.snapshot.lock().unwrap().jobs[0].event.task_id,
        first.id
    );
    let transaction = db.orm().begin().await.unwrap();
    let owner = scheduler.clone();
    let reload = tokio::spawn(async move { owner.reload().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !reload.is_finished(),
        "the only database connection must remain occupied"
    );
    reload.abort();
    assert!(reload.await.unwrap_err().is_cancelled());
    transaction.rollback().await.unwrap();
    assert_eq!(scheduler.job_count(), 1);
    scheduler.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn congested_triggers_are_bounded_and_never_replayed_after_capacity_recovers() {
    let db = crate::test_support::test_db(1).await;
    let task = definition(&db, "first").await;
    let start = Utc::now();
    let mut snapshot = ScheduleSnapshot::prepare(vec![task], start).unwrap();
    let (tx, mut events) = mpsc::channel(1);
    for second in 1..=1000 {
        snapshot.trigger_due(start + chrono::Duration::seconds(second), &tx);
    }
    assert_eq!(events.len(), 1);
    events.recv().await.unwrap();
    snapshot.trigger_due(start + chrono::Duration::seconds(1000), &tx);
    assert!(
        events.try_recv().is_err(),
        "full-queue triggers must not wait in detached tasks"
    );
    snapshot.trigger_due(start + chrono::Duration::seconds(1001), &tx);
    assert!(events.try_recv().is_ok());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_reuses_one_ticker_and_shutdown_is_terminal_and_releases_the_owner() {
    let db = crate::test_support::test_db(1).await;
    definition(&db, "first").await;
    let (scheduler, _) = Scheduler::new(db.clone());
    let scheduler = Arc::new(scheduler);
    scheduler.start().await.unwrap();
    let id = scheduler.ticker.lock().unwrap().as_ref().unwrap().id();
    for _ in 0..10 {
        scheduler.reload().await.unwrap();
    }
    assert_eq!(scheduler.ticker.lock().unwrap().as_ref().unwrap().id(), id);
    let mut stop = scheduler.shutdown_receiver();
    scheduler.shutdown().await.unwrap();
    stop.wait_for(|value| *value).await.unwrap();
    assert_eq!(scheduler.job_count(), 0);
    assert!(scheduler.ticker.lock().unwrap().is_none());
    assert!(matches!(
        scheduler.reload().await,
        Err(NuonoError::Conflict(_))
    ));
    let weak = Arc::downgrade(&scheduler);
    drop(scheduler);
    assert!(weak.upgrade().is_none());
    db.shutdown().await.unwrap();
}

#[test]
fn expired_finite_schedules_are_not_replayed_and_candidate_count_is_bounded() {
    let task = ScheduledTask {
        timezone: "UTC".into(),
        revision: 0,
        id: 1,
        name: "finite".into(),
        cron_expr: "0 0 0 * * * 2000".into(),
        action_type: "send_message".into(),
        action_params: Some(
            r#"{"target_type":"private","target_id":"1","message":"hello"}"#.into(),
        ),
        bot_id: None,
        enabled: true,
        last_run_at: None,
        next_run_at: None,
        created_at: None,
    };
    assert!(
        ScheduleSnapshot::prepare(vec![task.clone()], Utc::now())
            .unwrap()
            .jobs
            .is_empty()
    );
    assert!(matches!(
        ScheduleSnapshot::prepare(vec![task; MAX_SCHEDULED_TASKS + 1], Utc::now()),
        Err(NuonoError::Validation(_))
    ));
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_reload_blocked_on_database_capacity() {
    let db = crate::test_support::test_db(1).await;
    definition(&db, "first").await;
    let (scheduler, _) = Scheduler::new(db.clone());
    let scheduler = Arc::new(scheduler);
    scheduler.start().await.unwrap();
    let transaction = db.orm().begin().await.unwrap();
    let owner = scheduler.clone();
    let reload = tokio::spawn(async move { owner.reload().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!reload.is_finished());
    tokio::time::timeout(Duration::from_millis(100), scheduler.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scheduler.job_count(), 0);
    transaction.rollback().await.unwrap();
    assert!(matches!(
        reload.await.unwrap(),
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(scheduler.job_count(), 0);
    db.shutdown().await.unwrap();
}
