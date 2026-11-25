use super::*;

#[tokio::test]
async fn private_cards_and_versioned_assets_require_the_account_owner() {
    let fixture = Fixture::new(true).await;
    let alice = fixture.session("card-alice", "user").await;
    let bob = fixture.session("card-bob", "user").await;
    let admin = fixture.session("card-admin", "owner").await;
    let (status, list) = fixture.authenticated("/api/assistant/cards", &alice).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let id = list["data"]["active_card_id"].as_str().unwrap();
    let user_id = list["data"]["cards"][0]["user_id"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let filename = format!("{}.png", uuid::Uuid::new_v4());
    let root = fixture.state.paths.hub().join("companion-assets").join(id);
    tokio::fs::create_dir_all(&root).await.unwrap();
    tokio::fs::write(root.join(&filename), b"private-fixture-image")
        .await
        .unwrap();
    fixture
        .state
        .db
        .set_companion_asset(
            user_id,
            id,
            nuono_app::persona::PersonaAssetKind::Avatar,
            Some(filename.clone()),
            1,
        )
        .await
        .unwrap();
    let asset = format!("/api/assistant/cards/{id}/assets/avatar?v={filename}");
    for token in [&bob, &admin] {
        let (status, _) = fixture
            .authenticated(&format!("/api/assistant/cards/{id}"), token)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = fixture
            .authenticated_json(
                "POST",
                &format!("/api/assistant/cards/{id}/activate"),
                token,
                json!({}),
            )
            .await;
        assert_eq!(
            status,
            if token == &bob {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            }
        );
        let (status, _) = fixture.authenticated(&asset, token).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&asset)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"private-fixture-image"
    );
    fixture
        .state
        .db
        .set_companion_asset(
            user_id,
            id,
            nuono_app::persona::PersonaAssetKind::Avatar,
            None,
            2,
        )
        .await
        .unwrap();
    let (status, _) = fixture.authenticated(&asset, &alice).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a stale URL must not restore removed media"
    );
    fixture.close().await;
}

#[tokio::test]
async fn built_in_presets_create_independent_private_cards_and_edits_use_revision() {
    let fixture = Fixture::new(true).await;
    let token = fixture.session("card-presets", "admin").await;
    let (status, presets) = fixture
        .authenticated("/api/assistant/presets", &token)
        .await;
    assert_eq!(status, StatusCode::OK);
    let preset = presets["data"][0]["id"].as_str().unwrap();
    let path = format!("/api/assistant/presets/{preset}/copy");
    let (_, first) = fixture
        .authenticated_json("POST", &path, &token, json!({}))
        .await;
    let (_, second) = fixture
        .authenticated_json("POST", &path, &token, json!({}))
        .await;
    assert_ne!(first["data"]["id"], second["data"]["id"]);
    let id = first["data"]["id"].as_str().unwrap();
    let input = json!({"revision":1,"card":{"name":"My companion","system_prompt":"Speak gently","user_address":"friend"}});
    let (status, changed) = fixture
        .authenticated_json(
            "PUT",
            &format!("/api/assistant/cards/{id}"),
            &token,
            input.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["data"]["id"], id);
    assert_eq!(changed["data"]["revision"], "2");
    let (status, _) = fixture
        .authenticated_json("PUT", &format!("/api/assistant/cards/{id}"), &token, input)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = fixture
        .authenticated_json(
            "POST",
            "/api/assistant/cards",
            &token,
            json!({"name":"Malicious","user_id":999}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    fixture.close().await;
}

#[tokio::test]
async fn assigned_companion_is_usable_but_customer_cannot_change_it() {
    let f = Fixture::new(true).await;
    let admin = f.session("assign-persona-admin", "owner").await;
    let user = f.session("assign-persona-user", "user").await;
    let account = f
        .state
        .db
        .find_user_by_username("assign-persona-user")
        .await
        .unwrap()
        .unwrap()
        .id;
    let (status, created) = f
        .authenticated_json(
            "POST",
            &format!("/api/assistant/cards?account_id={account}"),
            &admin,
            json!({"name":"管理员分配的角色","system_prompt":"Speak gently"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["data"]["id"].as_str().unwrap();
    let card = format!("/api/assistant/cards/{id}");
    assert_eq!(
        f.authenticated_json(
            "POST",
            &format!("{card}/activate?account_id={account}"),
            &admin,
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, view) = f.authenticated("/api/assistant", &user).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["data"]["active_card"]["id"], id);
    for (method, path) in [
        ("POST", "/api/assistant/cards".to_string()),
        ("PUT", card.clone()),
        ("POST", format!("{card}/activate")),
        (
            "POST",
            "/api/assistant/presets/warm-companion/copy".to_string(),
        ),
        ("POST", format!("{card}/assets/avatar")),
        ("DELETE", format!("{card}/assets/avatar")),
        ("POST", "/api/group-claims".to_string()),
    ] {
        assert_eq!(
            f.authenticated_json(method, &path, &user, json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    let other = f.session("companion-other-customer", "user").await;
    assert_eq!(
        f.authenticated(&format!("{card}?account_id={account}"), &other)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    f.close().await;
}

#[tokio::test]
async fn customer_can_manage_own_cards_when_admin_enables_custom_cards() {
    let f = Fixture::new(true).await;
    let admin = f.session("custom-cards-admin", "owner").await;
    let alice = f.session("custom-cards-alice", "user").await;
    let bob = f.session("custom-cards-bob", "user").await;

    // 开启「允许普通用户自建角色卡」开关（无需先配置模型即可保存该策略开关）。
    let (status, settings) = f
        .authenticated_json(
            "PUT",
            "/api/assistant/settings",
            &admin,
            json!({
                "enabled": false,
                "require_grant": false,
                "allow_user_custom_cards": true,
                "daily_limit": 30,
                "raw_retention_days": 7
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["data"]["allow_user_custom_cards"], true);

    // 普通用户 alice 可以在 /assistant 视图看到开关状态并创建、编辑和切换自己的角色卡。
    let (status, view) = f.authenticated("/api/assistant", &alice).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["data"]["allow_user_custom_cards"], true);

    let (status, created) = f
        .authenticated_json(
            "POST",
            "/api/assistant/cards",
            &alice,
            json!({"name":"私有 RP 角色","system_prompt":"保持角色口吻"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["data"]["id"].as_str().unwrap();

    let (status, activated) = f
        .authenticated_json(
            "POST",
            &format!("/api/assistant/cards/{id}/activate"),
            &alice,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{activated}");

    // 其他用户 bob 仍无法操作 alice 的角色卡。
    let (status, _) = f
        .authenticated_json(
            "POST",
            &format!("/api/assistant/cards/{id}/activate"),
            &bob,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.close().await;
}
