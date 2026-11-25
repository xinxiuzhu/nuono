//! 按账号隔离的个人伴侣角色卡接口（支持管理员统一配置与分配）。
mod media;

use axum::{
    extract::{DefaultBodyLimit, FromRequestParts, State},
    http::{header::CACHE_CONTROL, request::Parts},
    response::IntoResponse,
};
use nuono_app::companion::{CompanionCard, CompanionInput, CompanionPreset};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::role_media::HttpError;
use crate::{
    AppState,
    api::{
        ApiReply, ApiResponse,
        input::{ApiCaller, ApiJson, ApiPath, ApiQuery},
    },
};

#[derive(Deserialize)]
struct AccountQuery {
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    account_id: Option<i64>,
}

/// 解析目标所属账号与管理员标识：默认取当前登录用户，仅持有运行管理权限的管理员可指定其他账号。
struct CardOwner {
    account_id: i64,
    is_admin: bool,
}

impl FromRequestParts<AppState> for CardOwner {
    type Rejection = ApiReply<serde_json::Value>;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ApiCaller(caller) = ApiCaller::from_request_parts(parts, state).await?;
        let ApiQuery(query) = ApiQuery::<AccountQuery>::from_request_parts(parts, state).await?;
        let account_id = query.account_id.unwrap_or(caller.user_id);
        if account_id <= 0 {
            return Err(ApiResponse::bad_request("账号 ID 无效"));
        }
        let is_admin = caller.has(nuono_core::auth::Capability::ManageRuntime);
        if account_id != caller.user_id && !is_admin {
            return Err(ApiResponse::forbidden("仅管理员可分配其他账号的角色"));
        }
        Ok(Self {
            account_id,
            is_admin,
        })
    }
}

/// 校验角色卡写权限：复用 [`CardOwner`] 解析结果，避免重复提取认证上下文。
struct CardEditor(i64);

impl FromRequestParts<AppState> for CardEditor {
    type Rejection = ApiReply<serde_json::Value>;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let owner = CardOwner::from_request_parts(parts, state).await?;
        if !owner.is_admin
            && !state
                .config
                .snapshot()
                .await
                .ai
                .assistant
                .allow_user_custom_cards
        {
            return Err(ApiResponse::forbidden("管理员未开启普通用户自定义角色卡"));
        }
        Ok(Self(owner.account_id))
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
struct CardView {
    #[serde(flatten)]
    card: CompanionCard,
    avatar_url: Option<String>,
    portrait_url: Option<String>,
    reference_audio_url: Option<String>,
}

impl From<CompanionCard> for CardView {
    fn from(card: CompanionCard) -> Self {
        let url = |kind: &str, filename: &Option<String>| {
            filename.as_ref().map(|filename| {
                format!(
                    "/api/assistant/cards/{}/assets/{kind}?v={filename}&account_id={}",
                    card.id, card.user_id
                )
            })
        };
        Self {
            avatar_url: url("avatar", &card.avatar_filename),
            portrait_url: url("portrait", &card.portrait_filename),
            reference_audio_url: url("reference_audio", &card.reference_audio_filename),
            card,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
struct CardList {
    active_card_id: String,
    cards: Vec<CardView>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CardUpdate {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    revision: i64,
    card: CompanionInput,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct RevisionRequest {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    revision: i64,
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(get, update))
        .routes(routes!(activate))
        .routes(routes!(presets))
        .routes(routes!(copy_preset))
        .merge(media::router())
        .layer(DefaultBodyLimit::max(super::role_media::MAX_REQUEST_BYTES))
}

#[utoipa::path(get, operation_id = "companion_list", path = "/assistant/cards", tag = "ai", responses((status = 200, body = ApiResponse<CardList>)))]
async fn list(
    State(state): State<AppState>,
    CardOwner { account_id, .. }: CardOwner,
) -> impl IntoResponse {
    let result = async {
        let active = state.db.ensure_default_companion_card(account_id).await?;
        let cards = state.db.list_companion_cards(account_id).await?;
        Ok(CardList {
            active_card_id: active.id,
            cards: cards.into_iter().map(Into::into).collect(),
        })
    }
    .await;
    (
        [(CACHE_CONTROL, "no-store")],
        ApiResponse::from_result(result),
    )
}

#[utoipa::path(post, operation_id = "companion_create", path = "/assistant/cards", tag = "ai", request_body = CompanionInput, responses((status = 200, body = ApiResponse<CardView>)))]
async fn create(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    ApiJson(input): ApiJson<CompanionInput>,
) -> ApiReply<CardView> {
    ApiResponse::from_result(
        state
            .db
            .create_companion_card(account_id, input)
            .await
            .map(Into::into),
    )
}

#[utoipa::path(get, operation_id = "companion_get", path = "/assistant/cards/{id}", tag = "ai", params(("id" = String, Path)), responses((status = 200, body = ApiResponse<CardView>)))]
async fn get(
    State(state): State<AppState>,
    CardOwner { account_id, .. }: CardOwner,
    ApiPath(id): ApiPath<String>,
) -> impl IntoResponse {
    (
        [(CACHE_CONTROL, "no-store")],
        ApiResponse::from_result(
            state
                .db
                .companion_card(account_id, &id)
                .await
                .map(CardView::from),
        ),
    )
}

#[utoipa::path(put, operation_id = "companion_update", path = "/assistant/cards/{id}", tag = "ai", params(("id" = String, Path)), request_body = CardUpdate, responses((status = 200, body = ApiResponse<CardView>)))]
async fn update(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    ApiPath(id): ApiPath<String>,
    ApiJson(input): ApiJson<CardUpdate>,
) -> ApiReply<CardView> {
    ApiResponse::from_result(
        state
            .db
            .update_companion_card(account_id, &id, input.revision, input.card)
            .await
            .map(Into::into),
    )
}

#[utoipa::path(post, operation_id = "companion_activate", path = "/assistant/cards/{id}/activate", tag = "ai", params(("id" = String, Path)), responses((status = 200, body = ApiResponse<CardView>)))]
async fn activate(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    ApiPath(id): ApiPath<String>,
) -> ApiReply<CardView> {
    ApiResponse::from_result(
        state
            .db
            .activate_companion_card(account_id, &id)
            .await
            .map(Into::into),
    )
}

#[utoipa::path(get, operation_id = "companion_presets", path = "/assistant/presets", tag = "ai", responses((status = 200, body = ApiResponse<Vec<CompanionPreset>>)))]
async fn presets(ApiCaller(_caller): ApiCaller) -> ApiReply<Vec<CompanionPreset>> {
    ApiResponse::ok(nuono_app::companion::presets())
}

#[utoipa::path(post, operation_id = "companion_copy_preset", path = "/assistant/presets/{id}/copy", tag = "ai", params(("id" = String, Path)), responses((status = 200, body = ApiResponse<CardView>)))]
async fn copy_preset(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    ApiPath(id): ApiPath<String>,
) -> ApiReply<CardView> {
    let Some(preset) = nuono_app::companion::presets()
        .into_iter()
        .find(|preset| preset.id == id)
    else {
        return ApiResponse::not_found("预设不存在");
    };
    ApiResponse::from_result(
        state
            .db
            .create_companion_card(account_id, preset.card)
            .await
            .map(Into::into),
    )
}
