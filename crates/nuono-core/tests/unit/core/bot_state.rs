use super::*;
use serde_json::json;
fn registry() -> (BotStateRegistry, mpsc::Receiver<TransportFrame>) {
    let r = BotStateRegistry::new();
    let (tx, rx) = mpsc::channel(2);
    r.register(
        42,
        1,
        tx,
        "ip".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    (r, rx)
}
#[test]
fn connection_is_not_login_evidence() {
    let (r, _rx) = registry();
    assert_eq!(r.snapshot(42).unwrap().status, "unknown");
}
#[test]
fn strict_status_and_platform_offline() {
    let (r, _rx) = registry();
    for invalid in [
        json!(null),
        json!({"nickname":"cached"}),
        json!({"online":"true"}),
        json!({"data":{"online":true},"echo":"x"}),
    ] {
        r.probe_result(42, 1, Ok(&invalid));
        assert_ne!(r.snapshot(42).unwrap().status, "online");
    }
    r.probe_result(42, 1, Ok(&json!({"online":false,"good":true})));
    let s = r.snapshot(42).unwrap();
    assert_eq!(s.status, "offline");
    assert_eq!(s.transport_connected, Some(true));
    r.probe_result(42, 1, Ok(&json!({"online":true,"good":false})));
    assert_eq!(r.snapshot(42).unwrap().status, "online");
}
#[test]
fn heartbeat_expiry_interval_and_failed_probe() {
    let (r, _rx) = registry();
    r.observe(42,1,&json!({"post_type":"meta_event","meta_event_type":"heartbeat","interval":60000,"status":{"online":true,"good":true}}));
    let entries = r.entries.lock().unwrap();
    let e = &entries[&42];
    let now = Instant::now();
    assert_eq!(
        project(42, e, now + Duration::from_secs(61)).status,
        "online"
    );
    assert_eq!(
        project(42, e, now + Duration::from_secs(121)).status,
        "degraded"
    );
    drop(entries);
    r.probe_result(42, 1, Err("probe_failed".into()));
    assert_eq!(r.snapshot(42).unwrap().status, "degraded");
}
#[test]
fn stale_generation_cannot_modify_or_remove_replacement() {
    let (r, _rx) = registry();
    let (tx, _new_rx) = mpsc::channel(2);
    r.register(
        42,
        2,
        tx,
        "new".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    r.observe(
        42,
        1,
        &json!({"post_type":"meta_event","meta_event_type":"heartbeat","status":{"online":true}}),
    );
    assert!(!r.probe_result(42, 1, Ok(&json!({"online":true}))));
    assert!(!r.remove(42, 1));
    assert_eq!(r.snapshot(42).unwrap().status, "unknown");
}
#[test]
fn late_probe_cannot_overwrite_newer_heartbeat() {
    let (r, _rx) = registry();
    let started = Instant::now();
    r.observe(
        42,
        1,
        &json!({"post_type":"meta_event","meta_event_type":"heartbeat","status":{"online":true}}),
    );
    assert!(!r.probe_result_since(42, 1, started, Ok(&json!({"online":false}))));
    assert_eq!(r.snapshot(42).unwrap().status, "online");
}
#[test]
fn closed_transport_overrides_online() {
    let (r, rx) = registry();
    r.probe_result(42, 1, Ok(&json!({"online":true})));
    drop(rx);
    assert_eq!(r.snapshot(42).unwrap().status, "offline");
}

#[tokio::test]
async fn probe_response_is_real_status() {
    let client = crate::core::onebot_client::ApiClient::new();
    let (tx, mut rx) = mpsc::channel(2);
    let call = client.call("get_status", serde_json::json!({}), &tx);
    let respond = async {
        let TransportFrame::Text(text) = rx.recv().await.unwrap() else {
            panic!()
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(request["action"], "get_status");
        client.on_response(&serde_json::json!({"status":"ok","retcode":0,"echo":request["echo"],"data":{"online":false,"good":true}}));
    };
    let (result, ()) = tokio::join!(call, respond);
    assert_eq!(parse_status(&result.unwrap()).unwrap(), (false, true));
}

#[test]
fn login_metadata_never_confirms_health_and_rejects_stale_or_wrong_accounts() {
    let (r, _rx) = registry();
    assert_eq!(
        r.login_info_result(42, 1, &json!({"user_id":"42","nickname":"  测试昵称  "}))
            .as_deref(),
        Some("测试昵称")
    );
    assert_eq!(
        r.snapshot(42).unwrap().nickname.as_deref(),
        Some("测试昵称")
    );
    assert_eq!(r.snapshot(42).unwrap().status, "unknown");
    for (generation, value) in [
        (0, json!({"user_id":42,"nickname":"stale"})),
        (1, json!({"user_id":43,"nickname":"other account"})),
        (1, json!({"user_id":42,"nickname":"   "})),
        (1, json!({"nickname":"missing id"})),
    ] {
        assert!(r.login_info_result(42, generation, &value).is_none());
    }
    assert_eq!(
        r.snapshot(42).unwrap().nickname.as_deref(),
        Some("测试昵称")
    );
    r.probe_result(42, 1, Ok(&json!({"online":false,"good":false})));
    r.login_info_result(42, 1, &json!({"user_id":42,"nickname":"new name"}));
    assert_eq!(r.snapshot(42).unwrap().status, "offline");
}

#[test]
fn client_health_advisory_does_not_mark_an_online_account_as_degraded() {
    let (r, _rx) = registry();
    r.probe_result(42, 1, Ok(&json!({"online":true,"good":false})));
    r.observe(
        42,
        1,
        &json!({"post_type":"message","self_id":42,"sender":{"nickname":"someone else"}}),
    );
    let status = r.snapshot(42).unwrap();
    assert_eq!(status.status, "online");
    assert_eq!(status.status_reason.as_deref(), Some("platform_unhealthy"));
    assert!(status.nickname.is_none());
}
