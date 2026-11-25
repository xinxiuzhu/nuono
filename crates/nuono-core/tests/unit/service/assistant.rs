use super::*;
use std::{future::Future, task::Poll, time::Duration};

#[tokio::test]
async fn view_waits_for_active_privacy_cleanup_but_keeps_failed_and_timed_out_fences() {
    let _env = crate::test_support::env_lock().await;
    let db = crate::test_support::test_db(2).await;
    let root = std::env::temp_dir().join(format!("nuono-assistant-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let _cleanup = std::mem::DropGuard::new(root.clone(), |root| {
        let _ = std::fs::remove_dir_all(root);
    });
    let config = Arc::new(
        ConfigManager::load(Some(root.join("nuono.toml")))
            .await
            .unwrap(),
    );
    let runtime = AgentRuntime::new(db.clone(), config.clone());
    let assistant = Assistant::new(
        db.clone(),
        config,
        runtime,
        &crate::core::Paths::with_root(root),
    );
    assistant.privacy_blocked.insert(7);
    assert!(assistant.guard_account_merge(7, 8).is_err());
    assert!(!assistant.deleting.contains(&7));
    assistant.privacy_blocked.remove(&7);
    assistant.busy.insert(8);
    assert!(assistant.guard_account_merge(7, 8).is_err());
    assert!(!assistant.busy.contains(&7));
    assert!(!assistant.deleting.contains(&7));
    assert!(assistant.busy.contains(&8));
    assistant.busy.remove(&8);
    let guard = assistant.guard_account_merge(8, 7).unwrap();
    for id in [7, 8] {
        assert!(assistant.busy.contains(&id));
        assert!(assistant.deleting.contains(&id));
    }
    assert!(assistant.guard_account_merge(7, 8).is_err());
    drop(guard);
    for id in [7, 8] {
        assert!(!assistant.busy.contains(&id));
        assert!(!assistant.deleting.contains(&id));
    }
    tokio::time::pause();

    for active in [&assistant.busy, &assistant.deleting] {
        active.insert(7);
        assistant.privacy_blocked.insert(7);
        let mut waiting = std::pin::pin!(assistant.wait_for_privacy_ready(7));
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await
        );
        // 其他无关账号的读取不会被当前账号的隐私删除操作阻塞。
        assistant.wait_for_privacy_ready(8).await.unwrap();
        assistant.privacy_blocked.remove(&7);
        tokio::time::advance(Duration::from_millis(25)).await;
        waiting.await.unwrap();
        active.remove(&7);

        // 若清理协程异常退出且未解除隐私屏障，后续读取仍须保持拦截以确保安全。
        active.insert(7);
        assistant.privacy_blocked.insert(7);
        let mut waiting = std::pin::pin!(assistant.wait_for_privacy_ready(7));
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await
        );
        active.remove(&7);
        tokio::time::advance(Duration::from_millis(25)).await;
        assert!(matches!(waiting.await, Err(NuonoError::Conflict(_))));
        assert!(assistant.privacy_blocked.contains(&7));
        assistant.privacy_blocked.remove(&7);
    }

    assistant.busy.insert(7);
    assistant.privacy_blocked.insert(7);
    assert!(matches!(
        assistant.wait_for_privacy_ready(7).await,
        Err(NuonoError::Conflict(_))
    ));
    assert!(assistant.privacy_blocked.contains(&7));
    tokio::time::resume();
    assistant.runtime.shutdown().await.unwrap();
    db.shutdown().await.unwrap();
}

#[test]
fn direct_sync_identity_only_matches_supported_direct_platforms() {
    assert!(is_direct_sync_identity("onebot", ""));
    assert!(is_direct_sync_identity("qq_official", "102088412"));
    assert!(!is_direct_sync_identity(
        "qq_official",
        "102088412/groups/G8899AABB"
    ));
    assert!(!is_direct_sync_identity("telegram", ""));
    assert!(!is_direct_sync_identity("feishu", "cli_a8c90f2e11"));
}
