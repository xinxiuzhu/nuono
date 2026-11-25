//! AI 联网搜索与网页抓取工具的管理员配置接口（绝不回显 API 密钥）。
use crate::api::input::ApiJson as Json;
use crate::{
    AppState,
    api::{ApiReply, ApiResponse},
};
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use nuono_app::web_settings::{SearchDepth, WebProvider, WebSettings, WebStrategy};
use nuono_core::{config::web_settings_revision, db::RuntimeConfigPatch};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_web, update_web))
}
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai_web::WebConfigResponse)]
struct WebConfigResponse {
    revision: String,
    search: Value,
    fetch: Value,
    providers: Vec<ProviderStatus>,
}
#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::ai_web::ProviderStatus)]
struct ProviderStatus {
    id: String,
    name: String,
    api_key_configured: bool,
    supports_search: bool,
    supports_fetch: bool,
}
fn response(settings: &WebSettings) -> Result<WebConfigResponse, nuono_core::error::NuonoError> {
    Ok(WebConfigResponse {
        revision: web_settings_revision(settings)?,
        search: serde_json::to_value(&settings.search)?,
        fetch: serde_json::to_value(&settings.fetch)?,
        providers: WebProvider::ALL
            .into_iter()
            .map(|p| ProviderStatus {
                id: p.id().into(),
                name: p.name().into(),
                api_key_configured: settings.credentials.key(p).is_some(),
                supports_search: true,
                supports_fetch: true,
            })
            .collect(),
    })
}
fn no_cache(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
#[utoipa::path(get, path="/web/config", operation_id="ai_web_get_config", tag="ai", responses((status=200, body=ApiResponse<WebConfigResponse>)))]
async fn get_web(State(state): State<AppState>) -> Response {
    let settings = &state.config.snapshot().await.ai.web;
    match response(settings) {
        Ok(value) => no_cache(ApiResponse::ok(value)),
        Err(error) => no_cache(ApiResponse::<Value>::from_err(&error)),
    }
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::ai_web::SearchPatch)]
struct SearchPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<String>)]
    primary: Option<WebProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<String>)]
    strategy: Option<WebStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<Vec<String>>)]
    fallbacks: Option<Vec<WebProvider>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<String>)]
    search_depth: Option<SearchDepth>,
}
#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::ai_web::FetchPatch)]
struct FetchPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<String>)]
    primary: Option<WebProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<String>)]
    strategy: Option<WebStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type=Option<Vec<String>>)]
    fallbacks: Option<Vec<WebProvider>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_content_chars: Option<usize>,
}
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::ai_web::WebConfigPatch)]
struct WebConfigPatch {
    search: Option<SearchPatch>,
    fetch: Option<FetchPatch>,
    /// 未提供的键或空字符串表示保持原密钥不变；显式传 `null` 表示清除对应密钥。
    #[serde(default)]
    #[schema(write_only, value_type=Object)]
    api_keys: BTreeMap<String, Option<String>>,
}
impl WebConfigPatch {
    fn into_patch(self) -> Result<RuntimeConfigPatch, String> {
        let mut settings = serde_json::Map::new();
        if let Some(value) = self.search {
            settings.insert("search".into(), json!(value));
        }
        if let Some(value) = self.fetch {
            settings.insert("fetch".into(), json!(value));
        }
        let mut secrets = vec![];
        for (id, key) in self.api_keys {
            let provider = WebProvider::ALL
                .into_iter()
                .find(|p| p.id() == id)
                .ok_or("不支持的联网服务")?;
            let key = match key {
                Some(key) if key.trim().is_empty() => continue,
                Some(key) if key.len() > 4096 || key.chars().any(char::is_control) => {
                    return Err("API 密钥格式无效".into());
                }
                Some(key) => Some(key.trim().to_owned()),
                None => None,
            };
            secrets.push((provider.secret_key().into(), key));
        }
        Ok(RuntimeConfigPatch {
            settings: Value::Object(settings),
            secrets,
        })
    }
}
fn expected_revision(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::IF_MATCH)?.to_str().ok()?;
    let value = raw.strip_prefix('"')?.strip_suffix('"')?;
    (value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then_some(value)
}
#[utoipa::path(post, path="/web/config", operation_id="ai_web_update_config", tag="ai", request_body=WebConfigPatch, responses((status=200, body=ApiResponse<WebConfigResponse>),(status=409,description="配置或密钥版本冲突"),(status=428,description="需要强 If-Match 版本号")))]
async fn update_web(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WebConfigPatch>,
) -> Response {
    let Some(revision) = expected_revision(&headers) else {
        return no_cache(ApiReply::<Value>::error(
            StatusCode::PRECONDITION_REQUIRED,
            "请先读取联网设置并携带 If-Match 版本号",
        ));
    };
    let patch = match body.into_patch() {
        Ok(patch) => patch,
        Err(error) => return no_cache(ApiReply::<Value>::error(StatusCode::BAD_REQUEST, error)),
    };
    match state
        .config
        .update_web_settings(patch.settings, patch.secrets, revision)
        .await
    {
        Ok(()) => get_web(State(state)).await,
        Err(nuono_core::error::NuonoError::Conflict(error)) => {
            no_cache(ApiReply::<Value>::error(StatusCode::CONFLICT, error))
        }
        Err(error) => no_cache(ApiResponse::<Value>::from_err(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_is_sparse_and_secret_clear_is_distinct_from_blank() {
        let patch: WebConfigPatch = serde_json::from_value(json!({"search":{"max_results":8},"api_keys":{"tavily":"","exa":null,"jina":"new-key"}})).unwrap();
        let patch = patch.into_patch().unwrap();
        assert_eq!(patch.settings, json!({"search":{"max_results":8}}));
        assert_eq!(
            patch.secrets,
            vec![
                ("ai.web.exa_api_key".into(), None),
                ("ai.web.jina_api_key".into(), Some("new-key".into()))
            ]
        );
        assert!(
            serde_json::from_value::<WebConfigPatch>(json!({"search":{"primary":"mcp"}})).is_err()
        );
        assert!(
            serde_json::from_value::<WebConfigPatch>(
                json!({"fetch":{"endpoint":"http://localhost"}})
            )
            .is_err()
        );
    }
    #[test]
    fn snapshot_never_echoes_keys_and_revision_requires_strong_exact_hash() {
        let mut config = WebSettings::default();
        config.credentials.tavily_api_key = Some("secret-fixture-do-not-echo".into());
        let data = serde_json::to_value(response(&config).unwrap()).unwrap();
        assert!(!data.to_string().contains("secret-fixture-do-not-echo"));
        assert_eq!(data["providers"][0]["api_key_configured"], true);
        let hash = "a".repeat(64);
        let mut headers = HeaderMap::new();
        for value in [hash.clone(), format!("W/\"{hash}\""), "*".into()] {
            headers.insert(header::IF_MATCH, HeaderValue::from_str(&value).unwrap());
            assert!(expected_revision(&headers).is_none());
        }
        headers.insert(
            header::IF_MATCH,
            HeaderValue::from_str(&format!("\"{hash}\"")).unwrap(),
        );
        assert_eq!(expected_revision(&headers), Some(hash.as_str()));
    }
}
