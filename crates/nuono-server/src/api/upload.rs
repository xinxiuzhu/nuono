//! 管理端壁纸上传接口。
//!
//! 路由依赖外层管理认证，只按 multipart 声明的 MIME 类型和大小筛选后写入静态目录，
//! 不进行图片解码或恶意内容扫描。因此部署时不得绕过认证公开此接口，并应让静态文件
//! 服务发送固定安全响应头；需要接收不可信上传时应在独立服务中做内容校验与重编码。

use crate::api::input::ApiPath as AxumPath;
use axum::extract::State;
use std::path::Path;
use tokio::fs;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiResponse;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(upload_wallpaper))
        .routes(routes!(list_wallpapers))
        .routes(routes!(delete_wallpaper))
        .layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024)) // 20MB
}

/// 认证页壁纸的公开查询路由。
///
/// 与 [`router`] 分开挂载，因为这条路由必须在未认证时可访问，而同文件其余路由不行。
/// 静态目录 `/wallpapers/*` 本身已经是公开的，所以这里返回其中一个文件名不扩大暴露面；
/// 真正需要认证保护的是「有哪些壁纸」和「能否改动壁纸」。
pub fn public_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(auth_wallpaper))
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[schema(as = api::upload::AuthWallpaper)]
pub struct AuthWallpaper {
    /// 最新上传的壁纸 URL。实例没有上传壁纸时为空字符串。
    pub url: String,
    /// 实例是否有已上传的壁纸。
    pub custom: bool,
}

/// GET /api/public/auth-wallpaper
///
/// 返回最新上传的壁纸，供认证页选择背景。
#[utoipa::path(
    get,
    path = "/public/auth-wallpaper",
    operation_id = "upload_auth_wallpaper",
    tag = "upload",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<AuthWallpaper>))
)]
async fn auth_wallpaper(State(state): State<AppState>) -> crate::api::ApiReply<AuthWallpaper> {
    let wallpaper = newest_wallpaper(&state.paths.wallpapers()).await;
    ApiResponse::ok(AuthWallpaper {
        custom: wallpaper.is_some(),
        url: wallpaper.unwrap_or_default(),
    })
}

/// 按修改时间取最新的一张壁纸。返回 `None` 表示还没有上传过自定义壁纸。
async fn newest_wallpaper(dir: &Path) -> Option<String> {
    let mut entries = fs::read_dir(dir).await.ok()?;
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_image_name(&name) {
            continue;
        }
        let modified = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, name));
        }
    }
    newest.map(|(_, name)| format!("/wallpapers/{name}"))
}

fn is_image_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// 壁纸扩展名映射：只接受能按扩展名静态服务为 image/* 的显式类型，未知类型拒绝。
///
/// 不做 `_ => jpg` 兜底——那会让任意 `image/*` 字节（如 SVG）落进公开静态目录，
/// 仅靠 Content-Type 正确性兜底；显式拒绝让「wallpapers 目录里只会是图片」成为
/// 硬保证而不是巧合。
fn wallpaper_extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// 列出已上传的壁纸（同源静态 URL）。
#[utoipa::path(
    get,
    path = "/upload/wallpapers",
    operation_id = "upload_list_wallpapers",
    tag = "upload",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<String>>))
)]
async fn list_wallpapers(State(state): State<AppState>) -> crate::api::ApiReply<Vec<String>> {
    let dir = state.paths.wallpapers();
    let mut urls = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_image_name(&name) {
                urls.push(format!("/wallpapers/{name}"));
            }
        }
    }
    urls.sort();
    ApiResponse::ok(urls)
}

/// 删除一张已上传的壁纸（仅接受纯文件名，防目录穿越）。
#[utoipa::path(
    delete,
    path = "/upload/wallpapers/{name}",
    operation_id = "upload_delete_wallpaper",
    tag = "upload",
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<bool>))
)]
async fn delete_wallpaper(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> crate::api::ApiReply<bool> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return ApiResponse::bad_request("非法文件名");
    }
    let path = state.paths.wallpapers().join(&name);
    match fs::remove_file(&path).await {
        Ok(_) => ApiResponse::ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ApiResponse::not_found("壁纸不存在"),
        Err(e) => ApiResponse::internal(e),
    }
}

/// 保存单个名为 `file` 的 multipart 字段，并返回同源静态 URL。
///
/// 20 MiB 是路由级请求体上限，10 MiB 是读取后文件上限；两层限制用途不同。
#[utoipa::path(
    post,
    path = "/upload/wallpaper",
    operation_id = "upload_upload_wallpaper",
    tag = "upload",
    request_body(content = super::docs::ImageUpload, content_type = "multipart/form-data"),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn upload_wallpaper(
    State(state): State<AppState>,
    crate::api::input::ApiMultipart(mut multipart): crate::api::input::ApiMultipart,
) -> crate::api::ApiReply<String> {
    let upload_dir = state.paths.wallpapers();
    if let Err(e) = fs::create_dir_all(&upload_dir).await {
        return ApiResponse::internal(e);
    }

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return super::input::multipart_error(error),
        };
        let name = field.name().unwrap_or("").to_string();
        if name != "file" {
            continue;
        }

        let content_type = field.content_type().unwrap_or("").to_string();
        let Some(ext) = wallpaper_extension(&content_type) else {
            return crate::api::ApiReply::error(
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "仅支持 JPEG/PNG/WebP/GIF 图片",
            );
        };

        let filename = format!("{}.{}", Uuid::new_v4(), ext);
        let filepath = upload_dir.join(&filename);

        match field.bytes().await {
            Ok(data) => {
                if data.len() > 10 * 1024 * 1024 {
                    return ApiResponse::too_large("文件大小不能超过 10 MB".to_string());
                }
                if let Err(e) = fs::write(&filepath, &data).await {
                    return ApiResponse::internal(e);
                }
                let url = format!("/wallpapers/{}", filename);
                return ApiResponse::ok(url);
            }
            Err(e) => return super::input::multipart_error(e),
        }
    }

    ApiResponse::bad_request("未找到上传文件".to_string())
}

#[cfg(test)]
mod tests {
    use super::wallpaper_extension;

    /// 未知 image/* 类型必须显式拒绝而不是兜底落成 .jpg：壁纸不做内容重编码，
    /// 兜底会让任意字节（如 SVG）进入公开静态目录，仅靠 Content-Type 正确性兜底。
    #[test]
    fn unknown_image_types_are_rejected_instead_of_falling_back_to_jpg() {
        assert_eq!(wallpaper_extension("image/jpeg"), Some("jpg"));
        assert_eq!(wallpaper_extension("image/jpg"), Some("jpg"));
        assert_eq!(wallpaper_extension("image/png"), Some("png"));
        assert_eq!(wallpaper_extension("image/webp"), Some("webp"));
        assert_eq!(wallpaper_extension("image/gif"), Some("gif"));
        assert_eq!(wallpaper_extension("image/svg+xml"), None);
        assert_eq!(wallpaper_extension("image/avif"), None);
        assert_eq!(wallpaper_extension("text/html"), None);
        assert_eq!(wallpaper_extension(""), None);
    }
}
