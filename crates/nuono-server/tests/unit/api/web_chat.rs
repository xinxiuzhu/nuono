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

struct AdminOnlyCommand;
#[async_trait::async_trait]
impl CommandHandler for AdminOnlyCommand {
    async fn handle(&self, bot: &Bot, _: &Event<'_>, _: MatchData) -> anyhow::Result<()> {
        bot.send("admin-only-reply").await?;
        Ok(())
    }
}

#[tokio::test]
async fn web_chat_maps_site_roles_avoids_shadow_users_and_routes_fallback_to_assistant() {
    use nuono_core::core::permission::PermissionLevel;
    use sea_orm::{ConnectionTrait, Statement};

    let f = Fixture::new(true).await;
    let user = f.session("chat-regular", "user").await;
    let owner = f.session("chat-owner", "owner").await;

    f.state
        .manager
        .plugin_host
        .register_plugin_config(Plugins::new("WebChatAuthTest"))
        .await;
    for trigger in Command::new("web-chat-admin-cmd", Arc::new(AdminOnlyCommand))
        .with_permission(PermissionLevel::BotAdmin)
        .to_triggers()
    {
        f.state
            .manager
            .plugin_host
            .trigger_registry()
            .register(trigger.with_plugin_name("WebChatAuthTest"));
    }
    for trigger in Command::new("web-chat-public-cmd", Arc::new(ReplyCommand)).to_triggers() {
        f.state
            .manager
            .plugin_host
            .trigger_registry()
            .register(trigger.with_plugin_name("WebChatAuthTest"));
    }

    let send_and_wait = |session_token: String, chat_id: String, text: String| {
        let f_ref = &f;
        async move {
            let (status, receipt) = f_ref
                .authenticated_json(
                    "POST",
                    &format!("/api/chat/sessions/{chat_id}/messages"),
                    &session_token,
                    json!({"text": text}),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{receipt}");
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let (_, body) = f_ref
                        .authenticated(&format!("/api/chat/sessions/{chat_id}"), &session_token)
                        .await;
                    if body["data"]["processing"] == 0 {
                        break body;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    };

    // 站长在网页对话中自动继承 BotOwner 权限，可直接触发管理员插件指令。
    let (_, owner_session) = f
        .authenticated_json("POST", "/api/chat/sessions", &owner, json!({}))
        .await;
    let owner_chat_id = owner_session["data"]["id"].as_str().unwrap().to_owned();
    let owner_result = send_and_wait(
        owner.clone(),
        owner_chat_id.clone(),
        "web-chat-admin-cmd".into(),
    )
    .await;
    assert!(
        owner_result["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["direction"] == "outgoing"
                && m["segments"].to_string().contains("admin-only-reply")),
        "{owner_result}"
    );

    // 普通用户无法触发管理员指令，但即使 AI 未开启也能触发普通插件指令，且普通闲聊由伴侣助手给出明确提示。
    let (_, user_session) = f
        .authenticated_json("POST", "/api/chat/sessions", &user, json!({}))
        .await;
    let user_chat_id = user_session["data"]["id"].as_str().unwrap().to_owned();
    let user_admin_attempt = send_and_wait(
        user.clone(),
        user_chat_id.clone(),
        "web-chat-admin-cmd".into(),
    )
    .await;
    let attempt_messages = user_admin_attempt["data"]["messages"].as_array().unwrap();
    assert!(
        !attempt_messages
            .iter()
            .any(|m| m["segments"].to_string().contains("admin-only-reply")),
        "普通用户绝不能执行管理员插件指令: {user_admin_attempt}"
    );
    assert!(
        attempt_messages.iter().any(|m| m["direction"] == "outgoing"
            && m["segments"].to_string().contains("网页 AI 对话尚未开启")),
        "未命中插件时应由个人伴侣助手统一兜底: {user_admin_attempt}"
    );

    let user_public_result = send_and_wait(
        user.clone(),
        user_chat_id.clone(),
        "web-chat-public-cmd".into(),
    )
    .await;
    assert!(
        user_public_result["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["direction"] == "outgoing"
                && m["segments"]
                    .to_string()
                    .contains("reply from the robot pipeline")),
        "即使 AI 未开启，普通插件指令也必须正常响应: {user_public_result}"
    );

    // 网页会话使用虚拟负数 ID 与真实账号上下文，绝不能在 nuono_users 中制造占位用户。
    let shadow_count = f
        .state
        .db
        .orm()
        .query_one_raw(Statement::from_string(
            f.state.db.backend(),
            "SELECT COUNT(*) AS n FROM nuono_users WHERE account_kind = 'shadow'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(shadow_count, 0, "网页对话不应创建任何 shadow 占位账号");

    f.authenticated_json(
        "DELETE",
        &format!("/api/chat/sessions/{owner_chat_id}"),
        &owner,
        Value::Null,
    )
    .await;
    f.authenticated_json(
        "DELETE",
        &format!("/api/chat/sessions/{user_chat_id}"),
        &user,
        Value::Null,
    )
    .await;
    f.close().await;
}
