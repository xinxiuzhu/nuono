use super::*;
use nuono_core::{
    core::{RuntimeActionSink, RuntimeBotRoute},
    domain::{ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform},
};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Default)]
struct Sink {
    actions: Mutex<Vec<Value>>,
    fail: bool,
}
#[async_trait::async_trait]
impl RuntimeActionSink for Sink {
    async fn send_action(&self, action: Value) -> nuono_core::Result<()> {
        self.actions.lock().unwrap().push(action);
        if self.fail {
            Err(nuono_core::NuonoError::Connection("fixture failure".into()))
        } else {
            Ok(())
        }
    }
}

fn target(channel: bool) -> RestartReplyTarget {
    let platform = if channel {
        Platform::Discord
    } else {
        Platform::OneBot
    };
    let identity = |kind, id: &str| {
        IdentityRef::new(
            platform.clone(),
            kind,
            if channel {
                IdentityScheme::Snowflake
            } else {
                IdentityScheme::Native
            },
            id,
            None::<String>,
        )
        .unwrap()
    };
    let route = RuntimeBotRoute::new(
        if channel { "discord" } else { "onebot" },
        "original-instance",
        "1234567890123456789",
    );
    RestartReplyTarget {
        bot: nuono_plugin::core::runtime_transport::route_identity(&route).unwrap(),
        route,
        conversation: if channel {
            ConversationRef::channel(
                identity(IdentityKind::Channel, "1234567890123456790"),
                Some(identity(IdentityKind::Organization, "1234567890123456791")),
            )
            .unwrap()
        } else {
            ConversationRef::direct(
                identity(IdentityKind::User, "123456789012345678901237"),
                None,
            )
            .unwrap()
        },
    }
}

#[tokio::test]
async fn completion_waits_for_ready_and_exact_bot_then_consumes_the_record() {
    let db = nuono_core::test_support::test_db(2).await;
    let reply = target(false);
    record(&db, reply.clone()).await.unwrap();
    let pending = load(&db).await;
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = Arc::new(Sink::default());
    let wrong = RuntimeBotRoute::new("onebot", "other-instance", &reply.route.bot_self_id);
    router.register(wrong, sink.clone());
    let early = router.register(reply.route.clone(), sink.clone());
    let ready = Arc::new(AtomicBool::new(false));
    let stop = CancellationToken::new();
    let task = tokio::spawn(run(
        pending,
        db.clone(),
        router.clone(),
        ready.clone(),
        stop.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(sink.actions.lock().unwrap().is_empty());
    router.unregister(&reply.route, early);
    ready.store(true, Ordering::Release);
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(sink.actions.lock().unwrap().is_empty());
    router.register(reply.route, sink.clone());
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    let actions = sink.actions.lock().unwrap().clone();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["params"]["user_id"], "123456789012345678901237");
    assert!(
        actions[0]["params"]["message"][0]["data"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Nuono 重启完成（耗时")
    );
    assert!(load(&db).await.is_none());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn channel_parent_is_preserved_and_failed_delivery_is_not_replayed() {
    let db = nuono_core::test_support::test_db(2).await;
    let reply = target(true);
    record(&db, reply.clone()).await.unwrap();
    let router = Arc::new(RuntimeActionRouter::new());
    let sink = Arc::new(Sink {
        fail: true,
        ..Sink::default()
    });
    router.register(reply.route, sink.clone());
    let error = deliver(&db, &router, load(&db).await.unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture failure"), "{error}");
    let actions = sink.actions.lock().unwrap().clone();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["params"]["channel_id"], 1234567890123456790_i64);
    assert_eq!(actions[0]["params"]["guild_id"], 1234567890123456791_i64);
    assert!(load(&db).await.is_none());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_invalid_or_replaced_records_cannot_send_an_old_notice() {
    let db = nuono_core::test_support::test_db(2).await;
    let reply = target(false);
    let mut value = serde_json::to_value(Receipt {
        id: uuid::Uuid::new_v4(),
        requested_at: Utc::now() - chrono::Duration::minutes(31),
        reply: reply.clone(),
    })
    .unwrap();
    db.set_global_config(KEY, value.clone()).await.unwrap();
    assert!(load(&db).await.is_none());
    assert!(db.get_global_config(KEY).await.unwrap().is_none());
    value["reply"]["route"]["instance_id"] = json!("forged-instance");
    value["requested_at"] = json!(Utc::now());
    db.set_global_config(KEY, value).await.unwrap();
    assert!(load(&db).await.is_none());
    record(&db, reply.clone()).await.unwrap();
    let old = load(&db).await.unwrap();
    record(&db, target(true)).await.unwrap();
    let router = RuntimeActionRouter::new();
    let sink = Arc::new(Sink::default());
    router.register(reply.route, sink.clone());
    deliver(&db, &router, old).await.unwrap();
    assert!(sink.actions.lock().unwrap().is_empty());
    assert!(load(&db).await.is_some());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn stopping_while_offline_retains_receipt_without_holding_up_shutdown() {
    let db = nuono_core::test_support::test_db(2).await;
    record(&db, target(false)).await.unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(run(
        load(&db).await,
        db.clone(),
        Arc::new(RuntimeActionRouter::new()),
        Arc::new(AtomicBool::new(true)),
        stop.clone(),
    ));
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(load(&db).await.is_some());
    db.shutdown().await.unwrap();
}
