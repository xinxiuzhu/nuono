use super::*;

#[tokio::test]
async fn accounts_merge_through_authenticated_http_and_receipts_survive_new_login() {
    let fixture = Fixture::new(true).await;
    let source = fixture.session("http-merge-source", "user").await;
    let target = fixture.session("http-merge-target", "user").await;
    let (status,prepared)=fixture.authenticated_json("POST","/api/account/merges",&target,json!({"source_username":"http-merge-source","source_password":"correct-horse-battery","target_password":"correct-horse-battery"})).await;
    assert_eq!(status, StatusCode::OK, "{prepared}");
    let plan = &prepared["data"];
    let id = plan["id"].as_str().unwrap();
    assert!(
        plan["source_user_id"].is_string()
            && plan["target_user_id"].is_string()
            && plan["revision"].is_string()
    );
    let (status, _) = fixture
        .authenticated(&format!("/api/account/merges/{id}"), &source)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, completed) = fixture
        .authenticated_json(
            "POST",
            &format!("/api/account/merges/{id}/confirm"),
            &target,
            json!({"revision":plan["revision"],"plan_digest":plan["plan_digest"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed["data"]["status"], "completed");
    for token in [&source, &target] {
        assert_eq!(
            fixture
                .authenticated("/api/account/sessions", token)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let retained = fixture
        .state
        .db
        .find_user_by_username("http-merge-target")
        .await
        .unwrap()
        .unwrap();
    let renewed = "http-merge-renewed";
    fixture
        .state
        .db
        .create_session_for_verified_user(
            &retained,
            &format!(
                "auth_session_{}",
                hex::encode(Sha256::digest(renewed.as_bytes()))
            ),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    let (status, history) = fixture.authenticated("/api/account/merges", renewed).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["data"][0]["id"], id);
    assert_eq!(history["data"][0]["status"], "completed");
    assert!(
        fixture
            .state
            .db
            .find_user_by_username("http-merge-source")
            .await
            .unwrap()
            .is_none()
    );
    fixture.close().await;
}

#[tokio::test]
async fn second_session_cannot_confirm_the_first_sessions_account_proofs() {
    let fixture = Fixture::new(true).await;
    fixture.session("http-proof-source", "user").await;
    let target = fixture.session("http-proof-target", "user").await;
    let (_,prepared)=fixture.authenticated_json("POST","/api/account/merges",&target,json!({"source_username":"http-proof-source","source_password":"correct-horse-battery","target_password":"correct-horse-battery"})).await;
    let plan = &prepared["data"];
    let id = plan["id"].as_str().unwrap();
    let user = fixture
        .state
        .db
        .find_user_by_username("http-proof-target")
        .await
        .unwrap()
        .unwrap();
    let second = "http-proof-other-session";
    fixture
        .state
        .db
        .create_session_for_verified_user(
            &user,
            &format!(
                "auth_session_{}",
                hex::encode(Sha256::digest(second.as_bytes()))
            ),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    let (status, result) = fixture
        .authenticated_json(
            "POST",
            &format!("/api/account/merges/{id}/confirm"),
            second,
            json!({"revision":plan["revision"],"plan_digest":plan["plan_digest"]}),
        )
        .await;
    assert!(status.is_client_error(), "{result}");
    assert!(
        fixture
            .state
            .db
            .verify_user_password("http-proof-source", "correct-horse-battery")
            .await
            .unwrap()
            .is_some()
    );
    fixture.close().await;
}
