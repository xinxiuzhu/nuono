use nuono_plugin::{PluginStateScope, serde_json::json};
use nuono_plugin_test::TestHost;

#[tokio::test]
async fn state_survives_context_recreation_and_isolates_plugin_collection_scope_and_host() {
    let host = TestHost::new().unwrap();
    let context = host.context("hello", json!({})).unwrap();
    let state = context
        .state("preferences", PluginStateScope::account(1).unwrap())
        .unwrap();
    state.set("nickname", 1, "小明").await.unwrap();
    drop(context);
    let restored = host.context("hello", json!({})).unwrap();
    assert_eq!(
        restored
            .state("preferences", PluginStateScope::account(1).unwrap())
            .unwrap()
            .get("nickname")
            .await
            .unwrap()
            .unwrap()
            .value,
        "小明"
    );
    for store in [
        restored
            .state("other", PluginStateScope::account(1).unwrap())
            .unwrap(),
        restored
            .state("preferences", PluginStateScope::account(2).unwrap())
            .unwrap(),
        host.context("other", json!({}))
            .unwrap()
            .state("preferences", PluginStateScope::account(1).unwrap())
            .unwrap(),
        TestHost::new()
            .unwrap()
            .context("hello", json!({}))
            .unwrap()
            .state("preferences", PluginStateScope::account(1).unwrap())
            .unwrap(),
    ] {
        assert!(store.get("nickname").await.unwrap().is_none());
        assert!(!store.delete("nickname").await.unwrap());
    }
    assert!(state.delete("nickname").await.unwrap());
    assert!(!state.delete("nickname").await.unwrap());
}

#[tokio::test]
async fn state_pages_and_validation_match_the_public_contract() {
    let host = TestHost::new().unwrap();
    assert!(host.context("../bad", json!({})).is_err());
    let context = host.context("hello", json!({})).unwrap();
    let state = context
        .state("preferences", PluginStateScope::global())
        .unwrap();
    for key in ["c", "a", "b"] {
        state.set(key, 1, key).await.unwrap();
    }
    let first = state.list(None, 2).await.unwrap();
    assert_eq!(
        first
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(state.list(Some("b"), 2).await.unwrap()[0].0, "c");
    assert!(state.list(Some("c"), 1).await.unwrap().is_empty());
    for key in ["", " bad", "bad\nkey"] {
        assert!(state.set(key, 1, "x").await.is_err());
        assert!(state.get(key).await.is_err());
        assert!(state.delete(key).await.is_err());
        assert!(state.list(Some(key), 1).await.is_err());
    }
    assert!(state.set(&"x".repeat(201), 1, "x").await.is_err());
    assert!(state.set("a", 0, "x").await.is_err());
    assert!(state.set("a", 1, &"x".repeat(64 * 1024)).await.is_err());
    assert_eq!(state.get("a").await.unwrap().unwrap().value, "a");
    assert!(state.list(None, 0).await.is_err());
    assert!(state.list(None, 101).await.is_err());
}

#[tokio::test]
async fn unsupported_database_work_fails_instead_of_simulating_success() {
    let host = TestHost::new().unwrap();
    let context = host.context("hello", json!({})).unwrap();
    assert!(context.db().begin().await.is_err());
    assert!(
        context
            .db()
            .execute("CREATE TABLE anything (id TEXT)", vec![])
            .await
            .is_err()
    );
    assert!(context.kv().set("name", "anything").await.is_err());
    assert!(context.db().get_secret("token").await.is_err());
    assert!(
        context
            .db()
            .run_plugin_migrations_for_backend("hello", &[])
            .await
            .is_err()
    );
}
