use super::*;
use crate::core::permission::ActorPermission;
use nuono_plugin::api::waiter::{WaitRegistration, WaitScope};

fn route() -> crate::core::RuntimeBotRoute {
    crate::core::RuntimeBotRoute::new("telegram", "wait-cleanup", "42")
}

fn request(action: InteractionAction) -> InteractionRegistration {
    InteractionRegistration {
        plugin: "wait-cleanup".into(),
        target: RuntimeMessageTarget::group(route(), "100"),
        actor_id: "7".into(),
        origin_chat_id: None,
        permission: ButtonPermission::All,
        allowed_user_ids: vec![],
        action,
        ttl_secs: 300,
    }
}

fn reply_request() -> InteractionRegistration {
    request(InteractionAction::Reply {
        value: "answer".into(),
    })
}

fn callback_request() -> InteractionRegistration {
    request(InteractionAction::Callback {
        callback_id: "independent".into(),
        data: Value::Null,
    })
}

fn register(waits: &WaitManager, timeout: Duration) -> WaitRegistration<Value> {
    waits.register_scoped_wait(
        "42:100:7".into(),
        timeout,
        WaitScope {
            route: Some(route()),
            plugin_name: Some("wait-cleanup".into()),
            user_id: "7".into(),
            ..Default::default()
        },
    )
}

fn click(token: &str) -> Value {
    json!({
        "post_type":"interaction", "route":route(), "user_id":"7", "self_id":"42",
        "message_type":"group", "group_id":"100", "interaction_id":"cleanup-click",
        "interaction_token":token
    })
}

fn fill_with_reply_choices(registry: &InteractionRegistry, reply: &str) {
    let mut state = registry.0.lock().unwrap();
    let entry = state.pending.get(reply).unwrap().clone();
    // 单次交互提问可能会为同一个等待会话注册多个选项。
    // 这里直接填满有界注册表，避免该故障测试桩反复全表扫描。
    for index in state.pending.len()..MAX_PENDING {
        state
            .pending
            .insert(format!("old-choice-{index}"), entry.clone());
    }
}

#[test]
fn ended_reply_waits_return_button_capacity_before_admission_and_keep_independent_buttons() {
    for reason in ["cancel", "drop", "replace", "respond"] {
        let waits = Arc::new(WaitManager::new());
        let registry = InteractionRegistry::with_wait_manager(&waits);
        let mut registration = Some(register(&waits, Duration::from_secs(60)));
        let snapshot = registration.as_ref().unwrap().snapshot().clone();
        let reply = registry
            .insert(reply_request(), Some(snapshot.clone()))
            .unwrap();
        let command = registry
            .insert(
                request(InteractionAction::Command {
                    command: "ping".into(),
                }),
                None,
            )
            .unwrap();
        let callback = registry.insert(callback_request(), None).unwrap();
        assert!(
            registry
                .claim(&click(&command), ActorPermission::everyone())
                .is_some()
        );
        fill_with_reply_choices(&registry, &reply);
        assert!(registry.insert(callback_request(), None).is_err());

        let _replacement = match reason {
            "cancel" => {
                assert!(waits.cancel_if_current(&snapshot));
                None
            }
            "drop" => {
                drop(registration.take());
                None
            }
            "replace" => Some(register(&waits, Duration::from_secs(60))),
            "respond" => {
                assert!(waits.respond(&snapshot.session_id, json!({"answer":true})));
                None
            }
            _ => unreachable!(),
        };
        assert!(
            registry.insert(callback_request(), None).is_ok(),
            "{reason}"
        );
        let state = registry.0.lock().unwrap();
        assert_eq!(state.pending.len(), 3, "{reason}");
        assert!(state.pending.contains_key(&command), "{reason}");
        assert!(state.pending.contains_key(&callback), "{reason}");
        assert_eq!(
            state.used_clicks.len(),
            1,
            "click deduplication must remain"
        );
    }
}

#[tokio::test]
async fn expired_wait_reclaims_reply_choices_before_button_capacity_is_checked() {
    let waits = Arc::new(WaitManager::new());
    let registry = InteractionRegistry::with_wait_manager(&waits);
    let registration = register(&waits, Duration::from_millis(20));
    let reply = registry
        .insert(reply_request(), Some(registration.snapshot().clone()))
        .unwrap();
    fill_with_reply_choices(&registry, &reply);
    tokio::time::sleep_until(tokio::time::Instant::from_std(registration.deadline())).await;

    assert!(registry.insert(callback_request(), None).is_ok());
    assert_eq!(registry.0.lock().unwrap().pending.len(), 1);
    assert!(!registration.is_active());
}

#[test]
fn reply_classification_discards_choices_after_a_reply_has_already_arrived() {
    let waits = Arc::new(WaitManager::new());
    let registry = InteractionRegistry::with_wait_manager(&waits);
    let mut registration = register(&waits, Duration::from_secs(60));
    let snapshot = registration.snapshot().clone();
    let reply = registry
        .insert(reply_request(), Some(snapshot.clone()))
        .unwrap();
    assert!(registry.is_reply(&click(&reply)));

    assert!(waits.respond(&snapshot.session_id, json!({"answer":"early"})));
    assert!(!registry.is_reply(&click(&reply)));
    assert!(registry.0.lock().unwrap().pending.is_empty());
    assert_eq!(registration.try_recv().unwrap(), json!({"answer":"early"}));
}

#[test]
fn claiming_stale_choices_cannot_remove_or_answer_the_replacement_generation() {
    let waits = Arc::new(WaitManager::new());
    let registry = InteractionRegistry::with_wait_manager(&waits);
    let old = register(&waits, Duration::from_secs(60));
    let old_reply = registry
        .insert(reply_request(), Some(old.snapshot().clone()))
        .unwrap();
    let replacement = register(&waits, Duration::from_secs(60));
    let new_reply = registry
        .insert(reply_request(), Some(replacement.snapshot().clone()))
        .unwrap();

    assert!(
        registry
            .claim(&click(&old_reply), ActorPermission::everyone())
            .is_none()
    );
    assert!(replacement.is_active());
    let claimed = registry
        .claim(&click(&new_reply), ActorPermission::everyone())
        .unwrap();
    assert_eq!(
        claimed.wait.unwrap().generation,
        replacement.snapshot().generation
    );
    assert!(
        replacement.is_active(),
        "claiming a choice does not consume the wait"
    );
}

#[test]
fn ownership_checks_discard_cancelled_reply_choices_and_keep_command_ownership() {
    let waits = Arc::new(WaitManager::new());
    let registry = InteractionRegistry::with_wait_manager(&waits);
    let registration = register(&waits, Duration::from_secs(60));
    let reply = registry
        .insert(reply_request(), Some(registration.snapshot().clone()))
        .unwrap();
    let command = registry
        .insert(
            request(InteractionAction::Command {
                command: "ping".into(),
            }),
            None,
        )
        .unwrap();
    drop(registration);

    assert!(!registry.owns_token("wait-cleanup", &reply, &click(&reply)));
    assert!(registry.owns_token("wait-cleanup", &command, &click(&command)));
    let state = registry.0.lock().unwrap();
    assert_eq!(state.pending.len(), 1);
    assert!(state.pending.contains_key(&command));
}
