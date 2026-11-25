use nuono_app::plugin_data::PluginDataStore;
use nuono_core::service::plugin_debug::get_plugin_kv;

#[tokio::test]
async fn storage_inspection_preserves_namespace_instead_of_inventing_plugin_ownership() {
    let db = nuono_core::test_support::test_db(2).await;
    db.sync_plugin("cache", "1", &serde_json::json!({}))
        .await
        .unwrap();
    db.kv_set("cache", "sample", "{\"count\":1}").await.unwrap();
    let rows = get_plugin_kv(&db, 200).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].namespace, "cache");
    let json = serde_json::to_value(&rows[0]).unwrap();
    assert!(json.get("namespace").is_some());
    assert!(json.get("plugin_name").is_none());
    db.shutdown().await.unwrap();
}
