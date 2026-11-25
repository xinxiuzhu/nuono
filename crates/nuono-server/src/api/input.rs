//! 统一请求参数提取器：当 JSON、路径、查询参数或表单解析失败时，返回与业务错误一致的 JSON 响应结构。
use super::{ApiReply, ApiResponse};
use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request},
    http::{StatusCode, request::Parts},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub struct ApiJson<T>(pub T);
impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiReply<Value>;
    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(raw) = Json::<Box<serde_json::value::RawValue>>::from_request(request, state)
            .await
            .map_err(|error| {
                let status = match error.status() {
                    StatusCode::PAYLOAD_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    _ => StatusCode::BAD_REQUEST,
                };
                ApiReply::error(status, "请求 JSON 格式、大小或字段类型无效")
            })?;
        nuono_domain::scalar::parse_json(raw.get())
            .map_err(|_| ApiResponse::bad_request("JSON 含重复字段、越界整数或非法文本"))?;
        serde_json::from_str(raw.get())
            .map(Self)
            .map_err(|_| ApiResponse::bad_request("请求 JSON 字段类型无效"))
    }
}
pub struct ApiPath<T>(pub T);
impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiReply<Value>;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| ApiResponse::bad_request("请求路径参数无效"))
    }
}
pub struct ApiQuery<T>(pub T);
impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiReply<Value>;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| ApiResponse::bad_request("请求查询参数无效"))
    }
}
/// 提取经过中间件认证的当前调用者身份（供需要区分数据归属的 handler 使用）。
pub struct ApiCaller(pub crate::middleware::CallerIdentity);
impl<S: Send + Sync> FromRequestParts<S> for ApiCaller {
    type Rejection = ApiReply<Value>;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::Extension::<crate::middleware::CallerIdentity>::from_request_parts(parts, state)
            .await
            .map(|axum::Extension(caller)| Self(caller))
            .map_err(|_| ApiResponse::forbidden("缺少调用者身份"))
    }
}
pub struct ApiMultipart(pub Multipart);
impl<S: Send + Sync> FromRequest<S> for ApiMultipart {
    type Rejection = ApiReply<Value>;
    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Multipart::from_request(request, state)
            .await
            .map(Self)
            .map_err(|error| {
                ApiReply::error(error.status(), "请求必须为有效的 multipart/form-data")
            })
    }
}
pub fn multipart_error<T: serde::Serialize>(
    error: axum::extract::multipart::MultipartError,
) -> ApiReply<T> {
    ApiReply::error(error.status(), "上传表单损坏或超过请求大小限制")
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        routing::post,
    };
    use tower::ServiceExt;
    #[derive(serde::Deserialize)]
    struct Payload {
        enabled: bool,
    }
    #[tokio::test]
    async fn malformed_requests_are_json_errors_and_never_reach_the_write_handler() {
        let app = Router::new().route(
            "/{id}",
            post(
                |ApiPath(_): ApiPath<i64>, ApiJson(payload): ApiJson<Payload>| async move {
                    assert!(payload.enabled);
                    panic!("invalid request must not execute the mutation");
                    #[allow(unreachable_code)]
                    ApiResponse::ok(())
                },
            ),
        );
        for (path, body) in [
            ("/1", "{"),
            ("/1", r#"{"enabled":"private-key"}"#),
            ("/invalid", r#"{"enabled":true}"#),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["success"], false);
            assert!(!value.to_string().contains("private-key"));
        }
    }
}
