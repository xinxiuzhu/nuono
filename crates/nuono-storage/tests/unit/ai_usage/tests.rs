use super::*;
use nuono_ai::usage::{ReportedUsage, UsageContext, UsageOperation, UsageOutcome};
use sea_orm::{ColumnTrait, QueryFilter};
fn event(id: &str, at: DateTime<Utc>, usage: ReportedUsage, outcome: UsageOutcome) -> UsageEvent {
    UsageEvent {
        request_id: id.into(),
        started_at_ms: at.timestamp_millis() as u64,
        elapsed_ms: 42,
        context: UsageContext {
            model_id: Some(7),
            name: "Fixture model".into(),
            provider: "Local fixture".into(),
            source: "agent".into(),
        },
        model: "fixture-chat".into(),
        operation: UsageOperation::Chat,
        outcome,
        usage,
    }
}
#[tokio::test]
async fn real_usage_aggregates_unknown_partial_failure_cancel_and_utc_boundaries() {
    let db = crate::test_support::test_db(2).await;
    let now = DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let observer = db.ai_usage_observer();
    observer.observe(event(
        "known",
        now - Duration::minutes(2),
        ReportedUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            total_tokens: Some(120),
            cached_input_tokens: Some(40),
            reasoning_output_tokens: Some(5),
        },
        UsageOutcome::Succeeded,
    ));
    // 重放相同的请求事件 ID：验证持久化路径上的重试不会导致同一请求被重复统计。
    observer.observe(event(
        "known",
        now - Duration::minutes(2),
        ReportedUsage::default(),
        UsageOutcome::Failed,
    ));
    observer.observe(event(
        "unknown",
        now - Duration::days(1),
        ReportedUsage::default(),
        UsageOutcome::Succeeded,
    ));
    observer.observe(event(
        "failed",
        now - Duration::days(2),
        ReportedUsage::default(),
        UsageOutcome::Failed,
    ));
    observer.observe(event(
        "cancelled",
        now - Duration::days(2),
        ReportedUsage::default(),
        UsageOutcome::Cancelled,
    ));
    let mut embedding = event(
        "embedding",
        now - Duration::days(1),
        ReportedUsage {
            input_tokens: Some(9),
            total_tokens: Some(9),
            ..Default::default()
        },
        UsageOutcome::Succeeded,
    );
    embedding.operation = UsageOperation::Embedding;
    embedding.context.source = "memory_embedding".into();
    observer.observe(embedding);
    // 查询窗口应完整包含窗口起始那一天（UTC 自然日）的全部记录，但排除更早的日期以及晚于当前时间的未来记录。
    observer.observe(event(
        "first-day",
        now - Duration::days(6) - Duration::hours(12),
        ReportedUsage {
            total_tokens: Some(0),
            ..Default::default()
        },
        UsageOutcome::Succeeded,
    ));
    observer.observe(event(
        "outside",
        now - Duration::days(7),
        ReportedUsage {
            total_tokens: Some(999),
            ..Default::default()
        },
        UsageOutcome::Succeeded,
    ));
    observer.observe(event(
        "future",
        now + Duration::seconds(1),
        ReportedUsage {
            total_tokens: Some(999),
            ..Default::default()
        },
        UsageOutcome::Succeeded,
    ));
    let summary = db.ai_usage_summary_at(7, now).await.unwrap();
    assert_eq!(summary.totals.requests, 6);
    assert_eq!(summary.totals.successes, 4);
    assert_eq!(summary.totals.failures, 1);
    assert_eq!(summary.totals.cancelled, 1);
    assert_eq!(summary.totals.known_usage_requests, 3);
    assert_eq!(summary.totals.unknown_usage_requests, 3);
    assert_eq!(summary.totals.input_tokens, Some(109));
    assert_eq!(summary.totals.output_tokens, Some(20));
    assert_eq!(summary.totals.total_tokens, Some(129));
    assert_eq!(summary.totals.cached_input_tokens, Some(40));
    assert_eq!(summary.totals.reasoning_output_tokens, Some(5));
    assert_eq!(summary.totals.total_reported_requests, 3);
    assert_eq!(summary.totals.output_reported_requests, 1);
    assert_eq!(summary.daily.len(), 7);
    assert_eq!(summary.daily[0].counts.total_tokens, Some(0));
    assert!(
        summary
            .daily
            .iter()
            .any(|day| day.counts.requests == 0 && day.counts.total_tokens.is_none())
    );
    assert_eq!(summary.models.len(), 2);
    assert_eq!(summary.unrecorded_requests, 0);
    let row = ai_usage_events::Entity::find()
        .filter(ai_usage_events::Column::RequestId.eq("known"))
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, "succeeded");
    assert_eq!(row.total_tokens, Some(120));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn no_provider_reports_stay_null_and_bad_ranges_are_explicit_errors() {
    let db = crate::test_support::test_db(2).await;
    let summary = db.ai_usage_summary(30).await.unwrap();
    assert_eq!(summary.totals.requests, 0);
    assert_eq!(summary.totals.input_tokens, None);
    assert_eq!(summary.totals.total_tokens, None);
    assert_eq!(summary.daily.len(), 30);
    assert!(db.ai_usage_summary(8).await.is_err());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn requests_in_the_current_millisecond_are_included_without_including_the_next_one() {
    let db = crate::test_support::test_db(2).await;
    let at = DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
    let observer = db.ai_usage_observer();
    observer.observe(event(
        "current",
        at,
        ReportedUsage::default(),
        UsageOutcome::Succeeded,
    ));
    observer.observe(event(
        "next",
        at + Duration::milliseconds(1),
        ReportedUsage::default(),
        UsageOutcome::Succeeded,
    ));
    let summary = db.ai_usage_summary_at(7, at).await.unwrap();
    assert_eq!(summary.totals.requests, 1);
    assert_eq!(summary.totals.unknown_usage_requests, 1);
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn invalid_metadata_is_visible_as_loss_without_discarding_following_valid_records() {
    let db = crate::test_support::test_db(2).await;
    let at = Utc::now() - Duration::seconds(1);
    let observer = db.ai_usage_observer();
    let mut invalid = event("bad", at, ReportedUsage::default(), UsageOutcome::Succeeded);
    invalid.model = "invalid\0model".into();
    observer.observe(invalid);
    observer.observe(event(
        "good",
        at,
        ReportedUsage {
            total_tokens: Some(4),
            ..Default::default()
        },
        UsageOutcome::Succeeded,
    ));
    let summary = db.ai_usage_summary(7).await.unwrap();
    assert_eq!(summary.totals.requests, 1);
    assert_eq!(summary.totals.total_tokens, Some(4));
    assert_eq!(summary.unrecorded_requests, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn excessive_model_groups_are_rejected_explicitly_and_a_shorter_range_still_works() {
    let db = crate::test_support::test_db(2).await;
    db.ensure_ai_module().await.unwrap();
    let at = Utc::now() - Duration::days(10);
    let mut rows = Vec::new();
    for index in 0..2001 {
        rows.push(ai_usage_events::ActiveModel {
            request_id: Set(format!("group-{index}")),
            started_at_ms: Set(at.timestamp_millis()),
            day: Set(at.format("%Y-%m-%d").to_string()),
            model_id: Set(None),
            model: Set(format!("model-{index}")),
            name: Set("Fixture".into()),
            provider: Set("Fixture".into()),
            source: Set("model_test".into()),
            operation: Set("chat".into()),
            outcome: Set("succeeded".into()),
            elapsed_ms: Set(1),
            input_tokens: Set(None),
            output_tokens: Set(None),
            total_tokens: Set(Some(1)),
            cached_input_tokens: Set(None),
            reasoning_output_tokens: Set(None),
        });
    }
    for chunk in rows.chunks(200) {
        ai_usage_events::Entity::insert_many(chunk.iter().cloned())
            .exec(db.orm())
            .await
            .unwrap();
    }
    assert!(
        matches!(db.ai_usage_summary(30).await, Err(NuonoError::Validation(message)) if message.contains("2000") && message.contains("缩短"))
    );
    assert_eq!(db.ai_usage_summary(7).await.unwrap().totals.requests, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_writer_join_is_reported_once_and_does_not_panic_on_shutdown_retry() {
    let db = crate::test_support::test_db(2).await;
    {
        let handle = db.ai_usage.handle.lock().await;
        handle.as_ref().unwrap().abort();
    }
    assert!(db.ai_usage.shutdown().await.is_err());
    db.ai_usage.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}
