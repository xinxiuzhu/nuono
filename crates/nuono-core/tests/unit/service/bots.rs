use super::*;
use crate::service::stats::get_dashboard_stats;

#[tokio::test]
async fn live_state_overrides_history_and_includes_unpersisted_connections() {
    use crate::core::bot_state::BotStateRegistry;
    use crate::service::stats::get_dashboard_stats_with_runtime;
    use std::time::Duration;
    let db = crate::test_support::test_db(2).await;
    db.update_bot_status("42", "online").await.unwrap();
    let historical = get_bot_list(&db).await.unwrap();
    assert_eq!(historical[0].status, "offline");
    assert_eq!(historical[0].transport_connected, Some(false));
    let runtime = BotStateRegistry::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(2);
    runtime.register(
        42,
        1,
        tx,
        "new-ip".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    assert_eq!(
        get_bot_list_with_runtime(&db, &runtime).await.unwrap()[0].status,
        "unknown"
    );
    runtime.probe_result(42, 1, Ok(&serde_json::json!({"online": true})));
    let (tx, _rx2) = tokio::sync::mpsc::channel(2);
    runtime.register(
        43,
        2,
        tx,
        "unpersisted".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    let bots = get_bot_list_with_runtime(&db, &runtime).await.unwrap();
    assert_eq!(bots.len(), 2);
    assert_eq!(bots[0].ip_address, "new-ip");
    let stats = get_dashboard_stats_with_runtime(&db, &runtime)
        .await
        .unwrap();
    assert_eq!(stats.online_bots, 1);
    assert_eq!(stats.bot_count, 2);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn internal_shutdown_record_is_not_a_registered_bot() {
    let db = crate::test_support::test_db(2).await;
    assert!(get_bot_list(&db).await.unwrap().is_empty());
    assert_eq!(get_dashboard_stats(&db).await.unwrap().bot_count, 0);

    // 即使旧内部记录误标 online，也不能让新实例出现一个在线或离线机器人。
    db.update_bot_status("system", "online").await.unwrap();
    assert!(get_bot_list(&db).await.unwrap().is_empty());
    let stats = get_dashboard_stats(&db).await.unwrap();
    assert_eq!(stats.bot_count, 0);
    assert_eq!(stats.online_bots, 0);

    // 已接入过的真实离线机器人必须保留，健康检查仍需提示它掉线。
    db.update_bot_status("123456", "offline").await.unwrap();
    let bots = get_bot_list(&db).await.unwrap();
    assert_eq!(bots.len(), 1);
    assert_eq!(bots[0].bot_id, "123456");
    assert_eq!(bots[0].status, "offline");
    let stats = get_dashboard_stats(&db).await.unwrap();
    assert_eq!(stats.bot_count, 1);
    assert_eq!(stats.online_bots, 0);

    db.update_bot_status("987654", "online").await.unwrap();
    assert_eq!(get_bot_list(&db).await.unwrap().len(), 2);
    let stats = get_dashboard_stats(&db).await.unwrap();
    assert_eq!(stats.bot_count, 2);
    assert_eq!(stats.online_bots, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_nickname_overrides_history_and_history_survives_disconnect() {
    use crate::core::bot_state::BotStateRegistry;
    use std::time::Duration;
    let db = crate::test_support::test_db(2).await;
    nuono_storage::connections::update_bot_nickname(&db, "42", "old name")
        .await
        .unwrap();
    db.update_bot_status("42", "online").await.unwrap();
    let runtime = BotStateRegistry::new();
    let (tx, _rx) = tokio::sync::mpsc::channel(2);
    runtime.register(
        42,
        1,
        tx,
        "ip".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    runtime.login_info_result(
        42,
        1,
        &serde_json::json!({"user_id":42,"nickname":"new name"}),
    );
    let live = get_bot_list_with_runtime(&db, &runtime).await.unwrap();
    assert_eq!(live[0].nickname.as_deref(), Some("new name"));
    assert_eq!(
        live[0].status, "unknown",
        "identity metadata never proves account health"
    );
    runtime.remove(42, 1);
    let history = get_bot_list_with_runtime(&db, &runtime).await.unwrap();
    assert_eq!(history[0].nickname.as_deref(), Some("old name"));
    assert_eq!(history[0].status, "offline");
    db.shutdown().await.unwrap();
}
