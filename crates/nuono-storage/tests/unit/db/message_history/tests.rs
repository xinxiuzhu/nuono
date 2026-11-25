use super::*;
use std::future::poll_fn;
use std::task::Poll;
use tokio::sync::oneshot;

fn message(id: &str) -> Value {
    serde_json::json!({"message_id": id, "message": "hello", "raw_message": "hello"})
}

fn limits() -> Limits {
    Limits {
        queue: 2,
        record_bytes: 128,
        retained_bytes: 512,
        write_timeout: Duration::from_secs(5),
        failure_backoff: Duration::ZERO,
    }
}

#[tokio::test]
async fn blocked_history_writer_rejects_overflow_without_waiting_then_recovers() {
    let gate = Arc::new(Semaphore::new(0));
    let (started, mut starts) = mpsc::unbounded_channel();
    let (finished, mut finishes) = mpsc::unbounded_channel();
    let worker_gate = gate.clone();
    let writer = HistoryWriter::start_with(limits(), move |record| {
        let gate = worker_gate.clone();
        let started = started.clone();
        let finished = finished.clone();
        async move {
            started.send(()).unwrap();
            gate.acquire().await.unwrap().forget();
            finished.send(record.message_id.unwrap()).unwrap();
            Ok(())
        }
    });
    writer
        .try_record("bot", &message("first"), "receive")
        .unwrap();
    starts.recv().await.unwrap(); // 确保唯一的后台写入协程已确定性地阻塞在模拟的数据库写入 await 点上。
    writer
        .try_record("bot", &message("second"), "receive")
        .unwrap();
    writer
        .try_record("bot", &message("third"), "receive")
        .unwrap();
    // 队列满后的入队调用必须立即同步失败返回，绝不能让出执行权（yield）或阻塞等待后台写入协程。
    for _ in 0..1000 {
        assert!(
            writer
                .try_record("bot", &message("overflow"), "receive")
                .is_err()
        );
    }
    let saturated = writer.stats();
    assert_eq!(saturated.accepted, 3);
    assert_eq!(saturated.dropped_full, 1000);
    assert!(saturated.retained_payload_bytes <= limits().retained_bytes);
    gate.add_permits(3);
    for id in ["first", "second", "third"] {
        assert_eq!(finishes.recv().await.unwrap(), id);
    }
    writer
        .try_record("bot", &message("recovered"), "receive")
        .unwrap();
    gate.add_permits(1);
    writer.shutdown().await.unwrap();
    assert_eq!(finishes.recv().await.unwrap(), "recovered");
    assert_eq!(writer.stats().written, 4);
    assert_eq!(writer.stats().retained_payload_bytes, 0);
}

#[tokio::test]
async fn history_payload_budget_includes_the_record_already_being_written() {
    let sample = message("1");
    let bytes = build_record("bot", &sample, "receive", 128)
        .unwrap()
        .payload_bytes();
    let mut limits = limits();
    limits.retained_bytes = bytes * 2 - 1;
    let (entered, waiting) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let mut entered = Some(entered);
    let mut gate = Some(gate);
    let writer = HistoryWriter::start_with(limits, move |_| {
        let entered = entered.take();
        let gate = gate.take();
        async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(())
        }
    });
    writer.try_record("bot", &sample, "receive").unwrap();
    waiting.await.unwrap();
    assert_eq!(writer.stats().retained_payload_bytes, bytes);
    assert!(writer.try_record("bot", &sample, "receive").is_err());
    assert_eq!(writer.stats().dropped_byte_budget, 1);
    assert_eq!(writer.stats().dropped_full, 0);
    release.send(()).unwrap();
    writer.shutdown().await.unwrap();
    assert_eq!(writer.stats().retained_payload_bytes, 0);
}

#[tokio::test]
async fn oversized_history_fields_and_json_fallback_are_rejected_before_admission() {
    let writer = HistoryWriter::start_with(limits(), |_| async { Ok(()) });
    for (bot, value) in [
        ("b".repeat(129), message("1")),
        (
            "bot".into(),
            serde_json::json!({"message": "x".repeat(129)}),
        ),
        (
            "bot".into(),
            serde_json::json!({"sender": {"nickname": "x".repeat(129)}}),
        ),
        (
            "bot".into(),
            serde_json::json!({"segments": ["x".repeat(129)]}),
        ),
    ] {
        assert!(writer.try_record(&bot, &value, "receive").is_err());
    }
    assert_eq!(writer.stats().accepted, 0);
    assert_eq!(writer.stats().dropped_oversized, 4);
    assert_eq!(writer.stats().retained_payload_bytes, 0);
    writer.shutdown().await.unwrap();
}

#[test]
fn history_payload_limit_counts_utf8_and_duplicated_json_without_truncating() {
    let message = serde_json::json!({"message": ["中文"]});
    let record = build_record("bot", &message, "receive", 1024).unwrap();
    assert_eq!(record.content, message.to_string());
    assert_eq!(record.raw_message, message.to_string());
    let size = record.payload_bytes();
    assert!(build_record("bot", &message, "receive", size).is_some());
    assert!(build_record("bot", &message, "receive", size - 1).is_none());
    // 当 message 与 raw_message 字段均已存在时，不得将未被使用的额外大字段序列化为临时 JSON 回退字符串。
    let message =
        serde_json::json!({"message": "ok", "raw_message": "ok", "unused": "x".repeat(4096)});
    assert!(build_record("bot", &message, "receive", 32).is_some());
}

#[tokio::test]
async fn history_close_is_shared_and_cancelled_shutdown_can_be_resumed() {
    let (entered, waiting) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let mut entered = Some(entered);
    let mut gate = Some(gate);
    let writer = HistoryWriter::start_with(limits(), move |_| {
        let entered = entered.take();
        let gate = gate.take();
        async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(())
        }
    });
    writer.try_record("bot", &message("1"), "receive").unwrap();
    waiting.await.unwrap();
    writer.try_record("bot", &message("2"), "receive").unwrap();
    let other = writer.clone();
    let mut interrupted = Box::pin(writer.shutdown());
    poll_fn(|cx| {
        assert!(interrupted.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(
        other
            .try_record("bot", &message("late"), "receive")
            .is_err()
    );
    drop(interrupted); // 取消第一个等待者后，后台写入协程及其 JoinHandle 仍保留在共享状态中供后续调用方继续等待。
    release.send(()).unwrap();
    let (first, second) = tokio::join!(writer.shutdown(), other.shutdown());
    first.unwrap();
    second.unwrap();
    assert_eq!(writer.stats().written, 2);
    assert_eq!(writer.stats().dropped_closed, 1);
    assert_eq!(writer.stats().retained_payload_bytes, 0);
}

#[tokio::test]
async fn history_write_failure_is_counted_without_retry_and_next_record_can_succeed() {
    let (failed, after_failure) = oneshot::channel();
    let mut failed = Some(failed);
    let writer = HistoryWriter::start_with(limits(), move |_| {
        let failed = failed.take();
        async move {
            if let Some(failed) = failed {
                let _ = failed.send(());
                Err(NuonoError::DatabaseQuery("injected write failure".into()))
            } else {
                Ok(())
            }
        }
    });
    writer
        .try_record("bot", &message("failed"), "receive")
        .unwrap();
    after_failure.await.unwrap();
    writer
        .try_record("bot", &message("recovered"), "receive")
        .unwrap();
    writer.shutdown().await.unwrap();
    let stats = writer.stats();
    assert_eq!(stats.accepted, 2);
    assert_eq!(stats.written, 1);
    assert_eq!(stats.dropped_write_failed, 1);
    assert_eq!(stats.dropped_timeout, 0);
    assert_eq!(stats.retained_payload_bytes, 0);
}

#[tokio::test]
async fn history_timeout_discards_backlog_during_cooldown_and_shutdown_finishes() {
    let (entered, waiting) = oneshot::channel();
    let mut entered = Some(entered);
    let mut limits = limits();
    limits.write_timeout = Duration::from_millis(10);
    limits.failure_backoff = Duration::from_secs(60);
    let writer = HistoryWriter::start_with(limits, move |_| {
        let entered = entered.take();
        async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            std::future::pending::<Result<()>>().await
        }
    });
    writer.try_record("bot", &message("1"), "receive").unwrap();
    waiting.await.unwrap();
    writer.try_record("bot", &message("2"), "receive").unwrap();
    writer.try_record("bot", &message("3"), "receive").unwrap();
    tokio::time::timeout(Duration::from_secs(2), writer.shutdown())
        .await
        .unwrap()
        .unwrap();
    let stats = writer.stats();
    assert_eq!(stats.dropped_timeout, 1);
    assert_eq!(stats.dropped_backoff, 2);
    assert_eq!(stats.dropped_write_failed, 0);
    assert_eq!(stats.retained_payload_bytes, 0);
}

#[tokio::test]
async fn history_shutdown_drains_real_database_and_rejects_late_records() {
    let db = crate::test_support::test_db(2).await;
    db.save_message_history("bot", &message("preserved"), "receive")
        .await
        .unwrap();
    // 此处仅先单独关闭并排空消息历史写入器，以便在顶层 Database 关闭连接池前直接查询数据库验证落盘结果。
    db.history.shutdown().await.unwrap();
    assert!(
        db.save_message_history("bot", &message("late"), "receive")
            .await
            .is_err()
    );
    let rows = crate::entity::messages::Entity::find()
        .all(db.orm())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].native_message_id.as_deref(), Some("preserved"));
    assert_eq!(rows[0].text_content, "hello");
    assert_eq!(rows[0].direction, "inbound");
    assert_eq!(db.message_history_stats().written, 1);
    assert_eq!(db.message_history_stats().dropped_closed, 1);
    db.shutdown().await.unwrap();
    assert!(
        db.save_message_history("bot", &message("after-close"), "receive")
            .await
            .is_err()
    );
    assert_eq!(db.message_history_stats().dropped_closed, 2);
}
