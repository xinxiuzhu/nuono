use super::*;
use serde_json::json;

fn policy() -> PluginTriggerPolicy {
    PluginTriggerPolicy {
        user_cooldown_ms: 0,
        max_in_flight_per_user: 1,
        group_requests_per_minute: 0,
        group_burst: 3,
    }
}

fn key(plugin: &str, user: &str, group: &str) -> (UserKey, Option<GroupKey>) {
    UserKey::from_event(
        plugin,
        &Event::new(&json!({
            "post_type":"message", "message_type":"group", "user_id":user, "group_id":group,
            "route":{"platform":"onebot","instance_id":"app","bot_self_id":"bot"}
        })),
    )
    .unwrap()
}

fn granted(admission: Admission) -> Arc<InvocationLease> {
    match admission {
        Admission::Granted(lease) => lease,
        _ => panic!("expected admission"),
    }
}

fn denied(admission: &Admission, expected: RejectionReason) {
    assert!(matches!(admission, Admission::Denied { reason, .. } if *reason == expected));
}

#[test]
fn concurrent_calls_atomically_reserve_one_slot_and_release_after_last_owner() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy("p", policy());
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let control = control.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let (key, group) = key("p", "u", "g");
                    let now = Instant::now();
                    control.admit(key, group, now, now)
                })
            })
            .collect();
        barrier.wait();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Admission::Granted(_)))
            .count(),
        1
    );
    drop(results);
    let (key, group) = key("p", "u", "g");
    let now = Instant::now();
    let _lease = granted(control.admit(key, group, now, now));
}

#[test]
fn cooldown_uses_receipt_time_and_queued_duplicates_do_not_run_after_completion() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 2_000,
            ..policy()
        },
    );
    let (key, group) = key("p", "u", "g");
    let now = Instant::now();
    let first = granted(control.admit(key.clone(), group.clone(), now, now));
    drop(first);
    denied(
        &control.admit(
            key.clone(),
            group.clone(),
            now,
            now + Duration::from_secs(20),
        ),
        RejectionReason::InFlight,
    );
    denied(
        &control.admit(
            key.clone(),
            group.clone(),
            now + Duration::from_secs(1),
            now + Duration::from_secs(20),
        ),
        RejectionReason::Cooldown,
    );
    let _lease = granted(control.admit(
        key,
        group,
        now + Duration::from_secs(2),
        now + Duration::from_secs(20),
    ));
}

#[test]
fn group_budget_is_shared_across_users_and_refills_without_a_fixed_minute_reset() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            group_requests_per_minute: 30,
            group_burst: 2,
            ..policy()
        },
    );
    let now = Instant::now();
    for user in ["a", "b"] {
        let (key, group) = key("p", user, "g");
        let _lease = granted(control.admit(key, group, now, now));
    }
    let (user, group) = key("p", "c", "g");
    denied(
        &control.admit(user.clone(), group.clone(), now, now),
        RejectionReason::GroupBudget,
    );
    denied(
        &control.admit(
            user.clone(),
            group.clone(),
            now + Duration::from_millis(1_999),
            now + Duration::from_millis(1_999),
        ),
        RejectionReason::GroupBudget,
    );
    let at = now + Duration::from_secs(2);
    let _lease = granted(control.admit(user, group, at, at));
    let (key, group) = key("p", "a", "other-group");
    let _other = granted(control.admit(key, group, at, at));
}

#[test]
fn refusal_does_not_charge_user_cd_or_another_group_token() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 5_000,
            group_requests_per_minute: 1,
            group_burst: 1,
            ..policy()
        },
    );
    let now = Instant::now();
    let (a, group) = key("p", "a", "g");
    let _a = granted(control.admit(a.clone(), group.clone(), now, now));
    denied(
        &control.admit(a, group.clone(), now, now),
        RejectionReason::InFlight,
    );
    let (b, _) = key("p", "b", "g");
    denied(
        &control.admit(b.clone(), group.clone(), now, now),
        RejectionReason::GroupBudget,
    );
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 5_000,
            ..policy()
        },
    );
    let _b = granted(control.admit(b, group, now, now));
}

#[test]
fn changing_policy_does_not_refill_tokens_or_cancel_existing_work() {
    let control = Arc::new(TriggerControl::default());
    let initial = PluginTriggerPolicy {
        group_requests_per_minute: 1,
        group_burst: 1,
        ..policy()
    };
    control.set_policy("p", initial);
    let now = Instant::now();
    let (a, group) = key("p", "a", "g");
    let active = granted(control.admit(a.clone(), group.clone(), now, now));
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 500,
            ..initial
        },
    );
    denied(
        &control.admit(a, group.clone(), now, now),
        RejectionReason::InFlight,
    );
    let (b, _) = key("p", "b", "g");
    denied(
        &control.admit(b, group, now, now),
        RejectionReason::GroupBudget,
    );
    assert_eq!(Arc::strong_count(&active), 1);
}

#[test]
fn hot_policy_change_uses_new_refill_rate_and_retains_a_lengthened_cooldown() {
    let control = Arc::new(TriggerControl::default());
    let now = Instant::now();
    control.set_policy_at(
        "p",
        PluginTriggerPolicy {
            group_requests_per_minute: 1,
            group_burst: 2,
            ..policy()
        },
        now,
    );
    for user in ["a", "b"] {
        let (key, group) = key("p", user, "g");
        drop(granted(control.admit(key, group, now, now)));
    }
    control.set_policy_at(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 600_000,
            group_requests_per_minute: 120,
            group_burst: 2,
            ..policy()
        },
        now,
    );
    let (c, group) = key("p", "c", "g");
    let half_second = now + Duration::from_millis(500);
    drop(granted(control.admit(
        c,
        group.clone(),
        half_second,
        half_second,
    )));
    let (a, _) = key("p", "a", "g");
    let after_cleanup = now + Duration::from_secs(301);
    denied(
        &control.admit(a, group, after_cleanup, after_cleanup),
        RejectionReason::Cooldown,
    );
}

#[test]
fn delayed_group_admission_uses_one_clock_and_cannot_double_refill_a_burst() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            group_requests_per_minute: 3,
            group_burst: 3,
            ..policy()
        },
    );
    let received = Instant::now();
    let (a, group) = key("p", "a", "g");
    drop(granted(control.admit(a, group.clone(), received, received)));
    let processed = received + Duration::from_secs(61);
    for user in ["b", "c", "d"] {
        let (key, group) = key("p", user, "g");
        drop(granted(control.admit(key, group, received, processed)));
    }
    let (e, _) = key("p", "e", "g");
    denied(
        &control.admit(e, group, processed, processed),
        RejectionReason::GroupBudget,
    );
}

#[test]
fn silent_refusals_do_not_take_the_next_commands_notice_window() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy("p", policy());
    let (key, group) = key("p", "a", "g");
    let now = Instant::now();
    let _active = granted(control.admit(key.clone(), group.clone(), now, now));
    let silent = control.admit(key.clone(), group.clone(), now, now);
    assert!(matches!(silent, Admission::Denied { .. }));
    let command = control.admit(key.clone(), group.clone(), now, now);
    let Admission::Denied { notice, .. } = command else {
        panic!()
    };
    assert!(notice.claim());
    let later_command = control.admit(key, group, now, now);
    let Admission::Denied { notice, .. } = later_command else {
        panic!()
    };
    assert!(!notice.claim());
}

#[test]
fn queued_receipt_preserves_completed_evidence_after_idle_cleanup() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy("p", policy());
    let received = Instant::now();
    *control.1.lock().unwrap().entry(received).or_default() += 1;
    let ticket = QueuedReceipt {
        control: control.clone(),
        received_at: received,
    };
    let (key, group) = key("p", "a", "g");
    drop(granted(control.admit(
        key.clone(),
        group.clone(),
        received,
        received,
    )));
    let later = received + Duration::from_secs(301);
    denied(
        &control.admit(key.clone(), group.clone(), received, later),
        RejectionReason::InFlight,
    );
    drop(ticket);
    assert!(control.1.lock().unwrap().is_empty());
    let _new = granted(control.admit(key, group, later, later));
}

#[test]
fn disabled_group_buckets_expire_even_under_frequent_cleanup() {
    let control = Arc::new(TriggerControl::default());
    let now = Instant::now();
    control.set_policy_at(
        "p",
        PluginTriggerPolicy {
            group_requests_per_minute: 30,
            ..policy()
        },
        now,
    );
    let (user, group) = key("p", "u", "g");
    drop(granted(control.admit(user, group.clone(), now, now)));
    control.set_policy_at("p", policy(), now);
    let mut state = control.0.lock().unwrap();
    for seconds in [20, 40] {
        state.cleanup(now + Duration::from_secs(seconds), None);
        assert!(state.groups.contains_key(group.as_ref().unwrap()));
    }
    state.cleanup(now + IDLE_RETENTION, None);
    assert!(state.groups.is_empty());
}

#[test]
fn long_cooldowns_survive_cleanup_and_capacity_never_evicts_live_leases() {
    let control = Arc::new(TriggerControl::default());
    control.set_policy(
        "p",
        PluginTriggerPolicy {
            user_cooldown_ms: 600_000,
            ..policy()
        },
    );
    let now = Instant::now();
    let (a, group) = key("p", "a", "g");
    drop(granted(control.admit(a.clone(), group.clone(), now, now)));
    let later = now + Duration::from_secs(301);
    denied(
        &control.admit(a, group, later, later),
        RejectionReason::Cooldown,
    );
    let mut state = control.0.lock().unwrap();
    let (template, _) = key("p", "live", "g");
    for i in 0..MAX_ENTRIES {
        let mut key = template.clone();
        key.actor = i.to_string();
        state.users.insert(
            key,
            UserState {
                active: 1,
                last_received: None,
                completed_at: None,
                last_notice: None,
                keep_until: now,
            },
        );
    }
    drop(state);
    let (new, group) = key("p", "new", "g");
    denied(
        &control.admit(new, group, later, later),
        RejectionReason::Capacity,
    );
    assert!(
        control
            .0
            .lock()
            .unwrap()
            .users
            .values()
            .filter(|user| user.active == 1)
            .count()
            >= MAX_ENTRIES
    );
}

#[test]
fn complete_native_scope_separates_apps_bots_channels_users_and_plugins() {
    let base = json!({"post_type":"message", "message_type":"channel", "user_id":"0001", "channel_id":"room", "guild_id":"guild", "route":{"platform":"discord","instance_id":"app","bot_self_id":"bot"}});
    let original = UserKey::from_event("p", &Event::new(&base)).unwrap();
    for pointer in [
        "/route/platform",
        "/route/instance_id",
        "/route/bot_self_id",
        "/user_id",
        "/channel_id",
        "/guild_id",
    ] {
        let mut changed = base.clone();
        *changed.pointer_mut(pointer).unwrap() = json!("different");
        assert_ne!(
            original.0,
            UserKey::from_event("p", &Event::new(&changed)).unwrap().0,
            "{pointer}"
        );
    }
    assert_ne!(
        original.0,
        UserKey::from_event("other", &Event::new(&base)).unwrap().0
    );
    assert_eq!(original.0.actor, "0001");
    let mut private = base;
    private["message_type"] = json!("private");
    assert!(
        UserKey::from_event("p", &Event::new(&private))
            .unwrap()
            .1
            .is_none()
    );
}

#[tokio::test]
async fn one_message_reuses_admission_and_only_one_notice_is_claimed() {
    let control = Arc::new(TriggerControl::default());
    let now = Instant::now();
    let (key, group) = key("p", "u", "g");
    let _active = granted(control.admit(key.clone(), group.clone(), now, now));
    let denied = control.admit(key.clone(), group.clone(), now, now);
    let Admission::Denied { notice, .. } = denied.clone() else {
        panic!()
    };
    assert!(notice.claim());
    let Admission::Denied { notice, .. } = denied else {
        panic!()
    };
    assert!(!notice.claim());
    let Admission::Denied { notice, .. } = control.admit(key, group, now, now) else {
        panic!()
    };
    assert!(!notice.claim());
    with_message_admission(async {
        MESSAGE_ADMISSION.with(|message| assert!(message.admitted.lock().unwrap().is_empty()));
        with_message_admission(async {
            MESSAGE_ADMISSION.with(|message| assert!(message.admitted.lock().unwrap().is_empty()));
        })
        .await;
    })
    .await;
}
