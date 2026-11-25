use super::*;

const DEADLINE: Duration = Duration::from_secs(2);

#[tokio::test]
async fn cancelled_context_replacement_retains_old_scope_until_blocking_work_exits() {
    let h = Harness::new("cancelled-context-replacement").await;
    let pool = Arc::new(
        nuono_plugin::RenderPool::new(nuono_plugin::RenderLimits {
            concurrency: 1,
            queue_capacity: 0,
        })
        .unwrap(),
    );
    let old = Arc::new(PluginTaskScope::with_render_pool(&h.plugin, pool));
    h.host
        .task_scopes
        .write()
        .await
        .insert(h.plugin.clone(), old.clone());
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let rendering_scope = old.clone();
    let rendering = tokio::spawn(async move {
        rendering_scope
            .render("old-instance-image", move || {
                let _ = started.send(());
                let _ = released.recv();
                Ok(())
            })
            .await
    });
    tokio::time::timeout(DEADLINE, entered)
        .await
        .unwrap()
        .unwrap();

    let host = h.host.clone();
    let name = h.plugin.clone();
    let first = tokio::spawn(async move {
        let _operation = host.plugin_operation(&name).await;
        host.plugin_context(&name, serde_json::json!({}), PluginActivationReason::Enable)
            .await
    });
    tokio::time::timeout(DEADLINE, async {
        while !old.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(Arc::ptr_eq(
        h.host.task_scopes.read().await.get(&h.plugin).unwrap(),
        &old,
    ));
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));

    let host = h.host.clone();
    let name = h.plugin.clone();
    let mut retry = tokio::spawn(async move {
        let _operation = host.plugin_operation(&name).await;
        host.plugin_context(&name, serde_json::json!({}), PluginActivationReason::Enable)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut retry)
            .await
            .is_err(),
        "retry must still wait for the cancelled attempt's old blocking work",
    );
    assert!(Arc::ptr_eq(
        h.host.task_scopes.read().await.get(&h.plugin).unwrap(),
        &old,
    ));
    assert!(!rendering.is_finished());

    release.send(()).unwrap();
    tokio::time::timeout(DEADLINE, rendering)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let replacement = tokio::time::timeout(DEADLINE, retry)
        .await
        .unwrap()
        .unwrap();
    assert!(!Arc::ptr_eq(replacement.tasks(), &old));
    assert!(Arc::ptr_eq(
        h.host.task_scopes.read().await.get(&h.plugin).unwrap(),
        replacement.tasks(),
    ));
    assert!(replacement.tasks().try_spawn("new-instance-work", async {}));
    h.host.shutdown().await.unwrap();
    h.cleanup().await;
}
