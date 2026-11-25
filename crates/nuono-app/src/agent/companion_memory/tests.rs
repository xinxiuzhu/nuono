use super::*;
use serde_json::json;

const SCOPE: &str = "account-chat:v1:fixture:1:card:00000000-0000-0000-0000-000000000001";

fn memory(id: i64, content: &str) -> MemoryEntry {
    MemoryEntry {
        id,
        scope_type: "user".into(),
        scope_key: SCOPE.into(),
        memory_key: format!("fact.{id}"),
        content: content.into(),
        tags_json: None,
        enabled: true,
        expires_at: None,
        created_by: None,
        updated_by: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[test]
fn companion_scope_must_match_the_authenticated_account_and_a_real_card_key() {
    let mut ctx = AgentContext::for_assistant(7, "hello".into());
    assert!(companion_scope(&ctx).is_err());
    let scope = crate::agent::companion_scope_key(7, "00000000-0000-0000-0000-000000000001");
    ctx.account_chat.as_mut().unwrap().memory_scope = Some(scope.clone());
    assert_eq!(companion_scope(&ctx).unwrap(), scope);
    ctx.user_id = 8;
    assert!(companion_scope(&ctx).is_err());
}

#[test]
fn planner_requires_live_same_scope_source_before_model() {
    use nuono_memory::{MemoryDetail, MemoryScope, MemorySource, OriginalState};
    let scope = MemoryScope {
        scope_type: "user".into(),
        scope_key: SCOPE.into(),
    };
    let candidate = memory(7, "喜欢茶");
    assert!(!source_qualified(&candidate, None, &scope));
    assert!(!source_qualified(&candidate, Some(&vec![]), &scope));
    let mut source = MemorySource {
        source_id: "turn:example".into(),
        scope: scope.clone(),
        detail: MemoryDetail::Summary,
        original: OriginalState::Reclaimed,
        media: None,
    };
    assert!(source_qualified(
        &candidate,
        Some(&vec![source.clone()]),
        &scope
    ));
    source.original = OriginalState::Deleted;
    assert!(!source_qualified(
        &candidate,
        Some(&vec![source.clone()]),
        &scope
    ));
    source.original = OriginalState::Available;
    source.scope.scope_key = "different-card".into();
    assert!(!source_qualified(&candidate, Some(&vec![source]), &scope));
}

#[test]
fn automatic_facts_require_verbatim_current_user_evidence() {
    let raw = json!({"intent":"observe","removals":[],"upserts":[{"key":"preference.tea","content":"用户喜欢乌龙茶。","evidence":"喜欢乌龙茶","importance":0.5}]}).to_string();
    let plan =
        parse_memory_plan(&raw, MemoryPhase::AfterReply, "我喜欢乌龙茶。", SCOPE, &[]).unwrap();
    assert!(!plan.control);
    assert_eq!(plan.upserts.len(), 1);
    assert!(
        parse_memory_plan(&raw, MemoryPhase::AfterReply, "今天天气很好。", SCOPE, &[]).is_err()
    );
    let mut malicious: serde_json::Value = serde_json::from_str(&raw).unwrap();
    malicious["upserts"][0]["importance"] = json!(1.1);
    assert!(
        parse_memory_plan(
            &malicious.to_string(),
            MemoryPhase::AfterReply,
            "我喜欢乌龙茶。",
            SCOPE,
            &[]
        )
        .is_err()
    );
}

#[test]
fn forgetting_rejects_unknown_foreign_duplicate_or_expired_targets() {
    let mut candidate = memory(7, "用户喜欢乌龙茶");
    let raw = json!({"intent":"forget","removals":[7],"upserts":[]}).to_string();
    let plan = parse_memory_plan(
        &raw,
        MemoryPhase::BeforeReply,
        "请忘掉我喜欢乌龙茶这件事。",
        SCOPE,
        &[candidate.clone()],
    )
    .unwrap();
    assert!(plan.control);
    assert_eq!(plan.intent, MemoryIntent::Forget);
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::AfterReply,
            "请忘掉乌龙茶",
            SCOPE,
            &[candidate.clone()]
        )
        .is_err()
    );
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::BeforeReply,
            "今天喝了乌龙茶",
            SCOPE,
            &[candidate.clone()]
        )
        .is_err()
    );
    assert!(parse_memory_plan(&raw, MemoryPhase::BeforeReply, "请忘掉乌龙茶", SCOPE, &[]).is_err());
    candidate.scope_key = "another-card".into();
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::BeforeReply,
            "请忘掉乌龙茶",
            SCOPE,
            &[candidate.clone()]
        )
        .is_err()
    );
    candidate.scope_key = SCOPE.into();
    candidate.expires_at = Some(chrono::Utc::now() - chrono::Duration::days(1));
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::BeforeReply,
            "请忘掉乌龙茶",
            SCOPE,
            &[candidate]
        )
        .is_err()
    );
    let duplicate = json!({"intent":"forget","removals":[7,7],"upserts":[]}).to_string();
    assert!(
        parse_memory_plan(
            &duplicate,
            MemoryPhase::BeforeReply,
            "请忘掉乌龙茶",
            SCOPE,
            &[memory(7, "茶")]
        )
        .is_err()
    );
}

#[test]
fn ambiguous_control_does_not_mutate_or_become_observational_memory() {
    let unchanged = r#"{"intent":"observe","removals":[],"upserts":[]}"#;
    let plan = parse_memory_plan(
        unchanged,
        MemoryPhase::BeforeReply,
        "忘掉那件事",
        SCOPE,
        &[],
    )
    .unwrap();
    assert_eq!(plan, MemoryPlan::needs_clarification());
    assert!(
        parse_memory_plan(unchanged, MemoryPhase::AfterReply, "忘掉那件事", SCOPE, &[]).is_err()
    );
    let disguised = r#"{"intent":"needs_clarification","removals":[7],"upserts":[]}"#;
    assert!(
        parse_memory_plan(
            disguised,
            MemoryPhase::BeforeReply,
            "忘掉那件事",
            SCOPE,
            &[memory(7, "茶")]
        )
        .is_err()
    );
}

#[test]
fn quotations_narration_and_negation_do_not_authorize_forgetting() {
    let delete = r#"{"intent":"forget","removals":[7],"upserts":[]}"#;
    for text in [
        "电影里他说‘忘掉过去’",
        "他说，忘掉关于茶的记忆",
        "请不要忘掉我喜欢茶",
        "我忘记带钥匙了",
        "Please do not forget that I like tea",
        "What does \"please forget about tea\" mean?",
    ] {
        assert!(!is_memory_control_request(text), "{text}");
        assert!(
            parse_memory_plan(
                delete,
                MemoryPhase::BeforeReply,
                text,
                SCOPE,
                &[memory(7, "tea")]
            )
            .is_err()
        );
    }
    for text in [
        "请忘掉「我喜欢茶」这件事",
        "你把我的生日记错了",
        "忘记我的旧地址",
        "Could you forget about my old address?",
    ] {
        assert!(is_memory_control_request(text), "{text}");
    }
}

#[test]
fn correction_needs_old_target_new_evidence_and_cannot_overwrite_other_key() {
    let candidates = [memory(7, "喜欢乌龙茶"), memory(8, "喜欢散步")];
    let raw = json!({"intent":"correct","removals":[7],"upserts":[{"key":"fact.7","content":"现在更喜欢红茶","evidence":"更喜欢红茶","importance":0.5}]}).to_string();
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::BeforeReply,
            "你记错了，我更喜欢红茶",
            SCOPE,
            &candidates
        )
        .unwrap()
        .control
    );
    let raw = raw.replace("fact.7", "fact.8");
    assert!(
        parse_memory_plan(
            &raw,
            MemoryPhase::BeforeReply,
            "你记错了，我更喜欢红茶",
            SCOPE,
            &candidates
        )
        .is_err()
    );
}

#[test]
fn canonical_keys_are_preserved_and_episode_namespace_is_reserved() {
    let mut old = memory(7, "喜欢茶");
    old.memory_key = "preference.drink".into();
    let correct = json!({"intent":"correct","removals":[7],"upserts":[{"key":"preference.drink","content":"现在喜欢水","evidence":"喜欢水","importance":0.5}]}).to_string();
    assert!(
        parse_memory_plan(
            &correct,
            MemoryPhase::BeforeReply,
            "你记错了，我喜欢水",
            SCOPE,
            &[old.clone()]
        )
        .is_ok()
    );
    let reserved = correct.replace("preference.drink", "episode:summary");
    assert!(
        parse_memory_plan(
            &reserved,
            MemoryPhase::BeforeReply,
            "你记错了，我喜欢水",
            SCOPE,
            &[old]
        )
        .is_err()
    );
}

fn experiences() -> Vec<CompanionExperience> {
    vec![
        CompanionExperience {
            turn_id: "first".into(),
            user_text: "我今天去公园散步，看见了很多盛开的花。".repeat(4),
            assistant_text: "你愿意分享最喜欢的花吗？".repeat(4),
        },
        CompanionExperience {
            turn_id: "second".into(),
            user_text: "这次散步让我觉得很轻松，之后想经常去。".repeat(4),
            assistant_text: "听起来是很愉快的一天。".repeat(4),
        },
    ]
}

#[test]
fn compression_requires_exact_sources_and_actually_saves_space() {
    let source = experiences();
    let raw = json!({"source_turn_ids":["second","first"],"summary":"用户分享公园散步的经历，觉得轻松，希望以后常去。"}).to_string();
    assert!(parse_companion_summary(&raw, &source).is_ok());
    for ids in [
        json!(["first"]),
        json!(["first", "foreign"]),
        json!(["first", "first", "second"]),
    ] {
        let raw = json!({"source_turn_ids":ids,"summary":"一次散步"}).to_string();
        assert!(parse_companion_summary(&raw, &source).is_err());
    }
    let unchanged = json!({"source_turn_ids":["first","second"],"summary":source.iter().map(|turn| format!("{}{}",turn.user_text,turn.assistant_text)).collect::<String>()}).to_string();
    assert!(parse_companion_summary(&unchanged, &source).is_err());
    let mut control = source;
    control[0].user_text = "请忘掉公园散步的事情".into();
    assert!(parse_companion_summary(&raw, &control).is_err());
}

#[test]
fn unknown_fields_and_excessive_model_outputs_are_rejected() {
    let raw = r#"{"intent":"observe","removals":[],"upserts":[],"run_sql":"DELETE"}"#;
    assert!(parse_memory_plan(raw, MemoryPhase::AfterReply, "普通闲聊", SCOPE, &[]).is_err());
    assert!(
        parse_memory_plan(
            &" ".repeat(MAX_RESPONSE_BYTES + 1),
            MemoryPhase::AfterReply,
            "普通闲聊",
            SCOPE,
            &[]
        )
        .is_err()
    );
    let too_many = json!({"intent":"observe","removals":[],"upserts":(0..9).map(|id| json!({"key":format!("fact.{id}"),"content":"事实","evidence":"事实","importance":0.5})).collect::<Vec<_>>()}).to_string();
    assert!(parse_memory_plan(&too_many, MemoryPhase::AfterReply, "这是事实", SCOPE, &[]).is_err());
}
