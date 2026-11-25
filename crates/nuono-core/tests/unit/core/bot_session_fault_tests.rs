use super::*;
use nuono_app::transport::TransportFrame;
use std::time::Duration;

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor did not release its resources");
}

fn request(text: &str) -> SendRequest {
    SendRequest::new(serde_json::json!({"message": text}))
}

/// 构造队列写满的发送 Actor：一帧阻塞在底层传输通道，一帧在工作协程内等待发送，
/// 另一帧占满 Actor 队列。后续发送请求必须保持挂起，直到连接被取消。
async fn saturated_actor(bot_id: i64) -> (Arc<SendActor>, mpsc::Receiver<TransportFrame>) {
    let (transport, receiver) = mpsc::channel(1);
    transport
        .try_send(TransportFrame::Text("occupied".into()))
        .unwrap();
    let actor = Arc::new(SendActor::with_capacity(bot_id, transport, 1));
    actor.send(request("inside-worker")).await.unwrap();
    until(|| actor.tx.capacity() == 1).await;
    actor.try_send(request("inside-queue")).unwrap();
    assert_eq!(actor.tx.capacity(), 0);
    (actor, receiver)
}

#[tokio::test]
async fn saturated_send_does_not_block_unregister_and_unregister_cancels_worker() {
    let manager = BotSessionManager::new();
    let (actor, mut transport) = saturated_actor(11).await;
    manager.register(11, actor.clone());
    let waiting = manager.send_to_bot(11, request("waiting"));
    tokio::pin!(waiting);
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    assert!(!manager.sessions.try_get_mut(&11).is_locked());

    let removed = manager.unregister(11).unwrap();
    assert!(Arc::ptr_eq(&removed, &actor));
    assert!(actor.is_closed());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .is_err()
    );
    until(|| actor.worker.is_finished()).await;
    assert_eq!(actor.info().await.queue_depth, 0);
    assert_eq!(actor.info().await.status, SessionStatus::Offline);
    assert!(transport.recv().await.is_some());
    assert!(
        transport.recv().await.is_none(),
        "aborted worker retained transport sender"
    );
}

#[tokio::test]
async fn replacement_releases_saturated_old_actor_without_touching_new_generation() {
    let manager = BotSessionManager::new();
    let (old, _old_transport) = saturated_actor(11).await;
    manager.register(11, old.clone());
    let waiting = manager.send_text_to_bot(11, "waiting");
    tokio::pin!(waiting);
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    assert!(!manager.sessions.try_get_mut(&11).is_locked());

    let (sender, mut transport) = mpsc::channel(1);
    let replacement = Arc::new(SendActor::with_capacity(11, sender, 1));
    manager.register(11, replacement.clone());
    assert!(old.is_closed());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .is_err()
    );
    assert!(manager.unregister_if_current(11, &old).is_none());
    assert!(Arc::ptr_eq(&manager.get(11).unwrap(), &replacement));
    assert!(!replacement.is_closed());
    replacement.send(request("healthy")).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), transport.recv())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn broadcast_rejects_full_actor_and_continues_to_healthy_bot() {
    let manager = BotSessionManager::new();
    let (blocked, _blocked_transport) = saturated_actor(11).await;
    manager.register(11, blocked.clone());
    let (sender, mut healthy_transport) = mpsc::channel(1);
    manager.register(22, Arc::new(SendActor::with_capacity(22, sender, 1)));

    tokio::time::timeout(
        Duration::from_secs(1),
        manager.broadcast(request("broadcast")),
    )
    .await
    .expect("full bot stalled the entire broadcast");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), healthy_transport.recv()).await.unwrap(),
        Some(TransportFrame::Text(text)) if text.contains("broadcast")
    ));
    assert_eq!(blocked.tx.capacity(), 0);
}

#[tokio::test]
async fn session_inspection_releases_registry_guards_before_waiting_for_actor_info() {
    for inspection in 0..3 {
        let manager = BotSessionManager::new();
        let (sender, _transport) = mpsc::channel(1);
        let actor = Arc::new(SendActor::with_capacity(11, sender, 1));
        manager.register(11, actor.clone());
        let info_lock = actor.info.write().await;
        let reading = async {
            match inspection {
                0 => {
                    manager.all_sessions_info().await;
                }
                1 => {
                    manager.global_stats().await;
                }
                _ => {
                    manager.session_info(11).await;
                }
            }
        };
        tokio::pin!(reading);
        assert!(futures::poll!(reading.as_mut()).is_pending());
        assert!(!manager.sessions.try_get_mut(&11).is_locked());
        manager.unregister(11).unwrap();
        drop(info_lock);
        tokio::time::timeout(Duration::from_secs(1), reading)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn last_actor_owner_drop_releases_blocked_worker_queue_and_transport() {
    let (actor, mut transport) = saturated_actor(11).await;
    let worker_info = Arc::downgrade(&actor.info);
    let worker = actor.worker.clone();
    drop(actor);
    until(|| worker.is_finished() && worker_info.upgrade().is_none()).await;
    assert!(transport.recv().await.is_some());
    assert!(transport.recv().await.is_none());
}

#[tokio::test]
async fn manager_drop_closes_even_externally_retained_actors() {
    let manager = BotSessionManager::new();
    let (actor, mut transport) = saturated_actor(11).await;
    manager.register(11, actor.clone());
    drop(manager);
    assert!(actor.is_closed());
    until(|| actor.worker.is_finished()).await;
    assert!(actor.try_send(request("late")).is_err());
    assert!(transport.recv().await.is_some());
    assert!(transport.recv().await.is_none());
}

#[tokio::test]
async fn closed_transport_terminates_actor_and_cleanup_is_generation_safe() {
    let manager = BotSessionManager::new();
    let (sender, transport) = mpsc::channel(1);
    let actor = Arc::new(SendActor::with_capacity(11, sender, 1));
    manager.register(11, actor.clone());
    drop(transport);
    actor.send(request("fail")).await.unwrap();
    until(|| actor.worker.is_finished()).await;
    assert!(actor.is_closed());
    assert_eq!(actor.info().await.error_count, 1);
    assert_eq!(actor.info().await.status, SessionStatus::Offline);
    assert!(actor.send(request("late")).await.is_err());
    manager.cleanup_disconnected().await;
    assert!(manager.get(11).is_none());

    let (sender, _transport) = mpsc::channel(1);
    let replacement = Arc::new(SendActor::with_capacity(11, sender, 1));
    manager.register(11, replacement.clone());
    // 模拟连接断开清理时，持有的旧连接快照与已被新连接替换的注册项发生竞态。
    assert!(manager.unregister_if_current(11, &actor).is_none());
    manager.cleanup_disconnected().await;
    assert!(Arc::ptr_eq(&manager.get(11).unwrap(), &replacement));
}

#[tokio::test]
async fn same_actor_registration_does_not_close_live_generation_but_closed_actor_stays_closed() {
    let manager = BotSessionManager::new();
    let (sender, _transport) = mpsc::channel(1);
    let actor = Arc::new(SendActor::with_capacity(11, sender, 1));
    manager.register(11, actor.clone());
    manager.register(11, actor.clone());
    assert!(!actor.is_closed());
    manager.unregister(11).unwrap();
    actor.close();
    manager.register(11, actor.clone());
    assert!(actor.try_send(request("cannot-resurrect")).is_err());
    manager.cleanup_disconnected().await;
    assert!(manager.get(11).is_none());
}
