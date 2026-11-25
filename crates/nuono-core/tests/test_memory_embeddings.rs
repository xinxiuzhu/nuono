//! 验证 PostgreSQL 与 SQLite 下按作用域隔离的大规模记忆召回，以及向量缓存可重建能力。
use chrono::{Duration, Utc};
use nuono_ai::embedding::EmbeddingModel;
use nuono_app::agent::store::{AgentMemoryRecord, AgentMemoryUpdateRecord, MemoryEmbeddingRecord};
use nuono_core::{db::Database, entity::agent_memory_embeddings as embeddings, test_support};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};

fn model() -> EmbeddingModel {
    EmbeddingModel {
        id: 7,
        model: "fixture-embedding".into(),
        revision: "revision-a".into(),
    }
}

fn record(memory_id: i64, content: &str) -> MemoryEmbeddingRecord {
    MemoryEmbeddingRecord {
        memory_id,
        model: model(),
        content_fingerprint: hex::encode(Sha256::digest(content.as_bytes())),
        vector: vec![1.0, 2.0],
    }
}

async fn memory(db: &Database, scope: &str, key: &str, content: &str) -> i64 {
    db.upsert_agent_memory(AgentMemoryRecord {
        scope_type: "user".into(),
        scope_key: scope.into(),
        memory_key: key.into(),
        content: content.into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        actor: None,
    })
    .await
    .unwrap()
    .0
    .id
}

#[tokio::test]
async fn wide_candidates_include_old_facts_and_keep_scope_status_and_stable_order() {
    use nuono_core::entity::agent_memories as memories;
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    let now = Utc::now();
    let rows: Vec<_> = (0..640)
        .map(|number| memories::ActiveModel {
            scope_type: Set("user".into()),
            scope_key: Set("a".into()),
            memory_key: Set(format!("fact-{number}")),
            content: Set(format!("Fact {number}")),
            tags_json: Set(None),
            enabled: Set(true),
            expires_at: Set(None),
            created_by: Set(None),
            updated_by: Set(None),
            created_at: Set(nuono_core::clock::store_time(
                now - Duration::seconds(640 - number),
            )),
            updated_at: Set(nuono_core::clock::store_time(
                now - Duration::seconds(640 - number),
            )),
            ..Default::default()
        })
        .collect();
    memories::Entity::insert_many(rows)
        .exec(db.orm())
        .await
        .unwrap();
    let old = db
        .get_agent_memory_by_key("user", "a", "fact-0")
        .await
        .unwrap()
        .unwrap();
    let pinned = db
        .get_agent_memory_by_key("user", "a", "fact-1")
        .await
        .unwrap()
        .unwrap();
    db.pin_memory(pinned.id, true).await.unwrap();
    let foreign = memory(&db, "b", "foreign", "Do not expose this user's fact").await;
    let disabled = memory(&db, "a", "disabled", "Disabled fact").await;
    let expired = memory(&db, "a", "expired", "Expired fact").await;
    db.update_agent_memory(
        disabled,
        AgentMemoryUpdateRecord {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.update_agent_memory(
        expired,
        AgentMemoryUpdateRecord {
            expires_at: Some(Some(now - Duration::seconds(1))),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let scopes = [("user".into(), "a".into())];
    let narrow = db.recall_memory_candidates(&scopes, 256).await.unwrap();
    assert!(!narrow.iter().any(|memory| memory.id == old.id));
    let wide = db.recall_memory_candidates(&scopes, 4096).await.unwrap();
    assert_eq!(wide.len(), 640);
    assert_eq!(wide[0].id, pinned.id);
    assert_eq!(wide.last().unwrap().id, old.id);
    assert!(wide.iter().all(
        |memory| memory.scope_key == "a" && ![foreign, disabled, expired].contains(&memory.id)
    ));
    assert_eq!(
        wide.iter().map(|memory| memory.id).collect::<Vec<_>>(),
        db.recall_memory_candidates(&scopes, u64::MAX)
            .await
            .unwrap()
            .iter()
            .map(|memory| memory.id)
            .collect::<Vec<_>>()
    );
    assert!(
        db.recall_memory_candidates(&[], 4096)
            .await
            .unwrap()
            .is_empty()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn cached_vectors_only_load_authorized_candidates_and_live_content() {
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    let a = memory(&db, "a", "a", "A likes cycling").await;
    let b = memory(&db, "b", "b", "B likes chess").await;
    assert_eq!(
        db.save_memory_embeddings(&[record(a, "A likes cycling"), record(b, "B likes chess")])
            .await
            .unwrap(),
        2
    );
    let candidates = db
        .recall_memory_candidates(&[("user".into(), "a".into())], 4096)
        .await
        .unwrap();
    let ids: Vec<_> = candidates.iter().map(|memory| memory.id).collect();
    let loaded = db.memory_embeddings(&ids, &model()).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].memory_id, a);
    assert_eq!(loaded[0].vector, vec![1.0, 2.0]);
    let mut other_revision = model();
    other_revision.revision = "revision-b".into();
    let mut other_name = model();
    other_name.model = "different-model".into();
    let mut other_id = model();
    other_id.id += 1;
    for other in [other_revision, other_name, other_id] {
        assert!(db.memory_embeddings(&[a], &other).await.unwrap().is_empty());
    }
    db.update_agent_memory(
        a,
        AgentMemoryUpdateRecord {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db.memory_embeddings(&[a], &model())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.save_memory_embeddings(&[record(a, "A likes cycling")])
            .await
            .unwrap(),
        0
    );
    db.update_agent_memory(
        a,
        AgentMemoryUpdateRecord {
            enabled: Some(true),
            expires_at: Some(Some(Utc::now() - Duration::seconds(1))),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db.memory_embeddings(&[a], &model())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.save_memory_embeddings(&[record(a, "A likes cycling")])
            .await
            .unwrap(),
        0
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn edits_invalidate_cached_and_inflight_vectors_and_revisions_replace_one_row() {
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    let id = memory(&db, "a", "home", "Lives in Shanghai").await;
    let old = record(id, "Lives in Shanghai");
    assert_eq!(
        db.save_memory_embeddings(std::slice::from_ref(&old))
            .await
            .unwrap(),
        1
    );
    db.update_agent_memory(
        id,
        AgentMemoryUpdateRecord {
            content: Some("Lives in Beijing".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db.memory_embeddings(&[id], &model())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.save_memory_embeddings(&[old]).await.unwrap(), 0);
    let fresh = record(id, "Lives in Beijing");
    assert_eq!(
        db.save_memory_embeddings(std::slice::from_ref(&fresh))
            .await
            .unwrap(),
        1
    );
    let mut next = fresh;
    next.model.revision = "revision-b".into();
    assert_eq!(
        db.save_memory_embeddings(std::slice::from_ref(&next))
            .await
            .unwrap(),
        1
    );
    assert_eq!(embeddings::Entity::find().count(db.orm()).await.unwrap(), 1);
    assert!(
        db.memory_embeddings(&[id], &model())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.memory_embeddings(&[id], &next.model)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(db.delete_agent_memory(id).await.unwrap());
    assert_eq!(embeddings::Entity::find().count(db.orm()).await.unwrap(), 0);
    assert_eq!(db.save_memory_embeddings(&[next]).await.unwrap(), 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_dimensions_and_vectors_are_cache_misses() {
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    let id = memory(&db, "a", "a", "Unchanged fact").await;
    db.save_memory_embeddings(&[record(id, "Unchanged fact")])
        .await
        .unwrap();
    embeddings::Entity::update_many()
        .set(embeddings::ActiveModel {
            dimensions: Set(1),
            ..Default::default()
        })
        .filter(embeddings::Column::MemoryId.eq(id))
        .exec(db.orm())
        .await
        .unwrap();
    assert!(
        db.memory_embeddings(&[id], &model())
            .await
            .unwrap()
            .is_empty()
    );
    for invalid in ["[0,0]", "[1,null]"] {
        embeddings::Entity::update_many()
            .set(embeddings::ActiveModel {
                dimensions: Set(2),
                vector_json: Set(invalid.into()),
                ..Default::default()
            })
            .filter(embeddings::Column::MemoryId.eq(id))
            .exec(db.orm())
            .await
            .unwrap();
        assert!(
            db.memory_embeddings(&[id], &model())
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        embeddings::Entity::update_many()
            .set(embeddings::ActiveModel {
                vector_json: Set("{malformed}".into()),
                ..Default::default()
            })
            .filter(embeddings::Column::MemoryId.eq(id))
            .exec(db.orm())
            .await
            .is_err(),
        "invalid JSON must be rejected before it enters the portable database"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_cache_batches_are_rejected_without_partial_publication() {
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    let a = memory(&db, "a", "a", "A fact").await;
    let b = memory(&db, "a", "b", "B fact").await;
    let valid = record(a, "A fact");
    let mut invalid = record(b, "B fact");
    invalid.vector = vec![1.0];
    assert!(
        db.save_memory_embeddings(&[valid.clone(), invalid])
            .await
            .is_err()
    );
    assert_eq!(embeddings::Entity::find().count(db.orm()).await.unwrap(), 0);
    for vector in [
        vec![],
        vec![0.0, 0.0],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![1.0; 4097],
    ] {
        let mut invalid = valid.clone();
        invalid.vector = vector;
        assert!(db.save_memory_embeddings(&[invalid]).await.is_err());
    }
    for invalid_model in [
        EmbeddingModel { id: 0, ..model() },
        EmbeddingModel {
            revision: "r".repeat(129),
            ..model()
        },
        EmbeddingModel {
            model: "m".repeat(513),
            ..model()
        },
    ] {
        let mut invalid = valid.clone();
        invalid.model = invalid_model.clone();
        assert!(db.save_memory_embeddings(&[invalid]).await.is_err());
        assert!(db.memory_embeddings(&[a], &invalid_model).await.is_err());
    }
    assert!(
        db.save_memory_embeddings(&[valid.clone(), valid.clone()])
            .await
            .is_err()
    );
    assert!(db.save_memory_embeddings(&vec![valid; 4097]).await.is_err());
    assert!(
        db.memory_embeddings(&vec![a; 4097], &model())
            .await
            .is_err()
    );
    assert_eq!(embeddings::Entity::find().count(db.orm()).await.unwrap(), 0);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn memory_vector_cache_is_a_protected_core_table() {
    use nuono_core::db::{BrowseRequest, TableOwner};
    let db = test_support::test_db(3).await;
    db.ensure_ai_module().await.unwrap();
    db.ensure_ai_module().await.unwrap();
    let schema = db
        .inspect_table("nuono_agent_memory_embeddings")
        .await
        .unwrap();
    assert_eq!(schema.table.owner, TableOwner::Core);
    assert!(!schema.table.browsable);
    assert!(
        schema
            .columns
            .iter()
            .all(|column| !column.readable && !column.writable)
    );
    assert!(
        db.browse_table("nuono_agent_memory_embeddings", &BrowseRequest::default())
            .await
            .is_err()
    );
    db.shutdown().await.unwrap();
}
