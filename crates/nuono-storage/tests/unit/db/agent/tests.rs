use super::*;

#[tokio::test]
async fn scoped_mutations_recheck_ownership_and_preserve_partial_updates() {
    let db = crate::test_support::test_db(2).await;
    let (row, _) = db
        .upsert_agent_memory(AgentMemoryRecord {
            scope_type: "user".into(),
            scope_key: "caller".into(),
            memory_key: "key".into(),
            content: "original".into(),
            tags_json: Some("[\"keep\"]".into()),
            enabled: true,
            expires_at: Some((crate::clock::db_now() + chrono::Duration::days(1)).into()),
            actor: None,
        })
        .await
        .unwrap();
    let scopes = [("user".into(), "caller".into())];
    // 模拟在工具完成读取与鉴权之后、执行更新之前，后台管理操作并发将该记忆行迁移到了其他作用域。
    db.update_agent_memory(
        row.id,
        AgentMemoryUpdateRecord {
            scope_key: Some("other".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db.update_agent_memory_in_scopes(
            row.id,
            &scopes,
            AgentMemoryUpdateRecord {
                content: Some("must not change".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        !db.delete_agent_memory_in_scopes(row.id, &scopes)
            .await
            .unwrap()
    );
    assert_eq!(
        db.get_agent_memory(row.id).await.unwrap().unwrap().content,
        "original"
    );
    let other = [("user".into(), "other".into())];
    let updated = db
        .update_agent_memory_in_scopes(
            row.id,
            &other,
            AgentMemoryUpdateRecord {
                content: Some("allowed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content, "allowed");
    assert_eq!(updated.tags_json, row.tags_json);
    assert_eq!(updated.expires_at, row.expires_at);
    assert!(
        db.update_agent_memory_in_scopes(row.id, &[], AgentMemoryUpdateRecord::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!db.delete_agent_memory_in_scopes(row.id, &[]).await.unwrap());
    assert!(
        db.update_agent_memory_in_scopes(
            row.id,
            &other,
            AgentMemoryUpdateRecord {
                scope_key: Some("caller".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        db.delete_agent_memory_in_scopes(row.id, &other)
            .await
            .unwrap()
    );
    db.shutdown().await.unwrap();
}
