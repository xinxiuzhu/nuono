use super::*;
use nuono_domain::{
    BotIdentity, DomainEventKind, DomainMessage, IdentityKind, IdentityRef, IdentityScheme,
    MessageSegment, Platform,
};
use sea_orm::PaginatorTrait;
use serde_json::json;

fn event_on(
    platform: Platform,
    bot_account: &str,
    namespace: &str,
    user: &str,
    group: Option<&str>,
) -> DomainEvent {
    let id = |kind, value: &str, namespace: Option<String>| {
        IdentityRef::new(
            platform.clone(),
            kind,
            IdentityScheme::Native,
            value,
            namespace,
        )
        .unwrap()
    };
    let actor = id(IdentityKind::User, user, Some(namespace.into()));
    let bot = BotIdentity::new(
        "bot-fixture",
        id(IdentityKind::Bot, bot_account, None),
        None,
    )
    .unwrap();
    let conversation = match group {
        Some(group) => {
            ConversationRef::group(id(IdentityKind::Group, group, Some(namespace.into()))).unwrap()
        }
        None => ConversationRef::direct(actor.clone(), None).unwrap(),
    };
    let message = DomainMessage::new(
        "message",
        vec![MessageSegment::text("private text must never be stored")],
        actor.clone(),
        json!({"token":"secret-token"}),
    )
    .unwrap();
    let now = crate::clock::db_now();
    DomainEvent::new(
        "event",
        now.into(),
        now.into(),
        bot,
        Some(actor),
        Some(conversation),
        DomainEventKind::Message(message),
        json!({"secret":"raw payload"}),
    )
    .unwrap()
}

fn event(namespace: &str, user: &str, group: Option<&str>) -> DomainEvent {
    event_on(
        Platform::Other("fixture".into()),
        "account",
        namespace,
        user,
        group,
    )
}
fn now() -> DateTime<Utc> {
    "2026-10-20T12:00:00Z".parse().unwrap()
}
async fn observe(
    db: &Database,
    event: &DomainEvent,
    kind: ActivityKind,
    plugin: &str,
    at: DateTime<Utc>,
) {
    db.activity
        .observe(Record::new(event, plugin, kind, at).unwrap());
}
async fn report(db: &Database, filter: ActivityFilter) -> ActivityReport {
    db.activity.flush().await.unwrap();
    db.activity_report_at(filter, now()).await.unwrap()
}

#[tokio::test]
async fn bot_accounts_share_reports_but_keep_separate_counter_buckets() {
    let db = crate::test_support::test_db(2).await;
    let first = event("app", "user", Some("group"));
    let second = event_on(
        Platform::Other("fixture".into()),
        "another-account",
        "app",
        "user",
        Some("group"),
    );
    for input in [&first, &first, &second] {
        observe(&db, input, ActivityKind::Message, "", now()).await;
    }
    let result = report(&db, Default::default()).await;
    assert_eq!(result.totals.messages, 3);
    assert_eq!(result.totals.active_users, 1);
    assert_eq!(result.totals.active_groups, 1);
    let records = rows::Entity::find().all(db.orm()).await.unwrap();
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].bucket_key, records[1].bucket_key);
    assert_eq!(records[0].bot_id, records[1].bot_id);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn daily_unique_users_are_not_summed_across_groups_or_days() {
    let db = crate::test_support::test_db(2).await;
    let first = event("app-a", "001", Some("group-a"));
    let second = event("app-a", "001", Some("group-b"));
    for _ in 0..3 {
        observe(&db, &first, ActivityKind::Message, "", now()).await;
    }
    observe(&db, &second, ActivityKind::Message, "", now()).await;
    observe(
        &db,
        &first,
        ActivityKind::Message,
        "",
        now() - Duration::days(1),
    )
    .await;
    observe(&db, &first, ActivityKind::Command, "weather", now()).await;
    observe(&db, &first, ActivityKind::Listener, "weather", now()).await;
    let result = report(
        &db,
        ActivityFilter {
            days: 7,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        (
            result.totals.messages,
            result.totals.plugin_calls,
            result.totals.active_users,
            result.totals.active_groups
        ),
        (5, 2, 1, 2)
    );
    assert_eq!(result.daily.len(), 7);
    assert_eq!(result.dau, 1);
    assert_eq!(result.daily[5].counts.active_users, 1);
    assert_eq!(result.daily[6].counts.active_users, 1);
    assert_eq!(result.daily[0].counts.messages, 0);
    assert_eq!(
        (
            result.plugins[0].counts.commands,
            result.plugins[0].counts.listeners
        ),
        (1, 1)
    );
    assert_eq!(result.users[0].label, "001");
    let json = serde_json::to_string(&result).unwrap();
    for secret in ["private text", "secret-token", "raw payload"] {
        assert!(!json.contains(secret));
    }
    assert_eq!(rows::Entity::find().count(db.orm()).await.unwrap(), 5);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_channel_id_under_different_parents_remains_distinct_when_drilling_down() {
    let db = crate::test_support::test_db(2).await;
    for parent in ["server-a", "server-b"] {
        let mut input = event("app", "001", None);
        let id = |kind, value| {
            IdentityRef::new(
                Platform::Other("fixture".into()),
                kind,
                IdentityScheme::Native,
                value,
                Some("app"),
            )
            .unwrap()
        };
        input.conversation = Some(
            ConversationRef::channel(
                id(IdentityKind::Channel, "same-channel"),
                Some(id(IdentityKind::Group, parent)),
            )
            .unwrap(),
        );
        observe(&db, &input, ActivityKind::Message, "", now()).await;
    }
    let all = report(&db, Default::default()).await;
    assert_eq!(all.totals.active_groups, 2);
    assert_eq!(all.totals.active_users, 1);
    assert_eq!(all.groups.len(), 2);
    assert_ne!(all.groups[0].key, all.groups[1].key);
    let selected = report(
        &db,
        ActivityFilter {
            scope_key: all.groups[0].key.clone(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(selected.totals.messages, 1);
    assert_eq!(selected.totals.active_groups, 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_namespaces_and_platforms_never_merge() {
    let db = crate::test_support::test_db(2).await;
    let first = event("app-a", "same", Some("same"));
    let second = event("app-b", "same", Some("same"));
    let other = event_on(
        Platform::Other("other".into()),
        "account",
        "app-a",
        "same",
        Some("same"),
    );
    for input in [&first, &second, &other] {
        observe(&db, input, ActivityKind::Message, "", now()).await;
        observe(&db, input, ActivityKind::Rule, "p", now()).await;
    }
    let result = report(&db, ActivityFilter::default()).await;
    assert_eq!(
        (
            result.dau,
            result.totals.active_groups,
            result.users.len(),
            result.groups.len()
        ),
        (3, 3, 3, 3)
    );
    let key = result
        .users
        .iter()
        .find(|row| row.namespace == "app-b")
        .unwrap()
        .key
        .clone();
    let selected = report(
        &db,
        ActivityFilter {
            user_key: key,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        (
            selected.dau,
            selected.totals.messages,
            selected.totals.plugin_calls
        ),
        (1, 1, 1)
    );
    let selected = report(
        &db,
        ActivityFilter {
            platform: "other".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!((selected.dau, selected.totals.plugin_calls), (1, 1));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn bound_filters_keep_literals_opaque_and_plugin_counts_do_not_become_message_counts() {
    let db = crate::test_support::test_db(2).await;
    let input = event("app", "001' OR 1=1 --", None);
    observe(&db, &input, ActivityKind::Message, "", now()).await;
    observe(&db, &input, ActivityKind::Command, "plugin:a", now()).await;
    observe(&db, &input, ActivityKind::Reply, "plugin:b", now()).await;
    let selected = report(
        &db,
        ActivityFilter {
            user_id: "001' OR 1=1 --".into(),
            plugin: "plugin:a".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        (
            selected.dau,
            selected.totals.messages,
            selected.totals.plugin_calls
        ),
        (1, 0, 1)
    );
    assert_eq!(selected.totals.active_groups, 0);
    let selected = report(
        &db,
        ActivityFilter {
            user_id: "' OR 1=1 --".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(selected.totals.active_users, 0);
    for days in [0, 91, u32::MAX] {
        assert!(
            db.activity_report_at(
                ActivityFilter {
                    days,
                    ..Default::default()
                },
                now()
            )
            .await
            .is_err()
        );
    }
    assert!(
        db.activity_report_at(
            ActivityFilter {
                plugin: "x".repeat(129),
                ..Default::default()
            },
            now()
        )
        .await
        .is_err()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn atomic_rollups_preserve_concurrent_calls_and_shutdown_stops_admission() {
    let db = crate::test_support::test_db(4).await;
    let input = Arc::new(event("app", "user", Some("group")));
    let mut calls = Vec::new();
    for _ in 0..16 {
        let db = db.clone();
        let input = input.clone();
        calls.push(tokio::spawn(async move {
            for _ in 0..10 {
                db.record_activity(&input, "p", ActivityKind::Command);
            }
        }));
    }
    for call in calls {
        call.await.unwrap();
    }
    db.activity.flush().await.unwrap();
    let result = db.activity_report(ActivityFilter::default()).await.unwrap();
    assert_eq!(
        (result.totals.plugin_calls, result.totals.active_users),
        (160, 1)
    );
    db.activity.shutdown().await.unwrap();
    db.record_activity(&input, "p", ActivityKind::Command);
    assert_eq!(db.activity.lost.load(Ordering::Relaxed), 1);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn calendar_windows_exclude_future_and_old_days_and_top_lists_are_bounded() {
    let db = crate::test_support::test_db(2).await;
    for n in 0..55 {
        observe(
            &db,
            &event("app", &format!("user-{n:02}"), None),
            ActivityKind::Message,
            "",
            now(),
        )
        .await;
    }
    observe(
        &db,
        &event("app", "old", None),
        ActivityKind::Message,
        "",
        now() - Duration::days(7),
    )
    .await;
    observe(
        &db,
        &event("app", "future", None),
        ActivityKind::Message,
        "",
        now() + Duration::days(1),
    )
    .await;
    let result = report(
        &db,
        ActivityFilter {
            days: 7,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        (result.totals.messages, result.dau, result.users.len()),
        (55, 55, 50)
    );
    assert!(result.groups.is_empty());
    db.shutdown().await.unwrap();
}
