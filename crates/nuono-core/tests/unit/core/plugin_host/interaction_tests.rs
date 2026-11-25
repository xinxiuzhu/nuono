use super::*;
use crate::core::permission::{ActorPermission, PermissionLevel};

fn route() -> crate::core::RuntimeBotRoute {
    crate::core::RuntimeBotRoute::new("telegram", "instance", "42")
}
fn request() -> InteractionRegistration {
    InteractionRegistration {
        plugin: "test".into(),
        target: RuntimeMessageTarget::group(route(), "100"),
        actor_id: "7".into(),
        origin_chat_id: None,
        permission: ButtonPermission::All,
        allowed_user_ids: vec![],
        action: InteractionAction::Callback {
            callback_id: "registered".into(),
            data: json!({"stored":true}),
        },
        ttl_secs: 300,
    }
}
fn click(token: &str) -> Value {
    json!({"post_type":"interaction", "route":route(),"user_id":"7", "self_id":"42",
        "message_type":"group","group_id":"100","interaction_id":"click-1",
        "interaction_token":token,"callback_id":"untrusted","data":{"stored":false}})
}

#[test]
fn token_is_scoped_single_use_and_does_not_trust_returned_action() {
    let registry = InteractionRegistry::default();
    let token = registry.insert(request(), None).unwrap();
    for (key, value) in [
        ("group_id", json!("elsewhere")),
        (
            "route",
            json!(crate::core::RuntimeBotRoute::new(
                "telegram",
                "other-instance",
                "42"
            )),
        ),
    ] {
        let mut wrong = click(&token);
        wrong[key] = value;
        assert!(
            registry
                .claim(&wrong, ActorPermission::everyone())
                .is_none()
        );
    }
    let claimed = registry
        .claim(&click(&token), ActorPermission::everyone())
        .unwrap();
    assert_eq!(claimed.plugin, "test");
    let InteractionAction::Callback { callback_id, data } = claimed.action else {
        panic!("stored callback expected");
    };
    assert_eq!(callback_id, "registered");
    assert_eq!(data, json!({"stored":true}));
    assert_eq!(claimed.message["user_id"], "7");
    assert_eq!(claimed.message["group_id"], "100");
    assert!(
        registry
            .claim(&click(&token), ActorPermission::everyone())
            .is_none()
    );
    let other = registry.insert(request(), None).unwrap();
    assert!(
        registry
            .claim(&click(&other), ActorPermission::everyone())
            .is_none(),
        "same native click cannot consume another token"
    );
}

#[test]
fn actor_and_manager_permissions_are_checked_without_burning_unauthorized_tokens() {
    let registry = InteractionRegistry::default();
    let mut specified = request();
    specified.permission = ButtonPermission::SpecifiedUser;
    specified.allowed_user_ids = vec!["8".into()];
    let token = registry.insert(specified, None).unwrap();
    assert!(
        registry
            .claim(&click(&token), ActorPermission::everyone())
            .is_none()
    );
    let mut authorized = click(&token);
    authorized["user_id"] = json!("8");
    assert!(
        registry
            .claim(&authorized, ActorPermission::everyone())
            .is_some()
    );
    let mut manager = request();
    manager.permission = ButtonPermission::Manager;
    let token = registry.insert(manager, None).unwrap();
    let mut event = click(&token);
    event["interaction_id"] = json!("manager-click");
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_none()
    );
    assert!(
        registry
            .claim(
                &event,
                ActorPermission::Allowed(PermissionLevel::GroupAdmin)
            )
            .is_some()
    );
}

#[test]
fn expiry_disable_and_reenable_never_revive_old_buttons() {
    let registry = InteractionRegistry::default();
    let expired = registry.insert(request(), None).unwrap();
    registry
        .0
        .lock()
        .unwrap()
        .pending
        .get_mut(&expired)
        .unwrap()
        .deadline = Instant::now() - Duration::from_secs(1);
    assert!(
        registry
            .claim(&click(&expired), ActorPermission::everyone())
            .is_none()
    );
    let disabled = registry.insert(request(), None).unwrap();
    registry.revoke("test");
    assert!(
        registry
            .claim(&click(&disabled), ActorPermission::everyone())
            .is_none()
    );
    assert!(registry.insert(request(), None).is_err());
    registry.enable("test");
    assert!(
        registry
            .claim(&click(&disabled), ActorPermission::everyone())
            .is_none()
    );
    let fresh = registry.insert(request(), None).unwrap();
    assert!(
        registry
            .claim(&click(&fresh), ActorPermission::everyone())
            .is_some()
    );
}

#[test]
fn feishu_chat_id_is_verified_before_restoring_registered_conversation_kind() {
    let registry = InteractionRegistry::default();
    let mut registration = request();
    registration.target = RuntimeMessageTarget::group(
        crate::core::RuntimeBotRoute::new("feishu", "app", "cli_bot"),
        "oc_group",
    );
    let token = registry.insert(registration.clone(), None).unwrap();
    let mut event = click(&token);
    event["route"] = json!(registration.target.route);
    event["message_type"] = json!("channel");
    event["chat_id"] = json!("oc_other");
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_none()
    );
    event["chat_id"] = json!("oc_group");
    let claimed = registry.claim(&event, ActorPermission::everyone()).unwrap();
    assert_eq!(claimed.message["message_type"], "group");
    assert_eq!(claimed.message["group_id"], "oc_group");

    registration.target.kind = RuntimeTargetKind::Private {
        user_id: "ou_actor".into(),
    };
    registration.actor_id = "ou_actor".into();
    registration.origin_chat_id = Some("oc_direct".into());
    let token = registry.insert(registration, None).unwrap();
    event["interaction_token"] = json!(token);
    event["interaction_id"] = json!("private-click");
    event["user_id"] = json!("ou_actor");
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_none(),
        "same actor in another chat is not its private conversation"
    );
    event["chat_id"] = json!("oc_direct");
    assert_eq!(
        registry
            .claim(&event, ActorPermission::everyone())
            .unwrap()
            .message["message_type"],
        "private"
    );
}

#[test]
fn pending_registry_has_a_hard_capacity_and_reclaims_expired_entries() {
    let registry = InteractionRegistry::default();
    for _ in 0..MAX_PENDING {
        registry.insert(request(), None).unwrap();
    }
    assert!(registry.insert(request(), None).is_err());
    registry
        .0
        .lock()
        .unwrap()
        .pending
        .values_mut()
        .next()
        .unwrap()
        .deadline = Instant::now() - Duration::from_secs(1);
    assert!(registry.insert(request(), None).is_ok());
}

#[test]
fn commands_can_be_clicked_again_but_the_same_click_cannot_be_replayed() {
    let registry = InteractionRegistry::default();
    let mut registration = request();
    registration.action = InteractionAction::Command {
        command: "wwping".into(),
    };
    let token = registry.insert(registration, None).unwrap();
    let mut event = click(&token);
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_some()
    );
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_none()
    );
    event["interaction_id"] = json!("another-click");
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_some()
    );
}

#[test]
fn callbacks_restore_before_execution_and_never_restore_across_disable() {
    let registry = InteractionRegistry::default();
    let token = registry.insert(request(), None).unwrap();
    let event = click(&token);
    drop(registry.claim(&event, ActorPermission::everyone()).unwrap()); // 模拟权限或准入校验未通过时丢弃认领凭证
    assert!(
        registry
            .claim(&event, ActorPermission::everyone())
            .is_none(),
        "a rejected native click is still deduplicated"
    );
    let mut retry = event;
    retry["interaction_id"] = json!("retry-click");
    let claimed = registry.claim(&retry, ActorPermission::everyone()).unwrap();
    assert!(claimed.reservation.unwrap().commit());
    retry["interaction_id"] = json!("after-execution");
    assert!(
        registry
            .claim(&retry, ActorPermission::everyone())
            .is_none(),
        "business callback is one-shot"
    );

    let token = registry.insert(request(), None).unwrap();
    retry["interaction_token"] = json!(token);
    let claimed = registry.claim(&retry, ActorPermission::everyone()).unwrap();
    registry.revoke("test");
    registry.enable("test");
    drop(claimed);
    retry["interaction_id"] = json!("after-reenable");
    assert!(
        registry
            .claim(&retry, ActorPermission::everyone())
            .is_none(),
        "an old reserved token cannot revive after re-enable"
    );

    let token = registry.insert(request(), None).unwrap();
    retry["interaction_token"] = json!(token);
    retry["interaction_id"] = json!("late-commit");
    let claimed = registry.claim(&retry, ActorPermission::everyone()).unwrap();
    registry.revoke("test");
    registry.enable("test");
    assert!(!claimed.epoch.is_current());
    assert!(
        !claimed.reservation.unwrap().commit(),
        "commit must reject an earlier plugin lifecycle"
    );
}
