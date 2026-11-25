use super::*;

#[tokio::test]
async fn service_controls_require_management_permission_csrf_and_explicit_confirmation() {
    let f = Fixture::new(true).await;
    let owner = f.session("power-owner", "owner").await;
    let admin = f.session("power-admin", "admin").await;
    let user = f.session("power-user", "user").await;
    for action in ["shutdown", "restart"] {
        let path = format!("/api/system/{action}");
        let body = json!({"confirmation":action});
        assert_eq!(
            f.request("POST", &path, body.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.authenticated_json("POST", &path, &user, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let response = f
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header("authorization", format!("Bearer {owner}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        for token in [&owner, &admin] {
            assert_eq!(
                f.authenticated_json("POST", &path, token, json!({"confirmation":"cancel"}))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                f.authenticated(&path, token).await.0,
                StatusCode::METHOD_NOT_ALLOWED
            );
        }
    }
    assert!(f.state.manager.framework_control.last_signal().is_none());
    f.close().await;
}
