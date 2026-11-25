//! Dashboard 品牌 Logo 与账号头像的强类型存储和图片 API。
//!
//! 头像是每个账号自己的资产，任何持有 `ManageOwnAccount` 的角色都会走这里，不限管理角色。

use crate::api::input::ApiQuery as Query;
use axum::{
    extract::{DefaultBodyLimit, Multipart, State, multipart::MultipartRejection},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tracing::{error, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiResponse;
use crate::asset_uploads::UploadLease;
use nuono_core::auth::Role;

const BRANDING_CONFIG_KEY: &str = "dashboard_branding";
const PROFILE_CONFIG_KEY: &str = "admin_profile";
const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 5 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 8192;
const MAX_SOURCE_PIXELS: u64 = 25_000_000;
const MAX_LOGO_SIDE: u32 = 1024;
const AVATAR_SIDE: u32 = 512;

#[derive(Debug, Clone, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[schema(as = api::identity_assets::DashboardBrandingConfig)]
#[serde(deny_unknown_fields)]
struct DashboardBrandingConfig {
    #[serde(default)]
    logo_filename: Option<String>,
    #[serde(default)]
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    updated_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, utoipa::ToSchema)]
#[schema(as = api::identity_assets::AdminProfileConfig)]
#[serde(deny_unknown_fields)]
struct AdminProfileConfig {
    #[serde(default)]
    avatar_filename: Option<String>,
    #[serde(default)]
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct AssetRecord {
    filename: Option<String>,
    updated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
enum AssetKind {
    Logo,
    Avatar,
}

/// 一个具体的资产存放位置：种类 + 归属账号。
///
/// Logo 是全站品牌，只有一份；头像属于各个账号，必须按 user_id 分开。
/// 早先两者共用 `admin_profile` 单键，导致所有账号看到同一张头像。
#[derive(Debug, Clone, Copy)]
struct AssetSlot {
    kind: AssetKind,
    /// 头像所属账号；Logo 恒为 None。
    user_id: Option<i64>,
}

impl AssetSlot {
    fn logo() -> Self {
        Self {
            kind: AssetKind::Logo,
            user_id: None,
        }
    }

    fn avatar_of(user_id: i64) -> Self {
        Self {
            kind: AssetKind::Avatar,
            user_id: Some(user_id),
        }
    }

    fn config_key(self) -> String {
        match (self.kind, self.user_id) {
            (AssetKind::Logo, _) => BRANDING_CONFIG_KEY.to_string(),
            // 头像一律按账号分键，不给 owner 留特例：特例意味着「owner 的头像键」和
            // 「某个 user_id 恰好为空的头像键」会撞在一起。代价是升级后旧的
            // admin_profile 记录成为孤儿，owner 需要重新上传一次头像。
            (AssetKind::Avatar, Some(user_id)) => format!("{PROFILE_CONFIG_KEY}:{user_id}"),
            (AssetKind::Avatar, None) => PROFILE_CONFIG_KEY.to_string(),
        }
    }
}

impl AssetKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Logo => "branding",
            Self::Avatar => "profile",
        }
    }

    fn public_path(self) -> &'static str {
        match self {
            Self::Logo => "/api/public/branding/logo",
            Self::Avatar => "/api/account/avatar",
        }
    }

    fn missing_message(self) -> &'static str {
        match self {
            Self::Logo => "未设置自定义 Logo",
            Self::Avatar => "未设置头像",
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::identity_assets::BrandingResponse)]
pub struct BrandingResponse {
    pub logo_url: Option<String>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::identity_assets::ProfileResponse)]
pub struct ProfileResponse {
    pub personal: nuono_app::personal_profile::PersonalProfile,
    pub username: String,
    /// 调用者自己的角色，Dashboard 据此决定导航和页面可见性。
    ///
    /// 序列化仍是 `"owner"`/`"admin"`/`"user"` 三个小写文本，与前端现有判断兼容。
    #[schema(value_type = String)]
    pub role: Role,
    /// 当前绑定邮箱。注册时邮箱必填，管理员直接建号可能没有邮箱。
    pub email: Option<String>,
    /// 邮箱是否已验证。前端据此展示验证状态与「修改邮箱」入口。
    pub email_verified: bool,
    pub avatar_url: Option<String>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub updated_at: Option<i64>,
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::identity_assets::AssetVersionQuery)]
#[into_params(parameter_in = Query)]
struct AssetVersionQuery {
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    v: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredImageFormat {
    Png,
    Jpeg,
    WebP,
}

impl DeclaredImageFormat {
    fn from_mime(content_type: &str) -> Option<Self> {
        match content_type {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::WebP),
            _ => None,
        }
    }

    fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::WebP => ImageFormat::WebP,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        crate::api::ApiReply::<()>::error(self.status, self.message).into_response()
    }
}

impl From<nuono_core::NuonoError> for ApiError {
    fn from(error: nuono_core::NuonoError) -> Self {
        // 复用统一错误分类（保留服务过载/停机等 HTTP 状态码及错误文案映射）。
        let reply = ApiResponse::<()>::from_err(&error);
        Self::new(
            reply.status,
            reply.body.0.error.unwrap_or_else(|| "上传失败".into()),
        )
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_branding))
        .routes(routes!(get_logo))
        .routes(routes!(upload_logo, delete_logo))
        .routes(routes!(get_profile, save_profile))
        .routes(routes!(get_avatar, upload_avatar, delete_avatar))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
}

#[utoipa::path(
    get,
    path = "/public/branding",
    operation_id = "identity_assets_get_branding",
    tag = "identity_assets",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BrandingResponse>))
)]
async fn get_branding(
    State(state): State<AppState>,
) -> Result<crate::api::ApiReply<BrandingResponse>, ApiError> {
    let record = load_asset_record(&state, AssetSlot::logo()).await?;
    Ok(ApiResponse::ok(branding_response(&record)))
}

#[utoipa::path(
    get,
    path = "/public/branding/logo",
    operation_id = "identity_assets_get_logo",
    tag = "identity_assets",
    params(AssetVersionQuery),
    responses((status = 200, description = "图片内容", body = Vec<u8>, content_type = "image/png"))
)]
async fn get_logo(
    State(state): State<AppState>,
    Query(query): Query<AssetVersionQuery>,
) -> Result<Response, ApiError> {
    serve_asset(&state, AssetSlot::logo(), query.v).await
}

#[utoipa::path(
    post,
    path = "/branding/logo",
    operation_id = "identity_assets_upload_logo",
    tag = "identity_assets",
    request_body(content = super::docs::ImageUpload, content_type = "multipart/form-data"),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BrandingResponse>))
)]
async fn upload_logo(
    State(state): State<AppState>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<crate::api::ApiReply<BrandingResponse>, ApiError> {
    let uploads = state.asset_uploads.clone();
    let lease = uploads.admit().map_err(ApiError::from)?;
    let (bytes, declared_format) = read_admitted_upload(&lease, multipart).await?;
    let record = uploads
        .run(lease, move |lease| async move {
            let png =
                process_uploaded_image(&lease, bytes, declared_format, AssetKind::Logo).await?;
            replace_asset(&state, AssetSlot::logo(), &png).await
        })
        .await
        .map_err(ApiError::from)??;
    Ok(ApiResponse::ok(branding_response(&record)))
}

#[utoipa::path(
    delete,
    path = "/branding/logo",
    operation_id = "identity_assets_delete_logo",
    tag = "identity_assets",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<BrandingResponse>))
)]
async fn delete_logo(
    State(state): State<AppState>,
) -> Result<crate::api::ApiReply<BrandingResponse>, ApiError> {
    let record = reset_asset(&state, AssetSlot::logo()).await?;
    Ok(ApiResponse::ok(branding_response(&record)))
}

#[utoipa::path(
    get,
    path = "/account/profile",
    operation_id = "identity_assets_get_profile",
    tag = "identity_assets",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ProfileResponse>))
)]
async fn get_profile(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> Result<crate::api::ApiReply<ProfileResponse>, ApiError> {
    let user = load_caller_user(&state, caller.user_id).await?;
    let record = load_asset_record(&state, AssetSlot::avatar_of(caller.user_id)).await?;
    Ok(ApiResponse::ok(profile_response(
        &user,
        &record,
        state
            .db
            .personal_profile(caller.user_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

#[utoipa::path(
    put, path = "/account/profile", operation_id = "save_personal_profile", tag = "identity_assets",
    request_body = nuono_app::personal_profile::PersonalProfile,
    responses((status = 200, body = ApiResponse<ProfileResponse>))
)]
async fn save_profile(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiJson(body): crate::api::input::ApiJson<
        nuono_app::personal_profile::PersonalProfile,
    >,
) -> Result<crate::api::ApiReply<ProfileResponse>, ApiError> {
    let body = body
        .validated()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let user = load_caller_user(&state, caller.user_id).await?;
    let personal = state
        .db
        .save_personal_profile(caller.user_id, body)
        .await
        .map_err(ApiError::from)?;
    let record = load_asset_record(&state, AssetSlot::avatar_of(caller.user_id)).await?;
    Ok(ApiResponse::ok(profile_response(&user, &record, personal)))
}

#[utoipa::path(
    get,
    path = "/account/avatar",
    operation_id = "identity_assets_get_avatar",
    tag = "identity_assets",
    params(AssetVersionQuery),
    responses((status = 200, description = "图片内容", body = Vec<u8>, content_type = "image/png"))
)]
async fn get_avatar(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    Query(query): Query<AssetVersionQuery>,
) -> Result<Response, ApiError> {
    serve_asset(&state, AssetSlot::avatar_of(caller.user_id), query.v).await
}

#[utoipa::path(
    post,
    path = "/account/avatar",
    operation_id = "identity_assets_upload_avatar",
    tag = "identity_assets",
    request_body(content = super::docs::ImageUpload, content_type = "multipart/form-data"),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ProfileResponse>))
)]
async fn upload_avatar(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<crate::api::ApiReply<ProfileResponse>, ApiError> {
    let uploads = state.asset_uploads.clone();
    let lease = uploads.admit().map_err(ApiError::from)?;
    let (bytes, declared_format) = read_admitted_upload(&lease, multipart).await?;
    let upload_state = state.clone();
    let record = uploads
        .run(lease, move |lease| async move {
            let png =
                process_uploaded_image(&lease, bytes, declared_format, AssetKind::Avatar).await?;
            replace_asset(&upload_state, AssetSlot::avatar_of(caller.user_id), &png).await
        })
        .await
        .map_err(ApiError::from)??;
    let user = load_caller_user(&state, caller.user_id).await?;
    Ok(ApiResponse::ok(profile_response(
        &user,
        &record,
        state
            .db
            .personal_profile(caller.user_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

#[utoipa::path(
    delete,
    path = "/account/avatar",
    operation_id = "identity_assets_delete_avatar",
    tag = "identity_assets",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<ProfileResponse>))
)]
async fn delete_avatar(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> Result<crate::api::ApiReply<ProfileResponse>, ApiError> {
    let record = reset_asset(&state, AssetSlot::avatar_of(caller.user_id)).await?;
    let user = load_caller_user(&state, caller.user_id).await?;
    Ok(ApiResponse::ok(profile_response(
        &user,
        &record,
        state
            .db
            .personal_profile(caller.user_id)
            .await
            .map_err(ApiError::from)?,
    )))
}

fn branding_response(record: &AssetRecord) -> BrandingResponse {
    BrandingResponse {
        logo_url: versioned_url(AssetKind::Logo, record),
        updated_at: record.updated_at,
    }
}

fn profile_response(
    user: &nuono_core::entity::users::Model,
    record: &AssetRecord,
    personal: nuono_app::personal_profile::PersonalProfile,
) -> ProfileResponse {
    ProfileResponse {
        personal,
        username: user.username.clone(),
        role: Role::from_db_value(&user.role),
        email: user.email.clone(),
        email_verified: user.email_verified,
        avatar_url: versioned_url(AssetKind::Avatar, record),
        updated_at: record.updated_at,
    }
}

/// 按调用者 id 回查账号，用于补齐 profile 里的邮箱与验证状态。
///
/// 中间件已经校验过会话对应的账号存在且 active，这里查不到只可能是并发下的极短窗口，
/// 直接按内部错误处理，不吞成「未登录」。
async fn load_caller_user(
    state: &AppState,
    user_id: i64,
) -> Result<nuono_core::entity::users::Model, ApiError> {
    state
        .db
        .find_user_by_id(user_id)
        .await
        .map_err(|e| {
            error!(error = %e, user_id, "加载调用者账号失败");
            ApiError::internal("加载账号资料失败，请稍后再试")
        })?
        .ok_or_else(|| ApiError::internal("账号不存在，请重新登录"))
}

fn versioned_url(kind: AssetKind, record: &AssetRecord) -> Option<String> {
    record.filename.as_ref().and_then(|_| {
        record
            .updated_at
            .map(|updated_at| format!("{}?v={updated_at}", kind.public_path()))
    })
}

async fn load_asset_record(state: &AppState, slot: AssetSlot) -> Result<AssetRecord, ApiError> {
    let config_key = slot.config_key();
    let value = state
        .db
        .get_global_config(&config_key)
        .await
        .map_err(|error| {
            error!(%error, %config_key, "读取身份资产配置失败");
            ApiError::internal("读取身份资产配置失败")
        })?;

    let record = match (slot.kind, value) {
        (AssetKind::Logo, Some(value)) => {
            let config: DashboardBrandingConfig =
                serde_json::from_value(value).map_err(|error| {
                    error!(%error, "dashboard_branding 配置格式无效");
                    ApiError::internal("品牌配置格式无效")
                })?;
            AssetRecord {
                filename: config.logo_filename,
                updated_at: config.updated_at,
            }
        }
        (AssetKind::Avatar, Some(value)) => {
            let config: AdminProfileConfig = serde_json::from_value(value).map_err(|error| {
                // 配置键沿用 `admin_profile`：改名要迁移数据，否则已有头像会失联。
                error!(%error, "账号资料配置记录格式无效");
                ApiError::internal("账号资料配置格式无效")
            })?;
            AssetRecord {
                filename: config.avatar_filename,
                updated_at: config.updated_at,
            }
        }
        (_, None) => AssetRecord {
            filename: None,
            updated_at: None,
        },
    };

    validate_asset_record(&record)?;
    Ok(record)
}

fn validate_asset_record(record: &AssetRecord) -> Result<(), ApiError> {
    if let Some(filename) = record.filename.as_deref()
        && (record.updated_at.is_none() || !is_valid_asset_filename(filename))
    {
        return Err(ApiError::internal("身份资产配置格式无效"));
    }
    Ok(())
}

fn is_valid_asset_filename(filename: &str) -> bool {
    if filename.contains('/') || filename.contains('\\') {
        return false;
    }
    let Some(stem) = filename.strip_suffix(".png") else {
        return false;
    };
    Uuid::parse_str(stem)
        .map(|uuid| uuid.to_string() == stem)
        .unwrap_or(false)
}

async fn save_asset_record(
    state: &AppState,
    slot: AssetSlot,
    record: &AssetRecord,
) -> Result<(), ApiError> {
    let value = match slot.kind {
        AssetKind::Logo => serde_json::to_value(DashboardBrandingConfig {
            logo_filename: record.filename.clone(),
            updated_at: record.updated_at,
        }),
        AssetKind::Avatar => serde_json::to_value(AdminProfileConfig {
            avatar_filename: record.filename.clone(),
            updated_at: record.updated_at,
        }),
    }
    .map_err(|error| {
        error!(%error, "序列化身份资产配置失败");
        ApiError::internal("保存身份资产配置失败")
    })?;

    let config_key = slot.config_key();
    let result = if let Some(user_id) = slot.user_id {
        state.db.set_account_avatar(user_id, value).await
    } else {
        state.db.set_global_config(&config_key, value).await
    };
    result.map_err(|error| {
        error!(%error,%config_key,"保存身份资产配置失败");
        ApiError::internal("保存身份资产配置失败")
    })
}

fn extract_multipart(
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Multipart, ApiError> {
    multipart.map_err(|error| ApiError::new(error.status(), "请求必须为 multipart/form-data"))
}

async fn read_admitted_upload(
    lease: &UploadLease,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(Vec<u8>, DeclaredImageFormat), ApiError> {
    lease
        .read(read_upload(extract_multipart(multipart)?))
        .await
        .map_err(ApiError::from)?
}

async fn read_upload(mut multipart: Multipart) -> Result<(Vec<u8>, DeclaredImageFormat), ApiError> {
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        ApiError::new(error.status(), "读取 multipart 请求失败或超过请求大小限制")
    })? {
        if field.name() != Some("file") {
            continue;
        }

        let content_type = field.content_type().unwrap_or_default();
        let declared_format = DeclaredImageFormat::from_mime(content_type).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "仅支持 PNG、JPEG 或 WebP 图片",
            )
        })?;
        let bytes = field
            .bytes()
            .await
            .map_err(|error| ApiError::new(error.status(), "读取上传文件失败或超过请求大小限制"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "图片文件不能超过 5 MiB",
            ));
        }
        return Ok((bytes.to_vec(), declared_format));
    }

    Err(ApiError::bad_request("multipart 请求缺少 file 字段"))
}

async fn process_uploaded_image(
    lease: &UploadLease,
    bytes: Vec<u8>,
    declared_format: DeclaredImageFormat,
    kind: AssetKind,
) -> Result<Vec<u8>, ApiError> {
    lease
        .blocking(move || process_image(&bytes, declared_format, kind))
        .await
        .map_err(ApiError::from)?
}

fn process_image(
    bytes: &[u8],
    declared_format: DeclaredImageFormat,
    kind: AssetKind,
) -> Result<Vec<u8>, ApiError> {
    let actual_format = image::guess_format(bytes)
        .map_err(|_| ApiError::bad_request("无法识别或图片数据已损坏"))?;
    if actual_format != declared_format.image_format() {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "图片实际格式与声明的 Content-Type 不一致",
        ));
    }

    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), actual_format)
        .into_dimensions()
        .map_err(|_| ApiError::bad_request("无法读取图片尺寸或图片数据已损坏"))?;
    validate_source_dimensions(width, height)?;

    let image = image::load_from_memory_with_format(bytes, actual_format)
        .map_err(|_| ApiError::bad_request("图片解码失败或图片数据已损坏"))?;
    let normalized = match kind {
        AssetKind::Logo => resize_logo(image),
        AssetKind::Avatar => crop_and_resize_avatar(image),
    };

    let mut output = Vec::new();
    normalized
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .map_err(|error| {
            error!(%error, "PNG 重编码失败");
            ApiError::internal("图片重编码失败")
        })?;
    Ok(output)
}

fn validate_source_dimensions(width: u32, height: u32) -> Result<(), ApiError> {
    if width == 0 || height == 0 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "图片宽高必须大于 0",
        ));
    }
    if width > MAX_SOURCE_SIDE || height > MAX_SOURCE_SIDE {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "图片任一边不能超过 8192 像素",
        ));
    }
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "图片总像素不能超过 2500 万",
        ));
    }
    Ok(())
}

fn resize_logo(image: DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width <= MAX_LOGO_SIDE && height <= MAX_LOGO_SIDE {
        return image;
    }

    let (target_width, target_height) = if width >= height {
        (
            MAX_LOGO_SIDE,
            ((u64::from(height) * u64::from(MAX_LOGO_SIDE)) / u64::from(width)).max(1) as u32,
        )
    } else {
        (
            ((u64::from(width) * u64::from(MAX_LOGO_SIDE)) / u64::from(height)).max(1) as u32,
            MAX_LOGO_SIDE,
        )
    };
    image.resize_exact(target_width, target_height, FilterType::Lanczos3)
}

fn crop_and_resize_avatar(image: DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    let side = width.min(height);
    let x = (width - side) / 2;
    let y = (height - side) / 2;
    image
        .crop_imm(x, y, side, side)
        .resize_exact(AVATAR_SIDE, AVATAR_SIDE, FilterType::Lanczos3)
}

async fn replace_asset(
    state: &AppState,
    slot: AssetSlot,
    png: &[u8],
) -> Result<AssetRecord, ApiError> {
    let _guard = state.identity_assets_lock.lock().await;
    let old_record = load_asset_record(state, slot).await?;
    let asset_dir = asset_directory(state, slot.kind);
    let temp_dir = state.paths.identity_assets().join(".tmp");
    tokio::fs::create_dir_all(&asset_dir)
        .await
        .map_err(|error| {
            error!(%error, directory = %asset_dir.display(), "创建身份资产目录失败");
            ApiError::internal("创建身份资产目录失败")
        })?;
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| {
            error!(%error, directory = %temp_dir.display(), "创建身份资产临时目录失败");
            ApiError::internal("创建身份资产目录失败")
        })?;

    let id = Uuid::new_v4();
    let filename = format!("{id}.png");
    let temp_path = temp_dir.join(format!("{id}.tmp"));
    let final_path = asset_dir.join(&filename);

    write_temp_file(&temp_path, png).await?;
    if let Err(error) = tokio::fs::rename(&temp_path, &final_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        error!(%error, "提交身份资产临时文件失败");
        return Err(ApiError::internal("保存身份资产文件失败"));
    }

    let new_record = AssetRecord {
        filename: Some(filename),
        updated_at: Some(next_updated_at(old_record.updated_at)),
    };
    if let Err(error) = save_asset_record(state, slot, &new_record).await {
        if let Err(rollback_error) = tokio::fs::remove_file(&final_path).await {
            warn!(%rollback_error, path = %final_path.display(), "回滚身份资产文件失败");
        }
        return Err(error);
    }

    remove_old_asset(state, slot.kind, old_record.filename.as_deref()).await;
    Ok(new_record)
}

async fn reset_asset(state: &AppState, slot: AssetSlot) -> Result<AssetRecord, ApiError> {
    let _guard = state.identity_assets_lock.lock().await;
    let old_record = load_asset_record(state, slot).await?;
    let new_record = AssetRecord {
        filename: None,
        updated_at: Some(next_updated_at(old_record.updated_at)),
    };
    save_asset_record(state, slot, &new_record).await?;
    remove_old_asset(state, slot.kind, old_record.filename.as_deref()).await;
    Ok(new_record)
}

fn next_updated_at(previous: Option<i64>) -> i64 {
    let now = chrono::Utc::now().timestamp_millis();
    previous.map_or(now, |previous| now.max(previous.saturating_add(1)))
}

async fn write_temp_file(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|error| {
            error!(%error, path = %path.display(), "创建身份资产临时文件失败");
            ApiError::internal("保存身份资产文件失败")
        })?;
    if let Err(error) = file.write_all(bytes).await {
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
        error!(%error, path = %path.display(), "写入身份资产临时文件失败");
        return Err(ApiError::internal("保存身份资产文件失败"));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
        error!(%error, path = %path.display(), "同步身份资产临时文件失败");
        return Err(ApiError::internal("保存身份资产文件失败"));
    }
    Ok(())
}

async fn remove_old_asset(state: &AppState, kind: AssetKind, filename: Option<&str>) {
    let Some(filename) = filename else {
        return;
    };
    let path = asset_directory(state, kind).join(filename);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(%error, path = %path.display(), "删除旧身份资产文件失败"),
    }
}

fn asset_directory(state: &AppState, kind: AssetKind) -> PathBuf {
    state.paths.identity_assets().join(kind.directory())
}

async fn serve_asset(
    state: &AppState,
    slot: AssetSlot,
    requested_version: Option<i64>,
) -> Result<Response, ApiError> {
    let _guard = state.identity_assets_lock.lock().await;
    let record = load_asset_record(state, slot).await?;
    let filename = record
        .filename
        .as_deref()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, slot.kind.missing_message()))?;
    let path = asset_directory(state, slot.kind).join(filename);
    let png = tokio::fs::read(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            error!(path = %path.display(), "身份资产配置指向的文件不存在");
            ApiError::internal("身份资产文件不可用")
        } else {
            error!(%error, path = %path.display(), "读取身份资产文件失败");
            ApiError::internal("读取身份资产文件失败")
        }
    })?;

    let cache_control = if requested_version.is_some() && requested_version == record.updated_at {
        match slot.kind {
            AssetKind::Logo => "public, max-age=31536000, immutable",
            AssetKind::Avatar => "private, max-age=31536000, immutable",
        }
    } else {
        "no-cache, max-age=0, must-revalidate"
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    Ok((headers, png).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn encode(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    #[test]
    fn rejects_bad_data_and_mime_spoofing() {
        let bad =
            process_image(b"not an image", DeclaredImageFormat::Png, AssetKind::Logo).unwrap_err();
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);

        let jpeg = encode(DynamicImage::new_rgb8(8, 8), ImageFormat::Jpeg);
        let spoofed = process_image(&jpeg, DeclaredImageFormat::Png, AssetKind::Logo).unwrap_err();
        assert_eq!(spoofed.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn validates_source_dimension_limits() {
        assert!(validate_source_dimensions(0, 10).is_err());
        assert!(validate_source_dimensions(10, 0).is_err());
        assert!(validate_source_dimensions(8193, 1).is_err());
        assert!(validate_source_dimensions(5001, 5000).is_err());
        assert!(validate_source_dimensions(5000, 5000).is_ok());
    }

    #[test]
    fn logo_resize_preserves_aspect_ratio_and_alpha() {
        let input =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(2048, 1024, Rgba([10, 20, 30, 77])));
        let bytes = encode(input, ImageFormat::Png);
        let output = process_image(&bytes, DeclaredImageFormat::Png, AssetKind::Logo).unwrap();
        let decoded = image::load_from_memory_with_format(&output, ImageFormat::Png).unwrap();

        assert_eq!(decoded.dimensions(), (1024, 512));
        assert_eq!(decoded.to_rgba8().get_pixel(512, 256).0[3], 77);
    }

    #[test]
    fn avatar_is_center_cropped_and_resized_to_512_square() {
        let mut input = RgbaImage::from_pixel(6, 2, Rgba([255, 0, 0, 255]));
        for y in 0..2 {
            for x in 2..4 {
                input.put_pixel(x, y, Rgba([0, 255, 0, 255]));
            }
            for x in 4..6 {
                input.put_pixel(x, y, Rgba([0, 0, 255, 255]));
            }
        }
        let bytes = encode(DynamicImage::ImageRgba8(input), ImageFormat::Png);
        let output = process_image(&bytes, DeclaredImageFormat::Png, AssetKind::Avatar).unwrap();
        let decoded = image::load_from_memory_with_format(&output, ImageFormat::Png)
            .unwrap()
            .to_rgba8();

        assert_eq!(decoded.dimensions(), (512, 512));
        assert_eq!(decoded.get_pixel(256, 256).0, [0, 255, 0, 255]);
    }

    #[test]
    fn only_server_generated_png_filenames_are_valid() {
        let valid = format!("{}.png", Uuid::new_v4());
        assert!(is_valid_asset_filename(&valid));
        assert!(!is_valid_asset_filename("../avatar.png"));
        assert!(!is_valid_asset_filename("avatar.png"));
        assert!(!is_valid_asset_filename(
            "550e8400-e29b-41d4-a716-446655440000.jpg"
        ));
    }
}
