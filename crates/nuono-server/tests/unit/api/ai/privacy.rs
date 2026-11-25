use super::*;
use nuono_app::agent::run::{AgentRunSummary, AgentRunView, ChildRunView, RunLimits};

#[test]
fn private_run_diagnostics_keep_usage_without_tasks_or_replies() {
    let summary = AgentRunSummaryResponse::from(AgentRunSummary {
        id: "run-1".into(),
        title: "private user input".into(),
        status: "completed".into(),
        platform: nuono_app::agent::ACCOUNT_CHAT_PLATFORM.into(),
        bot_id: 0,
        rounds: 1,
        tool_calls: 0,
        tokens: 47,
        approval_id: None,
        child_count: 0,
        updated_at: "2026-11-07".into(),
        reply_preview: Some("private reply".into()),
    });
    let value = serde_json::to_value(summary).unwrap();
    assert_eq!(value["id"], "run-1");
    assert_eq!(value["tokens"], 47);
    assert!(!value.to_string().contains("private user input"));
    assert!(value["reply_preview"].is_null());

    let view = AgentRunResponse::from(AgentRunView {
        id: "run-1".into(),
        title: "private title".into(),
        task: "private task".into(),
        platform: nuono_app::agent::ACCOUNT_CHAT_PLATFORM.into(),
        bot_id: 0,
        parent_id: None,
        node_id: None,
        depends_on: vec![],
        session_id: format!("{}legacy:4:card:one", nuono_app::agent::ACCOUNT_CHAT_PREFIX),
        status: "completed".into(),
        rounds: 2,
        tool_calls: 0,
        tokens: 71,
        approval_id: None,
        reply: Some("private reply".into()),
        limits: RunLimits::default(),
        children: vec![ChildRunView {
            id: "child-1".into(),
            node_id: Some("private child node".into()),
            depends_on: vec!["private child dependency".into()],
            title: "private child task".into(),
            status: "completed".into(),
            rounds: 1,
            tool_calls: 0,
            tokens: 11,
            approval_id: None,
            reply: Some("private child reply".into()),
        }],
    });
    let value = serde_json::to_value(view).unwrap();
    assert_eq!(value["tokens"], 71);
    assert_eq!(value["children"][0]["tokens"], 11);
    assert!(value["task"].as_str().unwrap().is_empty());
    assert!(value["reply"].is_null());
    assert!(!value.to_string().contains("private child"));
}

#[test]
fn private_scope_includes_legacy_and_all_cards_but_not_group_memory() {
    assert!(private_scope(&nuono_app::agent::assistant_chat_key(4)));
    assert!(private_scope(&format!(
        "{}:card:one",
        nuono_app::agent::assistant_chat_key(4)
    )));
    assert!(!private_scope("onebot:group:42"));
    assert!(!private_scope("onebot:private:42"));
}

#[test]
fn ordinary_agent_run_diagnostics_keep_task_titles() {
    let summary = AgentRunSummaryResponse::from(AgentRunSummary {
        id: "game-run".into(),
        title: "greeting plugin test".into(),
        status: "completed".into(),
        platform: "onebot".into(),
        bot_id: 1,
        rounds: 1,
        tool_calls: 0,
        tokens: 7,
        approval_id: None,
        child_count: 0,
        updated_at: "2026-11-07".into(),
        reply_preview: Some("hello".into()),
    });
    assert_eq!(summary.title, "greeting plugin test");
    assert_eq!(summary.reply_preview.as_deref(), Some("hello"));
}

#[tokio::test]
async fn administrative_memory_endpoints_cannot_read_or_modify_private_card_memory() {
    use crate::api::contract_tests::Fixture;
    use axum::http::StatusCode;
    let fixture = Fixture::new(false).await;
    let owner = fixture
        .state
        .db
        .create_user(nuono_core::db::NewUser {
            username: "private-memory-owner".into(),
            email: None,
            password: "private-memory-password".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap();
    let private_key = nuono_app::agent::assistant_chat_key(owner.id);
    let record = |scope_key: String, content: &str| AgentMemoryRecord {
        scope_type: "user".into(),
        scope_key,
        memory_key: "preference".into(),
        content: content.into(),
        tags_json: Some("[]".into()),
        enabled: true,
        expires_at: None,
        actor: None,
    };
    let (private, _) = fixture
        .state
        .db
        .upsert_agent_memory(record(private_key.clone(), "private preference"))
        .await
        .unwrap();
    fixture
        .state
        .db
        .upsert_agent_memory(record("onebot:game-user".into(), "game nickname"))
        .await
        .unwrap();
    let response = list_memories(
        State(fixture.state.clone()),
        Query(MemoryQuery {
            scope_type: None,
            scope_key: None,
            enabled: None,
            limit: Some(100),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 128 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("private preference"));
    assert!(body.contains("game nickname"));
    assert_eq!(
        get_memory(State(fixture.state.clone()), Path(private.id))
            .await
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_memory(State(fixture.state.clone()), Path(private.id))
            .await
            .into_response()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_memory_graph(
            State(fixture.state.clone()),
            Query(MemoryGraphQuery {
                scope_type: "user".into(),
                scope_key: private_key.clone(),
            })
        )
        .await
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        create_memory(
            State(fixture.state.clone()),
            Json(CreateMemoryPayload {
                scope_type: "user".into(),
                scope_key: private_key,
                memory_key: "new".into(),
                content: "admin injected memory".into(),
                tags: None,
                enabled: None,
                expires_at: None,
                actor: None,
            })
        )
        .await
        .into_response()
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .state
            .db
            .get_agent_memory(private.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "private preference"
    );
    fixture.close().await;
}
