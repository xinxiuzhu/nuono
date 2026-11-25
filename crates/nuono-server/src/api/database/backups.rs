//! 备份文件管理；路由统一由管理 API 的 ManageRuntime 权限保护。

use crate::api::input::{ApiJson as Json, ApiPath as UrlPath};
use nuono_core::{NuonoError, Result as NuonoResult};
use std::path::{Path, PathBuf};

use axum::{
    Extension,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    AppState,
    api::ApiResponse,
    backup::{self, BackupFormat},
    middleware::CallerIdentity,
};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(download_backup))
        .routes(routes!(delete_backup))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::database::BackupMetadata)]
pub struct BackupMetadata {
    /// automatic、manual 或 pre_restore。
    pub kind: String,
    /// 按文件头识别：postgres_custom、sqlite 或 unknown。
    pub format: String,
    /// 仅表示格式与当前后端匹配，完整性仍在实际恢复时校验。
    pub compatible: bool,
    /// 待恢复文件不能删除；标记损坏时保守地保护全部备份。
    pub restore_pending: bool,
    pub format_error: Option<String>,
}

/// 拒绝路径、目录和符号链接，所有文件必须直接位于真实备份目录中。
pub(super) fn resolve_backup_path(data_dir: &Path, name: &str) -> NuonoResult<PathBuf> {
    if !backup::validate_backup_filename(name) {
        return Err(NuonoError::Validation("备份文件名不合法".to_string()));
    }
    let directory = data_dir.join(backup::BACKUP_DIR);
    let directory_metadata = std::fs::symlink_metadata(&directory).map_err(backup_io)?;
    if !directory_metadata.file_type().is_dir() {
        return Err(NuonoError::Validation(
            "备份目录必须是普通目录，不能是符号链接".to_string(),
        ));
    }
    let path = directory.join(name);
    let metadata = std::fs::symlink_metadata(&path).map_err(backup_io)?;
    if !metadata.file_type().is_file() {
        return Err(NuonoError::Validation(
            "备份必须是普通文件，不能是目录或符号链接".to_string(),
        ));
    }
    let directory = std::fs::canonicalize(directory).map_err(backup_io)?;
    let path = std::fs::canonicalize(path).map_err(backup_io)?;
    if path.parent() != Some(directory.as_path()) {
        return Err(NuonoError::Validation("备份文件不在备份目录中".to_string()));
    }
    Ok(path)
}

pub(super) fn inspect_backup(
    data_dir: &Path,
    name: &str,
    database_url: &str,
) -> NuonoResult<BackupMetadata> {
    let path = resolve_backup_path(data_dir, name)?;
    let kind = if name.starts_with(backup::AUTO_BACKUP_PREFIX) {
        "automatic"
    } else if name.starts_with(backup::PRE_RESTORE_BACKUP_PREFIX) {
        "pre_restore"
    } else {
        "manual"
    };
    let lower = database_url.trim().to_ascii_lowercase();
    let sqlite_target = lower.starts_with("sqlite:") || lower == ":memory:";
    let sqlite_file = sqlite_target
        && matches!(
            nuono_core::db::sqlite_database_path(database_url),
            Ok(Some(_))
        );
    let (format, compatible, format_error) = match backup::detect_backup_format(&path) {
        Ok(BackupFormat::Sqlite) => ("sqlite", sqlite_file, None),
        Ok(BackupFormat::PostgresCustom) => ("postgres_custom", !sqlite_target, None),
        Err(error) => ("unknown", false, Some(error)),
    };
    let restore_pending = match backup::pending_restore_source(data_dir) {
        Ok(source) => source.as_ref() == Some(&path),
        Err(_) => true,
    };
    Ok(BackupMetadata {
        kind: kind.to_string(),
        format: format.to_string(),
        compatible,
        restore_pending,
        format_error,
    })
}

fn prepare_delete(data_dir: &Path, name: &str, confirmation: &str) -> NuonoResult<PathBuf> {
    if confirmation != name {
        return Err(NuonoError::Validation(
            "请完整输入备份文件名以确认删除".to_string(),
        ));
    }
    let path = resolve_backup_path(data_dir, name)?;
    if backup::pending_restore_source(data_dir)
        .map_err(NuonoError::Internal)?
        .as_ref()
        == Some(&path)
    {
        return Err(NuonoError::Conflict(
            "该备份正在等待恢复，不能删除".to_string(),
        ));
    }
    Ok(path)
}

fn backup_io(error: std::io::Error) -> NuonoError {
    if error.kind() == std::io::ErrorKind::NotFound {
        NuonoError::NotFound("备份文件不存在".into())
    } else {
        NuonoError::Internal(error.to_string())
    }
}
fn backup_error(error: NuonoError) -> Response {
    let mut response = ApiResponse::<()>::from_err(&error).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn download_error(status: StatusCode, error: impl Into<String>) -> Response {
    let mut response = crate::api::ApiReply::<()>::error(status, error).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn backup_file_response(
    file: tokio::fs::File,
    name: &str,
) -> Result<Response, String> {
    let metadata = file
        .metadata()
        .await
        .map_err(|_| "备份文件不可读取".to_string())?;
    if !metadata.is_file() {
        return Err("备份必须是普通文件".to_string());
    }
    // 从已打开的文件流式读取，文件大小不会决定服务器的内存占用。
    let stream = futures::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        let size = file.read(&mut buffer).await?;
        if size == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(size);
        Ok(Some((buffer, file)))
    });
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=\"nuono-backup.dump\"; filename*=UTF-8''{}",
        urlencoding::encode(name)
    ))
    .map_err(|_| "备份文件名无法用于下载".to_string())?;
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(metadata.len()));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

#[utoipa::path(
    get,
    path = "/backups/{filename}/download",
    operation_id = "database_download_backup",
    tag = "database",
    params(("filename" = String, Path, description = "备份列表中的完整文件名")),
    responses(
        (status = 200, description = "数据库备份附件，包含数据库中的密钥", content_type = "application/octet-stream", body = String),
        (status = 400, description = "备份名称或文件无效", body = ApiResponse<crate::api::docs::Empty>),
        (status = 404, description = "备份文件不可读取", body = ApiResponse<crate::api::docs::Empty>),
        (status = 500, description = "下载或审计失败", body = ApiResponse<crate::api::docs::Empty>)
    )
)]
async fn download_backup(
    State(state): State<AppState>,
    UrlPath(filename): UrlPath<String>,
    caller: Option<Extension<CallerIdentity>>,
) -> Response {
    // 备份任务直接写入最终文件名；等写入完成后再打开文件并确定 Content-Length。
    // handler 返回即释放锁，文件内容通过已打开的句柄继续流式发送。
    let _guard = state.restore_lock.lock().await;
    let path = match resolve_backup_path(&state.paths.data, &filename) {
        Ok(path) => path,
        Err(error) => return backup_error(error),
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) => return backup_error(backup_io(error)),
    };
    let actor = caller
        .as_ref()
        .map(|Extension(identity)| identity.username.as_str())
        .unwrap_or("local-admin");
    if let Err(error) = nuono_core::audit::record_event(
        &state.db,
        "database.backup_download_requested",
        None,
        Some(actor),
        serde_json::json!({ "filename": filename }),
    )
    .await
    {
        tracing::warn!(%error, "备份下载审计失败");
        return download_error(StatusCode::INTERNAL_SERVER_ERROR, "写入下载审计失败");
    }
    match backup_file_response(file, &filename).await {
        Ok(response) => response,
        Err(error) => download_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::database::DeleteBackupRequest)]
pub struct DeleteBackupRequest {
    /// 必须与路径中的完整文件名一致。
    pub confirmation: String,
}

#[utoipa::path(
    delete,
    path = "/backups/{filename}",
    operation_id = "database_delete_backup",
    tag = "database",
    params(("filename" = String, Path, description = "备份列表中的完整文件名")),
    request_body = DeleteBackupRequest,
    responses((status = 200, description = "删除结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn delete_backup(
    State(state): State<AppState>,
    UrlPath(filename): UrlPath<String>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<DeleteBackupRequest>,
) -> crate::api::ApiReply<String> {
    // 与恢复请求共用锁，校验、审计和删除期间不能新建引用此文件的恢复标记。
    let _guard = state.restore_lock.lock().await;
    let path = match prepare_delete(&state.paths.data, &filename, &body.confirmation) {
        Ok(path) => path,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let actor = caller
        .as_ref()
        .map(|Extension(identity)| identity.username.as_str())
        .unwrap_or("local-admin");
    // 先持久记录删除意图；数据库不可用时保留备份。
    if let Err(error) = nuono_core::audit::record_event(
        &state.db,
        "database.backup_delete_requested",
        None,
        Some(actor),
        serde_json::json!({ "filename": filename }),
    )
    .await
    {
        tracing::warn!(%error, "备份删除审计失败");
        return ApiResponse::internal("写入删除审计失败，备份已保留");
    }
    if let Err(error) = tokio::fs::remove_file(path).await {
        tracing::warn!(%filename, %error, "备份文件删除失败");
        return ApiResponse::internal("备份文件删除失败，请检查文件状态和访问权限");
    }
    if let Err(error) = nuono_core::audit::record_event(
        &state.db,
        "database.backup_deleted",
        None,
        Some(actor),
        serde_json::json!({ "filename": filename }),
    )
    .await
    {
        // 删除意图已经落库；文件已删除，不能对客户端宣称仍然存在。
        tracing::warn!(%filename, %error, "备份已删除，但完成审计写入失败");
    }
    ApiResponse::ok(format!("已删除备份 {filename}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQLITE_BACKUP: &str = "nuono_manual_backup_20261003_120000_sqlite.dump";
    const PG_BACKUP: &str = "nuono_auto_backup_20261003_120000_pg.dump";

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("nuono-backup-api-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join(backup::BACKUP_DIR)).unwrap();
            let fixture = Self(root);
            std::fs::write(fixture.backup(SQLITE_BACKUP), b"SQLite format 3\0").unwrap();
            std::fs::write(fixture.backup(PG_BACKUP), b"PGDMPtest").unwrap();
            fixture
        }

        fn backup(&self, name: &str) -> PathBuf {
            self.0.join(backup::BACKUP_DIR).join(name)
        }

        fn mark_restore(&self, name: &str) {
            std::fs::write(
                self.0.join(backup::RESTORE_MARKER),
                serde_json::to_vec(&backup::RestoreMarker {
                    source: self.backup(name).to_string_lossy().to_string(),
                    actor: "test-admin".to_string(),
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_paths_directories_and_control_characters() {
        let fixture = Fixture::new();
        for name in [
            "../nuono_manual_backup_fake.dump",
            "nuono_manual_backup_../fake.dump",
            "nuono_manual_backup_..\\fake.dump",
            " nuono_manual_backup_fake.dump",
            "nuono_manual_backup_fake\n.dump",
            "nuono_manual_backup_fake\0.dump",
        ] {
            assert!(resolve_backup_path(&fixture.0, name).is_err(), "{name:?}");
        }
        let directory_name = "nuono_manual_backup_directory.dump";
        std::fs::create_dir(fixture.backup(directory_name)).unwrap();
        assert!(resolve_backup_path(&fixture.0, directory_name).is_err());
        assert_eq!(
            resolve_backup_path(&fixture.0, SQLITE_BACKUP).unwrap(),
            std::fs::canonicalize(fixture.backup(SQLITE_BACKUP)).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_files_and_backup_directories() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let link_name = "nuono_manual_backup_symlink.dump";
        symlink(fixture.backup(SQLITE_BACKUP), fixture.backup(link_name)).unwrap();
        assert!(resolve_backup_path(&fixture.0, link_name).is_err());
        std::fs::rename(fixture.0.join(backup::BACKUP_DIR), fixture.0.join("moved")).unwrap();
        symlink(fixture.0.join("moved"), fixture.0.join(backup::BACKUP_DIR)).unwrap();
        assert!(resolve_backup_path(&fixture.0, SQLITE_BACKUP).is_err());
    }

    #[test]
    fn deletion_requires_exact_confirmation_and_preserves_pending_restore() {
        let fixture = Fixture::new();
        assert!(prepare_delete(&fixture.0, SQLITE_BACKUP, "").is_err());
        assert!(prepare_delete(&fixture.0, SQLITE_BACKUP, PG_BACKUP).is_err());
        fixture.mark_restore(SQLITE_BACKUP);
        assert!(
            prepare_delete(&fixture.0, SQLITE_BACKUP, SQLITE_BACKUP)
                .unwrap_err()
                .to_string()
                .contains("等待恢复")
        );
        assert!(fixture.backup(SQLITE_BACKUP).is_file());
        assert!(prepare_delete(&fixture.0, PG_BACKUP, PG_BACKUP).is_ok());
    }

    #[test]
    fn corrupt_restore_marker_blocks_deletion() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join(backup::RESTORE_MARKER), "invalid JSON").unwrap();
        assert!(prepare_delete(&fixture.0, PG_BACKUP, PG_BACKUP).is_err());
        assert!(
            inspect_backup(&fixture.0, PG_BACKUP, "postgres://localhost/test")
                .unwrap()
                .restore_pending
        );
    }

    #[test]
    fn metadata_identifies_backend_compatibility_and_unrecognized_files() {
        let fixture = Fixture::new();
        let sqlite = inspect_backup(&fixture.0, SQLITE_BACKUP, "sqlite://test.db").unwrap();
        assert_eq!(sqlite.kind, "manual");
        assert_eq!(sqlite.format, "sqlite");
        assert!(sqlite.compatible);
        assert!(!sqlite.restore_pending);
        let pg = inspect_backup(&fixture.0, PG_BACKUP, "postgres://localhost/test").unwrap();
        assert_eq!(pg.kind, "automatic");
        assert_eq!(pg.format, "postgres_custom");
        assert!(pg.compatible);
        assert!(
            !inspect_backup(&fixture.0, PG_BACKUP, "sqlite://test.db")
                .unwrap()
                .compatible
        );
        assert!(
            !inspect_backup(&fixture.0, SQLITE_BACKUP, "sqlite::memory:")
                .unwrap()
                .compatible
        );
        fixture.mark_restore(SQLITE_BACKUP);
        assert!(
            inspect_backup(&fixture.0, SQLITE_BACKUP, "sqlite://test.db")
                .unwrap()
                .restore_pending
        );
        std::fs::write(fixture.backup(PG_BACKUP), b"broken").unwrap();
        let invalid = inspect_backup(&fixture.0, PG_BACKUP, "postgres://localhost/test").unwrap();
        assert_eq!(invalid.format, "unknown");
        assert!(!invalid.compatible);
        assert!(invalid.format_error.is_some());
        assert!(prepare_delete(&fixture.0, PG_BACKUP, PG_BACKUP).is_ok());
    }

    #[tokio::test]
    async fn download_streams_exact_bytes_and_disables_caching() {
        let fixture = Fixture::new();
        let contents = vec![0x41; 192 * 1024 + 17];
        std::fs::write(fixture.backup(SQLITE_BACKUP), &contents).unwrap();
        let file = tokio::fs::File::open(fixture.backup(SQLITE_BACKUP))
            .await
            .unwrap();
        let response = backup_file_response(file, SQLITE_BACKUP).await.unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            contents.len().to_string()
        );
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains(SQLITE_BACKUP)
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), contents.len())
                .await
                .unwrap()
                .as_ref(),
            contents.as_slice()
        );
    }

    #[test]
    fn backup_management_routes_require_runtime_management_capability() {
        use axum::http::Method;
        use nuono_core::auth::Capability;

        for (method, path) in [
            (
                Method::GET,
                format!("/api/database/backups/{SQLITE_BACKUP}/download"),
            ),
            (
                Method::DELETE,
                format!("/api/database/backups/{SQLITE_BACKUP}"),
            ),
        ] {
            assert_eq!(
                crate::middleware::route_capability(&method, &path),
                Capability::ManageRuntime
            );
        }
    }
}
