use super::*;
use sea_orm::{ConnectionTrait, QueryOrder};
use serde_json::{Value, json};

fn run(id: &str, parent: Option<&str>, status: &str) -> AgentRunRecord {
    AgentRunRecord {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        session_id: format!("session:{id}"),
        status: status.into(),
        approval_id: None,
        revision: 0,
        state_json: json!({"context":{"content":"retained request"},
            "messages":[{"role":"user","content":"retained message"}],
            "pending":[{"id":"call-1","name":"mutate","arguments":{"value":42}}],
            "rounds":3,"tool_calls":2,"tokens":123,"active_ms":17,"reply":null,
            "future_field":{"preserve":true}})
        .to_string(),
    }
}

async fn seed(db: &Database, rows: &[(&str, Option<&str>, &str)]) {
    for &(id, parent, status) in rows {
        db.create_agent_run(run(id, parent, status)).await.unwrap();
    }
}

async fn row(db: &Database, id: &str) -> agent_runs::Model {
    agent_runs::Entity::find_by_id(id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap()
}

async fn rows(db: &Database) -> Vec<agent_runs::Model> {
    agent_runs::Entity::find()
        .order_by_asc(agent_runs::Column::Id)
        .all(db.orm())
        .await
        .unwrap()
}

async fn audit_count(db: &Database) -> u64 {
    audit_events::Entity::find()
        .filter(audit_events::Column::Category.eq(crate::audit::CATEGORY_EVENT))
        .filter(audit_events::Column::Action.eq("agent.run_interrupted"))
        .count(db.orm())
        .await
        .unwrap()
}

fn assert_checkpoint_retained(before: &str, after: &str, reason: &str) {
    let before: Value = serde_json::from_str(before).unwrap();
    let after: Value = serde_json::from_str(after).unwrap();
    for key in [
        "context",
        "messages",
        "pending",
        "rounds",
        "tool_calls",
        "tokens",
        "active_ms",
        "future_field",
    ] {
        assert_eq!(after[key], before[key], "checkpoint field {key}");
    }
    assert_eq!(after["reply"], reason);
    assert_eq!(after["interruption"]["reason"], reason);
    assert_eq!(after["interruption"]["previous_reply"], before["reply"]);
}

#[tokio::test]
async fn interruption_cas_is_revision_and_running_status_fenced_then_stops_only_active_children() {
    let db = crate::test_support::test_db(2).await;
    seed(
        &db,
        &[
            ("root", None, "running"),
            ("child", Some("root"), "running"),
            ("queued", Some("root"), "queued"),
            ("approval", Some("root"), "waiting_approval"),
            ("done", Some("root"), "completed"),
        ],
    )
    .await;
    let before = rows(&db).await;
    assert!(
        !db.interrupt_agent_run("missing", 0, "aborted")
            .await
            .unwrap()
    );
    assert!(!db.interrupt_agent_run("root", 1, "stale").await.unwrap());
    assert_eq!(rows(&db).await, before);
    assert_eq!(audit_count(&db).await, 0);
    let mut current = db.get_agent_run("root").await.unwrap().unwrap();
    assert!(
        !db.interrupt_agent_run("done", 0, "terminal cleanup")
            .await
            .unwrap()
    );
    for status in ["waiting_approval", "waiting_children"] {
        let revision = current.revision;
        current.status = status.into();
        db.update_agent_run(current.clone(), revision)
            .await
            .unwrap();
        current.revision += 1;
        let paused = rows(&db).await;
        assert!(
            !db.interrupt_agent_run("root", current.revision, "old cleanup")
                .await
                .unwrap()
        );
        assert_eq!(rows(&db).await, paused);
    }
    let revision = current.revision;
    current.status = "running".into();
    db.update_agent_run(current.clone(), revision)
        .await
        .unwrap();
    current.revision += 1;
    assert!(
        !db.interrupt_agent_run("root", 0, "stale after resume")
            .await
            .unwrap()
    );
    let terminal_child = row(&db, "done").await;
    assert!(
        db.interrupt_agent_run("root", current.revision, "task future was dropped")
            .await
            .unwrap()
    );
    assert!(
        !db.interrupt_agent_run("root", current.revision, "duplicate")
            .await
            .unwrap()
    );
    for id in ["root", "child", "queued", "approval"] {
        let changed = row(&db, id).await;
        assert_eq!(changed.status, "interrupted");
        assert_checkpoint_retained(
            &run(id, None, "running").state_json,
            &changed.state_json,
            "task future was dropped",
        );
    }
    assert_eq!(row(&db, "done").await, terminal_child);
    assert_eq!(audit_count(&db).await, 4);
    assert!(db.active_agent_run("session:root").await.unwrap().is_none());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_child_closes_paused_graph_but_does_not_override_a_live_parent() {
    let db = crate::test_support::test_db(2).await;
    seed(
        &db,
        &[
            ("paused", None, "waiting_children"),
            ("child", Some("paused"), "running"),
            ("sibling", Some("paused"), "waiting_approval"),
            ("done", Some("paused"), "completed"),
            ("live", None, "running"),
            ("live-child", Some("live"), "running"),
        ],
    )
    .await;
    let completed = row(&db, "done").await;
    assert!(
        db.interrupt_agent_run("child", 0, "child stopped")
            .await
            .unwrap()
    );
    for id in ["paused", "child", "sibling"] {
        assert_eq!(row(&db, id).await.status, "interrupted");
    }
    assert_eq!(row(&db, "done").await, completed);
    let live = row(&db, "live").await;
    assert!(
        db.interrupt_agent_run("live-child", 0, "child stopped")
            .await
            .unwrap()
    );
    assert_eq!(
        row(&db, "live").await,
        live,
        "a currently running parent handles its child's result"
    );
    db.shutdown().await.unwrap();
}

async fn approval(db: &Database, id: &str, status: &str) -> agent_tool_approvals::Model {
    let now = crate::clock::db_now();
    agent_tool_approvals::ActiveModel {
        run_id: Set(Some(id.into())),
        session_id: Set(format!("session:{id}")),
        bot_id: Set(1),
        platform: Set("onebot".into()),
        message_type: Set("private".into()),
        user_id: Set(2),
        tool_name: Set("mutate".into()),
        risk_level: Set("high_risk".into()),
        status: Set(status.into()),
        input_json: Set(Some(json!({"value":42}).to_string())),
        execution_json: Set(None),
        created_at: Set(crate::clock::store_time(now)),
        ..Default::default()
    }
    .insert(db.orm())
    .await
    .unwrap()
}

#[tokio::test]
async fn startup_recovery_is_idempotent_preserves_healthy_approvals_and_never_replays_executing() {
    let db = crate::test_support::test_db(2).await;
    seed(
        &db,
        &[
            ("running", None, "running"),
            ("r-child", Some("running"), "running"),
            ("r-queued", Some("running"), "queued"),
            ("waiting", None, "waiting_children"),
            ("w-child", Some("waiting"), "running"),
            ("w-queued", Some("waiting"), "queued"),
            ("already", None, "waiting_children"),
            ("a-child", Some("already"), "interrupted"),
            ("a-queued", Some("already"), "queued"),
            ("terminal", None, "failed"),
            ("orphan", Some("terminal"), "queued"),
            ("approval", None, "waiting_approval"),
            ("healthy", None, "waiting_children"),
            ("h-child", Some("healthy"), "waiting_approval"),
            ("h-queued", Some("healthy"), "queued"),
            ("h-done", Some("healthy"), "completed"),
            ("h-historical", Some("healthy"), "interrupted"),
        ],
    )
    .await;
    let mut healthy = db.get_agent_run("healthy").await.unwrap().unwrap();
    let mut checkpoint: Value = serde_json::from_str(&healthy.state_json).unwrap();
    checkpoint["delegation"] =
        json!({"call_id":"current", "children":["h-child","h-queued","h-done"]});
    healthy.state_json = checkpoint.to_string();
    db.update_agent_run(healthy, 0).await.unwrap();
    let executing = approval(&db, "approval", "executing").await;
    let pending = approval(&db, "h-child", "pending").await;
    let preserved_ids = [
        "approval",
        "healthy",
        "h-child",
        "h-queued",
        "h-done",
        "h-historical",
        "terminal",
        "a-child",
    ];
    let mut preserved = Vec::new();
    for id in preserved_ids {
        preserved.push(row(&db, id).await);
    }
    let summary = db
        .reconcile_agent_runs_before_start("previous process ended")
        .await
        .unwrap();
    assert_eq!(
        summary,
        AgentRunRecoverySummary {
            interrupted_runs: 9,
            interrupted_roots: 3,
            unresolved_executing_approvals: 1
        }
    );
    for previous in preserved {
        assert_eq!(row(&db, &previous.id).await, previous);
    }
    assert_eq!(
        db.get_agent_tool_approval(executing.id)
            .await
            .unwrap()
            .unwrap(),
        executing
    );
    assert_eq!(
        db.get_agent_tool_approval(pending.id)
            .await
            .unwrap()
            .unwrap(),
        pending
    );
    assert!(
        db.claim_agent_tool_approval_for_execution(executing.id)
            .await
            .unwrap()
            .is_none()
    );
    let recovered = rows(&db).await;
    assert_eq!(audit_count(&db).await, 9);
    let again = db
        .reconcile_agent_runs_before_start("restart again")
        .await
        .unwrap();
    assert_eq!(again.interrupted_runs, 0);
    assert_eq!(again.unresolved_executing_approvals, 1);
    assert_eq!(rows(&db).await, recovered);
    assert_eq!(audit_count(&db).await, 9);
    db.shutdown().await.unwrap();
}

async fn fail_audit(db: &Database) {
    let sql = if db.backend() == DbBackend::Postgres {
        "CREATE FUNCTION fail_interruption_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action = 'agent.run_interrupted' THEN RAISE EXCEPTION 'injected interruption audit failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_interruption_audit BEFORE INSERT ON nuono_audit_events FOR EACH ROW EXECUTE FUNCTION fail_interruption_audit()"
    } else {
        "CREATE TRIGGER reject_interruption_audit BEFORE INSERT ON nuono_audit_events WHEN NEW.action = 'agent.run_interrupted' BEGIN SELECT RAISE(ABORT, 'injected interruption audit failure'); END"
    };
    db.orm().execute_unprepared(sql).await.unwrap();
}

#[tokio::test]
async fn interruption_audit_failure_rolls_back_runtime_and_startup_graph_changes() {
    let db = crate::test_support::test_db(2).await;
    seed(
        &db,
        &[
            ("root", None, "running"),
            ("child", Some("root"), "running"),
        ],
    )
    .await;
    let before = rows(&db).await;
    fail_audit(&db).await;
    assert!(db.interrupt_agent_run("root", 0, "aborted").await.is_err());
    assert_eq!(rows(&db).await, before);
    assert!(
        db.reconcile_agent_runs_before_start("restart")
            .await
            .is_err()
    );
    assert_eq!(rows(&db).await, before);
    assert_eq!(audit_count(&db).await, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn interruption_child_write_failure_rolls_back_parent_and_its_audit() {
    let db = crate::test_support::test_db(2).await;
    seed(
        &db,
        &[
            ("root", None, "running"),
            ("child", Some("root"), "running"),
        ],
    )
    .await;
    let before = rows(&db).await;
    let sql = if db.backend() == DbBackend::Postgres {
        "CREATE FUNCTION fail_child_interrupt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id = 'child' AND NEW.status = 'interrupted' THEN RAISE EXCEPTION 'injected child failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_child_interrupt BEFORE UPDATE ON nuono_agent_runs FOR EACH ROW EXECUTE FUNCTION fail_child_interrupt()"
    } else {
        "CREATE TRIGGER reject_child_interrupt BEFORE UPDATE ON nuono_agent_runs WHEN NEW.id = 'child' AND NEW.status = 'interrupted' BEGIN SELECT RAISE(ABORT, 'injected child failure'); END"
    };
    db.orm().execute_unprepared(sql).await.unwrap();
    assert!(db.interrupt_agent_run("root", 0, "aborted").await.is_err());
    assert_eq!(rows(&db).await, before);
    assert_eq!(audit_count(&db).await, 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_interruption_and_completion_have_one_cas_winner() {
    let db = crate::test_support::test_db(2).await;
    seed(&db, &[("root", None, "running")]).await;
    let mut completed = run("root", None, "completed");
    completed.state_json = json!({"reply":"completed result"}).to_string();
    let (interrupt, finish) = tokio::join!(
        db.interrupt_agent_run("root", 0, "aborted"),
        db.update_agent_run(completed.clone(), 0),
    );
    assert_ne!(interrupt.unwrap(), finish.unwrap());
    let final_row = row(&db, "root").await;
    assert_eq!(final_row.revision, 1);
    if final_row.status == "completed" {
        assert_eq!(final_row.state_json, completed.state_json);
    }
    assert!(
        !db.interrupt_agent_run("root", 0, "stale cleanup")
            .await
            .unwrap()
    );
    assert_eq!(row(&db, "root").await, final_row);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_checkpoint_is_preserved_and_reason_is_retained_in_transactional_audit() {
    let db = crate::test_support::test_db(2).await;
    let mut malformed = run("root", None, "running");
    malformed.state_json = serde_json::to_string("not a checkpoint object").unwrap();
    db.create_agent_run(malformed.clone()).await.unwrap();
    assert!(db.interrupt_agent_run("root", 0, "aborted").await.unwrap());
    assert_eq!(row(&db, "root").await.state_json, malformed.state_json);
    let audit = audit_events::Entity::find()
        .filter(audit_events::Column::Category.eq(crate::audit::CATEGORY_EVENT))
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    let data: Value = serde_json::from_str(&audit.details_json).unwrap();
    assert_eq!(data["reason"], "aborted");
    assert_eq!(data["checkpoint_annotated"], false);
    db.shutdown().await.unwrap();
}

#[test]
fn full_checkpoint_is_not_grown_past_the_application_limit() {
    let mut original = json!({"padding":""}).to_string();
    let size = original.len();
    original = json!({"padding":"x".repeat(MAX_CHECKPOINT_BYTES - size)}).to_string();
    assert_eq!(original.len(), MAX_CHECKPOINT_BYTES);
    let (after, annotated) = interrupted_checkpoint(
        &original,
        "running",
        "aborted",
        "runtime_cleanup",
        crate::clock::db_now().into(),
    );
    assert!(!annotated);
    assert_eq!(after, original);
}
