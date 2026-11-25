use nuono_plugin::{Database, Event, RuntimeBotRoute, RuntimeMessageTarget, Subscriptions};
use nuono_plugin_test::TestBot;
use serde_json::json;

#[test]
fn event_targets_preserve_opaque_ids_and_reject_incomplete_events() {
    let route = RuntimeBotRoute::new("discord", "east-instance", "bot-999999999999999999999");
    for (kind, fields) in [
        ("private", json!({"user_id": "user-openid"})),
        ("group", json!({"group_id": "999999999999999999999"})),
        (
            "channel",
            json!({"channel_id": "channel-openid", "guild_id": "guild-openid"}),
        ),
    ] {
        let mut event = fields;
        event["route"] = json!(route);
        event["message_type"] = json!(kind);
        let target = Event::new(&event).message_target().unwrap();
        assert_eq!(target.route, route);
        assert_eq!(serde_json::to_value(&target).unwrap()["kind"]["type"], kind);
        event.as_object_mut().unwrap().remove("route");
        assert!(Event::new(&event).message_target().is_err());
    }
    for event in [
        json!({"route": route}),
        json!({"route": route, "message_type": "private"}),
        json!({"route": route, "message_type": "notice", "user_id": "u"}),
    ] {
        assert!(Event::new(&event).message_target().is_err());
    }
}

#[tokio::test]
async fn subscriptions_survive_recreation_keep_plugin_and_bot_isolation_and_route_sends() {
    let db = nuono_core::test_support::test_db(2).await;
    for plugin in ["weather", "other"] {
        db.sync_plugin(plugin, "1", &json!({})).await.unwrap();
    }
    let sdk_db = Database::new(db.clone());
    let route = RuntimeBotRoute::new("qq_official", "qq-east", "bot-openid");
    let event = json!({"route": route, "message_type": "private", "user_id": "member-openid"});
    let target = Event::new(&event).message_target().unwrap();
    let a = Subscriptions::new(sdk_db.clone(), "weather");
    let b = Subscriptions::new(sdk_db.clone(), "other");
    a.subscribe("daily", &Event::new(&event), json!({"city":"杭州"}))
        .await
        .unwrap();
    a.subscribe("daily", &Event::new(&event), json!({"city":"北京"}))
        .await
        .unwrap();
    assert!(b.list("daily").await.unwrap().is_empty());
    let another = RuntimeMessageTarget::private(
        RuntimeBotRoute::new("qq_official", "qq-west", "bot-openid"),
        "member-openid",
    );
    a.set("daily", another.clone(), json!({"city":"南京"}))
        .await
        .unwrap();
    let long_target = RuntimeMessageTarget::private(
        RuntimeBotRoute::new("discord", "long-instance", "bot"),
        "x".repeat(300),
    );
    a.set("long-topic", long_target.clone(), json!({}))
        .await
        .unwrap();
    assert_eq!(a.list("long-topic").await.unwrap()[0].target, long_target);
    assert!(a.remove("long-topic", &long_target).await.unwrap());
    assert!(a.set("", target.clone(), json!({})).await.is_err());
    // 并发重复订阅必须保留唯一记录，不能读后写插入两行。
    let (first, second) = tokio::join!(
        a.set("concurrent", target.clone(), 1),
        a.set("concurrent", target.clone(), 2)
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(a.list("concurrent").await.unwrap().len(), 1);
    assert!(a.remove("concurrent", &target).await.unwrap());
    sdk_db.kv_set("weather", "unrelated", "test").await.unwrap();
    sdk_db.kv_clear("weather").await.unwrap();
    assert_eq!(a.list("daily").await.unwrap().len(), 2);
    drop(a);
    let restored = Subscriptions::new(sdk_db, "weather");
    let records = restored.list("daily").await.unwrap();
    assert_eq!(records.len(), 2);
    let record = records.iter().find(|r| r.target == target).unwrap();
    assert_eq!(record.data["city"], "北京");
    let mut test = TestBot::new();
    test.connect_route(route);
    test.push_reply(json!({"message_ids":["receipt-1"], "raw":{}}));
    assert_eq!(
        record
            .send(test.bot(), "今日天气")
            .await
            .unwrap()
            .message_ids,
        ["receipt-1"]
    );
    assert_eq!(test.actions()[0]["params"]["openid"], "member-openid");
    let offline = records.iter().find(|r| r.target == another).unwrap();
    assert!(
        offline
            .send(test.bot(), "不得改投在线机器人")
            .await
            .is_err()
    );
    assert_eq!(test.actions().len(), 1);
    assert!(
        restored
            .unsubscribe("daily", &Event::new(&event))
            .await
            .unwrap()
    );
    assert!(
        !restored
            .unsubscribe("daily", &Event::new(&event))
            .await
            .unwrap()
    );
    assert_eq!(restored.list("daily").await.unwrap().len(), 1);
    assert!(restored.remove("daily", &another).await.unwrap());
    assert!(restored.list("daily").await.unwrap().is_empty());
    db.shutdown().await.unwrap();
}
