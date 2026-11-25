//! 整站数据迁移包的打包、上传校验与恢复接口（挂载于 `/api/database`，受 `ManageRuntime` 权限与 CSRF 保护）。
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use crate::operation_error::OperationError as OpError;
use crate::{AppState, api::ApiResponse, backup::transfer, middleware::CallerIdentity};
use axum::{
    Extension,
    extract::{DefaultBodyLimit, State},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::io::AsyncWriteExt;
use utoipa_axum::{router::OpenApiRouter, routes};

struct UploadWorkspace {
    path: std::path::PathBuf,
    preserve: bool,
}

impl UploadWorkspace {
    fn keep(&mut self) {
        self.preserve = true;
    }
}

impl Drop for UploadWorkspace {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, create))
        .routes(routes!(import))
        .routes(routes!(result))
        .routes(routes!(inspect, remove))
        .routes(routes!(download))
        .routes(routes!(restore))
        .layer(DefaultBodyLimit::max(
            transfer::MAX_UPLOAD as usize + 1024 * 1024,
        ))
}
async fn plugins(state: &AppState) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in state.manager.plugin_host.all_plugins().await {
        let info = state.manager.plugin_host.get_plugin_info(&name).await;
        out.insert(name, info.version);
    }
    out
}
fn reply(result: Result<impl serde::Serialize, impl Into<OpError>>) -> crate::api::ApiReply<Value> {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => ApiResponse::ok(value),
            Err(error) => ApiResponse::internal(error),
        },
        Err(error) => ApiResponse::from_err(&error.into().0),
    }
}
#[utoipa::path(get,path="/transfers",operation_id="database_transfer_list",tag="database",responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn list(State(state): State<AppState>) -> crate::api::ApiReply<Value> {
    let _guard = state.restore_lock.lock().await;
    reply(transfer::list(&state.paths.data).map(|items| {
        items
            .into_iter()
            .map(|(id, manifest)| {
                let processed = transfer::workspace(&state.paths.data, &id)
                    .is_ok_and(|work| work.join("rollback").exists());
                json!({"id":id,"manifest":manifest,"processed":processed})
            })
            .collect::<Vec<_>>()
    }))
}
#[utoipa::path(post,path="/transfers",operation_id="database_transfer_create",tag="database",responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn create(State(state): State<AppState>) -> crate::api::ApiReply<Value> {
    let operations = state.database_operations.clone();
    match operations
        .run(state.restore_lock.clone(), async move {
            let cfg = state.config.snapshot().await;
            reply(
                transfer::create(
                    &state.paths.data,
                    &cfg.database.url,
                    state.config.config_path(),
                    plugins(&state).await,
                )
                .await,
            )
        })
        .await
    {
        Ok(response) => response,
        Err(error) => ApiResponse::from_err(&error),
    }
}
#[utoipa::path(post,path="/transfers/import",operation_id="database_transfer_import",tag="database",responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn import(
    State(state): State<AppState>,
    crate::api::input::ApiMultipart(mut multipart): crate::api::input::ApiMultipart,
) -> crate::api::ApiReply<Value> {
    let guard = state.restore_lock.clone().lock_owned().await;
    if transfer::is_pending(&state.paths.data) {
        return ApiResponse::conflict("等待恢复期间不能上传");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let work = match transfer::workspace(&state.paths.data, &id) {
        Ok(p) => p,
        Err(e) => return ApiResponse::internal(e),
    };
    if let Err(e) = tokio::fs::create_dir(&work).await {
        return ApiResponse::internal(e);
    }
    let mut cleanup = UploadWorkspace {
        path: work.clone(),
        preserve: false,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).await;
    }
    let uploaded = async {
        let mut field = multipart
            .next_field()
            .await
            .map_err(super::super::input::multipart_error)?
            .ok_or_else(|| ApiResponse::bad_request("未选择文件"))?;
        if field.name() != Some("file") {
            return Err(ApiResponse::bad_request("请用 file 字段上传迁移包"));
        }
        let mut file = tokio::fs::File::create(work.join("archive.nuono.tar.gz"))
            .await
            .map_err(ApiResponse::internal)?;
        let mut size = 0u64;
        while let Some(bytes) = field
            .chunk()
            .await
            .map_err(super::super::input::multipart_error)?
        {
            size += bytes.len() as u64;
            if size > transfer::MAX_UPLOAD {
                return Err(ApiResponse::too_large("迁移包超过 512 MiB"));
            }
            file.write_all(&bytes)
                .await
                .map_err(ApiResponse::internal)?;
        }
        file.sync_all().await.map_err(ApiResponse::internal)?;
        if multipart
            .next_field()
            .await
            .map_err(super::super::input::multipart_error)?
            .is_some()
        {
            return Err(ApiResponse::bad_request("一次只接受一个迁移包"));
        }
        Ok::<_, crate::api::ApiReply<Value>>(())
    }
    .await;
    if let Err(e) = uploaded {
        let _ = tokio::fs::remove_dir_all(&work).await;
        return e;
    }
    let operations = state.database_operations.clone();
    match operations
        .run_owned(guard, async move {
            let cfg = state.config.snapshot().await;
            let data = state.paths.data.clone();
            let url = cfg.database.url.clone();
            let current = plugins(&state).await;
            let checked = tokio::task::spawn_blocking(move || {
                transfer::inspect_upload(&data, &url, &id, &current)
            })
            .await
            .map_err(|e| OpError::from(e.to_string()))
            .and_then(|v| v);
            if checked.is_err() {
                let _ = tokio::fs::remove_dir_all(&work).await;
            } else {
                cleanup.keep();
            }
            reply(checked.map(|mut preview| {
                if let Err(error) = transfer::machine_file_conflicts(
                    &state.paths.data,
                    &preview.manifest.roots,
                    state.config.config_path(),
                ) {
                    preview.blockers.push(error);
                }
                preview
            }))
        })
        .await
    {
        Ok(response) => response,
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(get,path="/transfers/result",operation_id="database_transfer_result",tag="database",responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn result(State(state): State<AppState>) -> crate::api::ApiReply<Value> {
    reply(
        transfer::read_status(&state.paths.data).map(
            |result| json!({"result":result,"pending":transfer::is_pending(&state.paths.data)}),
        ),
    )
}
#[utoipa::path(get,path="/transfers/{id}",operation_id="database_transfer_inspect",tag="database",params(("id"=String,Path)),responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn inspect(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<Value> {
    let _guard = state.restore_lock.lock().await;
    let cfg = state.config.snapshot().await;
    let current = plugins(&state).await;
    reply(
        transfer::preview(&state.paths.data, &cfg.database.url, &id, &current).map(
            |mut preview| {
                if let Err(error) = transfer::machine_file_conflicts(
                    &state.paths.data,
                    &preview.manifest.roots,
                    state.config.config_path(),
                ) {
                    preview.blockers.push(error);
                }
                preview
            },
        ),
    )
}
#[utoipa::path(delete,path="/transfers/{id}",operation_id="database_transfer_remove",tag="database",params(("id"=String,Path)),responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<Value> {
    let _guard = state.restore_lock.lock().await;
    reply(transfer::remove(&state.paths.data, &id).map(|()| true))
}
#[utoipa::path(get,path="/transfers/{id}/download",operation_id="database_transfer_download",tag="database",params(("id"=String,Path)),responses((status=200,description="迁移包附件")))]
async fn download(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let _guard = state.restore_lock.lock().await;
    let path = match transfer::workspace(&state.paths.data, &id) {
        Ok(p) => p.join("archive.nuono.tar.gz"),
        Err(e) => return ApiResponse::<()>::from_err(&e.0).into_response(),
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return ApiResponse::<()>::bad_request("迁移包必须是普通文件").into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ApiResponse::<()>::not_found("迁移包不存在").into_response();
        }
        Err(error) => return ApiResponse::<()>::internal(error).into_response(),
    }
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            match super::backups::backup_file_response(file, &format!("nuono-transfer-{id}.tar.gz"))
                .await
            {
                Ok(response) => response,
                Err(e) => ApiResponse::<()>::internal(e).into_response(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ApiResponse::<()>::not_found("迁移包不存在").into_response()
        }
        Err(e) => ApiResponse::<()>::internal(e).into_response(),
    }
}
#[derive(Deserialize, utoipa::ToSchema)]
struct RestoreTransfer {
    confirmation: String,
}
#[utoipa::path(post,path="/transfers/{id}/restore",operation_id="database_transfer_restore",tag="database",params(("id"=String,Path)),request_body=RestoreTransfer,responses((status=200,body=ApiResponse<super::super::docs::AnyJson>)))]
async fn restore(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Json(body): Json<RestoreTransfer>,
) -> crate::api::ApiReply<Value> {
    if body.confirmation != id {
        return ApiResponse::bad_request("请输入完整迁移包编号以确认");
    }
    // 确认恢复后在独立任务中持锁跑完校验与恢复计划落盘；客户端断开 HTTP 连接不能打断阻塞校验。
    let operations = state.database_operations.clone();
    let work = operations.run(state.restore_lock.clone(), async move {
        let _source_guard = match crate::source_build::operation_lock().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                return ApiResponse::conflict("插件构建或缓存维护正在进行，请完成后再安排恢复");
            }
        };
        let cfg = state.config.snapshot().await;
        let current = plugins(&state).await;
        let data = state.paths.data.clone();
        let url = cfg.database.url.clone();
        let task_id = id.clone();
        let config_path = state.config.config_path().to_path_buf();
        let scheduled = tokio::task::spawn_blocking(move || {
            let preview = transfer::preview(&data, &url, &task_id, &current)?;
            transfer::machine_file_conflicts(&data, &preview.manifest.roots, &config_path)
                .map_err(OpError::conflict)?;
            transfer::schedule(&data, &url, &task_id, &current)
        })
        .await
        .map_err(|e| OpError::from(e.to_string()))
        .and_then(|v| v);
        if let Err(e) = scheduled {
            return ApiResponse::from_err(&e.0);
        }
        let actor = caller
            .as_ref()
            .map(|i| i.0.username.as_str())
            .unwrap_or("local-admin");
        if let Err(e) = nuono_core::audit::record_event(
            &state.db,
            "database.transfer_requested",
            None,
            Some(actor),
            json!({"id":id}),
        )
        .await
        {
            let _ =
                tokio::fs::remove_file(transfer::directory(&state.paths.data).join("pending.json"))
                    .await;
            return ApiResponse::internal(e);
        }
        state
            .manager
            .framework_control
            .request_restart("full data restore");
        ApiResponse::ok(json!({"scheduled":true,"id":id}))
    });
    match work.await {
        Ok(response) => response,
        Err(error) => ApiResponse::from_err(&error)
            .context("恢复安排未正常完成，请刷新并检查恢复状态，勿重复提交"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Mutex, oneshot};

    async fn cancelled_upload_waiter(preserve: bool) {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("upload");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("archive.nuono.tar.gz"), b"uploaded archive").unwrap();
        let operations = crate::database_operations::DatabaseOperations::new();
        let task_operations = operations.clone();
        let lock = Arc::new(Mutex::new(()));
        let guard = lock.clone().lock_owned().await;
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        let mut cleanup = UploadWorkspace {
            path: work.clone(),
            preserve: false,
        };
        let request = tokio::spawn(async move {
            task_operations
                .run_owned(guard, async move {
                    started.send(()).unwrap();
                    blocked.await.unwrap();
                    if preserve {
                        // 闭包通过调用可变方法持有 cleanup 守卫所有权，确保在任务完成时才触发 Drop。
                        cleanup.keep();
                    }
                })
                .await
        });
        ready.await.unwrap();
        assert!(work.is_dir(), "the operation must own the workspace guard");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(
            work.is_dir(),
            "cancelling the response cannot clean active work"
        );
        assert!(lock.try_lock().is_err());
        operations.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), operations.wait())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        operations.wait().await;
        assert!(lock.try_lock().is_ok());
        assert_eq!(work.exists(), preserve);
        if preserve {
            assert_eq!(
                std::fs::read(work.join("archive.nuono.tar.gz")).unwrap(),
                b"uploaded archive"
            );
        }
    }

    #[tokio::test]
    async fn cancelled_upload_waiter_preserves_successful_validation_workspace() {
        cancelled_upload_waiter(true).await;
    }

    #[tokio::test]
    async fn cancelled_upload_waiter_cleans_failed_validation_workspace_after_completion() {
        cancelled_upload_waiter(false).await;
    }
}
