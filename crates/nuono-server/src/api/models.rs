//! AI 模型目录及 provider 探测 API。
//!
//! 返回值始终掩码 API key，更新请求中的掩码占位符也不会覆盖已存密钥。远端模型探测
//! 只接受 HTTPS 且拒绝显式的本机/私网 IP；生产环境仍应通过出口代理、DNS 策略和网络
//! ACL 防止域名重绑定或受控域名解析到内部地址。

use super::input::{ApiJson as Json, ApiPath as Path};
use crate::AppState;
use crate::api::{ApiReply, ApiResponse};
use axum::extract::{FromRef, State};
use nuono_app::model_catalog::{AiModelRecord, ModelChange};
use nuono_core::db::{CreateAiModel, Database, UpdateAiModel};
use nuono_core::error::NuonoError;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(Clone)]
struct ModelState {
    db: nuono_core::db::Database,
    config: Arc<nuono_core::config::ConfigManager>,
}
impl FromRef<AppState> for ModelState {
    fn from_ref(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            config: state.config.clone(),
        }
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_models, create_model))
        .routes(routes!(list_provider_health))
        .routes(routes!(get_model, update_model, delete_model))
        .routes(routes!(fetch_provider_models))
        .routes(routes!(set_default_model))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::models::AiModelResponse)]
pub struct AiModelResponse {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub provider_type: String,
    pub model_id: String,
    #[schema(write_only, format = Password)]
    pub api_key: Option<String>,
    pub base_url: String,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

fn mask_api_key(key: &Option<String>) -> String {
    match key {
        // 按字符切片，避免非 ASCII key 在字节边界 panic。
        Some(k) if k.chars().count() > 8 => {
            let head: String = k.chars().take(4).collect();
            let tail: String = k
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("{}****{}", head, tail)
        }
        Some(_) => "****".to_string(),
        None => String::new(),
    }
}

fn model_to_response(m: &AiModelRecord) -> AiModelResponse {
    AiModelResponse {
        id: m.id,
        name: m.name.clone(),
        provider: m.provider.clone(),
        provider_type: m.provider_type.clone(),
        model_id: m.model_id.clone(),
        api_key: Some(mask_api_key(&m.api_key)),
        base_url: m.base_url.clone(),
        is_default: m.is_default,
        enabled: m.enabled,
        created_at: m.created_at.map(nuono_core::clock::format_wire_ts),
        updated_at: m.updated_at.map(nuono_core::clock::format_wire_ts),
    }
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "models_list_models",
    tag = "models",
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<Vec<AiModelResponse>>))
)]
async fn list_models(State(state): State<ModelState>) -> ApiReply<Vec<AiModelResponse>> {
    match state.db.list_ai_models().await {
        Ok(models) => ApiResponse::ok(
            models
                .into_iter()
                .map(|model| model_to_response(&model.into()))
                .collect(),
        ),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get,
    path = "/provider-health",
    operation_id = "models_list_provider_health",
    tag = "models",
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<Vec<super::docs::ProviderHealthSchema>>))
)]
async fn list_provider_health(
    State(state): State<ModelState>,
) -> ApiReply<Vec<nuono_core::audit::ProviderHealthRecord>> {
    match nuono_core::audit::list_provider_health(&state.db, 500).await {
        Ok(records) => ApiResponse::ok(records),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get,
    path = "/{id}",
    operation_id = "models_get_model",
    tag = "models",
    params(("id" = String, Path)),
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<AiModelResponse>))
)]
async fn get_model(
    State(state): State<ModelState>,
    Path(id): Path<i64>,
) -> ApiReply<AiModelResponse> {
    match state.db.get_ai_model(id).await {
        Ok(Some(model)) => ApiResponse::ok(model_to_response(&model.into())),
        Ok(None) => ApiResponse::from_err(&NuonoError::NotFound(format!("模型 #{id}"))),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::models::CreateModelPayload)]
pub struct CreateModelPayload {
    pub name: String,
    pub provider: String,
    #[serde(default = "default_provider_type")]
    pub provider_type: Option<String>,
    pub model_id: String,
    #[schema(write_only, format = Password)]
    pub api_key: Option<String>,
    pub base_url: String,
    pub is_default: Option<bool>,
    pub enabled: Option<bool>,
}

fn default_provider_type() -> Option<String> {
    Some("openai_compatible".to_string())
}

#[utoipa::path(
    post,
    path = "/",
    operation_id = "models_create_model",
    tag = "models",
    request_body = CreateModelPayload,
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<AiModelResponse>))
)]
async fn create_model(
    State(state): State<ModelState>,
    Json(body): Json<CreateModelPayload>,
) -> ApiReply<AiModelResponse> {
    let provider_type = body
        .provider_type
        .unwrap_or_else(|| "openai_compatible".into());
    match state
        .config
        .change_ai_model(
            ModelChange::Create {
                input: CreateAiModel {
                    name: &body.name,
                    provider: &body.provider,
                    provider_type: &provider_type,
                    model_id: &body.model_id,
                    api_key: body.api_key.as_deref(),
                    base_url: &body.base_url,
                    is_default: body.is_default.unwrap_or(false),
                },
                enabled: body.enabled.unwrap_or(true),
            },
            Default::default(),
        )
        .await
    {
        Ok(Some(model)) => ApiResponse::ok(model_to_response(&model)),
        Ok(None) => ApiResponse::from_err(&NuonoError::Internal("创建模型未返回记录".into())),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::models::UpdateModelPayload)]
pub struct UpdateModelPayload {
    pub name: Option<String>,
    pub provider: Option<String>,
    pub provider_type: Option<String>,
    pub model_id: Option<String>,
    #[schema(write_only, format = Password)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub is_default: Option<bool>,
    pub enabled: Option<bool>,
}

#[utoipa::path(
    put,
    path = "/{id}",
    operation_id = "models_update_model",
    tag = "models",
    request_body = UpdateModelPayload,
    params(("id" = String, Path)),
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<AiModelResponse>))
)]
async fn update_model(
    State(state): State<ModelState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateModelPayload>,
) -> ApiReply<AiModelResponse> {
    let api_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty() && !key.contains("****"));
    match state
        .config
        .change_ai_model(
            ModelChange::Update {
                id,
                patch: UpdateAiModel {
                    name: body.name.as_deref(),
                    provider: body.provider.as_deref(),
                    provider_type: body.provider_type.as_deref(),
                    model_id: body.model_id.as_deref(),
                    api_key,
                    base_url: body.base_url.as_deref(),
                    is_default: body.is_default,
                    enabled: body.enabled,
                },
            },
            Default::default(),
        )
        .await
    {
        Ok(Some(model)) => ApiResponse::ok(model_to_response(&model)),
        Ok(None) => ApiResponse::from_err(&NuonoError::Internal("更新模型未返回记录".into())),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    delete,
    path = "/{id}",
    operation_id = "models_delete_model",
    tag = "models",
    params(("id" = String, Path)),
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<super::docs::Empty>))
)]
async fn delete_model(State(state): State<ModelState>, Path(id): Path<i64>) -> ApiReply<()> {
    match state
        .config
        .change_ai_model(ModelChange::Delete { id }, Default::default())
        .await
    {
        Ok(_) => ApiResponse::ok(()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    post,
    path = "/default/{id}",
    operation_id = "models_set_default_model",
    tag = "models",
    params(("id" = String, Path)),
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<super::docs::Empty>))
)]
async fn set_default_model(State(state): State<ModelState>, Path(id): Path<i64>) -> ApiReply<()> {
    match state
        .config
        .change_ai_model(
            ModelChange::Update {
                id,
                patch: UpdateAiModel {
                    is_default: Some(true),
                    ..Default::default()
                },
            },
            Default::default(),
        )
        .await
    {
        Ok(_) => ApiResponse::ok(()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::models::FetchModelsPayload)]
pub struct FetchModelsPayload {
    pub base_url: String,
    #[schema(write_only, format = Password)]
    pub api_key: Option<String>,
    /// 留空 Key 时复用此模型的密钥；只允许发送到它已保存的服务地址。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub model_id: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::models::ProviderModelItem)]
pub struct ProviderModelItem {
    pub id: String,
    pub name: String,
}

/// 对 provider 探测目标做最小 SSRF 防护。
///
/// 检查 URL 语法、HTTPS、userinfo 和字面量 IP；fetch 客户端的每一跳重定向都会
/// 重新经过这里（见 `fetch_provider_models` 的 redirect policy）。域名解析后的地址
/// 不在此处复核（DNS rebinding），所以网络出口策略仍是生产环境的最终边界。
fn validate_provider_base_url(base_url: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(base_url).map_err(|_| "base_url 不是有效 URL".to_string())?;
    if !matches!(url.scheme(), "https") {
        return Err("base_url 必须使用 https".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("base_url 不能包含用户名或密码".to_string());
    }

    let host = url
        .host_str()
        .ok_or_else(|| "base_url 缺少 host".to_string())?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err("base_url 不能指向 localhost".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || matches!(v6.segments()[0] & 0xfe00, 0xfc00)
                    || matches!(v6.segments()[0] & 0xffc0, 0xfe80)
            }
        };
        if blocked {
            return Err("base_url 不能指向内网、本机或保留地址".to_string());
        }
    }

    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[utoipa::path(
    post,
    path = "/fetch",
    operation_id = "models_fetch_provider_models",
    tag = "models",
    request_body = FetchModelsPayload,
    responses((status = 200, description = "成功；失败使用对应的 4xx/5xx 状态码与相同 JSON 信封", body = ApiResponse<Vec<ProviderModelItem>>))
)]
async fn fetch_provider_models(
    State(state): State<ModelState>,
    Json(body): Json<FetchModelsPayload>,
) -> ApiReply<Vec<ProviderModelItem>> {
    let base_url = match validate_provider_base_url(&body.base_url) {
        Ok(url) => url,
        Err(e) => return ApiResponse::bad_request(e),
    };

    let api_key = match resolve_discovery_key(&state.db, &body, &base_url).await {
        Ok(key) => key,
        Err(error) => return ApiResponse::from_err(&error),
    };

    // 重定向逐跳复检：初始 URL 过了校验不代表 302 的目标也安全 —— 公网地址一个
    // 重定向就能把请求（连同 Bearer key）送进 169.254.169.254 或本机管理面。
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("重定向次数过多");
            }
            if let Err(message) = validate_provider_base_url(attempt.url().as_str()) {
                return attempt.error(format!("重定向目标被 SSRF 防护拒绝: {message}"));
            }
            attempt.follow()
        }))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => return ApiResponse::from_err(&NuonoError::Internal(e.to_string())),
    };
    let url = format!("{}/models", base_url);

    match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body_text = match resp.text().await {
                Ok(t) => t,
                Err(e) => return ApiResponse::from_err(&NuonoError::Http(e.to_string())),
            };

            if !status.is_success() {
                return ApiResponse::from_err(&NuonoError::Http(format!(
                    "模型目录上游返回 HTTP {status}"
                )));
            }

            #[derive(Debug, serde::Deserialize)]
            struct ModelsResponse {
                data: Vec<ModelObject>,
            }
            #[derive(Debug, serde::Deserialize)]
            struct ModelObject {
                id: String,
            }

            let models_resp: ModelsResponse = match serde_json::from_str(&body_text) {
                Ok(r) => r,
                Err(_) => match serde_json::from_str::<Vec<ModelObject>>(&body_text) {
                    Ok(arr) => ModelsResponse { data: arr },
                    Err(e) => return ApiResponse::from_err(&NuonoError::Http(e.to_string())),
                },
            };

            let models: Vec<ProviderModelItem> = models_resp
                .data
                .into_iter()
                .map(|m| ProviderModelItem {
                    id: m.id.clone(),
                    name: m.id,
                })
                .collect();

            ApiResponse::ok(models)
        }
        Err(e) => ApiResponse::from_err(&if e.is_timeout() {
            NuonoError::Timeout("模型目录探测".into())
        } else {
            NuonoError::Http(e.to_string())
        }),
    }
}

async fn resolve_discovery_key(
    db: &Database,
    body: &FetchModelsPayload,
    base_url: &str,
) -> Result<String, NuonoError> {
    if let Some(key) = body.api_key.as_deref().map(str::trim)
        && !key.is_empty()
        && !key.contains("****")
    {
        return Ok(key.to_owned());
    }
    let model = match body.model_id {
        Some(id) => Some(
            db.get_ai_model(id)
                .await?
                .ok_or_else(|| NuonoError::NotFound("模型不存在".into()))?,
        ),
        None => db.get_default_ai_model().await?,
    };
    // 复用的密钥只能发给原服务；修改地址后必须显式输入 Key。
    let model = model
        .filter(|model| validate_provider_base_url(&model.base_url).as_deref() == Ok(base_url))
        .ok_or_else(|| {
            NuonoError::Validation("服务地址与已保存的模型不一致，请填写 API Key 后重试".into())
        })?;
    model
        .api_key
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| NuonoError::Validation("需要提供 API Key".into()))
}

#[cfg(test)]
#[path = "../../tests/unit/api/models/tests.rs"]
mod tests;
