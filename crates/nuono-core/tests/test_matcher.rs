use nuono_core::core::matcher::{
    EventContext, Matcher, MatcherRegistry, Session, SessionBuilder, SessionManager,
};
use nuono_core::core::platform::{
    MessageType, SenderInfo, UnifiedEvent, UnifiedMessage, UnifiedSegment,
};
use std::time::Duration;

fn make_group_message(content: &str) -> UnifiedEvent {
    UnifiedEvent::Message(UnifiedMessage {
        message_id: "1".to_string(),
        self_id: 123456,
        user_id: 100,
        group_id: Some(200),
        content: content.to_string(),
        message_type: MessageType::Group,
        sub_type: "normal".to_string(),
        sender: SenderInfo {
            user_id: "100".to_string(),
            nickname: Some("TestUser".to_string()),
            card: None,
            role: None,
        },
        time: 1700000000,
        raw: serde_json::json!({}),
        segments: vec![UnifiedSegment::Text {
            text: content.to_string(),
        }],
    })
}

fn make_private_message(content: &str) -> UnifiedEvent {
    UnifiedEvent::Message(UnifiedMessage {
        message_id: "2".to_string(),
        self_id: 123456,
        user_id: 100,
        group_id: None,
        content: content.to_string(),
        message_type: MessageType::Private,
        sub_type: "friend".to_string(),
        sender: SenderInfo {
            user_id: "100".to_string(),
            nickname: Some("TestUser".to_string()),
            card: None,
            role: None,
        },
        time: 1700000000,
        raw: serde_json::json!({}),
        segments: vec![UnifiedSegment::Text {
            text: content.to_string(),
        }],
    })
}

#[test]
fn test_session_basic() {
    let mut session = Session::new("test:1:100");
    session.set("key1", serde_json::json!("value1"));
    assert_eq!(session.get("key1"), Some(&serde_json::json!("value1")));
    assert!(session.get("nonexistent").is_none());
}

#[test]
fn test_session_timeout() {
    let session = Session::new("test:1:100").with_timeout(Duration::from_millis(1));
    assert!(!session.is_expired());
}

#[test]
fn test_session_builder() {
    let session = SessionBuilder::new("bot:group:123")
        .timeout(Duration::from_secs(60))
        .steps(3)
        .build();
    assert_eq!(session.id, "bot:group:123");
    assert_eq!(session.total_steps, 3);
}

#[test]
fn test_session_manager() {
    let manager = SessionManager::new();
    let session = Session::new("test_session");
    manager.create(session);
    assert!(manager.has_session("test_session"));
    assert!(!manager.has_session("nonexistent"));
    assert!(manager.get("test_session").is_some());
    manager.remove("test_session");
    assert!(!manager.has_session("test_session"));
}

#[test]
fn test_session_manager_expired_cleanup() {
    let manager = SessionManager::new();
    let mut session = Session::new("expires_soon");
    session.expires_at = Some(std::time::Instant::now());
    manager.create(session);
    let removed = manager.cleanup_expired();
    assert_eq!(removed, 1);
    assert!(!manager.has_session("expires_soon"));
}

#[test]
fn test_matcher_command_matching() {
    let matcher = Matcher::new("test_cmd").command("hello").block(true);
    assert!(matcher.matches(&make_group_message("hello world")));
    assert!(!matcher.matches(&make_group_message("/hello world")));
    assert!(!matcher.matches(&make_group_message("helloworld")));
    assert!(!matcher.matches(&make_group_message("goodbye")));
}

#[test]
fn test_matcher_keyword_matching() {
    let matcher = Matcher::new("test_keyword").keyword("天气").block(false);
    let matching_event = make_group_message("今天天气不错");
    assert!(matcher.matches(&matching_event));
    let non_matching_event = make_group_message("今天吃什么");
    assert!(!matcher.matches(&non_matching_event));
}

#[test]
fn test_matcher_group_only() {
    let matcher = Matcher::new("group_only_cmd").command("test").group_only();
    let group_event = make_group_message("test");
    assert!(matcher.matches(&group_event));
    let private_event = make_private_message("test");
    assert!(!matcher.matches(&private_event));
}

#[test]
fn test_matcher_private_only() {
    let matcher = Matcher::new("private_only_cmd")
        .command("test")
        .private_only();
    let private_event = make_private_message("test");
    assert!(matcher.matches(&private_event));
    let group_event = make_group_message("test");
    assert!(!matcher.matches(&group_event));
}

#[tokio::test]
async fn test_matcher_registry() {
    let session_mgr = std::sync::Arc::new(SessionManager::new());
    let registry = MatcherRegistry::new(session_mgr);
    let matcher = Matcher::new("test").command("ping").block(true);
    registry.register(matcher).await;
    assert_eq!(registry.matcher_count().await, 1);
}

#[test]
fn test_session_advance() {
    let mut session = Session::new("test").with_steps(3);
    assert_eq!(session.current_step, 0);
    assert!(session.advance());
    assert_eq!(session.current_step, 1);
    assert!(session.advance());
    assert_eq!(session.current_step, 2);
    assert!(!session.advance());
    assert_eq!(session.current_step, 3);
}

#[test]
fn test_event_context() {
    let event = make_group_message("hello");
    let ctx = EventContext::new(event);
    assert_eq!(ctx.user_id(), Some(100));
    assert_eq!(ctx.group_id(), Some(200));
    assert_eq!(ctx.content(), Some("hello"));
    assert!(ctx.state.is_empty());
}
