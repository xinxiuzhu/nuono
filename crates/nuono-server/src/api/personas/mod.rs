//! 角色库、群绑定与受认证保护的角色素材。
mod media;

use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::{
    Extension,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
};
use nuono_app::persona::{PersonaBinding, PersonaInput, PersonaRecord};
use nuono_core::Capability;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::ApiResponse;
use crate::{AppState, middleware::CallerIdentity};

use super::role_media::HttpError;

fn authorize(admin_auth_enabled: bool, caller: Option<&CallerIdentity>) -> Result<(), HttpError> {
    match caller {
        Some(caller) if caller.has(Capability::ManageRuntime) => Ok(()),
        None if !admin_auth_enabled => Ok(()),
        _ => Err(HttpError(StatusCode::FORBIDDEN, "无权管理 AI 角色".into())),
    }
}

async fn authorize_request(
    state: &AppState,
    caller: Option<&CallerIdentity>,
) -> Result<(), HttpError> {
    authorize(
        state.config.snapshot().await.security.admin_auth_enabled,
        caller,
    )
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::personas::PersonaResponse)]
pub(super) struct PersonaResponse {
    id: String,
    name: String,
    description: String,
    system_prompt: String,
    enabled: bool,
    avatar_url: Option<String>,
    portrait_url: Option<String>,
    reference_audio_url: Option<String>,
    reference_text: String,
    language: String,
    tts_provider: Option<String>,
    voice_id: Option<String>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    revision: i64,
    updated_at: String,
}

impl From<PersonaRecord> for PersonaResponse {
    fn from(record: PersonaRecord) -> Self {
        let url = |kind: &str, filename: &Option<String>| {
            filename.as_ref().map(|filename| {
                format!("/api/ai/personas/{}/assets/{kind}?v={filename}", record.id)
            })
        };
        Self {
            avatar_url: url("avatar", &record.avatar_filename),
            portrait_url: url("portrait", &record.portrait_filename),
            reference_audio_url: url("reference_audio", &record.reference_audio_filename),
            id: record.id,
            name: record.name,
            description: record.description,
            system_prompt: record.system_prompt,
            enabled: record.enabled,
            reference_text: record.reference_text,
            language: record.language,
            tts_provider: record.tts_provider,
            voice_id: record.voice_id,
            revision: record.revision,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::personas::PersonaWrite)]
#[serde(deny_unknown_fields)]
struct PersonaWrite {
    name: String,
    #[serde(default)]
    description: String,
    system_prompt: String,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default)]
    reference_text: String,
    #[serde(default = "language_default")]
    language: String,
    #[serde(default)]
    tts_provider: Option<String>,
    #[serde(default)]
    voice_id: Option<String>,
    /// 更新必填；创建时忽略。素材单独提交，不接受客户端文件路径。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    revision: Option<i64>,
}

fn enabled_default() -> bool {
    true
}
fn language_default() -> String {
    "zh".into()
}

impl PersonaWrite {
    fn input(self) -> PersonaInput {
        PersonaInput {
            name: self.name,
            description: self.description,
            system_prompt: self.system_prompt,
            enabled: self.enabled,
            reference_text: self.reference_text,
            language: self.language,
            tts_provider: self.tts_provider,
            voice_id: self.voice_id,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::personas::RevisionRequest)]
#[serde(deny_unknown_fields)]
struct RevisionRequest {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    revision: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[schema(as = api::personas::PersonaBinding)]
#[serde(deny_unknown_fields)]
struct BindingWire {
    platform: String,
    bot_id: String,
    group_id: String,
    #[serde(default)]
    namespace: Option<String>,
    persona_id: String,
}

impl From<PersonaBinding> for BindingWire {
    fn from(binding: PersonaBinding) -> Self {
        Self {
            platform: binding.platform,
            bot_id: binding.bot_id,
            group_id: binding.group_id,
            namespace: binding.namespace,
            persona_id: binding.persona_id,
        }
    }
}

impl From<BindingWire> for PersonaBinding {
    fn from(binding: BindingWire) -> Self {
        Self {
            platform: binding.platform,
            bot_id: binding.bot_id,
            group_id: binding.group_id,
            namespace: binding.namespace,
            persona_id: binding.persona_id,
        }
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_personas, create_persona))
        .routes(routes!(get_persona, update_persona, delete_persona))
        .routes(routes!(list_bindings, assign_persona, remove_binding))
        .merge(media::router())
        .layer(DefaultBodyLimit::max(media::MAX_REQUEST_BYTES))
}

#[utoipa::path(get, path = "/personas", tag = "personas", responses((status = 200, body = ApiResponse<Vec<PersonaResponse>>)))]
async fn list_personas(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
) -> Result<crate::api::ApiReply<Vec<PersonaResponse>>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    Ok(ApiResponse::ok(
        state
            .db
            .list_ai_personas()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(post, path = "/personas", tag = "personas", request_body = PersonaWrite, responses((status = 200, body = ApiResponse<PersonaResponse>)))]
async fn create_persona(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<PersonaWrite>,
) -> Result<crate::api::ApiReply<PersonaResponse>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let id = uuid::Uuid::new_v4().to_string();
    Ok(ApiResponse::ok(
        state.db.create_ai_persona(&id, body.input()).await?.into(),
    ))
}

#[utoipa::path(get, path = "/personas/{id}", tag = "personas", params(("id" = String, Path)), responses((status = 200, body = ApiResponse<PersonaResponse>)))]
async fn get_persona(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
) -> Result<crate::api::ApiReply<PersonaResponse>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    Ok(ApiResponse::ok(load_persona(&state, &id).await?.into()))
}

async fn load_persona(state: &AppState, id: &str) -> Result<PersonaRecord, HttpError> {
    state
        .db
        .get_ai_persona(id)
        .await?
        .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "角色不存在".into()))
}

#[utoipa::path(put, path = "/personas/{id}", tag = "personas", params(("id" = String, Path)), request_body = PersonaWrite, responses((status = 200, body = ApiResponse<PersonaResponse>)))]
async fn update_persona(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Json(body): Json<PersonaWrite>,
) -> Result<crate::api::ApiReply<PersonaResponse>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let revision = body
        .revision
        .ok_or_else(|| HttpError(StatusCode::BAD_REQUEST, "更新角色必须携带 revision".into()))?;
    Ok(ApiResponse::ok(
        state
            .db
            .update_ai_persona(&id, revision, body.input())
            .await?
            .into(),
    ))
}

#[utoipa::path(delete, path = "/personas/{id}", tag = "personas", params(("id" = String, Path)), request_body = RevisionRequest, responses((status = 200, body = ApiResponse<bool>)))]
async fn delete_persona(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Json(body): Json<RevisionRequest>,
) -> Result<crate::api::ApiReply<bool>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    state.db.delete_ai_persona(&id, body.revision).await?;
    // 文件保持不可变，供在途任务中的角色快照继续读取。删除记录不立即回收素材。
    Ok(ApiResponse::ok(true))
}

#[utoipa::path(get, path = "/persona-bindings", tag = "personas", responses((status = 200, body = ApiResponse<Vec<BindingWire>>)))]
async fn list_bindings(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
) -> Result<crate::api::ApiReply<Vec<BindingWire>>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    Ok(ApiResponse::ok(
        state
            .db
            .list_ai_persona_bindings()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(put, path = "/persona-bindings", tag = "personas", request_body = BindingWire, responses((status = 200, body = ApiResponse<bool>)))]
async fn assign_persona(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<BindingWire>,
) -> Result<crate::api::ApiReply<bool>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    state.db.upsert_ai_persona_binding(body.into()).await?;
    Ok(ApiResponse::ok(true))
}

#[utoipa::path(delete, path = "/persona-bindings", tag = "personas", request_body = BindingWire, responses((status = 200, body = ApiResponse<bool>)))]
async fn remove_binding(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<BindingWire>,
) -> Result<crate::api::ApiReply<bool>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let binding: PersonaBinding = body.into();
    state
        .db
        .remove_ai_persona_binding_if(&binding.key(), &binding.persona_id)
        .await?;
    Ok(ApiResponse::ok(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_core::{CallerIdentity, Role};

    #[test]
    fn persona_management_requires_runtime_capability() {
        for role in [Role::Owner, Role::Admin] {
            let caller = CallerIdentity::new(1, "owner", role);
            assert!(authorize(true, Some(&caller)).is_ok());
        }
        let caller = CallerIdentity::new(2, "customer", Role::User);
        assert!(authorize(true, Some(&caller)).is_err());
        assert!(authorize(true, None).is_err());
    }

    #[test]
    fn role_writes_cannot_supply_files_or_bypass_revision() {
        let body = serde_json::json!({"name":"小菲比","system_prompt":"温柔好奇"});
        let parsed: PersonaWrite = serde_json::from_value(body.clone()).unwrap();
        assert!(parsed.revision.is_none());
        assert_eq!(parsed.language, "zh");
        let mut malicious = body;
        malicious["avatar_filename"] = "../../secret".into();
        assert!(serde_json::from_value::<PersonaWrite>(malicious).is_err());
    }
}
