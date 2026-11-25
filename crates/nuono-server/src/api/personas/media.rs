//! 群角色（Persona）的不可变托管素材文件接口（文件落盘后再通过乐观锁 CAS 更新元数据文件名）。
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};
use axum::{
    Extension,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::Response,
};
use nuono_app::persona::{PersonaAssetKind, validate_asset_filename, validate_persona_id};
use serde::Deserialize;
use std::path::PathBuf;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{HttpError, PersonaResponse, RevisionRequest, authorize_request, load_persona};
use crate::{AppState, api::ApiResponse, middleware::CallerIdentity};

pub(super) use crate::api::role_media::MAX_REQUEST_BYTES;
use crate::api::role_media::{
    MAX_AUDIO_BYTES, audio_extension, content_type, normalize_image, read_upload, write_file,
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct AssetQuery {
    v: Option<String>,
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_asset, upload_asset, remove_asset))
}

fn kind(value: &str) -> Result<PersonaAssetKind, HttpError> {
    match value {
        "avatar" => Ok(PersonaAssetKind::Avatar),
        "portrait" => Ok(PersonaAssetKind::Portrait),
        "reference_audio" => Ok(PersonaAssetKind::ReferenceAudio),
        _ => Err(HttpError(
            StatusCode::BAD_REQUEST,
            "未知角色素材类型".into(),
        )),
    }
}

#[utoipa::path(get, operation_id = "get_persona_asset", path = "/personas/{id}/assets/{kind}", tag = "personas", params(("id" = String, Path), ("kind" = String, Path), AssetQuery), responses((status = 200, description = "经认证读取的图片或参考音频", body = Vec<u8>)))]
async fn get_asset(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path((id, slot)): Path<(String, String)>,
    Query(query): Query<AssetQuery>,
) -> Result<Response, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let kind = kind(&slot)?;
    let versioned = query.v.is_some();
    let filename = if let Some(filename) = query.v {
        // 角色解绑并删除其记录后，旧快照仍然可以读取。
        filename
    } else {
        let record = load_persona(&state, &id).await?;
        match kind {
            PersonaAssetKind::Avatar => record.avatar_filename,
            PersonaAssetKind::Portrait => record.portrait_filename,
            PersonaAssetKind::ReferenceAudio => record.reference_audio_filename,
        }
        .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "角色尚未上传此素材".into()))?
    };
    validate_asset_filename(kind, &filename)?;
    let path = directory(&state, &id).await?.join(&filename);
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| HttpError(StatusCode::NOT_FOUND, "角色素材不存在".into()))?;
    if !metadata.is_file() || metadata.len() > MAX_AUDIO_BYTES as u64 {
        return Err(HttpError(StatusCode::NOT_FOUND, "角色素材无效".into()));
    }
    let content_type = content_type(&filename)
        .ok_or_else(|| HttpError(StatusCode::BAD_REQUEST, "素材格式不支持".into()))?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "读取角色素材失败".into()))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(
            header::CACHE_CONTROL,
            if versioned {
                "private, max-age=31536000, immutable"
            } else {
                "private, no-cache"
            },
        )
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(bytes))
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "读取角色素材失败".into()))
}

#[utoipa::path(post, operation_id = "upload_persona_asset", path = "/personas/{id}/assets/{kind}", tag = "personas", params(("id" = String, Path), ("kind" = String, Path)), request_body(content = crate::api::docs::PersonaAssetUpload, content_type = "multipart/form-data"), responses((status = 200, body = ApiResponse<PersonaResponse>)))]
async fn upload_asset(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path((id, slot)): Path<(String, String)>,
    crate::api::input::ApiMultipart(multipart): crate::api::input::ApiMultipart,
) -> Result<crate::api::ApiReply<PersonaResponse>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let kind = kind(&slot)?;
    let uploads = state.asset_uploads.clone();
    let lease = uploads.admit()?;
    let (bytes, mime, supplied_revision) = lease.read(read_upload(multipart, kind)).await??;
    // 文件完整接收后，在独立任务中继续完成图片解码、元数据更新与异常清理，
    // 确保中途客户端断开 HTTP 连接也不会留下半截状态。
    uploads
        .run(lease, move |lease| async move {
            let (bytes, extension) = if kind == PersonaAssetKind::ReferenceAudio {
                let extension = audio_extension(&bytes, &mime)?;
                (bytes, extension)
            } else {
                let bytes = lease
                    .blocking(move || normalize_image(&bytes, &mime, kind))
                    .await??;
                (bytes, "png")
            };
            let _guard = state.identity_assets_lock.lock().await;
            let record = load_persona(&state, &id).await?;
            let revision = supplied_revision;
            if revision != record.revision {
                return Err(HttpError(
                    StatusCode::CONFLICT,
                    "角色已修改，请刷新后再上传".into(),
                ));
            }
            let directory = directory(&state, &id).await?;
            tokio::fs::create_dir_all(&directory).await.map_err(|_| {
                HttpError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "创建角色素材目录失败".into(),
                )
            })?;
            // 创建目录后再次校验路径，防止目录预先被替换为符号链接。
            let directory = directory_checked(&state, &id).await?;
            let asset_id = uuid::Uuid::new_v4();
            let filename = format!("{asset_id}.{extension}");
            let temporary = directory.join(format!("{asset_id}.tmp"));
            let final_path = directory.join(&filename);
            if let Err(error) = write_file(&temporary, &bytes).await {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error);
            }
            if tokio::fs::rename(&temporary, &final_path).await.is_err() {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(HttpError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "保存角色素材失败".into(),
                ));
            }
            match state
                .db
                .set_persona_asset(&id, kind, Some(filename), revision)
                .await
            {
                Ok(record) => Ok(ApiResponse::ok(record.into())),
                Err(error) => {
                    let _ = tokio::fs::remove_file(&final_path).await;
                    Err(error.into())
                }
            }
        })
        .await?
}

#[utoipa::path(delete, operation_id = "remove_persona_asset", path = "/personas/{id}/assets/{kind}", tag = "personas", params(("id" = String, Path), ("kind" = String, Path)), request_body = RevisionRequest, responses((status = 200, body = ApiResponse<PersonaResponse>)))]
async fn remove_asset(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path((id, slot)): Path<(String, String)>,
    Json(body): Json<RevisionRequest>,
) -> Result<crate::api::ApiReply<PersonaResponse>, HttpError> {
    authorize_request(&state, caller.as_ref().map(|caller| &caller.0)).await?;
    let kind = kind(&slot)?;
    let _guard = state.identity_assets_lock.lock().await;
    // 仅清空数据库中的当前素材槽位，磁盘上的不可变历史文件继续保留，供已挂起任务安全读取。
    Ok(ApiResponse::ok(
        state
            .db
            .set_persona_asset(&id, kind, None, body.revision)
            .await?
            .into(),
    ))
}

async fn directory(state: &AppState, id: &str) -> Result<PathBuf, HttpError> {
    validate_persona_id(id)?;
    directory_checked(state, id).await
}

async fn directory_checked(state: &AppState, id: &str) -> Result<PathBuf, HttpError> {
    let root = state.paths.persona_assets();
    let path = root.join(id);
    for component in [&root, &path] {
        match tokio::fs::symlink_metadata(component).await {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(HttpError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "角色素材路径必须是实际目录".into(),
                ));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(HttpError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "检查角色素材目录失败".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(path)
}
