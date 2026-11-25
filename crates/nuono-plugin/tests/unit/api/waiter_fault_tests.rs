use super::*;
use serde_json::json;
use std::{sync::Barrier, thread};

const LONG_WAIT: Duration = Duration::from_secs(60);

fn owned(plugin: &str) -> WaitScope {
    WaitScope {
        plugin_name: Some(plugin.to_owned()),
        ..Default::default()
    }
}

fn entries(waits: &WaitManager) -> usize {
    // 直接读取内部保留状态，避免隐式触发定期清理或列表遍历清理逻辑。
    waits.registry.state.lock().unwrap().entries.len()
}

#[test]
fn default_budget_is_shared_by_raw_context_and_scoped_registrations() {
    let waits = WaitManager::new();
    let active: Vec<_> = (0..DEFAULT_WAIT_CAPACITY)
        .map(|index| {
            waits
                .try_register_wait(index.to_string(), LONG_WAIT)
                .unwrap()
        })
        .collect();
    assert_eq!(entries(&waits), 1024);
    assert!(matches!(
        waits.try_register_event_wait("event".into(), LONG_WAIT),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        waits.try_register_scoped_wait("raw".into(), LONG_WAIT, owned("first")),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        waits.try_register_scoped_event_wait("scoped".into(), LONG_WAIT, owned("second")),
        Err(NuonoError::Overloaded(_))
    ));
    drop(active);
    assert_eq!(entries(&waits), 0);
    assert!(
        waits
            .try_register_event_wait("fresh".into(), LONG_WAIT)
            .is_ok()
    );
}

#[test]
fn concurrent_distinct_registrations_never_exceed_capacity() {
    let waits = Arc::new(WaitManager::with_capacity(8));
    let ready = Arc::new(Barrier::new(33));
    let jobs: Vec<_> = (0..32)
        .map(|index| {
            let waits = waits.clone();
            let ready = ready.clone();
            thread::spawn(move || {
                ready.wait();
                match waits.try_register_wait(index.to_string(), LONG_WAIT) {
                    Ok(registration) => Some(registration),
                    Err(NuonoError::Overloaded(_)) => None,
                    Err(error) => panic!("unexpected admission error: {error}"),
                }
            })
        })
        .collect();
    ready.wait();
    let active: Vec<_> = jobs
        .into_iter()
        .filter_map(|job| job.join().unwrap())
        .collect();
    assert_eq!(active.len(), 8);
    assert_eq!(entries(&waits), 8);
    drop(active);
    assert_eq!(entries(&waits), 0);
}

#[test]
fn replacement_at_capacity_is_atomic_and_old_drops_preserve_the_winner() {
    let waits = Arc::new(WaitManager::with_capacity(1));
    let ready = Arc::new(Barrier::new(17));
    let jobs: Vec<_> = (0..16)
        .map(|_| {
            let waits = waits.clone();
            let ready = ready.clone();
            thread::spawn(move || {
                ready.wait();
                waits.try_register_wait("same".into(), LONG_WAIT).unwrap()
            })
        })
        .collect();
    ready.wait();
    let registrations: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    let generation = waits.snapshot("same").unwrap().generation;
    let mut current = None;
    for registration in registrations {
        if registration.snapshot().generation == generation {
            current = Some(registration);
        } else {
            assert!(!registration.is_active());
            drop(registration);
        }
    }
    let mut current = current.unwrap();
    assert!(current.is_active());
    assert_eq!(entries(&waits), 1);
    assert!(waits.respond("same", json!({"answer":42})));
    assert_eq!(current.try_recv().unwrap(), json!({"answer":42}));
    assert_eq!(entries(&waits), 0);
}

#[test]
fn rejected_admission_never_replaces_live_wait_or_notifies_a_new_registration() {
    let waits = WaitManager::with_capacity(1);
    let original = waits.try_register_wait("live".into(), LONG_WAIT).unwrap();
    let snapshot = original.snapshot().clone();
    let changes = waits.subscribe_changes();
    assert!(matches!(
        waits.try_register_wait("excess".into(), LONG_WAIT),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(!changes.has_changed().unwrap());
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(matches!(
            waits.try_register_wait("live".into(), timeout),
            Err(NuonoError::Validation(_))
        ));
        assert_eq!(
            waits.snapshot("live").unwrap().generation,
            snapshot.generation
        );
        assert!(original.is_active());
        assert!(!changes.has_changed().unwrap());
    }
    let mut rejected = waits.register_wait("legacy-excess".into(), LONG_WAIT);
    assert!(matches!(
        rejected.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert_eq!(entries(&waits), 1);
}

#[test]
fn zero_capacity_and_invalid_event_timeouts_fail_closed_without_a_runtime() {
    let waits = WaitManager::with_capacity(0);
    assert!(matches!(
        waits.try_register_wait("raw".into(), LONG_WAIT),
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        waits.try_register_scoped_event_wait("event".into(), Duration::MAX, owned("plugin")),
        Err(NuonoError::Validation(_))
    ));
    let mut legacy = waits.register_event_wait("legacy".into(), Duration::MAX);
    assert!(matches!(
        legacy.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert_eq!(entries(&waits), 0);
}

#[tokio::test(start_paused = true)]
async fn direct_await_expires_itself_without_host_maintenance() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits
        .try_register_wait("direct".into(), Duration::from_secs(5))
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), registration).await;
    assert!(
        outcome
            .expect("registration ignored its own deadline")
            .is_err()
    );
    assert_eq!(entries(&waits), 0);
    assert!(
        waits
            .try_register_wait("replacement".into(), LONG_WAIT)
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn receive_result_reports_own_deadline_as_typed_timeout() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits
        .try_register_event_wait("typed".into(), Duration::from_secs(5))
        .unwrap();
    assert!(matches!(
        registration.receive(LONG_WAIT).await,
        Err(NuonoError::Timeout(_))
    ));
    assert_eq!(entries(&waits), 0);
}

#[tokio::test(start_paused = true)]
async fn shorter_receive_timeout_releases_capacity_without_waiting_for_registered_deadline() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits.try_register_wait("short".into(), LONG_WAIT).unwrap();
    assert!(matches!(
        registration.receive(Duration::from_secs(3)).await,
        Err(NuonoError::Timeout(_))
    ));
    assert_eq!(entries(&waits), 0);
    assert!(waits.try_register_wait("fresh".into(), LONG_WAIT).is_ok());
}

#[tokio::test(start_paused = true)]
async fn try_recv_after_expiry_closes_and_releases_an_unpolled_registration() {
    let waits = WaitManager::with_capacity(1);
    let mut registration = waits
        .try_register_wait("unpolled".into(), Duration::from_secs(5))
        .unwrap();
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        registration.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert_eq!(entries(&waits), 0);
    assert!(waits.try_register_wait("fresh".into(), LONG_WAIT).is_ok());
}

#[tokio::test(start_paused = true)]
async fn admission_reclaims_unpolled_expired_wait_before_capacity_check() {
    let waits = WaitManager::with_capacity(1);
    let expired = waits
        .try_register_wait("expired".into(), Duration::from_secs(5))
        .unwrap();
    tokio::time::advance(Duration::from_secs(6)).await;
    let fresh = waits.try_register_wait("fresh".into(), LONG_WAIT).unwrap();
    assert!(fresh.is_active());
    assert_eq!(entries(&waits), 1);
    assert!(matches!(expired.cancelled().await, NuonoError::Timeout(_)));
    assert!(matches!(
        expired.receive(LONG_WAIT).await,
        Err(NuonoError::Timeout(_))
    ));
    assert!(fresh.is_active());
}

#[tokio::test(start_paused = true)]
async fn cancelled_watcher_covers_cancel_replace_plugin_stop_and_clear() {
    for action in ["cancel", "replace", "plugin", "clear"] {
        let waits = WaitManager::with_capacity(2);
        let registration = waits
            .try_register_scoped_wait("session".into(), LONG_WAIT, owned("owner"))
            .unwrap();
        let untouched = waits
            .try_register_scoped_wait("peer".into(), LONG_WAIT, owned("peer"))
            .unwrap();
        let cancelled = registration.cancelled();
        tokio::pin!(cancelled);
        assert!(futures::poll!(&mut cancelled).is_pending());
        let replacement = match action {
            "cancel" => {
                assert!(waits.cancel("session"));
                None
            }
            "replace" => Some(
                waits
                    .try_register_scoped_wait("session".into(), LONG_WAIT, owned("owner"))
                    .unwrap(),
            ),
            "plugin" => {
                waits.cancel_plugin("owner");
                None
            }
            "clear" => {
                waits.clear();
                None
            }
            _ => unreachable!(),
        };
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), &mut cancelled)
                .await
                .unwrap(),
            NuonoError::Conflict(_)
        ));
        if action != "clear" {
            assert!(untouched.is_active());
        }
        if let Some(replacement) = replacement {
            assert!(replacement.is_active());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn early_reply_is_preserved_and_does_not_signal_prompt_cancellation() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits
        .try_register_wait("session".into(), LONG_WAIT)
        .unwrap();
    assert!(waits.respond("session", json!({"early":true})));
    let replacement = waits
        .try_register_wait("session".into(), LONG_WAIT)
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), registration.cancelled())
            .await
            .is_err()
    );
    assert_eq!(
        registration.receive(LONG_WAIT).await.unwrap(),
        json!({"early":true})
    );
    assert!(replacement.is_active());
}

#[tokio::test(start_paused = true)]
async fn even_an_early_reply_keeps_absolute_deadline_for_a_stuck_prompt() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits
        .try_register_wait("session".into(), Duration::from_secs(5))
        .unwrap();
    assert!(waits.respond("session", json!({"early":true})));
    assert!(matches!(
        registration.cancelled().await,
        NuonoError::Timeout(_)
    ));
    assert_eq!(
        registration.receive(LONG_WAIT).await.unwrap(),
        json!({"early":true})
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_remains_conflict_even_when_receiver_is_polled_after_deadline() {
    let waits = WaitManager::with_capacity(1);
    let registration = waits
        .try_register_wait("session".into(), Duration::from_secs(5))
        .unwrap();
    assert!(waits.cancel("session"));
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        registration.receive(LONG_WAIT).await,
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(entries(&waits), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelled_and_replaced_futures_cannot_remove_a_new_generation() {
    let waits = Arc::new(WaitManager::with_capacity(1));
    let old = waits
        .try_register_wait("session".into(), Duration::from_secs(1))
        .unwrap();
    let old_snapshot = old.snapshot().clone();
    let task = tokio::spawn(old.receive(LONG_WAIT));
    tokio::task::yield_now().await;
    let current = waits
        .try_register_wait("session".into(), LONG_WAIT)
        .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!waits.cancel_if_current(&old_snapshot));
    assert!(current.is_active());
    assert_eq!(entries(&waits), 1);
}

#[tokio::test]
async fn cancelling_a_running_receive_releases_budget_before_the_next_registration() {
    let waits = Arc::new(WaitManager::with_capacity(1));
    let registration = waits
        .try_register_wait("running".into(), LONG_WAIT)
        .unwrap();
    let task = tokio::spawn(registration.receive(LONG_WAIT));
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(entries(&waits), 0);
    assert!(waits.try_register_wait("next".into(), LONG_WAIT).is_ok());
}

#[test]
fn disabling_one_owner_releases_only_its_budget_and_blocks_late_admission() {
    let waits = WaitManager::with_capacity(2);
    let first = waits
        .try_register_scoped_wait("first".into(), LONG_WAIT, owned("first-owner"))
        .unwrap();
    let peer = waits
        .try_register_scoped_wait("peer".into(), LONG_WAIT, owned("peer-owner"))
        .unwrap();
    waits.cancel_plugin("first-owner");
    assert!(matches!(
        waits.try_register_scoped_wait("late".into(), LONG_WAIT, owned("first-owner")),
        Err(NuonoError::Conflict(_))
    ));
    let mut first = first;
    assert!(matches!(
        first.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert!(peer.is_active());
    waits.enable_plugin("first-owner");
    let fresh = waits
        .try_register_scoped_wait("fresh".into(), LONG_WAIT, owned("first-owner"))
        .unwrap();
    assert!(fresh.is_active());
    assert_eq!(entries(&waits), 2);
}

#[test]
fn cancellation_and_delivery_race_has_exactly_one_winner_and_releases_capacity() {
    for _ in 0..32 {
        let waits = Arc::new(WaitManager::with_capacity(1));
        let mut registration = waits.try_register_wait("race".into(), LONG_WAIT).unwrap();
        let snapshot = registration.snapshot().clone();
        let start = Arc::new(Barrier::new(3));
        let respond = thread::spawn({
            let waits = waits.clone();
            let start = start.clone();
            let snapshot = snapshot.clone();
            move || {
                start.wait();
                waits.respond_if_current(
                    &snapshot,
                    WaitEvent {
                        raw: json!({"won":true}),
                        domain: None,
                        locale: Default::default(),
                    },
                )
            }
        });
        let cancel = thread::spawn({
            let waits = waits.clone();
            let start = start.clone();
            move || {
                start.wait();
                waits.cancel_if_current(&snapshot)
            }
        });
        start.wait();
        let sent = respond.join().unwrap();
        let cancelled = cancel.join().unwrap();
        assert_ne!(sent, cancelled);
        if sent {
            assert_eq!(registration.try_recv().unwrap(), json!({"won":true}));
        } else {
            assert!(matches!(
                registration.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ));
        }
        assert_eq!(entries(&waits), 0);
        assert!(waits.try_register_wait("fresh".into(), LONG_WAIT).is_ok());
    }
}

#[tokio::test]
async fn waiter_preserves_typed_admission_errors() {
    let waits = Arc::new(WaitManager::with_capacity(1));
    let active = waits
        .try_register_wait("occupied".into(), LONG_WAIT)
        .unwrap();
    let waiter = Waiter::new("other".into(), waits.clone());
    let error = waiter.wait(LONG_WAIT).await.unwrap_err();
    assert!(matches!(error, NuonoError::Overloaded(_)));
    let error = waiter.wait_event(LONG_WAIT).await.unwrap_err();
    assert!(matches!(error, NuonoError::Overloaded(_)));
    assert!(active.is_active());
}
