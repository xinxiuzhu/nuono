use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use nuono_memory::*;

fn scope(card: &str) -> MemoryScope {
    MemoryScope {
        scope_type: "user".into(),
        scope_key: format!("account:1:card:{card}"),
    }
}
fn source(card: &str, id: &str) -> MemorySource {
    MemorySource {
        source_id: format!("turn:{id}"),
        scope: scope(card),
        detail: MemoryDetail::Full,
        original: OriginalState::Available,
        media: None,
    }
}
fn memory(card: &str) -> MemoryEntry {
    let now = Utc.with_ymd_and_hms(2026, 11, 7, 0, 0, 0).unwrap();
    MemoryEntry {
        id: 1,
        scope_type: "user".into(),
        scope_key: scope(card).scope_key,
        memory_key: "favorite".into(),
        content: "喜欢茉莉花茶".into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        created_by: None,
        updated_by: None,
        created_at: now,
        updated_at: now,
    }
}
fn hits(rows: &[MemoryEntry]) -> Vec<MemoryHit> {
    rank_memories(
        rows,
        &[scope("a")],
        &HashMap::new(),
        "茉莉花茶",
        None,
        &MemoryPolicy::default(),
        rows[0].created_at,
        4,
        &MemoryRecallBudget::default(),
    )
    .unwrap()
}
fn image() -> MemoryMediaRef {
    MemoryMediaRef {
        asset_id: "asset_0123456789abcdef0123456789abcdef".into(),
        mime: "image/png".into(),
        sha256: "a".repeat(64),
        byte_length: 100,
        media: MemoryMediaKind::Image,
    }
}

#[test]
fn multiple_sources_attach_without_changing_rank_or_crossing_scope() {
    let rows = vec![memory("a")];
    let hits = hits(&rows);
    assert_eq!(hits.len(), 1);
    let sources = HashMap::from([(1, vec![source("a", "first"), source("a", "second")])]);
    let result = attach_sources(&scope("a"), &rows, hits.clone(), &sources).unwrap();
    assert_eq!(result[0].hit, hits[0]);
    assert_eq!(result[0].sources.len(), 2);
    assert_eq!(result[0].scope, scope("a"));
    assert!(attach_sources(&scope("b"), &rows, hits.clone(), &sources).is_err());
    let foreign = HashMap::from([(1, vec![source("b", "foreign")])]);
    assert!(attach_sources(&scope("a"), &rows, hits.clone(), &foreign).is_err());
    let duplicate = HashMap::from([(1, vec![source("a", "same"); 2])]);
    assert!(attach_sources(&scope("a"), &rows, hits.clone(), &duplicate).is_err());
    assert!(attach_sources(&scope("a"), &rows, hits, &HashMap::new()).is_err());
}

#[test]
fn reclaimed_original_is_never_returned_as_readable_and_deleted_source_is_rejected() {
    let mut reference = source("a", "image");
    reference.media = Some(image());
    assert!(reference.readable_media(&scope("a")).unwrap().is_some());
    reference.original = OriginalState::Reclaimed;
    assert!(
        reference.validate(&scope("a")).is_err(),
        "cannot still claim full detail"
    );
    reference.detail = MemoryDetail::Summary;
    assert!(reference.readable_media(&scope("a")).unwrap().is_none());
    let restored: MemorySource =
        serde_json::from_str(&serde_json::to_string(&reference).unwrap()).unwrap();
    assert!(restored.readable_media(&scope("a")).unwrap().is_none());
    assert!(restored.readable_media(&scope("b")).is_err());
    reference.detail = MemoryDetail::Outline;
    reference.original = OriginalState::Deleted;
    assert!(reference.validate(&scope("a")).is_err());
    let rows = vec![memory("a")];
    assert!(
        attach_sources(
            &scope("a"),
            &rows,
            hits(&rows),
            &HashMap::from([(1, vec![reference])])
        )
        .is_err()
    );
}

#[test]
fn media_reference_rejects_unmanaged_ids_hashes_and_out_of_bound_metadata() {
    for id in [
        "https://example.test/photo",
        "data:image/png;base64,AA==",
        "/local/file.png",
        "asset_anything",
    ] {
        let mut media = image();
        media.asset_id = id.into();
        assert!(media.validate().is_err());
    }
    let mut media = image();
    media.sha256 = "z".repeat(64);
    assert!(media.validate().is_err());
    media = image();
    media.byte_length = 32 * 1024 * 1024 + 1;
    assert!(media.validate().is_err());
    media = image();
    media.mime = "image/svg+xml".into();
    assert!(media.validate().is_err());
    media = image();
    media.media = MemoryMediaKind::Audio { duration_ms: 1000 };
    assert!(media.validate().is_err());
    media.mime = "audio/wav".into();
    media.validate().unwrap();
    media.media = MemoryMediaKind::Audio { duration_ms: 0 };
    assert!(media.validate().is_err());
    media.media = MemoryMediaKind::Audio {
        duration_ms: 300_001,
    };
    assert!(media.validate().is_err());
}

#[test]
fn bad_hit_identity_and_unbounded_sources_fail_closed() {
    let rows = vec![memory("a")];
    let mut hit = hits(&rows);
    hit[0].memory_id = 99;
    let sources = HashMap::from([(1, vec![source("a", "first")])]);
    assert!(attach_sources(&scope("a"), &rows, hit, &sources).is_err());
    let mut hit = hits(&rows);
    hit[0].row_index = 99;
    assert!(attach_sources(&scope("a"), &rows, hit, &sources).is_err());
    let sources = HashMap::from([(1, (0..33).map(|i| source("a", &i.to_string())).collect())]);
    assert!(attach_sources(&scope("a"), &rows, hits(&rows), &sources).is_err());
}
