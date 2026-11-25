use super::*;

#[tokio::test]
async fn reverse_ws_uses_uuidv7_preserves_nickname_and_skips_redundant_writes() {
    let db = crate::test_support::test_db(2).await;
    update_bot_nickname(&db, "42", "first name").await.unwrap();

    let rows = get_registered_bot_connections(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    let initial_id = rows[0].instance_id.clone();
    assert!(!initial_id.starts_with("conn:"), "不再拼接 conn: 前缀");
    assert_eq!(
        uuid::Uuid::parse_str(&initial_id)
            .expect("实例 ID 应为 UUID")
            .get_version_num(),
        7
    );
    assert_eq!(rows[0].bot_id, "42");
    assert_eq!(rows[0].platform, "onebot");
    assert_eq!(rows[0].runtime_status, "offline");
    assert_eq!(rows[0].nickname.as_deref(), Some("first name"));
    assert_eq!(rows[0].last_connected_at_ms, None);

    // 相同昵称重复上报不应触发写库或递增 revision。
    let rev_before_noop = bot_instances::Entity::find_by_id(&initial_id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap()
        .revision;
    update_bot_nickname(&db, "42", "first name").await.unwrap();
    let rev_after_noop = bot_instances::Entity::find_by_id(&initial_id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap()
        .revision;
    assert_eq!(rev_after_noop, rev_before_noop, "相同昵称重复刷新不应写库");

    // 上线后记录 last_connected_at_ms，下线后保留该时间戳与昵称。
    db.update_bot_status("42", "online").await.unwrap();
    let online = bot_instances::Entity::find_by_id(&initial_id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(online.runtime_status, "online");
    assert!(online.last_connected_at_ms.is_some());

    db.update_bot_status("42", "offline").await.unwrap();
    let offline = bot_instances::Entity::find_by_id(&initial_id)
        .one(db.orm())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(offline.runtime_status, "offline");
    assert_eq!(offline.last_connected_at_ms, online.last_connected_at_ms);

    update_bot_nickname(&db, "42", "new name").await.unwrap();
    let after = get_registered_bot_connections(&db).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].instance_id, initial_id);
    assert_eq!(after[0].nickname.as_deref(), Some("new name"));
    assert_eq!(after[0].runtime_status, "offline");
    assert_eq!(after[0].last_connected_at_ms, online.last_connected_at_ms);

    // system 内部标记不应被登记为机器人实例。
    db.update_bot_status("system", "online").await.unwrap();
    assert_eq!(get_registered_bot_connections(&db).await.unwrap().len(), 1);

    db.delete_bot("42").await.unwrap();
    assert!(
        get_registered_bot_connections(&db)
            .await
            .unwrap()
            .is_empty()
    );
    db.shutdown().await.unwrap();
}
