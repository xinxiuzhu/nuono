use super::*;
use nuono_app::agent::store::{AgentMemoryRecord, AgentMemoryUpdateRecord};
use std::{collections::HashMap, sync::Arc};

async fn seed(db: &Database, key: &str, tags: Option<&str>) -> memories::Model {
    db.create_agent_memory(AgentMemoryRecord {
        scope_type: "user".into(),
        scope_key: "memory-metadata-test".into(),
        memory_key: key.into(),
        content: format!("memory {key}"),
        tags_json: tags.map(str::to_owned),
        enabled: true,
        expires_at: None,
        actor: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn recall_metadata_preserves_tag_defaults_explicit_unpins_and_content_edits() {
    let db = crate::test_support::test_db(3).await;
    let inherited = seed(&db, "inherited", Some(r#"["explicit"]"#)).await;
    let unpinned = seed(&db, "unpinned", Some(r#"["explicit"]"#)).await;
    let ordinary = seed(&db, "ordinary", None).await;
    assert!(
        db.memory_profiles(&[inherited.id])
            .await
            .unwrap()
            .is_empty()
    );
    db.pin_memory(unpinned.id, false).await.unwrap();
    let at = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
    db.reinforce_memories(
        &[
            inherited.id,
            unpinned.id,
            ordinary.id,
            inherited.id,
            i64::MAX,
        ],
        at,
    )
    .await
    .unwrap();
    let profiles: HashMap<_, _> = db
        .memory_profiles(&[inherited.id, unpinned.id, ordinary.id])
        .await
        .unwrap()
        .into_iter()
        .map(|profile| (profile.memory_id, profile))
        .collect();
    assert!(profiles[&inherited.id].pinned);
    assert!(!profiles[&unpinned.id].pinned);
    assert!(!profiles[&ordinary.id].pinned);
    for profile in profiles.values() {
        assert_eq!(profile.recall_count, 1);
        assert_eq!(profile.last_recalled_at, Some(at));
    }
    assert_eq!(
        db.recall_memory_candidates(&[("user".into(), "memory-metadata-test".into())], 1)
            .await
            .unwrap()[0]
            .id,
        inherited.id,
    );
    // 记忆被召回并累加召回计数时，不会修改原记忆的正文内容、更新时间（updated_at）或内容指纹。
    assert_eq!(
        db.get_agent_memory(inherited.id)
            .await
            .unwrap()
            .unwrap()
            .updated_at,
        inherited.updated_at
    );
    db.update_agent_memory(
        inherited.id,
        AgentMemoryUpdateRecord {
            content: Some("edited source".into()),
            tags_json: Some("[]".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.reinforce_memories(&[inherited.id, unpinned.id], at)
        .await
        .unwrap();
    let profile = db.memory_profiles(&[inherited.id]).await.unwrap().remove(0);
    assert!(profile.pinned, "first recall freezes the inherited default");
    assert_eq!(profile.recall_count, 2);
    assert!(!db.memory_profiles(&[unpinned.id]).await.unwrap()[0].pinned);
    db.delete_agent_memory(inherited.id).await.unwrap();
    assert!(
        db.memory_profiles(&[inherited.id])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.memory_profiles(&[ordinary.id]).await.unwrap()[0].recall_count,
        1
    );
    assert!(db.pin_memory(inherited.id, true).await.unwrap().is_none());
    db.reinforce_memories(&[inherited.id], at).await.unwrap();
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_recalls_increment_once_per_request_without_losing_explicit_unpin() {
    let db = crate::test_support::test_db(4).await;
    let memory = seed(&db, "concurrent", Some(r#"["pinned"]"#)).await;
    db.pin_memory(memory.id, false).await.unwrap();
    let start = Arc::new(tokio::sync::Barrier::new(13));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let db = db.clone();
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            db.reinforce_memories(&[memory.id, memory.id], chrono::Utc::now())
                .await
        });
    }
    start.wait().await;
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    let profile = db.memory_profiles(&[memory.id]).await.unwrap().remove(0);
    assert_eq!(profile.recall_count, 12);
    assert!(!profile.pinned);
    assert!(profile.last_recalled_at.is_some());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn recall_counter_overflow_rolls_back_the_entire_batch() {
    let db = crate::test_support::test_db(2).await;
    let fresh = seed(&db, "fresh", Some(r#"["explicit"]"#)).await;
    let full = seed(&db, "full", None).await;
    db.pin_memory(full.id, false).await.unwrap();
    memories::Entity::update_many()
        .col_expr(memories::Column::RecallCount, Expr::value(i64::MAX))
        .filter(memories::Column::Id.eq(full.id))
        .exec(db.orm())
        .await
        .unwrap();
    assert!(matches!(
        db.reinforce_memories(&[fresh.id, full.id], chrono::Utc::now())
            .await,
        Err(NuonoError::Validation(_))
    ));
    let row = db.get_agent_memory(fresh.id).await.unwrap().unwrap();
    assert_eq!(row.pinned, None);
    assert_eq!(row.recall_count, 0);
    assert_eq!(row.last_recalled_at, None);
    assert_eq!(
        db.memory_profiles(&[full.id]).await.unwrap()[0].recall_count,
        i64::MAX
    );
    db.shutdown().await.unwrap();
}
