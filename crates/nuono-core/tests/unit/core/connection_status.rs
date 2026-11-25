use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn periodic_probe_recovers_a_quiet_account_and_keeps_identity_failure_separate() {
    let db = crate::test_support::test_db(2).await;
    let client = Arc::new(ApiClient::new());
    let states = Arc::new(BotStateRegistry::new());
    let (sender, mut receiver) = mpsc::channel(8);
    states.register(
        42,
        1,
        sender.clone(),
        "ip".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    let task = tokio::spawn(monitor(
        client.clone(),
        states.clone(),
        db.clone(),
        42,
        1,
        sender,
        Duration::from_millis(200),
    ));
    // 模拟客户端起初上报服务不健康、随后在既无聊天消息也无心跳到达期间恢复健康的场景（不支持身份查询 API）。
    // 首次确认在线后应自动触发一次 get_group_list 并将群信息入库，后续轮询周期内不再重复请求。
    let mut group_sync_calls = 0usize;
    for (round, healthy) in [false, true].into_iter().enumerate() {
        let expected_queries = if round == 0 { 3 } else { 2 };
        for _ in 0..expected_queries {
            let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            let TransportFrame::Text(text) = frame else {
                panic!("expected a query frame");
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            match request["action"].as_str().unwrap() {
                "get_status" => client.on_response(&json!({
                    "status": "ok",
                    "retcode": 0,
                    "echo": request["echo"],
                    "data": {"online": true, "good": healthy}
                })),
                "get_login_info" => client.on_response(&json!({
                    "status": "failed",
                    "retcode": 1404,
                    "echo": request["echo"],
                    "data": null
                })),
                "get_group_list" => {
                    group_sync_calls += 1;
                    client.on_response(&json!({
                        "status": "ok",
                        "retcode": 0,
                        "echo": request["echo"],
                        "data": [{"group_id": 100001, "group_name": "自动同步测试群"}]
                    }));
                }
                other => panic!("unexpected query action: {other}"),
            }
        }
        let expected_reason = if healthy {
            "status_confirmed"
        } else {
            "platform_unhealthy"
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = states.snapshot(42).unwrap();
                if snapshot.status_reason.as_deref() == Some(expected_reason) {
                    assert_eq!(snapshot.status, "online");
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(group_sync_calls, 1);
    let synced = db
        .group_for_onebot("100001")
        .await
        .unwrap()
        .expect("OneBot 群应在确认在线后自动同步入库");
    assert_eq!(synced.display_name, "自动同步测试群");
    assert!(states.snapshot(42).unwrap().nickname.is_none());
    states.remove(42, 1);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    db.shutdown().await.unwrap();
}
