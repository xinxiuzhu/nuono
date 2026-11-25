use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use nuono_core::{
    config::{ConfigManager, NuonoConfig},
    db::RuntimeConfigPatch,
};
use sea_orm::ConnectionTrait;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn harness() -> (Router, ModelState, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nuono.toml");
    std::fs::write(&path, toml::to_string(&NuonoConfig::default()).unwrap()).unwrap();
    let db = nuono_core::test_support::test_db(3).await;
    let config = Arc::new(ConfigManager::load(Some(path)).await.unwrap());
    config.attach_database(db.clone()).await.unwrap();
    let state = ModelState { db, config };
    let app = Router::new()
        .route("/", get(list_models).post(create_model))
        .route(
            "/{id}",
            get(get_model).put(update_model).delete(delete_model),
        )
        .route("/default/{id}", post(set_default_model))
        .route("/fetch", post(fetch_provider_models))
        .with_state(state.clone());
    (app, state, root)
}

#[tokio::test]
async fn discovery_reuses_only_the_selected_models_key_at_its_saved_endpoint() {
    let (app, state, _root) = harness().await;
    let (_, created) = request(&app, "POST", "/", model(false)).await;
    let id = created["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let base = "https://example.invalid/v1";
    let mut payload = FetchModelsPayload {
        base_url: base.into(),
        api_key: None,
        model_id: Some(id),
    };
    assert_eq!(
        resolve_discovery_key(&state.db, &payload, base)
            .await
            .unwrap(),
        "fixture-private-secret"
    );
    assert!(
        resolve_discovery_key(&state.db, &payload, "https://other.invalid/v1")
            .await
            .is_err()
    );
    assert!(
        resolve_discovery_key(&state.db, &payload, "https://example.invalid/other")
            .await
            .is_err()
    );
    payload.api_key = Some("fixt****cret".into());
    assert!(
        resolve_discovery_key(&state.db, &payload, "https://other.invalid/v1")
            .await
            .is_err()
    );
    payload.api_key = Some("  explicitly-entered-key  ".into());
    assert_eq!(
        resolve_discovery_key(&state.db, &payload, "https://other.invalid/v1")
            .await
            .unwrap(),
        "explicitly-entered-key"
    );
    payload.api_key = None;
    payload.model_id = Some(i64::MAX);
    assert!(matches!(
        resolve_discovery_key(&state.db, &payload, base).await,
        Err(NuonoError::NotFound(_))
    ));
    payload.model_id = None;
    // 已禁用的模型不会作为默认候选模型，因此未指定 model_id 时不会意外复用它的已存密钥。
    assert!(
        resolve_discovery_key(&state.db, &payload, base)
            .await
            .is_err()
    );
    request(
        &app,
        "PUT",
        &format!("/{id}"),
        json!({"enabled":true,"is_default":true}),
    )
    .await;
    assert_eq!(
        resolve_discovery_key(&state.db, &payload, base)
            .await
            .unwrap(),
        "fixture-private-secret"
    );
    let (status, body) = request(
        &app,
        "POST",
        "/fetch",
        json!({"base_url":"http://127.0.0.1/v1","model_id":id}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!body.to_string().contains("fixture-private-secret"));
    let (status, body) = request(
        &app,
        "POST",
        "/fetch",
        json!({"base_url":base,"model_id":i32::MAX}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.to_string().contains("fixture-private-secret"));
    state.db.shutdown().await.unwrap();
}
async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn model(enabled: bool) -> Value {
    json!({"name":"fixture","provider":"fixture","model_id":"model","base_url":"https://example.invalid/v1","api_key":"fixture-private-secret","enabled":enabled})
}

#[tokio::test]
async fn model_routes_preserve_the_envelope_and_report_validation_missing_and_conflicts() {
    let (app, state, _root) = harness().await;
    let (status, created) = request(&app, "POST", "/", model(false)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["success"], true);
    assert_eq!(created["data"]["enabled"], false);
    assert!(!created.to_string().contains("fixture-private-secret"));
    let id = created["data"]["id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let path = format!("/{id}");
    assert_eq!(
        request(&app, "GET", "/2147483647", Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "POST", "/default/2147483647", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "PUT", &path, json!({"provider_type":"invalid"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "PUT", &path, json!({"enabled":true})).await.0,
        StatusCode::OK
    );
    state
        .config
        .update_patch(RuntimeConfigPatch {
            settings: json!({"ai":{"chat_model_alias":format!("id:{id}")}}),
            secrets: vec![],
        })
        .await
        .unwrap();
    for (method, body) in [("DELETE", Value::Null), ("PUT", json!({"enabled":false}))] {
        let (status, body) = request(&app, method, &path, body).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["success"], false);
        assert!(body["data"].is_null());
    }
    assert!(state.db.get_ai_model(id).await.unwrap().unwrap().enabled);
    state.db.shutdown().await.unwrap();
}

#[tokio::test]
async fn transactional_storage_failure_is_500_without_a_partial_model_or_secret_leak() {
    let (app, state, _root) = harness().await;
    let sql = if state.db.backend() == sea_orm::DbBackend::Postgres {
        "CREATE FUNCTION fail_api_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.key = 'runtime.settings.v1' THEN RAISE EXCEPTION 'fixture-private-secret'; END IF; RETURN NEW; END; $$; CREATE TRIGGER reject_api_settings BEFORE UPDATE ON nuono_global_configs FOR EACH ROW EXECUTE FUNCTION fail_api_settings()"
    } else {
        "CREATE TRIGGER reject_api_settings BEFORE UPDATE ON nuono_global_configs WHEN NEW.key = 'runtime.settings.v1' BEGIN SELECT RAISE(ABORT, 'fixture-private-secret'); END"
    };
    state.db.orm().execute_unprepared(sql).await.unwrap();
    let (status, body) = request(&app, "POST", "/", model(true)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["success"], false);
    assert!(body["data"].is_null());
    assert!(!body.to_string().contains("fixture-private-secret"));
    assert!(state.db.list_ai_models().await.unwrap().is_empty());
    state.db.shutdown().await.unwrap();
}
