//! 个人伴侣角色卡的人格设定与自动记忆隔离集成测试（穿透真实 HTTP 路由与模型调用边界）。
use super::*;
use nuono_app::{
    agent::{companion_history_key, companion_scope_key},
    companion::CompanionCard,
};
use std::time::Duration;

const VIEW: &str = "/api/assistant";
const SEND: &str = "/api/assistant/messages";
const CARDS: &str = "/api/assistant/cards";
const CLEAR: &str = "/api/assistant/data/clear-conversation";
const CLEAR_ALL: &str = "/api/assistant/data/clear-all";
const A_FACT: &str = "我喜欢松林茶。";
const B_FACT: &str = "我喜欢赤砂茶。";
const FORGET: &str = "请忘掉我喜欢松林茶这件事。";

fn last_user(body: &Value) -> &str {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap()
}

fn is_planner(body: &Value) -> bool {
    body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .contains("个人伴侣的记忆整理器")
}

fn model_reply(body: &Value) -> String {
    if !is_planner(body) {
        return if body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("fixture-persona-a")
        {
            "A 正在陪你聊。"
        } else {
            "B 正在陪你聊。"
        }
        .into();
    }
    let payload: Value = serde_json::from_str(last_user(body)).unwrap();
    let text = payload["user_text"].as_str().unwrap();
    if text == FORGET {
        let ids = payload["memories"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|memory| memory["content"] == A_FACT)
            .map(|memory| memory["id"].clone())
            .collect::<Vec<_>>();
        return json!({"intent":"forget","removals":ids,"upserts":[]}).to_string();
    }
    let updates = if matches!(text, A_FACT | B_FACT) {
        vec![json!({"key":"preference.tea","content":text,"evidence":text,"importance":0.8})]
    } else {
        Vec::new()
    };
    json!({"intent":"observe","removals":[],"upserts":updates}).to_string()
}

async fn view(fixture: &Fixture, token: &str) -> Value {
    let (status, body) = fixture.authenticated(VIEW, token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn send(fixture: &Fixture, token: &str, text: &str) -> Value {
    let expected = view(fixture, token).await["turns"]
        .as_array()
        .unwrap()
        .len()
        + 1;
    let (status, body) = fixture
        .authenticated_json(
            "POST",
            SEND,
            token,
            json!({"request_id":uuid::Uuid::new_v4(),"text":text}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let data = view(fixture, token).await;
            if data["busy"] == false && data["turns"].as_array().unwrap().len() == expected {
                assert_eq!(data["turns"][expected - 1]["status"], "succeeded", "{data}");
                return data;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("private character turn did not settle")
}

async fn activate(fixture: &Fixture, token: &str, staff: &str, account: i64, id: &str) -> Value {
    let (status, body) = fixture
        .authenticated_json(
            "POST",
            &format!("{CARDS}/{id}/activate?account_id={account}"),
            staff,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = view(fixture, token).await;
    assert_eq!(data["active_card"]["id"], id);
    data
}

async fn facts(fixture: &Fixture, scope: &str) -> Vec<String> {
    fixture
        .state
        .db
        .list_agent_memories_for_scopes(&[("user".into(), scope.into())], 100)
        .await
        .unwrap()
        .into_iter()
        .map(|memory| memory.content)
        .collect()
}

fn chat_request(requests: &[Value], text: &str) -> Value {
    requests
        .iter()
        .rev()
        .find(|request| !is_planner(request) && last_user(request) == text)
        .unwrap_or_else(|| panic!("missing model request for {text}"))
        .clone()
}

fn assert_private_prompt(body: &Value, persona: &str, present: &str, absent: &str) {
    let messages = body["messages"].to_string();
    assert!(messages.contains(persona), "{body}");
    assert!(messages.contains(present), "{body}");
    assert!(
        !messages.contains(absent),
        "cross-character context leaked: {body}"
    );
    assert!(
        body["messages"].as_array().unwrap().iter().any(|message| {
            message["role"] == "system"
                && message["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(present)
        }),
        "the fact must be recalled from memory, not only visible in history: {body}"
    );
}

#[tokio::test]
async fn companion_experience_isolates_cards_and_honors_forgetting_and_clear_routes() {
    let fixture = Fixture::new(true).await;
    let staff = fixture.session("companion-experience-staff", "owner").await;
    let alice = fixture.session("companion-experience-alice", "user").await;
    let bob = fixture.session("companion-experience-bob", "user").await;
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let provider = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post({
            let captured = captured.clone();
            move |axum::Json(body): axum::Json<Value>| {
                let captured = captured.clone();
                async move {
                    let content = model_reply(&body);
                    captured.lock().await.push(body);
                    axum::Json(json!({
                        "id":"companion-fixture", "object":"chat.completion", "created":1,
                        "model":"fixture", "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    fixture
        .state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "companion-experience-fixture",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-key"),
            base_url: &format!("http://{address}/v1"),
            is_default: true,
        })
        .await
        .unwrap();
    fixture.state.config.update_patch(nuono_core::db::RuntimeConfigPatch {
        settings: json!({"ai":{"enabled":true,"agent_memory_enabled":true,"agent_memory_auto_extract_enabled":true}}),
        secrets: Vec::new(),
    }).await.unwrap();
    let (status, body) = fixture
        .authenticated_json(
            "PUT",
            "/api/assistant/settings",
            &staff,
            json!({"enabled":true,"require_grant":true,"daily_limit":10}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let user_id = fixture
        .state
        .db
        .find_user_by_username("companion-experience-alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let initial = view(&fixture, &alice).await;
    assert_eq!(initial["available"], false);
    // 首次访问个人助手接口时，会在创建角色卡 A 和 B 之前先自动初始化一张默认角色卡。
    let default_card: CompanionCard =
        serde_json::from_value(initial["active_card"].clone()).unwrap();
    let mut cards = Vec::<CompanionCard>::new();
    for (name, prompt) in [("A", "fixture-persona-a"), ("B", "fixture-persona-b")] {
        let (status, body) = fixture
            .authenticated_json(
                "POST",
                &format!("{CARDS}?account_id={user_id}"),
                &staff,
                json!({"name":name,"system_prompt":prompt,"user_address":"朋友"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        cards.push(serde_json::from_value(body["data"].clone()).unwrap());
    }
    let (status, body) = fixture.authenticated_json("PUT", "/api/assistant/grants", &staff,
        json!({"account_id":user_id.to_string(),"expires_at":(chrono::Utc::now()+chrono::Duration::days(1)).timestamp_millis().to_string(),"daily_limit":10})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let a = &cards[0].id;
    let b = &cards[1].id;
    let a_scope = companion_scope_key(user_id, a);
    let b_scope = companion_scope_key(user_id, b);

    activate(&fixture, &alice, &staff, user_id, a).await;
    let first = send(&fixture, &alice, A_FACT).await;
    assert_eq!(first["remaining"], 9);
    assert_eq!(first["messages"][1]["content"], "A 正在陪你聊。");
    assert_eq!(facts(&fixture, &a_scope).await, [A_FACT]);
    send(&fixture, &alice, "松林茶是什么？").await;

    let switched = activate(&fixture, &alice, &staff, user_id, b).await;
    assert_eq!(switched["messages"], json!([]));
    assert_eq!(switched["turns"], json!([]));
    let second = send(&fixture, &alice, B_FACT).await;
    assert_eq!(second["messages"][1]["content"], "B 正在陪你聊。");
    assert_eq!(facts(&fixture, &b_scope).await, [B_FACT]);
    assert_eq!(facts(&fixture, &a_scope).await, [A_FACT]);
    send(&fixture, &alice, "赤砂茶是什么？").await;

    let restored = activate(&fixture, &alice, &staff, user_id, a).await;
    assert_eq!(restored["messages"].as_array().unwrap().len(), 4);
    assert!(!restored["messages"].to_string().contains(B_FACT));
    let continued = send(&fixture, &alice, "松林茶还记得吗？").await;
    assert_eq!(continued["remaining"], 5);
    {
        let requests = captured.lock().await;
        for text in ["松林茶是什么？", "松林茶还记得吗？"] {
            assert_private_prompt(
                &chat_request(&requests, text),
                "fixture-persona-a",
                A_FACT,
                B_FACT,
            );
        }
        assert_private_prompt(
            &chat_request(&requests, "赤砂茶是什么？"),
            "fixture-persona-b",
            B_FACT,
            A_FACT,
        );
        assert!(
            !chat_request(&requests, B_FACT)["messages"]
                .to_string()
                .contains(A_FACT)
        );
    }

    let before_epoch = fixture.state.db.companion_epoch(&a_scope).await.unwrap();
    let forgotten = send(&fixture, &alice, FORGET).await;
    assert_eq!(
        forgotten["remaining"], continued["remaining"],
        "forgetting must not consume chat quota"
    );
    let after_epoch = fixture.state.db.companion_epoch(&a_scope).await.unwrap();
    assert!(after_epoch > before_epoch);
    assert!(facts(&fixture, &a_scope).await.is_empty());
    assert_eq!(facts(&fixture, &b_scope).await, [B_FACT]);
    assert_eq!(
        forgotten["messages"].as_array().unwrap().len(),
        6,
        "old history remains viewable after forgetting"
    );
    assert!(
        fixture
            .state
            .db
            .recent_ai_conversations(&companion_history_key(&a_scope, after_epoch), 20)
            .await
            .unwrap()
            .is_empty()
    );
    let receipts = fixture
        .state
        .db
        .assistant_turns_for_card(user_id, a)
        .await
        .unwrap();
    let control = receipts.last().unwrap();
    assert!(control.control);
    assert_eq!(control.text, "记忆调整");
    assert!(control.prompt_snapshot.is_empty());
    assert!(!control.retryable);
    assert!(!serde_json::to_string(control).unwrap().contains(FORGET));

    send(&fixture, &alice, "现在想聊什么？").await;
    {
        let requests = captured.lock().await;
        let next = chat_request(&requests, "现在想聊什么？");
        assert!(
            !next["messages"].to_string().contains(A_FACT),
            "forgotten detail reentered prompt: {next}"
        );
        assert!(!next["messages"].to_string().contains(FORGET));
    }

    for token in [&bob, &staff] {
        let (status, _) = fixture
            .authenticated_json("POST", CLEAR, token, json!({"card_id":a}))
            .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "another account cannot clear Alice's card"
        );
    }
    let (status, body) = fixture
        .authenticated_json("POST", CLEAR, &alice, json!({"card_id":b}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(facts(&fixture, &b_scope).await, [B_FACT]);
    let cleared = activate(&fixture, &alice, &staff, user_id, b).await;
    assert_eq!(cleared["messages"], json!([]));
    assert_eq!(cleared["turns"], json!([]));
    let retained = send(&fixture, &alice, "赤砂茶还记得吗？").await;
    assert_eq!(retained["remaining"], 3);
    {
        let requests = captured.lock().await;
        let request = chat_request(&requests, "赤砂茶还记得吗？");
        assert_private_prompt(&request, "fixture-persona-b", B_FACT, A_FACT);
        assert_eq!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user")
                .count(),
            1,
            "cleared history must not be sent to the model"
        );
        assert!(request["messages"].to_string().contains("原始细节已回收"));
    }

    let (status, body) = fixture
        .authenticated_json("POST", CLEAR_ALL, &alice, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (card, scope) in [(a, &a_scope), (b, &b_scope)] {
        assert!(facts(&fixture, scope).await.is_empty());
        let data = activate(&fixture, &alice, &staff, user_id, card).await;
        assert_eq!(data["messages"], json!([]));
        assert_eq!(data["turns"], json!([]));
        assert_eq!(
            data["remaining"], retained["remaining"],
            "deletion must preserve quota records"
        );
    }
    let (status, body) = fixture.authenticated(CARDS, &alice).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut retained_cards: Vec<CompanionCard> =
        serde_json::from_value(body["data"]["cards"].clone()).unwrap();
    cards.push(default_card);
    cards.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    retained_cards.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    assert_eq!(
        retained_cards, cards,
        "clearing personal data must preserve the original default and custom cards unchanged"
    );
    upstream.abort();
    fixture.close().await;
}
