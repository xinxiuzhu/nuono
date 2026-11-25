use super::*;
use axum::body::to_bytes;
use nuono_core::NuonoError;
use serde_json::{Value, json};

#[tokio::test]
async fn service_results_preserve_success_envelopes() {
    for data in [json!([]), json!({"id": 7}), Value::Null] {
        let response = ApiResponse::from_result(Ok(data.clone())).into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"success":true,"data":data,"error":null})
        );
    }
}

#[tokio::test]
async fn service_results_preserve_typed_statuses_and_error_redaction() {
    for error in [
        NuonoError::Unauthorized("scope denied".into()),
        NuonoError::Validation("invalid field".into()),
        NuonoError::NotFound("missing row".into()),
        NuonoError::Conflict("already exists".into()),
        NuonoError::Overloaded("worker queue full".into()),
        NuonoError::Timeout("private upstream URL".into()),
        NuonoError::Http("private upstream credentials".into()),
        NuonoError::DatabaseQuery("SELECT private-secret".into()),
        NuonoError::from(sea_orm::DbErr::Custom("SELECT private-secret".into())),
        NuonoError::from(anyhow::anyhow!("private root cause").context("private context")),
        NuonoError::NotImplemented("unsupported backend".into()),
    ] {
        let expected = ApiResponse::<Value>::from_err(&error).into_response();
        let actual = ApiResponse::<Value>::from_result(Err(error)).into_response();
        assert_eq!(actual.status(), expected.status());
        assert_eq!(actual.headers(), expected.headers());
        let expected = to_bytes(expected.into_body(), 4096).await.unwrap();
        let actual = to_bytes(actual.into_body(), 4096).await.unwrap();
        assert_eq!(actual, expected);
        assert!(!String::from_utf8_lossy(&actual).contains("private"));
    }
}
