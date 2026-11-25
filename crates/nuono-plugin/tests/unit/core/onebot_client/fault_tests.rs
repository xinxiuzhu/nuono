use super::*;
use nuono_app::transport::TransportFrame;
use serde_json::json;
use tokio::sync::mpsc;

fn echo(frame: TransportFrame) -> String {
    let TransportFrame::Text(text) = frame else {
        panic!("expected text frame")
    };
    serde_json::from_str::<Value>(&text).unwrap()["echo"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn cancelling_a_queue_blocked_notification_removes_its_registration() {
    let client = ApiClient::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let mut send = Box::pin(client.send("send_msg", json!({}), &tx));
    assert!(futures::poll!(send.as_mut()).is_pending());
    assert_eq!(client.pending.len(), 1);
    drop(send);
    assert!(client.pending.is_empty());
    rx.recv().await.unwrap();
    assert!(
        rx.try_recv().is_err(),
        "cancelled notification must not be replayed"
    );
}

#[tokio::test(start_paused = true)]
async fn unanswered_notifications_expire_without_further_client_activity() {
    let client = ApiClient::with_limits(2, Duration::from_secs(30));
    let (tx, mut rx) = mpsc::channel(4);
    client.send("send_msg", json!({}), &tx).await.unwrap();
    let first = echo(rx.recv().await.unwrap());
    client.send("send_msg", json!({}), &tx).await.unwrap();
    let second = echo(rx.recv().await.unwrap());
    assert!(client.send("overflow", json!({}), &tx).await.is_err());
    assert_eq!(client.pending.len(), 2);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        client.pending.is_empty(),
        "idle notifications must be reaped by one maintenance worker"
    );
    client.on_response(&json!({"echo":first,"status":"ok","data":{}}));
    client.on_response(&json!({"echo":second,"status":"ok","data":{}}));
    assert!(
        client.pending.is_empty(),
        "late replies cannot resurrect old registrations"
    );
    client.send("recovered", json!({}), &tx).await.unwrap();
    assert_eq!(client.pending.len(), 1);
}

#[tokio::test]
async fn calls_and_notifications_share_admission_and_responses_release_capacity() {
    let client = ApiClient::with_limits(2, REQUEST_TIMEOUT);
    let (tx, mut rx) = mpsc::channel(4);
    let mut call = Box::pin(client.call("get_status", json!({}), &tx));
    assert!(futures::poll!(call.as_mut()).is_pending());
    let call_echo = echo(rx.recv().await.unwrap());
    client.send("send_msg", json!({}), &tx).await.unwrap();
    let notification_echo = echo(rx.recv().await.unwrap());
    assert!(client.call("overflow", json!({}), &tx).await.is_err());
    assert_eq!(client.pending.len(), 2);
    assert!(
        rx.try_recv().is_err(),
        "overloaded calls must not enter the transport queue"
    );
    client.on_response(&json!({"echo":notification_echo,"status":"ok","data":{}}));
    client.send("replacement", json!({}), &tx).await.unwrap();
    assert_eq!(client.pending.len(), 2);
    client.on_response(&json!({"echo":call_echo,"status":"ok","retcode":0,"data":{"ok":true}}));
    assert_eq!(call.await.unwrap(), json!({"ok":true}));
    assert_eq!(client.pending.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn blocked_notification_times_out_cleans_up_and_never_sends_later() {
    let client = ApiClient::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(TransportFrame::Text("occupied".into()))
        .await
        .unwrap();
    let mut send = Box::pin(client.send("send_msg", json!({}), &tx));
    assert!(futures::poll!(send.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = send.await.unwrap_err();
    assert!(error.to_string().contains("timeout"));
    assert!(client.pending.is_empty());
    rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn an_expired_response_is_rejected_even_before_the_next_maintenance_tick() {
    let client = ApiClient::new();
    let (tx, mut rx) = mpsc::channel(1);
    let mut call = Box::pin(client.call("get_status", json!({}), &tx));
    assert!(futures::poll!(call.as_mut()).is_pending());
    let request_echo = echo(rx.recv().await.unwrap());
    // 停掉后台清理任务，以便单独验证响应接收侧的截止时间检查逻辑。
    client.maintenance.lock().unwrap().take();
    tokio::time::advance(Duration::from_secs(31)).await;
    client.on_response(&json!({"echo":request_echo,"status":"ok","retcode":0,"data":"late"}));
    assert!(
        call.await.is_err(),
        "late success must not be treated as a current response"
    );
    assert!(client.pending.is_empty());
}

#[tokio::test(start_paused = true)]
async fn capacity_released_after_deadline_does_not_admit_late_actions() {
    for wait_for_response in [false, true] {
        let client = ApiClient::new();
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(TransportFrame::Text("occupied".into()))
            .await
            .unwrap();
        let mut request: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send + '_>,
        > = if wait_for_response {
            Box::pin(async { client.call("get_status", json!({}), &tx).await.map(|_| ()) })
        } else {
            Box::pin(client.send("send_msg", json!({}), &tx))
        };
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(31)).await;
        rx.recv().await.unwrap();
        let error = request.await.unwrap_err();
        assert!(error.to_string().contains("timeout"));
        assert!(
            rx.try_recv().is_err(),
            "late capacity must not turn expired work into a new action"
        );
        assert!(client.pending.is_empty());
    }
}

#[tokio::test]
async fn transport_failure_and_invalid_action_leave_no_pending_records() {
    let client = ApiClient::new();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    assert!(client.call("get_status", json!({}), &tx).await.is_err());
    assert!(client.send("send_msg", json!({}), &tx).await.is_err());
    assert!(client.pending.is_empty());
    let (tx, mut rx) = mpsc::channel(1);
    assert!(
        client
            .send("x".repeat(MAX_ACTION_BYTES + 1), json!({}), &tx)
            .await
            .is_err()
    );
    assert!(client.call("", json!({}), &tx).await.is_err());
    assert!(client.pending.is_empty());
    assert!(rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_notifications_cannot_exceed_the_shared_limit() {
    const CAPACITY: usize = 8;
    let client = Arc::new(ApiClient::with_limits(CAPACITY, REQUEST_TIMEOUT));
    let (tx, _rx) = mpsc::channel(64);
    let mut tasks = tokio::task::JoinSet::new();
    let start = Arc::new(tokio::sync::Barrier::new(33));
    for _ in 0..32 {
        let (client, tx, start) = (client.clone(), tx.clone(), start.clone());
        tasks.spawn(async move {
            start.wait().await;
            client.send("send_msg", json!({}), &tx).await
        });
    }
    start.wait().await;
    let mut accepted = 0;
    while let Some(result) = tasks.join_next().await {
        if result.unwrap().is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, CAPACITY);
    assert_eq!(client.pending.len(), CAPACITY);
    assert_eq!(tx.max_capacity() - tx.capacity(), CAPACITY);
}

#[tokio::test]
async fn dropping_client_releases_the_idle_maintenance_worker_and_correlations() {
    let client = ApiClient::new();
    let pending = Arc::downgrade(&client.pending);
    let (tx, _rx) = mpsc::channel(1);
    client.send("send_msg", json!({}), &tx).await.unwrap();
    assert!(client.maintenance.lock().unwrap().is_some());
    tokio::task::yield_now().await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        while pending.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("maintenance must not keep the correlation table alive");
}
