//! AI 用量队列关闭测试：验证多个并发调用或中途被取消的 `shutdown` 必须在关闭数据库连接池前排空所有待落盘的用量记录。
use super::*;
use nuono_ai::usage::{ReportedUsage, UsageContext, UsageEvent, UsageOperation, UsageOutcome};
use std::future::{Future, poll_fn};

async fn shutdown_round_trip(interrupt_first_waiter: bool) {
    let mut db = crate::test_support::test_db(2).await;
    let name = db.test_db_name.take().unwrap(); // 取出测试库标识，防止 shutdown 时自动销毁测试库，以便关闭连接池后重新连入校验落盘数据。
    let config = crate::config::DatabaseConfig {
        url: if db.backend() == DbBackend::Sqlite {
            format!("sqlite://{name}?mode=rw")
        } else {
            let mut url = url::Url::parse(
                &std::env::var("NUONO_TEST_DATABASE_URL")
                    .unwrap_or_else(|_| "postgres://nuono:nuono@127.0.0.1:5432/postgres".into()),
            )
            .unwrap();
            url.set_path(&name);
            url.to_string()
        },
        max_connections: 2,
    };
    let observer = db.ai_usage_observer();
    for index in 0..128 {
        observer.observe(UsageEvent {
            request_id: format!("shutdown-{index}"),
            started_at_ms: (chrono::Utc::now() - chrono::Duration::seconds(1)).timestamp_millis()
                as u64,
            elapsed_ms: 1,
            context: UsageContext {
                source: "agent".into(),
                ..Default::default()
            },
            model: "fixture".into(),
            operation: UsageOperation::Chat,
            outcome: UsageOutcome::Succeeded,
            usage: ReportedUsage {
                total_tokens: Some(1),
                ..Default::default()
            },
        });
    }
    if interrupt_first_waiter {
        let mut interrupted = Box::pin(db.shutdown());
        poll_fn(|cx| {
            assert!(interrupted.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(interrupted);
    }
    let other = db.clone();
    let (first, second) = tokio::join!(db.shutdown(), other.shutdown());
    first.unwrap();
    second.unwrap();
    let mut reopened = Database::from_config(&config).await.unwrap();
    reopened.mark_test_db(name);
    let summary = reopened.ai_usage_summary(7).await.unwrap();
    assert_eq!(summary.totals.requests, 128);
    assert_eq!(summary.totals.total_tokens, Some(128));
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn ai_usage_concurrent_database_shutdown_waits_for_all_queued_records() {
    shutdown_round_trip(false).await;
}
#[tokio::test]
async fn ai_usage_cancelled_shutdown_waiter_can_be_resumed_by_another_clone() {
    shutdown_round_trip(true).await;
}
