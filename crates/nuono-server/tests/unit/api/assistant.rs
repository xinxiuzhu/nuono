use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use nuono_app::transport::TransportFrame;
use nuono_core::core::RuntimeBotRoute;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BOT: i64 = 123450;
const QQ: i64 = 123451;
const SETTINGS: &str = "/api/assistant/settings";
const VIEW: &str = "/api/assistant";
const SEND: &str = "/api/assistant/messages";
const LINK: &str = "/api/assistant/link";

async fn registered_user(fixture: &Fixture) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (mail, received) = tokio::sync::oneshot::channel();
    let smtp = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = socket.split();
        let mut lines = BufReader::new(reader).lines();
        let mut mail = Some(mail);
        writer.write_all(b"220 fixture ESMTP\r\n").await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let reply = match line.split_whitespace().next() {
                Some("EHLO") => "250-fixture\r\n250 AUTH PLAIN\r\n",
                Some("AUTH") => "235 authenticated\r\n",
                Some("MAIL" | "RCPT") => "250 ok\r\n",
                Some("DATA") => {
                    writer.write_all(b"354 send message\r\n").await.unwrap();
                    let mut message = String::new();
                    while let Some(line) = lines.next_line().await.unwrap() {
                        if line == "." {
                            break;
                        }
                        message.push_str(&line);
                        message.push_str("\r\n");
                    }
                    mail.take().unwrap().send(message).unwrap();
                    "250 queued\r\n"
                }
                Some("QUIT") => {
                    writer.write_all(b"221 goodbye\r\n").await.unwrap();
                    break;
                }
                _ => "500 unsupported\r\n",
            };
            writer.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    fixture.state.config.update_patch(nuono_core::db::RuntimeConfigPatch {
        settings: json!({"mail": {
            "enabled":true, "registration_enabled":true, "smtp_host":"127.0.0.1", "smtp_port":port,
            "smtp_username":"fixture", "from_address":"fixture@example.test", "encryption":"none",
            "timeout_secs":4
        }}),
        secrets: vec![
            ("mail.smtp_password".into(), Some("fixture-password".into())),
            ("security.registration_otp_pepper".into(), Some("fixture-registration-pepper-that-is-long-enough".into())),
        ],
    }).await.unwrap();
    let username = "assistant-user";
    let email = "assistant@example.test";
    let password = "correct-horse-battery";
    let (status, body) = fixture
        .request(
            "POST",
            "/api/auth/register",
            json!({
                "username":username, "email":email, "password":password
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let message = tokio::time::timeout(Duration::from_secs(4), received)
        .await
        .unwrap()
        .unwrap();
    smtp.abort();

    // 当测试 SMTP 服务端未声明支持 8BITMIME 时，Lettre 会将 UTF-8 纯文本 MIME 分段编码为 Base64。
    let (_, body) = message.split_once("\r\n\r\n").unwrap();
    let text = body
        .split("\r\n--")
        .find_map(|part| {
            let (headers, content) = part.split_once("\r\n\r\n")?;
            if !headers.to_ascii_lowercase().contains("text/plain") {
                return None;
            }
            if headers.to_ascii_lowercase().contains("base64") {
                let encoded = content.split_whitespace().collect::<String>();
                return String::from_utf8(STANDARD.decode(encoded).ok()?).ok();
            }
            Some(content.to_owned())
        })
        .unwrap_or_else(|| body.to_owned());
    let code = text
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| part.len() == nuono_core::db::REGISTRATION_OTP_LENGTH)
        .expect("SMTP fixture did not receive a verification code");
    let (status, body) = fixture
        .request(
            "POST",
            "/api/auth/verify-registration-otp",
            json!({
                "email":email, "code":code
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let login = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username":username,"password":password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    login
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|value| {
            value
                .to_str()
                .ok()?
                .strip_prefix("session_token=")?
                .split(';')
                .next()
                .map(str::to_owned)
        })
        .expect("registered user did not receive a session")
}

async fn onebot_message(
    fixture: &Fixture,
    route: &RuntimeBotRoute,
    sender: i64,
    id: i64,
    kind: &str,
    text: &str,
) -> tokio::sync::mpsc::Receiver<TransportFrame> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut event = json!({
        "post_type": "message",
        "message_type": if kind == "normal" { "group" } else { "private" },
        "sub_type": kind,
        "self_id": BOT,
        "user_id": sender,
        "time": chrono::Utc::now().timestamp(),
        "message_id": id,
        "raw_message": text,
        "message": [{"type": "text", "data": {"text": text}}],
    });
    if kind == "normal" {
        event["group_id"] = json!(123452);
    }
    fixture
        .state
        .manager
        .event_kernel
        .process_runtime_post_event_with_route(BOT, &event.to_string(), event, tx, Some(route))
        .await
        .unwrap();
    rx
}

async fn expect_qq_reply(mut messages: tokio::sync::mpsc::Receiver<TransportFrame>, text: &str) {
    let frame = tokio::time::timeout(Duration::from_secs(4), messages.recv())
        .await
        .expect("OneBot did not receive a reply")
        .expect("no OneBot reply");
    let TransportFrame::Text(payload) = frame else {
        panic!("unexpected OneBot frame")
    };
    assert!(payload.contains(text), "{payload}");
}

async fn confirm_link(fixture: &Fixture, token: &str, issued: &Value) {
    let path = format!(
        "/api/account/bindings/{}",
        issued["data"]["request_id"].as_str().unwrap()
    );
    let (status, candidate) = fixture.authenticated(&path, token).await;
    assert_eq!(status, StatusCode::OK, "{candidate}");
    assert_eq!(candidate["data"]["state"], "candidate");
    assert_eq!(
        candidate["data"]["candidate"]["native_user_id"],
        QQ.to_string()
    );
    let (status, confirmed) = fixture
        .authenticated_json(
            "POST",
            &path,
            token,
            json!({"revision": candidate["data"]["candidate"]["revision"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
}

async fn wait_for_view(fixture: &Fixture, token: &str, turns: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let (status, body) = fixture.authenticated(VIEW, token).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let data = &body["data"];
            if data["busy"] == false && data["messages"].as_array().unwrap().len() == turns * 2 {
                return data.clone();
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    })
    .await
    .expect("assistant conversation did not settle")
}

#[tokio::test]
async fn assistant_shares_context_and_quota_between_web_and_verified_qq() {
    let f = Fixture::new(true).await;
    disable_automatic_memory(&f).await;
    let staff = f.session("assistant-staff", "owner").await;
    let user = registered_user(&f).await;
    let other = f.session("assistant-other", "user").await;
    let group = f
        .state
        .db
        .ensure_group(&nuono_core::db::groups::GroupIdentity::onebot("123452"), "")
        .await
        .unwrap();
    let account = f
        .state
        .db
        .find_user_by_username("assistant-user")
        .await
        .unwrap()
        .unwrap();
    let (status,grant)=f.authenticated_json("POST",&format!("/api/groups/{}/authorizations",group.id),&staff,json!({"user_id":account.id.to_string(),"source_key":"assistant-fixture","plugin_id":null,"valid_from_ms":"0","expires_at_ms":null})).await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert_eq!(
        f.authenticated(&format!("/api/groups/{}/plugins", group.id), &user)
            .await
            .0,
        StatusCode::OK
    );
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let provider = axum::Router::new().route("/v1/chat/completions", axum::routing::post({
        let captured = captured.clone();
        move |axum::Json(body): axum::Json<Value>| {
            let captured = captured.clone();
            async move {
                let mut requests = captured.lock().await;
                requests.push(body);
                axum::Json(json!({
                    "id": "fixture-response",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "fixture",
                    "choices": [{
                        "index": 0, "message": {"role": "assistant", "content": format!("fixture reply {}", requests.len())},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    // 开启 AI 总开关无需预置模型，用于解锁管理端模型配置；但开启具体对外场景仍要求至少一个可用模型。
    let (status, result) = f
        .authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, _) = f
        .authenticated_json(
            "PUT",
            SETTINGS,
            &staff,
            json!({"enabled": true, "daily_limit": 3}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    f.state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "assistant-fixture",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-key"),
            base_url: &format!("http://{address}/v1"),
            is_default: true,
        })
        .await
        .unwrap();

    let config = json!({"enabled":true,"daily_limit":3,"qq_bot_id":BOT.to_string(),"qq_instance_id":"assistant-fixture"});
    assert_eq!(
        f.authenticated_json("PUT", SETTINGS, &user, config.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, result) = f.authenticated_json("PUT", SETTINGS, &staff, config).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(f.state.config.snapshot().await.ai.enabled);
    assert!(!f.state.config.snapshot().await.ai.reply_enabled);
    assert_eq!(wait_for_view(&f, &user, 0).await["remaining"], 3);

    // 临时关闭 AI 总开关时，网页对话应立即进入禁用态而不清空子开关配置；重新开启后自动恢复。
    let (status, result) = f
        .authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": false}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let paused = wait_for_view(&f, &user, 0).await;
    assert_eq!(paused["enabled"], false);
    assert_eq!(paused["access_state"], "disabled");
    let (status, result) = f
        .authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(wait_for_view(&f, &user, 0).await["remaining"], 3);

    let (status, result) = f
        .authenticated_json("POST", LINK, &user, json!({"qq":QQ.to_string()}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let command = result["data"]["command"].as_str().unwrap();
    let route = RuntimeBotRoute::new("onebot", "assistant-fixture", BOT.to_string());
    let other_route = RuntimeBotRoute::new("onebot", "wrong-instance", BOT.to_string());
    onebot_message(&f, &other_route, QQ, 1, "friend", command).await;
    expect_qq_reply(
        onebot_message(&f, &route, QQ + 1, 4, "friend", command).await,
        "绑定请求无效",
    )
    .await;
    assert!(wait_for_view(&f, &user, 0).await["qq"].is_null());
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 5, "normal", command).await,
        "已收到身份验证",
    )
    .await;
    assert!(wait_for_view(&f, &user, 0).await["qq"].is_null());
    confirm_link(&f, &user, &result).await;
    assert_eq!(wait_for_view(&f, &user, 0).await["qq"], QQ.to_string());

    let first = json!({"request_id":uuid::Uuid::new_v4(),"text":"fixture web secret"});
    let (status, result) = f.authenticated_json("POST", SEND, &user, first).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let web = wait_for_view(&f, &user, 1).await;
    assert_eq!(web["remaining"], 2);
    assert_eq!(web["messages"][1]["content"], "fixture reply 1");

    expect_qq_reply(
        onebot_message(&f, &route, QQ, 6, "friend", "fixture QQ secret").await,
        "fixture reply 2",
    )
    .await;
    let qq = wait_for_view(&f, &user, 2).await;
    assert_eq!(qq["remaining"], 1);
    assert_eq!(qq["messages"][2]["content"], "fixture QQ secret");

    let third = json!({"request_id":uuid::Uuid::new_v4(),"text":"what did I say in QQ?"});
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, third.clone())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(wait_for_view(&f, &user, 3).await["remaining"], 0);
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, third).await.0,
        StatusCode::OK
    );
    let fourth = json!({"request_id":uuid::Uuid::new_v4(),"text":"over quota"});
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, fourth).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 7, "friend", "over quota").await,
        "额度已用完",
    )
    .await;
    assert_eq!(wait_for_view(&f, &other, 0).await["remaining"], 3);
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("fixture web secret")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("fixture QQ secret")
    );
    drop(requests);

    assert_eq!(
        f.authenticated_json("DELETE", LINK, &user, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert!(wait_for_view(&f, &user, 3).await["qq"].is_null());

    let config = json!({"enabled":true,"require_grant":true,"daily_limit":3});
    assert_eq!(
        f.authenticated_json("PUT", SETTINGS, &staff, config)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(wait_for_view(&f, &other, 0).await["available"], false);
    let other_id = f
        .state
        .db
        .find_user_by_username("assistant-other")
        .await
        .unwrap()
        .unwrap()
        .id;
    let grant = json!({
        "account_id": other_id.to_string(),
        "expires_at": (chrono::Utc::now() + chrono::Duration::days(1)).timestamp_millis().to_string(),
        "daily_limit": 2,
    });
    let grants = "/api/assistant/grants";
    assert_eq!(
        f.authenticated_json("PUT", grants, &other, grant.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.authenticated_json("PUT", grants, &staff, grant).await.0,
        StatusCode::OK
    );
    assert_eq!(wait_for_view(&f, &other, 0).await["remaining"], 2);
    assert_eq!(
        f.authenticated_json("PUT", grants, &staff, json!({"account_id":other_id}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(wait_for_view(&f, &other, 0).await["available"], false);
    upstream.abort();
    f.close().await;
}

#[derive(Default)]
struct FaultModel {
    mode: std::sync::atomic::AtomicU8,
    calls: std::sync::atomic::AtomicUsize,
    requests: Mutex<Vec<Value>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl FaultModel {
    fn mode(&self, mode: u8) {
        self.mode.store(mode, std::sync::atomic::Ordering::SeqCst);
    }
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

async fn fault_model(f: &Fixture) -> (Arc<FaultModel>, tokio::task::JoinHandle<()>) {
    disable_automatic_memory(f).await;
    use axum::response::IntoResponse;
    let model = Arc::new(FaultModel::default());
    let router = axum::Router::new().route("/v1/chat/completions", axum::routing::post({
        let model = model.clone();
        move |axum::Json(body): axum::Json<Value>| {
            let model = model.clone();
            async move {
                model.requests.lock().await.push(body);
                model.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                model.entered.notify_one();
                let mode = model.mode.load(std::sync::atomic::Ordering::SeqCst);
                if mode == 3 { model.release.notified().await; }
                match mode {
                    1 => (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"message":"private-provider-detail","type":"fixture"}}))).into_response(),
                    2 => std::future::pending().await,
                    _ => axum::Json(json!({
                        "id":"fault-fixture", "object":"chat.completion", "created":1, "model":"fixture",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"recovered answer"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    })).into_response(),
                }
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    f.state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "fault-model",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-key"),
            base_url: &format!("http://{address}/v1"),
            is_default: true,
        })
        .await
        .unwrap();
    (model, server)
}

async fn fault_accounts(f: &Fixture) -> (String, String, RuntimeBotRoute) {
    let staff = f.session("fault-staff", "owner").await;
    let user = f.session("fault-user", "user").await;
    let route = RuntimeBotRoute::new("onebot", "assistant-fixture", BOT.to_string());
    let (status, result) = f
        .authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, result) = f.authenticated_json("PUT", SETTINGS, &staff,
        json!({"enabled":true,"daily_limit":3,"qq_bot_id":BOT.to_string(),"qq_instance_id":"assistant-fixture"})).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, result) = f
        .authenticated_json("POST", LINK, &user, json!({"qq":QQ.to_string()}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    expect_qq_reply(
        onebot_message(
            f,
            &route,
            QQ,
            100,
            "friend",
            result["data"]["command"].as_str().unwrap(),
        )
        .await,
        "已收到身份验证",
    )
    .await;
    confirm_link(f, &user, &result).await;
    (staff, user, route)
}

async fn settled_turns(f: &Fixture, token: &str, count: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let (status, result) = f.authenticated(VIEW, token).await;
            assert_eq!(status, StatusCode::OK, "{result}");
            let view = result["data"].clone();
            let turns = view["turns"].as_array().unwrap();
            if view["busy"] == false
                && turns.len() == count
                && turns.iter().all(|turn| turn["status"] != "running")
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("assistant receipts did not settle")
}

#[tokio::test]
async fn assistant_failures_refund_and_explicit_web_qq_retries_share_one_history_and_quota() {
    let f = Fixture::new(true).await;
    let (model, server) = fault_model(&f).await;
    let (_, user, route) = fault_accounts(&f).await;
    model.mode(1);
    let body = json!({"request_id":uuid::Uuid::new_v4(),"text":"only one question in history"});
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, body.clone())
            .await
            .0,
        StatusCode::OK
    );
    let failed = settled_turns(&f, &user, 1).await;
    assert_eq!(failed["remaining"], 3);
    assert_eq!(failed["turns"][0]["failure"], "model_failed");
    assert!(failed["messages"].as_array().unwrap().is_empty());
    assert!(!failed.to_string().contains("private-provider-detail"));
    let failed_calls = model.calls();
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, body.clone())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        model.calls(),
        failed_calls,
        "a transport replay must not regenerate a failed turn"
    );
    let mut changed = body;
    changed["text"] = json!("different payload");
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, changed).await.0,
        StatusCode::CONFLICT
    );
    let other = f.session("fault-other", "user").await;
    let retry = json!({"request_id":uuid::Uuid::new_v4(),"retry_of":failed["turns"][0]["id"]});
    assert_eq!(
        f.authenticated_json("POST", SEND, &other, retry.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    model.mode(0);
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, retry.clone())
            .await
            .0,
        StatusCode::OK
    );
    let success = settled_turns(&f, &user, 2).await;
    assert_eq!(success["remaining"], 2);
    assert_eq!(success["turns"][0]["retryable"], false);
    assert_eq!(success["turns"][1]["reply"], "recovered answer");
    assert_eq!(success["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, retry).await.0,
        StatusCode::OK
    );
    let calls = model.calls();
    assert_eq!(calls, failed_calls + 1);
    let captured = model.requests.lock().await;
    assert_eq!(
        captured.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["content"] == "only one question in history")
            .count(),
        1
    );
    drop(captured);

    model.mode(1);
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 101, "friend", "QQ failure question").await,
        "额度已返还",
    )
    .await;
    let failed = settled_turns(&f, &user, 3).await;
    assert_eq!(failed["remaining"], 2);
    assert_eq!(failed["turns"][2]["failure"], "model_failed");
    let calls = model.calls();
    let mut duplicate = onebot_message(&f, &route, QQ, 101, "friend", "QQ failure question").await;
    assert!(duplicate.recv().await.is_none());
    assert_eq!(model.calls(), calls);
    model.mode(0);
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 102, "friend", "重试").await,
        "recovered answer",
    )
    .await;
    let after = settled_turns(&f, &user, 4).await;
    assert_eq!(after["remaining"], 1);
    let calls = model.calls();
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 103, "friend", "重试").await,
        "recovered answer",
    )
    .await;
    assert_eq!(
        model.calls(),
        calls,
        "retrieving a saved reply never calls the model"
    );
    assert_eq!(settled_turns(&f, &user, 4).await["remaining"], 1);
    let mut duplicate = onebot_message(&f, &route, QQ, 103, "friend", "重试").await;
    assert!(duplicate.recv().await.is_none());
    server.abort();
    f.close().await;
}

#[tokio::test]
async fn assistant_timeout_and_shutdown_refund_and_persist_interruptions() {
    let f = Fixture::new(true).await;
    let (model, server) = fault_model(&f).await;
    let (_, user, route) = fault_accounts(&f).await;
    f.state
        .config
        .update_patch(nuono_core::db::RuntimeConfigPatch {
            settings: json!({"ai":{"provider_timeout_secs":1}}),
            secrets: vec![],
        })
        .await
        .unwrap();
    model.mode(2);
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 110, "friend", "timeout question").await,
        "回复超时，额度已返还",
    )
    .await;
    let timed_out = settled_turns(&f, &user, 1).await;
    assert_eq!(timed_out["turns"][0]["failure"], "timed_out");
    assert_eq!(timed_out["remaining"], 3);
    f.state
        .config
        .update_patch(nuono_core::db::RuntimeConfigPatch {
            settings: json!({"ai":{"provider_timeout_secs":60}}),
            secrets: vec![],
        })
        .await
        .unwrap();
    let before = model.calls();
    let body = json!({"request_id":uuid::Uuid::new_v4(),"text":"interrupted question"});
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, body.clone())
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while model.calls() == before {
            model.entered.notified().await;
        }
    })
    .await
    .unwrap();
    f.state
        .manager
        .event_kernel
        .agent_runtime
        .shutdown()
        .await
        .unwrap();
    let stopped = settled_turns(&f, &user, 2).await;
    assert_eq!(stopped["remaining"], 3);
    assert_eq!(stopped["turns"][1]["status"], "interrupted");
    assert!(stopped["messages"].as_array().unwrap().is_empty());
    assert_eq!(
        f.state
            .db
            .reconcile_assistant_turns_before_start()
            .await
            .unwrap(),
        0
    );
    // 构造重启后的全新宿主对象，保留原库，不重新执行原请求。
    let restarted = ConnectionManager::new(
        f.state.db.clone(),
        f.state.paths.clone(),
        Arc::new(OneBotAdapter),
        f.state.config.clone(),
        f.state.plugin_config.clone(),
    )
    .await
    .unwrap();
    let account = f
        .state
        .db
        .find_user_by_username("fault-user")
        .await
        .unwrap()
        .unwrap()
        .id;
    let assistant = &restarted.event_kernel.assistant;
    assert_eq!(assistant.view(account).await.unwrap().remaining, 3);
    assert_eq!(
        assistant
            .send_web(
                account,
                serde_json::from_value(body["request_id"].clone()).unwrap(),
                "interrupted question"
            )
            .await
            .unwrap(),
        nuono_app::assistant::AssistantAdmission::Duplicate
    );
    model.mode(0);
    assert_eq!(
        assistant
            .retry_web(
                account,
                uuid::Uuid::new_v4(),
                stopped["turns"][1]["id"].as_str().unwrap().into()
            )
            .await
            .unwrap(),
        nuono_app::assistant::AssistantAdmission::Accepted
    );
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let view = assistant.view(account).await.unwrap();
            if !view.busy
                && view.turns.len() == 3
                && view.turns[2].status == nuono_app::assistant::TurnStatus::Succeeded
            {
                assert_eq!(view.remaining, 2);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    restarted.scheduler.shutdown().await.unwrap();
    restarted
        .event_kernel
        .agent_runtime
        .shutdown()
        .await
        .unwrap();
    restarted.plugin_host.shutdown().await.unwrap();
    server.abort();
    f.close().await;
}

#[tokio::test]
async fn assistant_unknown_qq_delivery_replays_saved_answer_and_pending_web_replays_do_not_regenerate()
 {
    let f = Fixture::new(true).await;
    let (model, server) = fault_model(&f).await;
    let (_, user, route) = fault_accounts(&f).await;
    model.mode(3);
    let pending = onebot_message(&f, &route, QQ, 120, "friend", "lost delivery").await;
    tokio::time::timeout(Duration::from_secs(4), async {
        while model.calls() == 0 {
            model.entered.notified().await;
        }
    })
    .await
    .unwrap();
    drop(pending);
    model.release.notify_one();
    let unknown = settled_turns(&f, &user, 1).await;
    assert_eq!(unknown["turns"][0]["delivery"], "unknown");
    assert_eq!(unknown["turns"][0]["reply"], "recovered answer");
    assert_eq!(unknown["remaining"], 2);
    model.mode(1);
    expect_qq_reply(
        onebot_message(&f, &route, QQ, 121, "friend", "重试").await,
        "recovered answer",
    )
    .await;
    assert_eq!(model.calls(), 1);
    assert_eq!(
        settled_turns(&f, &user, 1).await["turns"][0]["delivery"],
        "sent"
    );
    model.mode(3);
    let body =
        json!({"request_id":uuid::Uuid::new_v4(),"text":"request with lost acknowledgement"});
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, body.clone())
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while model.calls() == 1 {
            model.entered.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.authenticated_json("POST", SEND, &user, body).await.0,
        StatusCode::OK
    );
    assert_eq!(model.calls(), 2);
    assert_eq!(f.authenticated(VIEW, &user).await.1["data"]["remaining"], 1);
    model.release.notify_one();
    assert_eq!(settled_turns(&f, &user, 2).await["remaining"], 1);
    server.abort();
    f.close().await;
}

// 以下测试用例需要精确断言对话模型的调用次数以及额度扣费/退还行为；
// 自动记忆提取会额外调用一次模型，其端到端流程由独立的集成测试覆盖，此处先关闭。
async fn disable_automatic_memory(f: &Fixture) {
    f.state
        .config
        .update_patch(nuono_core::db::RuntimeConfigPatch {
            settings: json!({"ai":{"agent_memory_enabled":false}}),
            secrets: vec![],
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn assistant_clear_drains_an_active_reply_and_old_request_cannot_repopulate_history() {
    let f = Fixture::new(true).await;
    let (model, server) = fault_model(&f).await;
    let (_, user, _) = fault_accounts(&f).await;
    model.mode(3);
    let request =
        json!({"request_id":uuid::Uuid::new_v4(),"text":"a private message being erased"});
    let (status, body) = f
        .authenticated_json("POST", SEND, &user, request.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.calls() == 0 {
            model.entered.notified().await;
        }
    })
    .await
    .unwrap();
    let (_, state) = f.authenticated(VIEW, &user).await;
    let card = state["data"]["active_card"]["id"].as_str().unwrap();
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(5),
        f.authenticated_json(
            "POST",
            "/api/assistant/data/clear-conversation",
            &user,
            json!({"card_id":card}),
        ),
    )
    .await
    .expect("erasure must cancel the active reply before reporting success");
    assert_eq!(status, StatusCode::OK, "{body}");
    // 允许被断开的下游模型调用在会话清空后自行跑完；但其产出绝不能再写回已清空的对话历史。
    model.release.notify_waiters();
    let state = settled_turns(&f, &user, 0).await;
    assert!(state["messages"].as_array().unwrap().is_empty());
    assert_eq!(state["remaining"], 3);
    let (status, body) = f.authenticated_json("POST", SEND, &user, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(model.calls(), 1, "erased request IDs remain deduplicated");
    f.state
        .db
        .reconcile_assistant_turns_before_start()
        .await
        .unwrap();
    let state = settled_turns(&f, &user, 0).await;
    assert!(state["messages"].as_array().unwrap().is_empty());
    server.abort();
    f.state
        .manager
        .event_kernel
        .agent_runtime
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn unified_assistant_account_management_keeps_access_and_persona_scoped() {
    let f = Fixture::new(true).await;
    let staff = f.session("assistant-manager", "owner").await;
    let alice = f.session("assistant-managed-alice", "user").await;
    let bob = f.session("assistant-managed-bob", "user").await;
    let account = f
        .state
        .db
        .find_user_by_username("assistant-managed-alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let other = f
        .state
        .db
        .find_user_by_username("assistant-managed-bob")
        .await
        .unwrap()
        .unwrap()
        .id;
    let path = format!("/api/assistant/grants?account_id={account}");
    let (status, before) = f.authenticated(&path, &staff).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(before["data"]["account_id"], account.to_string());
    assert!(before["data"]["grant"].is_null());
    for token in [&alice, &bob] {
        assert_eq!(f.authenticated(&path, token).await.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(
        f.authenticated("/api/assistant/grants?account_id=0", &staff)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.authenticated("/api/assistant/grants?account_id=2147483647", &staff)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    f.state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "assistant-admin-fixture",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-key"),
            base_url: "http://127.0.0.1:9/v1",
            is_default: true,
        })
        .await
        .unwrap();
    let (status, result) = f
        .authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, result) = f
        .authenticated_json(
            "PUT",
            SETTINGS,
            &staff,
            json!({"enabled":true,"require_grant":true,"daily_limit":30,"raw_retention_days":14}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let until = chrono::Utc::now() + chrono::Duration::days(30);
    let until = chrono::DateTime::from_timestamp(until.timestamp(), 0).unwrap();
    let (status, result) = f
        .authenticated_json(
            "PUT",
            "/api/assistant/grants",
            &staff,
            json!({"account_id":account.to_string(),"expires_at":until.timestamp_millis().to_string(),"daily_limit":42}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (_, saved) = f.authenticated(&path, &staff).await;
    assert_eq!(saved["data"]["grant"]["daily_limit"], 42);
    assert_eq!(
        saved["data"]["grant"]["expires_at"],
        json!(until.timestamp_millis().to_string())
    );
    let (_, other_grant) = f
        .authenticated(&format!("/api/assistant/grants?account_id={other}"), &staff)
        .await;
    assert!(other_grant["data"]["grant"].is_null());
    let (status, card) = f
        .authenticated_json(
            "POST",
            &format!("/api/assistant/cards?account_id={account}"),
            &staff,
            json!({"name":"Assigned companion","system_prompt":"Speak gently"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    let id = card["data"]["id"].as_str().unwrap();
    assert_eq!(
        f.authenticated_json(
            "POST",
            &format!("/api/assistant/cards/{id}/activate?account_id={account}"),
            &staff,
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, view) = f.authenticated(VIEW, &alice).await;
    assert_eq!(view["data"]["available"], true);
    assert_eq!(view["data"]["active_card"]["id"], id);
    assert_eq!(
        f.authenticated(VIEW, &bob).await.1["data"]["available"],
        false
    );
    assert_eq!(
        f.authenticated_json(
            "PUT",
            "/api/assistant/grants",
            &staff,
            json!({"account_id":account})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(f.authenticated(&path, &staff).await.1["data"]["grant"].is_null());
    let (_, revoked) = f.authenticated(VIEW, &alice).await;
    assert_eq!(revoked["data"]["available"], false);
    assert_eq!(
        revoked["data"]["active_card"]["id"], id,
        "revocation preserves the assigned persona"
    );
    f.close().await;
}

#[tokio::test]
async fn user_ai_access_tools_catalog_and_user_group_authorizations_contract() {
    let f = Fixture::new(true).await;
    let staff = f.session("entity-staff", "owner").await;
    let alice = f.session("entity-alice", "user").await;
    let account_id = f
        .state
        .db
        .find_user_by_username("entity-alice")
        .await
        .unwrap()
        .unwrap()
        .id;

    // 1. 已注册工具目录可供管理端读取，普通用户被拒。
    assert_eq!(
        f.authenticated("/api/ai/tools/catalog", &alice).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, catalog) = f.authenticated("/api/ai/tools/catalog", &staff).await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    assert!(catalog["data"].is_array());

    // 2. 开启 AI 总开关与网页对话后，按账号暂停 AI 会同时反映在账号列表、个人对话状态与发消息拦截上。
    f.state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "entity-fixture",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-key"),
            base_url: "http://127.0.0.1:9/v1",
            is_default: true,
        })
        .await
        .unwrap();
    assert_eq!(
        f.authenticated_json("POST", "/api/ai/config", &staff, json!({"enabled": true}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        f.authenticated_json(
            "PUT",
            SETTINGS,
            &staff,
            json!({"enabled": true, "daily_limit": 5})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.authenticated(VIEW, &alice).await.1["data"]["access_state"],
        "ready"
    );
    let (status, paused) = f
        .authenticated_json(
            "PATCH",
            &format!("/api/users/{account_id}/ai-access"),
            &staff,
            json!({"blocked": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["data"]["ai_blocked"], true);

    let (_, users) = f.authenticated("/api/users", &staff).await;
    let alice_row = users["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["username"] == "entity-alice")
        .unwrap();
    assert_eq!(alice_row["ai_blocked"], true);

    let (_, blocked_view) = f.authenticated(VIEW, &alice).await;
    assert_eq!(blocked_view["data"]["available"], false);
    assert_eq!(blocked_view["data"]["access_state"], "blocked");
    let (send_status, _) = f
        .authenticated_json(
            "POST",
            "/api/assistant/messages",
            &alice,
            json!({"request_id": uuid::Uuid::new_v4(), "text": "hello"}),
        )
        .await;
    assert_eq!(send_status, StatusCode::FORBIDDEN);

    assert_eq!(
        f.authenticated_json(
            "PATCH",
            &format!("/api/users/{account_id}/ai-access"),
            &staff,
            json!({"blocked": false}),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        f.authenticated(VIEW, &alice).await.1["data"]["access_state"],
        "ready"
    );

    // 3. 按用户维度列出其名下的群组插件配置授权，支撑用户详情抽屉统一管理。
    let group = f
        .state
        .db
        .ensure_group(
            &nuono_core::db::groups::GroupIdentity::onebot("667788"),
            "测试群",
        )
        .await
        .unwrap();
    assert_eq!(
        f.authenticated_json(
            "POST",
            &format!("/api/groups/{}/authorizations", group.id),
            &staff,
            json!({
                "user_id": account_id.to_string(),
                "source_key": "entity-drawer-grant",
                "plugin_id": null,
                "valid_from_ms": "0",
                "expires_at_ms": null
            })
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, user_grants) = f
        .authenticated(
            &format!("/api/groups/users/{account_id}/authorizations"),
            &staff,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{user_grants}");
    let grants = user_grants["data"].as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["group_id"], group.id.to_string());
    assert_eq!(grants[0]["user_id"], account_id.to_string());

    // 4. 按群组实体暂停与恢复 AI 自动回复：普通用户被拒绝，管理员可切换并在群列表中实时反映。
    assert_eq!(
        f.authenticated_json(
            "PATCH",
            &format!("/api/groups/{}/ai-access", group.id),
            &alice,
            json!({"blocked": true}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.authenticated_json(
            "PATCH",
            "/api/groups/999999/ai-access",
            &staff,
            json!({"blocked": true}),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, paused_group) = f
        .authenticated_json(
            "PATCH",
            &format!("/api/groups/{}/ai-access", group.id),
            &staff,
            json!({"blocked": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{paused_group}");
    assert_eq!(paused_group["data"]["ai_blocked"], true);
    assert!(
        f.state
            .config
            .snapshot()
            .await
            .ai
            .is_group_blocked(group.id)
    );

    let (_, listed_groups) = f.authenticated("/api/groups", &staff).await;
    let target_group_id = group.id.to_string();
    let group_row = listed_groups["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["group_id"] == target_group_id.as_str())
        .unwrap();
    assert_eq!(group_row["ai_blocked"], true);

    let (status, resumed_group) = f
        .authenticated_json(
            "PATCH",
            &format!("/api/groups/{}/ai-access", group.id),
            &staff,
            json!({"blocked": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resumed_group}");
    assert_eq!(resumed_group["data"]["ai_blocked"], false);
    assert!(
        !f.state
            .config
            .snapshot()
            .await
            .ai
            .is_group_blocked(group.id)
    );

    f.close().await;
}
