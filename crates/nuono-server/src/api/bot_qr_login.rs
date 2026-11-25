//! 扫码会话沿用实例管理权限、CSRF 校验和当前管理员身份。
use crate::{
    AppState,
    api::{
        ApiResponse,
        input::{ApiJson as Json, ApiPath as Path},
    },
    bot_instances::{BotInstanceView, CreateBotInstanceRequest},
    bot_qr_login::QrLoginView,
    middleware::CallerIdentity,
};
use axum::{Extension, extract::State};
use nuono_core::{
    auth::Capability,
    error::{NuonoError, Result},
};
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start))
        .routes(routes!(poll))
        .routes(routes!(cancel))
        .routes(routes!(complete))
}

async fn owner(state: &AppState, caller: Option<Extension<CallerIdentity>>) -> Result<i64> {
    match caller {
        Some(caller) if caller.has(Capability::ManageRuntime) => Ok(caller.user_id),
        None if !state.config.snapshot().await.security.admin_auth_enabled => Ok(0),
        _ => Err(NuonoError::Unauthorized("需要机器人管理权限".into())),
    }
}

#[utoipa::path(post, path = "/bot-instance-types/{adapter}/qr-login", operation_id = "bot_instances_qr_start", tag = "bot_instances",
    params(("adapter" = String, Path)),
    responses((status = 200, description = "申请二维码；不返回临时密钥或机器人凭据", body = ApiResponse<QrLoginView>)))]
async fn start(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(adapter): Path<String>,
) -> crate::api::ApiReply<QrLoginView> {
    let result = async {
        let owner = owner(&state, caller).await?;
        ensure_business(&state)?;
        let provider = nuono_adapter::running()
            .get(&adapter)
            .ok_or_else(|| NuonoError::NotFound("适配器未安装".into()))?;
        state.bot_instances.qr_logins.start(owner, provider).await
    }
    .await;
    match result {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path = "/bot-instance-qr-logins/{id}/poll", operation_id = "bot_instances_qr_poll", tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "查询绑定结果；只返回公开配置", body = ApiResponse<QrLoginView>)))]
async fn poll(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<QrLoginView> {
    let result = async {
        ensure_business(&state)?;
        state
            .bot_instances
            .qr_logins
            .poll(owner(&state, caller).await?, &id)
            .await
    }
    .await;
    match result {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(delete, path = "/bot-instance-qr-logins/{id}", operation_id = "bot_instances_qr_cancel", tag = "bot_instances",
    params(("id" = String, Path)),
    responses((status = 200, description = "释放临时绑定会话，不删除已保存的实例", body = ApiResponse<String>)))]
async fn cancel(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<String> {
    let result = async {
        state
            .bot_instances
            .qr_logins
            .cancel(owner(&state, caller).await?, &id)
            .await
    }
    .await;
    match result {
        Ok(()) => ApiResponse::ok(id),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(post, path = "/bot-instance-qr-logins/{id}/complete", operation_id = "bot_instances_qr_complete", tag = "bot_instances",
    params(("id" = String, Path)), request_body = CreateBotInstanceRequest,
    responses((status = 200, description = "用已绑定凭据保存实例；同会话重试返回原实例", body = ApiResponse<BotInstanceView>)))]
async fn complete(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Path(id): Path<String>,
    Json(request): Json<CreateBotInstanceRequest>,
) -> crate::api::ApiReply<BotInstanceView> {
    let result = async {
        state
            .bot_instances
            .qr_logins
            .complete(
                owner(&state, caller).await?,
                &id,
                request,
                state.bot_instances.clone(),
            )
            .await
    }
    .await;
    match result {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error),
    }
}

fn ensure_business(state: &AppState) -> Result<()> {
    if state.manager.business_is_running() {
        Ok(())
    } else {
        Err(NuonoError::Conflict(
            "业务尚未启动，请先完成管理员设置".into(),
        ))
    }
}
