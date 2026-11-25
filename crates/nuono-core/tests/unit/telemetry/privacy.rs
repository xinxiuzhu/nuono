use super::*;

#[test]
fn historical_private_message_files_are_redacted_on_read() {
    for direction in ["RECV", "SEND", "接收", "发送"] {
        for scene in ["私聊", "会话", "消息"] {
            let message = format!("[{direction}][onebot_v11] {scene}(17) => 私人旧正文");
            let (result, _) = sanitize_file_line(&format!("INFO {message}"));
            assert!(!result.contains("私人旧正文"), "{direction}/{scene}");
            let (result, _) = sanitize_file_line(
                &serde_json::json!({
                    "message":message, "fields":{"run_id":"r-1"}
                })
                .to_string(),
            );
            assert!(!result.contains("私人旧正文"), "{direction}/{scene}");
            assert!(result.contains("r-1"));
        }
    }
}

#[test]
fn usage_counts_remain_visible_while_tokens_are_redacted() {
    let (value, _) = sanitize_value(
        "",
        serde_json::json!({"tokens":71,"access_token":"secret","password":"hidden"}),
    );
    assert_eq!(value["tokens"], 71);
    assert_eq!(value["access_token"], REDACTED);
    assert_eq!(value["password"], REDACTED);
}
