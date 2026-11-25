use super::*;
use nuono_core::{
    api::Bot,
    core::{Command, CommandHandler, MatchData, Plugins},
    plugin::Event,
};

struct ReplyCommand;
#[async_trait::async_trait]
impl CommandHandler for ReplyCommand {
    async fn handle(&self, bot: &Bot, _: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        bot.send("reply from the robot pipeline").await?;
        Ok(())
    }
}

#[tokio::test]
async fn web_chat_resumes_owns_and_runs_real_commands_without_test_privileges() {
    let f = Fixture::new(true).await;
    let alice = f.session("chat-alice", "user").await;
    let bob = f.session("chat-bob", "user").await;
    let admin = f.session("chat-admin", "owner").await;
    let open = |body| f.authenticated_json("POST", "/api/chat/sessions", &alice, body);
    assert_eq!(
        f.request("POST", "/api/chat/sessions", json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        open(json!({"group_role":"owner"})).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, session) = open(json!({})).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let id = session["data"]["id"].as_str().unwrap();
    assert_eq!(session["data"]["message_type"], "private");
    assert_eq!(session["data"]["group_role"], "member");
    assert_eq!(session["data"]["sender_name"], "chat-alice");
    assert_eq!(session["data"]["test_mode"], false);
    assert_eq!(open(json!({})).await.1["data"]["id"], id);

    for (method, path, body) in [
        ("GET", format!("/api/chat/sessions/{id}"), Value::Null),
        (
            "POST",
            format!("/api/chat/sessions/{id}/messages"),
            json!({"text":"hello"}),
        ),
        (
            "GET",
            format!("/api/chat/sessions/{id}/media/file"),
            Value::Null,
        ),
        ("DELETE", format!("/api/chat/sessions/{id}"), Value::Null),
    ] {
        assert_eq!(
            f.authenticated_json(method, &path, &bob, body).await.0,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    let test_options =
        json!({"message_type":"group","sender_name":"test actor","group_role":"owner"});
    assert_eq!(
        f.authenticated_json(
            "POST",
            "/api/sandbox/sessions",
            &alice,
            test_options.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, testing) = f
        .authenticated_json("POST", "/api/sandbox/sessions", &admin, test_options)
        .await;
    assert_eq!(status, StatusCode::OK, "{testing}");
    let test_id = testing["data"]["id"].as_str().unwrap();
    assert_eq!(
        f.authenticated(&format!("/api/chat/sessions/{test_id}"), &admin)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    f.state
        .manager
        .plugin_host
        .register_plugin_config(Plugins::new("WebChatTest"))
        .await;
    for trigger in Command::new("web-chat-proof", Arc::new(ReplyCommand)).to_triggers() {
        f.state
            .manager
            .plugin_host
            .trigger_registry()
            .register(trigger.with_plugin_name("WebChatTest"));
    }
    let (status, receipt) = f
        .authenticated_json(
            "POST",
            &format!("/api/chat/sessions/{id}/messages"),
            &alice,
            json!({"text":"web-chat-proof"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (_, body) = f
                .authenticated(&format!("/api/chat/sessions/{id}"), &alice)
                .await;
            if body["data"]["processing"] == 0 {
                break body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let messages = result["data"]["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message["direction"] == "outgoing"
                && message["segments"]
                    .to_string()
                    .contains("reply from the robot pipeline")),
        "{result}"
    );
    assert!(
        messages
            .iter()
            .all(|message| message["direction"] != "system" && message.get("action").is_none()),
        "ordinary chat does not expose diagnostic traffic: {result}"
    );
    assert_eq!(
        f.authenticated_json(
            "DELETE",
            &format!("/api/chat/sessions/{id}"),
            &alice,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, next) = open(json!({})).await;
    assert_ne!(next["data"]["id"], id);
    f.state
        .sandbox
        .delete(
            f.state
                .db
                .find_user_by_username("chat-alice")
                .await
                .unwrap()
                .unwrap()
                .id,
            next["data"]["id"].as_str().unwrap(),
        )
        .unwrap();
    f.authenticated_json(
        "DELETE",
        &format!("/api/sandbox/sessions/{test_id}"),
        &admin,
        Value::Null,
    )
    .await;
    f.close().await;
}
