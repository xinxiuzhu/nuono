use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use crate::backup::{self, RESTORE_MARKER};
use crate::middleware::CallerIdentity;
use axum::{
    Extension,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;
use utoipa_axum::{router::OpenApiRouter, routes};

mod backups;
mod browser;
mod policies;
mod records;
mod transfers;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(browser::router())
        .merge(records::router())
        .merge(backups::router())
        .merge(transfers::router())
        .routes(routes!(list_tables))
        .routes(routes!(db_stats))
        .routes(routes!(backup))
        .routes(routes!(list_backups))
        .routes(routes!(restore))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::database::DbStats)]
pub struct DbStats {
    pub db_type: String,
    pub connected: bool,
    pub table_count: usize,
    /// 已废弃的全库总行数统计；为保持旧客户端兼容固定返回 null。
    pub total_records: Option<u64>,
    pub db_size: String,
    /// SQLite 时为数据库文件大小；PostgreSQL 是服务器数据库，不存在可供前端
    /// 下载的本地数据库文件，恒为 0。
    pub file_size_bytes: u64,
    pub row_counts: Value,
    pub last_backup: Option<String>,
    pub auto_backup_enabled: bool,
    pub auto_backup_retention_days: u64,
}

/// 人类可读的文件大小（SQLite 文件库统计用）。
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[utoipa::path(
    get,
    path = "/tables",
    operation_id = "database_list_tables",
    tag = "database",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<String>>))
)]
async fn list_tables(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(std::time::Duration::from_secs(15), list_tables_inner(state)).await {
        Ok(result) => ([(header::CACHE_CONTROL, "no-store")], result).into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            [(header::CACHE_CONTROL, "no-store")],
            ApiResponse::<Vec<String>>::timeout("数据库查询超时，请稍后重试"),
        )
            .into_response(),
    }
}

async fn list_tables_inner(state: AppState) -> crate::api::ApiReply<Vec<String>> {
    match state.db.inspect_tables().await {
        Ok(tables) => ApiResponse::ok(tables.into_iter().map(|table| table.name).collect()),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get,
    path = "/stats",
    operation_id = "database_db_stats",
    tag = "database",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<DbStats>))
)]
async fn db_stats(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(std::time::Duration::from_secs(15), db_stats_inner(state)).await {
        Ok(result) => ([(header::CACHE_CONTROL, "no-store")], result).into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            [(header::CACHE_CONTROL, "no-store")],
            ApiResponse::<DbStats>::timeout("数据库查询超时，请稍后重试"),
        )
            .into_response(),
    }
}

async fn db_stats_inner(state: AppState) -> crate::api::ApiReply<DbStats> {
    let cfg = state.config.get().await;
    let tables = match state.db.inspect_tables().await {
        Ok(tables) => tables,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let connected = state.db.ping().await.unwrap_or(false);
    let size = match state.db.database_size_bytes().await {
        Ok(size) => size,
        Err(error) => return ApiResponse::from_err(&error),
    };
    // 数据库统计按实际后端：SQLite 报文件大小，PostgreSQL 由服务端管理。
    let (db_type, db_size, file_size_bytes) = if state.db.backend().as_str() == "Sqlite" {
        let file_size = nuono_core::db::sqlite_database_path(&cfg.database.url)
            .ok()
            .flatten()
            .and_then(|path| std::fs::metadata(&path).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        ("SQLite".to_string(), format_size(size), file_size)
    } else {
        ("PostgreSQL".to_string(), format_size(size), 0)
    };
    let last_backup = {
        let _guard = state.restore_lock.lock().await;
        read_backups(&state.paths.data, &cfg.database.url)
            .first()
            .map(|entry| entry.name.clone())
    };
    ApiResponse::ok(DbStats {
        db_type,
        connected,
        table_count: tables.len(),
        total_records: None,
        db_size,
        file_size_bytes,
        row_counts: serde_json::json!({}),
        last_backup,
        auto_backup_enabled: cfg.backup.enabled,
        auto_backup_retention_days: cfg.backup.retention_days,
    })
}

#[utoipa::path(
    post,
    path = "/backup",
    operation_id = "database_backup",
    tag = "database",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn backup(State(state): State<AppState>) -> crate::api::ApiReply<String> {
    let operations = state.database_operations.clone();
    match operations
        .run(state.restore_lock.clone(), backup_locked(state))
        .await
    {
        Ok(response) => response,
        Err(error) => ApiResponse::from_err(&error),
    }
}

async fn backup_locked(state: AppState) -> crate::api::ApiReply<String> {
    let data_dir = state.paths.data.clone();
    let backup_dir = data_dir.join(backup::BACKUP_DIR);
    if let Err(e) = tokio::fs::create_dir_all(&backup_dir).await {
        return ApiResponse::internal(e);
    }
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let unique_id = uuid::Uuid::new_v4().simple().to_string();
    let path = backup_path_for(&data_dir, &ts, &unique_id);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let cfg = state.config.get().await;

    let result = backup::dump_database(&cfg.database.url, &path).await;
    crate::operations::record_backup_result(
        &state.paths.data,
        result.as_ref().map(|_| ()).map_err(ToString::to_string),
    )
    .await;
    match result {
        Ok(()) => {
            info!(path = %path.display(), "数据库备份完成");
            ApiResponse::ok(name)
        }
        Err(e) => ApiResponse::internal(e),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::database::BackupEntry)]
pub struct BackupEntry {
    pub name: String,
    pub size: u64,
    pub created_at: String,
    #[serde(flatten)]
    pub metadata: backups::BackupMetadata,
}

#[utoipa::path(
    get,
    path = "/backups",
    operation_id = "database_list_backups",
    tag = "database",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<BackupEntry>>))
)]
async fn list_backups(State(state): State<AppState>) -> crate::api::ApiReply<Vec<BackupEntry>> {
    let _guard = state.restore_lock.lock().await;
    let cfg = state.config.get().await;
    ApiResponse::ok(read_backups(&state.paths.data, &cfg.database.url))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::database::RestoreRequest)]
pub struct RestoreRequest {
    pub filename: String,
}

#[utoipa::path(
    post,
    path = "/restore",
    operation_id = "database_restore",
    tag = "database",
    request_body = RestoreRequest,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn restore(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<RestoreRequest>,
) -> crate::api::ApiReply<String> {
    let name = body.filename.trim().to_string();
    if !backup::validate_backup_filename(&name) {
        return ApiResponse::bad_request("备份文件名不合法");
    }
    // 恢复请求一旦受理，必须在独立任务中持有互斥锁跑完恢复前备份与标记落盘；
    // 客户端中途断开 HTTP 连接也不能中断已进入互斥区的操作。
    let operations = state.database_operations.clone();
    let work = operations.run(state.restore_lock.clone(), async move {
        let _source_guard = match crate::source_build::operation_lock().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                return ApiResponse::conflict("插件构建或缓存维护正在进行，请完成后再安排恢复");
            }
        };
        let marker = state.paths.data.join(RESTORE_MARKER);
        if std::fs::symlink_metadata(&marker).is_ok()
            || backup::transfer::is_pending(&state.paths.data)
        {
            return ApiResponse::conflict("已有一个数据库恢复请求正在等待重启");
        }
        let source = match backups::resolve_backup_path(&state.paths.data, &name) {
            Ok(path) => path,
            Err(error) => return ApiResponse::from_err(&error),
        };
        let cfg = state.config.get().await;
        match backups::inspect_backup(&state.paths.data, &name, &cfg.database.url) {
            Ok(metadata) if metadata.compatible => {}
            Ok(metadata) => {
                return ApiResponse::conflict(
                    metadata
                        .format_error
                        .unwrap_or_else(|| "备份格式与当前数据库不匹配，无法恢复".to_string()),
                );
            }
            Err(error) => return ApiResponse::from_err(&error),
        }
        let pre_name = format!(
            "{}{}_{}.dump",
            backup::PRE_RESTORE_BACKUP_PREFIX,
            chrono::Local::now().format("%Y%m%d_%H%M%S"),
            uuid::Uuid::new_v4().simple()
        );
        let pre_path = state.paths.data.join(backup::BACKUP_DIR).join(&pre_name);
        if let Err(e) = backup::dump_database(&cfg.database.url, &pre_path).await {
            return ApiResponse::internal(e);
        }

        let source = match std::fs::canonicalize(&source) {
            Ok(path) => path,
            Err(e) => return ApiResponse::internal(e),
        };
        let actor = caller
            .as_ref()
            .map(|Extension(identity)| identity.username.as_str())
            .unwrap_or("local-admin");
        let marker_data = backup::RestoreMarker {
            source: source.to_string_lossy().to_string(),
            actor: actor.to_string(),
        };
        let marker_json = match serde_json::to_string(&marker_data) {
            Ok(json) => json,
            Err(e) => return ApiResponse::internal(e),
        };
        let mut marker_file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(file) => file,
            Err(error) => {
                return ApiResponse::internal(error);
            }
        };
        if let Err(e) = marker_file
            .write_all(marker_json.as_bytes())
            .and_then(|_| marker_file.sync_all())
        {
            let _ = std::fs::remove_file(&marker);
            return ApiResponse::internal(e);
        }
        if let Err(e) = nuono_core::audit::record_event(
            &state.db,
            "database.restore_requested",
            None,
            Some(actor),
            serde_json::json!({ "filename": name, "pre_restore_backup": pre_name }),
        )
        .await
        {
            let _ = std::fs::remove_file(&marker);
            return ApiResponse::internal(e);
        }
        info!(filename = %name, actor, "🔁 请求从备份恢复数据库，准备重启");
        state
            .manager
            .framework_control
            .request_restart("database restore");

        ApiResponse::ok(format!(
            "将在重启后从 {name} 恢复。服务会停止；未配置进程守护时需在终端重新启动。账号、会话与数据将回到备份时点（恢复前的现状已另行备份）。"
        ))
    });
    match work.await {
        Ok(response) => response,
        Err(error) => ApiResponse::from_err(&error)
            .context("恢复安排未正常完成，请刷新并检查恢复状态，勿重复提交"),
    }
}

fn read_backups(data_dir: &Path, database_url: &str) -> Vec<BackupEntry> {
    let mut backups = Vec::new();
    let dir = data_dir.join(backup::BACKUP_DIR);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !backup::validate_backup_filename(&name) {
                continue;
            }
            let Ok(inspection) = backups::inspect_backup(data_dir, &name, database_url) else {
                continue;
            };
            let metadata = entry.metadata().ok();
            let created_at = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
                .map(|dt| {
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            backups.push(BackupEntry {
                name,
                size: metadata.as_ref().map(|m| m.len()).unwrap_or_default(),
                created_at,
                metadata: inspection,
            });
        }
    }
    backups.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.name.cmp(&a.name))
    });
    backups
}

fn backup_path_for(data_dir: &Path, timestamp: &str, unique_id: &str) -> PathBuf {
    data_dir.join(backup::BACKUP_DIR).join(format!(
        "{}{}_{}.dump",
        backup::MANUAL_BACKUP_PREFIX,
        timestamp,
        unique_id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_are_safe_and_in_data_backup() {
        let root = Path::new("/srv/nuono/data");
        assert_eq!(
            backup_path_for(root, "20260815_120000", "abc"),
            PathBuf::from("/srv/nuono/data/backup/nuono_manual_backup_20260815_120000_abc.dump",)
        );
        assert!(backup::validate_backup_filename(
            "nuono_manual_backup_20260815_120000_abc.dump"
        ));
        assert!(!backup::validate_backup_filename("../nuono.dump"));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }
}
