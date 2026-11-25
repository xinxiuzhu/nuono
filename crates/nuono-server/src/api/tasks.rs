use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use axum::extract::State;
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(create))
        .routes(routes!(toggle))
        .routes(routes!(remove))
        .routes(routes!(update))
        .routes(routes!(preview))
        .routes(routes!(runs))
        .routes(routes!(run_once))
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "tasks_list",
    tag = "tasks",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::tasks::ScheduledTask>>))
)]
async fn list(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<nuono_core::service::tasks::ScheduledTask>> {
    ApiResponse::from_result(nuono_core::service::tasks::list(&state.db).await)
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::tasks::CreateBody)]
struct CreateBody {
    name: String,
    cron_expr: String,
    #[serde(default = "nuono_app::scheduled_tasks::default_timezone")]
    timezone: String,
    action_type: String,
    action_params: Option<String>,
    target_id: Option<String>,
    content: Option<String>,
    /// 投递 Bot 的 OneBot self_id；空 = 老行为（广播，执行时告警）
    bot_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/",
    operation_id = "tasks_create",
    tag = "tasks",
    request_body = CreateBody,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<nuono_core::service::tasks::ScheduledTask>))
)]
async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> crate::api::ApiReply<nuono_core::service::tasks::ScheduledTask> {
    let (action_type, action_params) = match normalize_task_action(&body) {
        Ok(action) => action,
        Err(e) => return ApiResponse::bad_request(e),
    };
    match nuono_core::service::tasks::create_definition(
        &state.db,
        &nuono_app::scheduled_tasks::TaskDefinition {
            name: body.name,
            cron_expr: body.cron_expr,
            timezone: body.timezone,
            action_type,
            action_params,
            bot_id: body.bot_id,
        },
    )
    .await
    {
        Ok(task) => match state.manager.scheduler.reload().await {
            Ok(()) => ApiResponse::ok(task),
            Err(e) => ApiResponse::from_err(&e)
                .context("任务变更已保存，但调度器刷新失败，请检查任务状态后重试刷新"),
        },
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::tasks::ToggleBody)]
struct ToggleBody {
    enabled: bool,
}

#[utoipa::path(
    post,
    path = "/{id}/toggle",
    operation_id = "tasks_toggle",
    tag = "tasks",
    request_body = ToggleBody,
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn toggle(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ToggleBody>,
) -> crate::api::ApiReply<()> {
    match nuono_core::service::tasks::toggle(&state.db, id, body.enabled).await {
        Ok(()) => match state.manager.scheduler.reload().await {
            Ok(()) => ApiResponse::ok(()),
            Err(e) => ApiResponse::from_err(&e)
                .context("任务变更已保存，但调度器刷新失败，请检查任务状态后重试刷新"),
        },
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/{id}",
    operation_id = "tasks_remove",
    tag = "tasks",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> crate::api::ApiReply<()> {
    match nuono_core::service::tasks::remove(&state.db, id).await {
        Ok(()) => match state.manager.scheduler.reload().await {
            Ok(()) => ApiResponse::ok(()),
            Err(e) => ApiResponse::from_err(&e)
                .context("任务变更已保存，但调度器刷新失败，请检查任务状态后重试刷新"),
        },
        Err(e) => ApiResponse::from_err(&e),
    }
}

fn normalize_task_action(body: &CreateBody) -> Result<(String, Option<String>), String> {
    match body.action_type.as_str() {
        "send_group_message" => Ok((
            "send_message".to_string(),
            Some(
                serde_json::json!({
                    "target_type": "group",
                    "target_id": body.target_id.as_deref().unwrap_or_default(),
                    "message": body.content.as_deref().unwrap_or_default(),
                })
                .to_string(),
            ),
        )),
        "send_private_message" => Ok((
            "send_message".to_string(),
            Some(
                serde_json::json!({
                    "target_type": "private",
                    "target_id": body.target_id.as_deref().unwrap_or_default(),
                    "message": body.content.as_deref().unwrap_or_default(),
                })
                .to_string(),
            ),
        )),
        "send_message" | "push_subscribe" | "send_native_message" => {
            Ok((body.action_type.clone(), body.action_params.clone()))
        }
        action_type => Err(format!(
            "不支持的任务动作类型: {}（支持 send_group_message、send_private_message、send_message、send_native_message、push_subscribe）",
            action_type
        )),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct UpdateBody {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    revision: i64,
    #[serde(flatten)]
    definition: nuono_app::scheduled_tasks::TaskDefinition,
}

#[utoipa::path(put, path="/{id}", operation_id="tasks_update", tag="tasks", params(("id"=String, Path)), request_body=UpdateBody,
    responses((status=200, description="保存任务并刷新调度快照", body=ApiResponse<nuono_core::service::tasks::ScheduledTask>)))]
async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> crate::api::ApiReply<nuono_core::service::tasks::ScheduledTask> {
    match nuono_core::service::tasks::update(&state.db, id, body.revision, &body.definition).await {
        Ok(task) => match state.manager.scheduler.reload().await {
            Ok(()) => ApiResponse::ok(task),
            Err(error) => ApiResponse::from_err(&error)
                .context("任务已保存，但调度器刷新失败，请重新加载核对"),
        },
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
struct PreviewBody {
    cron_expr: String,
    timezone: String,
}

#[utoipa::path(post, path="/preview", operation_id="tasks_preview", tag="tasks", request_body=PreviewBody,
    responses((status=200, description="未来五次执行时间（RFC3339，含所选时区偏移）", body=ApiResponse<Vec<String>>)))]
async fn preview(Json(body): Json<PreviewBody>) -> crate::api::ApiReply<Vec<String>> {
    if body.cron_expr.len() > nuono_app::scheduled_tasks::MAX_CRON_BYTES {
        return ApiResponse::bad_request("Cron 表达式过长");
    }
    let zone = match nuono_app::scheduled_tasks::timezone(&body.timezone) {
        Ok(zone) => zone,
        Err(error) => return ApiResponse::from_err(&error),
    };
    ApiResponse::from_result(
        nuono_app::scheduled_tasks::next_occurrences(
            &body.cron_expr,
            &body.timezone,
            chrono::Utc::now(),
            5,
        )
        .map(|dates| {
            dates
                .into_iter()
                .map(|date| date.with_timezone(&zone).to_rfc3339())
                .collect()
        }),
    )
}

#[utoipa::path(get, path="/{id}/runs", operation_id="tasks_runs", tag="tasks", params(("id"=String, Path)),
    responses((status=200, description="最近100次执行记录，保留30天", body=ApiResponse<Vec<nuono_core::service::tasks::TaskRun>>)))]
async fn runs(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<Vec<nuono_core::service::tasks::TaskRun>> {
    ApiResponse::from_result(nuono_core::service::tasks::list_runs(&state.db, id).await)
}

#[derive(Deserialize, utoipa::ToSchema)]
struct RunBody {
    request_id: String,
}

#[utoipa::path(post, path="/{id}/run", operation_id="tasks_run_once", tag="tasks", params(("id"=String, Path)), request_body=RunBody,
    responses((status=200, description="手动执行回执；同一请求编号不重复投递", body=ApiResponse<nuono_core::service::tasks::TaskRun>)))]
async fn run_once(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RunBody>,
) -> crate::api::ApiReply<nuono_core::service::tasks::TaskRun> {
    let scheduler = state.manager.scheduler.clone();
    let paths = state.paths.clone();
    let result = state
        .database_operations
        .run(state.restore_lock.clone(), async move {
            if paths.data.join(crate::backup::RESTORE_MARKER).exists()
                || crate::backup::transfer::is_pending(&paths.data)
            {
                return Err(nuono_core::NuonoError::Conflict(
                    "已安排数据恢复，不能开始新的任务投递".into(),
                ));
            }
            scheduler.run_once(id, &body.request_id).await
        })
        .await
        .and_then(|result| result);
    ApiResponse::from_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(action_type: &str) -> CreateBody {
        CreateBody {
            name: "test".to_string(),
            cron_expr: "0 0 0 * * *".to_string(),
            timezone: "UTC".into(),
            action_type: action_type.to_string(),
            action_params: Some(r#"{"topic":"news","message":"hello"}"#.to_string()),
            target_id: Some("42".to_string()),
            content: Some("hello".to_string()),
            bot_id: None,
        }
    }

    #[test]
    fn rejects_actions_without_real_scheduler_execution() {
        for action_type in [
            "http_request",
            "plugin_hook",
            "custom",
            "unknown",
            "run_sop",
        ] {
            let error = normalize_task_action(&body(action_type)).unwrap_err();
            assert!(error.contains(action_type));
        }
    }

    #[test]
    fn accepts_only_supported_scheduler_actions() {
        let (action_type, params) = normalize_task_action(&body("send_group_message")).unwrap();
        assert_eq!(action_type, "send_message");
        let params: serde_json::Value = serde_json::from_str(&params.unwrap()).unwrap();
        assert_eq!(params["target_type"], "group");
        assert_eq!(params["target_id"], "42");
        assert_eq!(params["message"], "hello");

        for action_type in ["send_message", "push_subscribe"] {
            assert!(normalize_task_action(&body(action_type)).is_ok());
        }
    }
}
