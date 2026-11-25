use chrono::{DateTime, Duration, TimeZone, Utc};
use nuono_memory::*;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}
fn memory(id: i64, key: &str, text: &str) -> MemoryEntry {
    MemoryEntry {
        id,
        scope_type: "user".into(),
        scope_key: "owner".into(),
        memory_key: key.into(),
        content: text.into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        created_by: None,
        updated_by: None,
        created_at: now() - Duration::days(365),
        updated_at: now() - Duration::days(365 - id),
    }
}
fn scopes() -> Vec<MemoryScope> {
    vec![MemoryScope {
        scope_type: "user".into(),
        scope_key: "owner".into(),
    }]
}
fn rank(
    records: &[MemoryEntry],
    query: &str,
    semantic: Option<&HashMap<i64, f64>>,
) -> Vec<MemoryHit> {
    rank_memories(
        records,
        &scopes(),
        &HashMap::new(),
        query,
        semantic,
        &MemoryPolicy::default(),
        now(),
        12,
        &MemoryRecallBudget::default(),
    )
    .unwrap()
}

#[test]
fn only_direct_evidence_admits_records_even_when_pinned_and_recent() {
    let relevant = memory(1, "tea", "I prefer EarlGrey tea.");
    let mut unrelated = memory(2, "hobby", "I collect antique stamps.");
    unrelated.tags_json = Some("[\"explicit\",\"pinned\"]".into());
    unrelated.updated_at = now();
    let rows = vec![relevant, unrelated];
    assert_eq!(
        rank(&rows, "EarlGrey", None)
            .iter()
            .map(|hit| hit.memory_id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(rank(&rows, "火星轨道周期", None).is_empty());
    for query in ["", " ", "???", "what is the", "请问"] {
        assert!(rank(&rows, query, Some(&HashMap::from([(1, 1.0), (2, 1.0)]))).is_empty());
    }
}

#[test]
fn scopes_enabled_and_expiry_are_filtered_before_scoring_or_budgeting() {
    let valid = memory(1, "tea", "EarlGrey");
    let mut foreign = memory(2, "tea", "EarlGrey foreign-sentinel");
    foreign.scope_key = "other-owner".into();
    foreign.content = "EarlGrey ".repeat(20_000);
    let mut group = memory(3, "tea", "EarlGrey group-sentinel");
    group.scope_type = "group".into();
    let mut disabled = memory(4, "tea", "EarlGrey disabled");
    disabled.enabled = false;
    let mut expired = memory(5, "tea", "EarlGrey expired");
    expired.expires_at = Some(now());
    let scores = HashMap::from([(1, 1.0), (2, 1.0), (3, 1.0), (4, 1.0), (5, 1.0)]);
    let rows = [valid, foreign, group, disabled, expired];
    assert_eq!(
        rank(&rows, "EarlGrey", Some(&scores))
            .iter()
            .map(|hit| hit.memory_id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(
        rank_memories(
            &rows,
            &[],
            &HashMap::new(),
            "EarlGrey",
            Some(&scores),
            &MemoryPolicy::default(),
            now(),
            12,
            &MemoryRecallBudget::default()
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn output_candidate_query_and_duplicate_identity_budgets_are_enforced() {
    let rows = [memory(1, "tea", "EarlGrey"), memory(2, "tea", "EarlGrey")];
    let mut budget = MemoryRecallBudget::default();
    budget.lexical.max_rows = 1;
    assert!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &budget
        )
        .is_err()
    );
    budget = MemoryRecallBudget::default();
    budget.lexical.max_query_bytes = 3;
    assert!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &budget
        )
        .is_err()
    );
    budget = MemoryRecallBudget::default();
    budget.max_results = 1;
    assert_eq!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &budget
        )
        .unwrap()
        .len(),
        1
    );
    budget.max_output_bytes = 120;
    assert_eq!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &budget
        )
        .unwrap()
        .len(),
        1
    );
    budget.max_output_bytes = 100;
    assert!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &budget
        )
        .unwrap()
        .is_empty()
    );
    let duplicate = [rows[0].clone(), rows[0].clone()];
    assert!(
        rank_memories(
            &duplicate,
            &scopes(),
            &HashMap::new(),
            "EarlGrey",
            None,
            &MemoryPolicy::default(),
            now(),
            12,
            &MemoryRecallBudget::default()
        )
        .is_err()
    );
}

#[test]
fn semantic_evidence_requires_finite_threshold_and_does_not_change_old_fact_strength() {
    let rows = [memory(1, "residence", "2019年我在杭州租了第一间公寓。")];
    assert!(rank(&rows, "最早居住的城市", None).is_empty());
    let semantic = HashMap::from([(1, 0.9)]);
    let hit = rank(&rows, "最早居住的城市", Some(&semantic))[0].clone();
    assert_eq!(hit.score.semantic, Some(0.9));
    assert_eq!(hit.score.lexical.matched_exact_terms, 0);
    assert_eq!(hit.score.strength, 1.0);
    assert!(rank(&rows, "最早居住的城市", Some(&HashMap::from([(1, 0.1)]))).is_empty());
    for invalid in [f64::NAN, f64::INFINITY, 1.1, -1.1] {
        assert!(
            rank_memories(
                &rows,
                &scopes(),
                &HashMap::new(),
                "residence",
                Some(&HashMap::from([(1, invalid)])),
                &MemoryPolicy::default(),
                now(),
                12,
                &MemoryRecallBudget::default()
            )
            .is_err()
        );
    }
    let mut policy = MemoryPolicy {
        forgetting_enabled: true,
        ..Default::default()
    };
    let profile = MemoryProfile::for_memory(&rows[0]);
    assert!(strength(&rows[0], &profile, &policy, now()) < 0.05);
    assert_eq!(
        rank_memories(
            &rows,
            &scopes(),
            &HashMap::new(),
            "最早居住的城市",
            Some(&semantic),
            &policy,
            now(),
            12,
            &MemoryRecallBudget::default()
        )
        .unwrap()
        .len(),
        1
    );
    for id in [Some(0), Some(-1)] {
        policy.embedding_model_id = id;
        assert!(policy.validate().is_err());
    }
    policy.embedding_model_id = Some(42);
    policy.semantic_threshold = f64::NAN;
    assert!(policy.validate().is_err());
}

#[test]
fn partial_semantic_cache_keeps_uncached_lexical_evidence_on_its_original_scale() {
    let rows = [memory(1, "tea", "EarlGrey"), memory(2, "hobby", "stamps")];
    let lexical = rank(&rows, "EarlGrey", None);
    let partial = rank(&rows, "EarlGrey", Some(&HashMap::from([(2, 0.1)])));
    assert_eq!(partial[0].memory_id, 1);
    assert_eq!(partial[0].score.semantic, None);
    assert_eq!(partial[0].score.relevance, lexical[0].score.relevance);
    let confirmed_negative = rank(&rows, "EarlGrey", Some(&HashMap::from([(1, -1.0)])));
    assert_eq!(confirmed_negative[0].score.semantic, Some(-1.0));
    assert!(confirmed_negative[0].score.relevance < lexical[0].score.relevance);
}

#[test]
fn graph_evidence_and_record_fingerprints_preserve_existing_contract() {
    let rows = [
        memory(1, "tea", "EarlGrey"),
        memory(2, "food", "peanuts allergy"),
    ];
    let graph = MemoryGraph {
        topics: vec![MemoryTopic {
            name: "Preferences".into(),
            summary: "Recorded preferences".into(),
            source_ids: vec![1, 2],
        }],
        links: vec![MemoryLink {
            from_id: 1,
            to_id: 2,
            relation: "related".into(),
            reason: "Both are stated preferences".into(),
        }],
    };
    graph.validate(&rows).unwrap();
    let mut crossed = rows.clone();
    crossed[1].scope_key = "foreign-owner".into();
    assert!(graph.validate(&crossed).is_err());
    assert!(graph.validate(&[rows[0].clone(), rows[0].clone()]).is_err());
    let mut foreign = graph.clone();
    foreign.links[0].to_id = 99;
    assert!(foreign.validate(&rows).is_err());
    let mut omitted = graph;
    omitted.topics[0].source_ids.pop();
    assert!(omitted.validate(&rows).is_err());
    assert_eq!(
        fingerprint(&rows),
        fingerprint(&[rows[1].clone(), rows[0].clone()])
    );
    let mut changed = rows.clone();
    changed[0].content = "coffee".into();
    assert_ne!(fingerprint(&rows), fingerprint(&changed));
    // 旧版配置没有语义检索相关字段，反序列化时默认回退到纯关键词检索模式。
    let policy: MemoryPolicy = serde_json::from_str("{\"half_life_days\":30}").unwrap();
    assert_eq!(policy.embedding_model_id, None);
    assert_eq!(policy.semantic_threshold, 0.5);
}

#[derive(Deserialize)]
struct Fixture {
    memories: Vec<FixtureMemory>,
    queries: Vec<FixtureQuery>,
}
#[derive(Deserialize)]
struct FixtureMemory {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    id: i64,
    key: String,
    content: String,
    vector: Vec<f32>,
}
#[derive(Deserialize)]
struct FixtureQuery {
    query: String,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    expected: Option<i64>,
    vector: Vec<f32>,
}

// 旧版基于二元字符切分（bigram）的相关度打分实现：
// 即便相关度为 0 也会放行近期记录，并按时间新旧程度排序。
fn old_terms(text: &str) -> BTreeSet<String> {
    let chars = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(2048)
        .collect::<Vec<_>>();
    chars
        .windows(2)
        .map(|window| window.iter().collect())
        .collect()
}
fn old_top(records: &[MemoryEntry], query: &str) -> Option<i64> {
    let query = old_terms(query);
    let mut rows = records
        .iter()
        .map(|record| {
            let words = old_terms(&format!("{} {}", record.memory_key, record.content));
            let relevance = if query.is_empty() || words.is_empty() {
                0.0
            } else {
                query.intersection(&words).count() as f64 / words.len().min(query.len()) as f64
            };
            (record, 1.0 + relevance * 2.0)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|(a, sa), (b, sb)| {
        sb.total_cmp(sa)
            .then(b.updated_at.cmp(&a.updated_at))
            .then(b.id.cmp(&a.id))
    });
    rows.first().map(|row| row.0.id)
}

#[test]
fn fixed_fixture_compares_previous_bigram_bm25_and_controlled_semantic_policy() {
    let fixture: Fixture = serde_json::from_str(include_str!("fixtures/recall.json")).unwrap();
    let records = fixture
        .memories
        .iter()
        .map(|row| memory(row.id, &row.key, &row.content))
        .collect::<Vec<_>>();
    let vectors = fixture
        .memories
        .iter()
        .map(|row| row.vector.clone())
        .collect::<Vec<_>>();
    let mut old_correct = 0;
    let mut lexical_correct = 0;
    let mut hybrid_correct = 0;
    for case in &fixture.queries {
        old_correct += usize::from(old_top(&records, &case.query) == case.expected);
        let lexical = rank(&records, &case.query, None)
            .first()
            .map(|hit| hit.memory_id);
        lexical_correct += usize::from(lexical == case.expected);
        let semantic = if case.expected.is_some() {
            Some(
                records
                    .iter()
                    .zip(nuono_rag::cosine_scores(&vectors, &case.vector).unwrap())
                    .map(|(row, score)| (row.id, score))
                    .collect::<HashMap<_, _>>(),
            )
        } else {
            Some(
                records
                    .iter()
                    .map(|row| (row.id, 0.0))
                    .collect::<HashMap<_, _>>(),
            )
        };
        let hybrid = rank(&records, &case.query, semantic.as_ref())
            .first()
            .map(|hit| hit.memory_id);
        assert_eq!(
            hybrid, case.expected,
            "controlled fixture query: {}",
            case.query
        );
        hybrid_correct += usize::from(hybrid == case.expected);
    }
    assert!(
        lexical_correct > old_correct,
        "BM25 must fix exact numbers/words and irrelevant admission in this fixture"
    );
    assert!(
        hybrid_correct > lexical_correct,
        "controlled semantic evidence must add the paraphrase fixture"
    );
    eprintln!(
        "deterministic fixture: old bigram {old_correct}/{}, BM25 {lexical_correct}/{}, hybrid with analytical vectors {hybrid_correct}/{}; not a real-model benchmark",
        fixture.queries.len(),
        fixture.queries.len(),
        fixture.queries.len()
    );
}
