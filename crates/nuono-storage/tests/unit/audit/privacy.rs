use super::*;
use serde_json::json;

#[test]
fn private_audit_keeps_correlation_and_usage_without_personal_payloads() {
    let scope = format!("{ACCOUNT_CHAT_PREFIX}namespace:7:card:role");
    let result = diagnostic_event_data(
        "agent.provider_failed",
        json!({
            "session_id": scope, "run_id": "run-17", "status": "failed",
            "elapsed_ms": 81, "tokens": 23, "error_kind": "request_failed",
            "error": "upstream echoed private conversation", "content": "private conversation",
            "memory_key": "personal medical condition", "tags": ["sensitive preference"],
            "checkpoint": {"messages": ["private conversation"]}
        }),
    );
    assert_eq!(result["run_id"], "run-17");
    assert_eq!(result["tokens"], 23);
    assert_eq!(result["elapsed_ms"], 81);
    assert_eq!(result["error_kind"], "request_failed");
    assert!(!result.to_string().contains("private conversation"));
    assert!(result.get("memory_key").is_none());
    assert!(result.get("tags").is_none());
    assert!(result.get("checkpoint").is_none());
}

#[test]
fn nested_legacy_account_scope_is_private_but_game_diagnostics_remain_available() {
    let data = json!({"scopes": [{"scope_key": format!("{ACCOUNT_CHAT_PREFIX}old:7")}], "content": "secret"});
    assert_eq!(
        diagnostic_event_data("agent.memory_loaded", data),
        json!({"private": true})
    );
    let game =
        json!({"plugin": "greeting", "command": "greet", "stage": "dispatch", "elapsed_ms": 12});
    assert_eq!(diagnostic_event_data("plugin.command", game.clone()), game);
}

#[tokio::test]
async fn private_provider_health_still_has_failure_category_and_duration() {
    let db = crate::test_support::test_db(2).await;
    crate::audit::record_event(
        &db,
        "agent.provider_failed",
        None,
        None,
        json!({
            "platform": ACCOUNT_CHAT_PLATFORM, "provider": "test-provider", "model": "test-model",
            "status": "failed", "elapsed_ms": 31, "error_kind": "request_failed",
            "error": "private text in upstream response",
        }),
    )
    .await
    .unwrap();
    let health = crate::audit::list_provider_health(&db, 50).await.unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].failures, 1);
    assert_eq!(health[0].last_error.as_deref(), Some("request_failed"));
    assert_eq!(health[0].last_elapsed_ms, Some(31));
    let events = crate::audit::get_event_logs(&db, 50).await.unwrap();
    assert!(!events[0].data.as_deref().unwrap().contains("private text"));
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn command_pages_and_exports_contain_only_declared_metadata() {
    let db = crate::test_support::test_db(2).await;
    crate::status_commands::record_status_command(&db, "bot", "user", "greet")
        .await
        .unwrap();
    let (page, _) = crate::audit::get_command_logs_page(&db, 10, None)
        .await
        .unwrap();
    let export = crate::audit::get_command_logs_for_export(&db)
        .await
        .unwrap();
    for row in page.iter().chain(&export) {
        assert_eq!(row.command, "greet");
        let json = serde_json::to_value(row).unwrap();
        for key in ["args", "result", "error"] {
            assert!(
                json.get(key).is_none(),
                "command metadata must not expose {key}"
            );
        }
    }
    db.shutdown().await.unwrap();
}
