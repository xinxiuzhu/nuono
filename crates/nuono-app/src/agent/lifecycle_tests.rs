use super::*;

async fn idle(runtime: &AgentRuntime) {
    tokio::time::timeout(Duration::from_secs(2), runtime.wait_for_idle())
        .await
        .expect("owned work settles")
        .unwrap();
}
fn only_run(store: &Store) -> AgentRunRecord {
    store.runs.lock().unwrap().values().next().unwrap().clone()
}

#[tokio::test]
async fn unpolled_drop_retains_capacity_until_durable_interruption() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    let task = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap();
    let id = only_run(&store).id;
    let gate = Arc::new(Semaphore::new(0));
    *store.cleanup_gate.lock().unwrap() = Some(gate.clone());
    drop(task);
    until(|| store.interruption_calls.load(Ordering::SeqCst) == 1).await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        runtime.submit_text(context(2, "hello")).await,
        Err(NuonoError::Conflict(_))
    ));
    assert_eq!(
        store.get_agent_run(&id).await.unwrap().unwrap().status,
        "running"
    );
    *store.cleanup_gate.lock().unwrap() = None;
    gate.add_permits(1);
    idle(&runtime).await;
    assert_eq!(
        store.get_agent_run(&id).await.unwrap().unwrap().status,
        "interrupted"
    );
    model.gate.add_permits(1);
    let new = runtime
        .submit_text(context(1, "new"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_ne!(new.run_id, id);
}

#[tokio::test]
async fn first_history_failure_settles_created_run_before_same_session_retry() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    store.fail_history.store(true, Ordering::SeqCst);
    assert!(runtime.submit_text(context(1, "hello")).await.is_err());
    idle(&runtime).await;
    assert_eq!(only_run(&store).status, "interrupted");
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    store.fail_history.store(false, Ordering::SeqCst);
    model.gate.add_permits(1);
    assert_eq!(
        runtime
            .submit_text(context(1, "retry"))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .content,
        "done"
    );
}

#[tokio::test]
async fn cancelled_create_acknowledgement_is_reconciled_without_replaying_execution() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    *store.create_gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    let worker = runtime.clone();
    let preparing = tokio::spawn(async move { worker.submit_text(context(1, "hello")).await });
    until(|| !store.runs.lock().unwrap().is_empty()).await;
    preparing.abort();
    assert!(matches!(preparing.await, Err(error) if error.is_cancelled()));
    idle(&runtime).await;
    assert_eq!(only_run(&store).status, "interrupted");
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert!(runtime.runs.is_empty());
}

#[tokio::test]
async fn provider_panic_is_caught_and_owned_cleanup_marks_interrupted() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    model.panic_chat.store(true, Ordering::SeqCst);
    assert!(
        runtime
            .submit_text(context(1, "hello"))
            .await
            .unwrap()
            .unwrap()
            .await
            .is_err()
    );
    idle(&runtime).await;
    assert_eq!(only_run(&store).status, "interrupted");
    assert_eq!(model.active.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.resources.models.available_permits(), 1);
}

#[tokio::test]
async fn shutdown_cancels_active_and_unpolled_workers_and_seals_every_execution_entry() {
    let (runtime, store, model) = runtime(2, 1, Mode::Slow);
    let active = tokio::spawn(
        runtime
            .submit_text(context(1, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 1).await;
    let unpolled = runtime
        .submit_text(context(2, "hello"))
        .await
        .unwrap()
        .unwrap();
    runtime.request_shutdown();
    assert!(runtime.submit_text(context(3, "hello")).await.is_err());
    assert!(runtime.resume_approval(123).await.is_err());
    assert!(runtime.collect_children("unused").await.is_err());
    tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(active.await.unwrap().is_err());
    assert!(unpolled.await.is_err());
    assert!(
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .all(|row| row.status == "interrupted")
    );
    assert_eq!(model.active.load(Ordering::SeqCst), 0);
    assert!(runtime.runs.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn bounded_cleanup_failure_is_observable_and_next_session_request_repairs_it() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    store.fail_interrupt.store(true, Ordering::SeqCst);
    drop(
        runtime
            .submit_text(context(1, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), runtime.wait_for_idle())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(store.interruption_calls.load(Ordering::SeqCst), 3);
    let old = only_run(&store);
    assert_eq!(old.status, "running");
    assert!(runtime.runs.is_empty());
    store.fail_interrupt.store(false, Ordering::SeqCst);
    model.gate.add_permits(1);
    let new = runtime
        .submit_text(context(1, "retry"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    assert_ne!(old.id, new.run_id);
    assert_eq!(
        store.get_agent_run(&old.id).await.unwrap().unwrap().status,
        "interrupted"
    );
    idle(&runtime).await;
}

#[tokio::test]
async fn cleanup_cas_does_not_chase_a_newer_revision() {
    let (runtime, store, _) = runtime(1, 1, Mode::Slow);
    let task = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap();
    let id = only_run(&store).id;
    let gate = Arc::new(Semaphore::new(0));
    *store.cleanup_gate.lock().unwrap() = Some(gate.clone());
    drop(task);
    until(|| store.interruption_calls.load(Ordering::SeqCst) == 1).await;
    store.runs.lock().unwrap().get_mut(&id).unwrap().revision += 1;
    gate.add_permits(1);
    idle(&runtime).await;
    assert_eq!(store.interruption_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.get_agent_run(&id).await.unwrap().unwrap().status,
        "running"
    );
}

#[tokio::test]
async fn aborting_parent_quiesces_children_and_interrupts_the_live_graph() {
    let (runtime, store, model) = runtime(1, 1, Mode::Delegation);
    let parent = tokio::spawn(
        runtime
            .submit_text(context(1, "delegate"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 2).await;
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    idle(&runtime).await;
    assert_eq!(store.runs.lock().unwrap().len(), 3);
    assert!(
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .all(|row| row.status == "interrupted")
    );
    assert_eq!(model.active.load(Ordering::SeqCst), 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn healthy_approval_pause_survives_runtime_shutdown_without_tool_replay() {
    let (runtime, store, model) = runtime(1, 1, Mode::Approval);
    let paused = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let row = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(row.status, "waiting_approval");
    runtime.shutdown().await.unwrap();
    let after = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    assert_eq!(after.status, "waiting_approval");
    assert_eq!(after.state_json, row.state_json);
    assert_eq!(after.revision, row.revision);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
}

fn another_runtime(store: Arc<Store>, model: Arc<Model>) -> AgentRuntime {
    let registry = Arc::new(ToolRegistry::new());
    registry.register(ApprovalTool).unwrap();
    AgentRuntime::with_ports_and_resource_limits(
        store,
        Arc::new(Config::default()),
        Arc::new(Factory(model)),
        registry,
        AgentResourceLimits {
            max_active_executions: 1,
            max_model_requests: 1,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn another_runtime_cannot_guess_a_live_execution_is_an_orphan() {
    let (first, store, model) = runtime(1, 1, Mode::Slow);
    let active = tokio::spawn(
        first
            .submit_text(context(1, "hello"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 1).await;
    let id = only_run(&store).id;
    let second = another_runtime(store.clone(), model.clone());
    assert!(second.submit_text(context(1, "new request")).await.is_err());
    assert_eq!(
        store.get_agent_run(&id).await.unwrap().unwrap().status,
        "running"
    );
    assert_eq!(store.interruption_calls.load(Ordering::SeqCst), 0);
    model.gate.add_permits(1);
    assert_eq!(active.await.unwrap().unwrap().content, "done");
    idle(&first).await;
    idle(&second).await;
}

#[tokio::test]
async fn aborted_losing_cas_cannot_interrupt_the_winners_execution_owner() {
    let (first, store, model) = runtime(1, 1, Mode::Approval);
    let paused = first
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();
    let row = store.get_agent_run(&paused.run_id).await.unwrap().unwrap();
    let approval = row.approval_id.unwrap();
    store
        .approvals
        .lock()
        .unwrap()
        .get_mut(&approval)
        .unwrap()
        .status = "rejected".into();
    let second = another_runtime(store.clone(), model.clone());
    *store.resume_barrier.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));
    *store.failed_cas_gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    model.hold_all.store(true, Ordering::SeqCst);
    let worker = first.clone();
    let a = tokio::spawn(async move { worker.resume_approval(approval).await });
    let worker = second.clone();
    let b = tokio::spawn(async move { worker.resume_approval(approval).await });
    until(|| {
        store.failed_cas.load(Ordering::SeqCst) == 1 && model.calls.load(Ordering::SeqCst) == 2
    })
    .await;
    let winner = store.get_agent_run(&row.id).await.unwrap().unwrap();
    let owner = serde_json::from_str::<Value>(&winner.state_json).unwrap()["execution_owner"]
        .as_str()
        .unwrap()
        .to_owned();
    let a_won = first.runs.get(&row.session_id).unwrap().execution_owner == owner;
    let (winning, losing, loser_runtime) = if a_won {
        (a, b, &second)
    } else {
        (b, a, &first)
    };
    losing.abort();
    assert!(losing.await.unwrap_err().is_cancelled());
    idle(loser_runtime).await;
    let preserved = store.get_agent_run(&row.id).await.unwrap().unwrap();
    assert_eq!(preserved.status, "running");
    assert_eq!(preserved.revision, winner.revision);
    assert_eq!(
        store.interruption_calls.load(Ordering::SeqCst),
        0,
        "the losing nonce must not even attempt interruption"
    );
    model.gate.add_permits(1);
    assert_eq!(winning.await.unwrap().unwrap().unwrap().content, "done");
    idle(&first).await;
    idle(&second).await;
}

#[tokio::test]
async fn explicit_parent_cancellation_waits_for_cancelled_children_before_reply() {
    let (runtime, store, model) = runtime(1, 1, Mode::Delegation);
    let parent = tokio::spawn(
        runtime
            .submit_text(context(1, "delegate"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 2).await;
    let id = store
        .runs
        .lock()
        .unwrap()
        .values()
        .find(|row| row.parent_id.is_none())
        .unwrap()
        .id
        .clone();
    runtime.cancel_run(&id).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), parent)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(reply.content.contains("取消"));
    assert!(
        store
            .list_agent_children(&id)
            .await
            .unwrap()
            .iter()
            .all(|row| row.status == "cancelled")
    );
    assert_eq!(
        store.get_agent_run(&id).await.unwrap().unwrap().status,
        "cancelled"
    );
    idle(&runtime).await;
    assert_eq!(model.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_cancel_then_unpolled_drop_closes_queued_and_paused_descendants() {
    let (runtime, store, model) = runtime(1, 1, Mode::Slow);
    let task = runtime
        .submit_text(context(1, "hello"))
        .await
        .unwrap()
        .unwrap();
    let parent = only_run(&store);
    for (suffix, status) in [
        ("queued", "queued"),
        ("approval", "waiting_approval"),
        ("graph", "waiting_children"),
    ] {
        let mut child = parent.clone();
        child.id = format!("{}:{suffix}", parent.id);
        child.session_id = format!("child:{}", child.id);
        child.parent_id = Some(parent.id.clone());
        child.status = status.into();
        store.runs.lock().unwrap().insert(child.id.clone(), child);
    }
    // 模拟显式取消父任务后，调用方在继续遍历取消子任务前就已中断的场景。
    runtime.cancel_one_run(&parent.id).await.unwrap();
    drop(task);
    idle(&runtime).await;
    assert!(
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .all(|row| row.status == "cancelled")
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn parent_timeout_cancels_children_before_a_slow_terminal_checkpoint() {
    let (runtime, store, model) = runtime(1, 1, Mode::Delegation);
    let runtime = runtime.with_run_limits(super::super::run::RunLimits {
        timeout: Duration::from_millis(200),
        ..Default::default()
    });
    // 先暂缓父任务首轮模型返回，让子任务的独立超时截止时间明显晚于父任务，
    // 避免并发负载下子任务自身超时与父任务取消产生毫秒级竞态。
    model.hold_all.store(true, Ordering::SeqCst);
    let gate = Arc::new(Semaphore::new(0));
    *store.terminal_gate.lock().unwrap() = Some(gate.clone());
    let parent = tokio::spawn(
        runtime
            .submit_text(context(1, "delegate"))
            .await
            .unwrap()
            .unwrap(),
    );
    until(|| model.calls.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    model.hold_all.store(false, Ordering::SeqCst);
    model.gate.add_permits(1);
    until(|| model.calls.load(Ordering::SeqCst) == 2).await;
    let id = store
        .runs
        .lock()
        .unwrap()
        .values()
        .find(|row| row.parent_id.is_none())
        .unwrap()
        .id
        .clone();
    until(|| store.terminal_attempts.load(Ordering::SeqCst) == 1).await;
    until(|| {
        store
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|row| row.parent_id.is_some())
            .all(|row| !super::super::run::is_active(&row.status))
    })
    .await;
    let children_before_commit = store.list_agent_children(&id).await.unwrap();
    gate.add_permits(1);
    parent.await.unwrap().unwrap();
    idle(&runtime).await;
    // 运行循环可能在异步超时触发前就在预算检查点检测到截止时间已到；无论哪种情况，都必须在父任务写入终态前先取消所有子任务。
    assert!(matches!(
        store
            .get_agent_run(&id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "timed_out" | "budget_exceeded"
    ));
    assert!(
        children_before_commit
            .iter()
            .all(|row| row.status == "cancelled"),
        "children stopped before parent terminal commit: {:?}",
        children_before_commit
            .iter()
            .map(|row| &row.status)
            .collect::<Vec<_>>()
    );
}
