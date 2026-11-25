use super::*;
use serde_json::json;

#[test]
fn private_input_and_reply_content_never_enter_diagnostic_trace() {
    let secret = "我最私人的经历和长期记忆";
    let summary = inbound_summary(
        &json!({
            "post_type": "message", "message_type": "private", "platform": "onebot_v11",
            "user_id": 17, "message_id": "m-23", "raw_message": secret,
        }),
        400,
    )
    .unwrap();
    assert_eq!(summary.message_id.as_deref(), Some("m-23"));
    assert_eq!(summary.content_length, secret.chars().count());
    assert!(!summary.line.contains(secret));
    for action in [
        json!({"action":"send_private_msg", "params":{"user_id":17,"message":secret}}),
        json!({"action":"send_msg", "params":{"user_id":17,"message":secret}}),
    ] {
        let summary = outbound_summary("onebot_v11", &action, 400).unwrap();
        assert!(!summary.line.contains(secret));
        assert_eq!(summary.conversation_id, "17");
    }
}

#[test]
fn game_group_previews_keep_commands_and_redact_credentials_before_trace() {
    let summary = inbound_summary(
        &json!({
            "post_type":"message", "message_type":"group", "group_id":42,"user_id":17,
            "raw_message":"greet password=my-secret-token",
        }),
        400,
    )
    .unwrap();
    assert!(summary.line.contains("greet"));
    assert!(!summary.line.contains("my-secret-token"));
}
