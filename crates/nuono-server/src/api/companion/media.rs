//! 伴侣角色卡的不可变托管素材文件接口（文件落盘后再通过乐观锁 CAS 更新元数据文件名）。
use crate::api::input::{ApiJson as Json, ApiPath as Path, ApiQuery as Query};
use axum::{
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::Response,
};
use nuono_app::persona::{PersonaAssetKind, validate_asset_filename, validate_persona_id};
use serde::Deserialize;
use std::path::PathBuf;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{CardEditor, CardOwner, CardView, HttpError, RevisionRequest};
use crate::{AppState, api::ApiResponse};

const MAX_CARD_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CARD_ASSET_FILES: usize = 64;

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

#[utoipa::path(get, operation_id = "get_companion_card_asset", path = "/assistant/cards/{id}/assets/{kind}", tag = "ai", params(("id" = String, Path), ("kind" = String, Path), AssetQuery), responses((status = 200, description = "经认证读取的图片或参考音频", body = Vec<u8>)))]
async fn get_asset(
    State(state): State<AppState>,
    CardOwner { account_id, .. }: CardOwner,
    Path((id, slot)): Path<(String, String)>,
    Query(query): Query<AssetQuery>,
) -> Result<Response, HttpError> {
    let owned = state.db.companion_card(account_id, &id).await?;
    let kind = kind(&slot)?;
    let filename = match kind {
        PersonaAssetKind::Avatar => owned.avatar_filename,
        PersonaAssetKind::Portrait => owned.portrait_filename,
        PersonaAssetKind::ReferenceAudio => owned.reference_audio_filename,
    }
    .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "角色卡尚未上传此素材".into()))?;
    if query.v.as_ref().is_some_and(|version| version != &filename) {
        return Err(HttpError(
            StatusCode::NOT_FOUND,
            "角色卡素材版本已变更".into(),
        ));
    }
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
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(bytes))
        .map_err(|_| HttpError(StatusCode::INTERNAL_SERVER_ERROR, "读取角色素材失败".into()))
}

#[utoipa::path(post, operation_id = "upload_companion_card_asset", path = "/assistant/cards/{id}/assets/{kind}", tag = "ai", params(("id" = String, Path), ("kind" = String, Path)), request_body(content = crate::api::docs::PersonaAssetUpload, content_type = "multipart/form-data"), responses((status = 200, body = ApiResponse<CardView>)))]
async fn upload_asset(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    Path((id, slot)): Path<(String, String)>,
    crate::api::input::ApiMultipart(multipart): crate::api::input::ApiMultipart,
) -> Result<crate::api::ApiReply<CardView>, HttpError> {
    state.db.companion_card(account_id, &id).await?;
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
            let record = state.db.companion_card(account_id, &id).await?;
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
            // 上传互斥锁覆盖配额检查、文件落盘与数据库元数据 CAS 更新；
            // 历史素材文件同样计入配额，超限时在创建临时文件前直接拒绝。
            ensure_asset_budget(&directory, bytes.len() as u64).await?;
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
                .set_companion_asset(account_id, &id, kind, Some(filename), revision)
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

#[utoipa::path(delete, operation_id = "remove_companion_card_asset", path = "/assistant/cards/{id}/assets/{kind}", tag = "ai", params(("id" = String, Path), ("kind" = String, Path)), request_body = RevisionRequest, responses((status = 200, body = ApiResponse<CardView>)))]
async fn remove_asset(
    State(state): State<AppState>,
    CardEditor(account_id): CardEditor,
    Path((id, slot)): Path<(String, String)>,
    Json(body): Json<RevisionRequest>,
) -> Result<crate::api::ApiReply<CardView>, HttpError> {
    state.db.companion_card(account_id, &id).await?;
    let kind = kind(&slot)?;
    let _guard = state.identity_assets_lock.lock().await;
    // 仅清空数据库中的当前素材槽位，磁盘上的不可变历史文件继续保留，供已挂起任务安全读取。
    Ok(ApiResponse::ok(
        state
            .db
            .set_companion_asset(account_id, &id, kind, None, body.revision)
            .await?
            .into(),
    ))
}

async fn directory(state: &AppState, id: &str) -> Result<PathBuf, HttpError> {
    validate_persona_id(id)?;
    directory_checked(state, id).await
}

async fn directory_checked(state: &AppState, id: &str) -> Result<PathBuf, HttpError> {
    let root = state.paths.hub().join("companion-assets");
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

fn budget_exceeded() -> HttpError {
    HttpError(
        StatusCode::PAYLOAD_TOO_LARGE,
        "当前角色卡的素材历史已达上限（128 MiB 或 64 个文件），请联系管理员清理旧素材后再上传。"
            .into(),
    )
}

async fn ensure_asset_budget(directory: &std::path::Path, incoming: u64) -> Result<(), HttpError> {
    if incoming > MAX_CARD_ASSET_BYTES {
        return Err(budget_exceeded());
    }
    let mut entries = tokio::fs::read_dir(directory).await.map_err(|_| {
        HttpError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "读取角色素材预算失败".into(),
        )
    })?;
    let mut bytes = incoming;
    let mut files = 0usize;
    while let Some(entry) = entries.next_entry().await.map_err(|_| {
        HttpError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "读取角色素材预算失败".into(),
        )
    })? {
        let metadata = tokio::fs::symlink_metadata(entry.path())
            .await
            .map_err(|_| {
                HttpError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "检查角色素材文件失败".into(),
                )
            })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(HttpError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "角色素材目录只能包含普通文件".into(),
            ));
        }
        files += 1;
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(budget_exceeded)?;
        if files >= MAX_CARD_ASSET_FILES || bytes > MAX_CARD_ASSET_BYTES {
            return Err(budget_exceeded());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/api/companion/media.rs"]
mod tests;
