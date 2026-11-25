//! 界面主题偏好设置的 HTTP 接口（首次启动时会自动从旧版 `theme_config.toml` 导入一次初始配置）。
use crate::api::input::ApiJson as Json;
use crate::{AppState, api::ApiResponse};
use axum::extract::State;
use nuono_app::preferences::{ThemeConfig, ThemeService, ThemeStore};
use utoipa_axum::{router::OpenApiRouter, routes};
pub(crate) async fn initialize(
    db: &nuono_core::db::Database,
    paths: &nuono_core::core::Paths,
) -> nuono_core::error::Result<()> {
    if let Some(config) = db.load_theme().await? {
        config.validate()?;
        return Ok(());
    }
    let path = paths.data.join("theme_config.toml");
    let seed = match tokio::fs::read_to_string(&path).await {
        Ok(content) => toml::from_str(&content).map_err(|e| {
            nuono_core::error::NuonoError::ConfigLoad(format!("{}: {e}", path.display()))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ThemeConfig::default(),
        Err(error) => {
            return Err(nuono_core::error::NuonoError::ConfigLoad(format!(
                "{}: {error}",
                path.display()
            )));
        }
    };
    ThemeService::new(db).initialize(&seed).await?;
    Ok(())
}
#[utoipa::path(
    get,
    path = "/theme",
    operation_id = "theme_get_theme",
    tag = "theme",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::ThemeSettings>))
)]
pub async fn get_theme(State(state): State<AppState>) -> crate::api::ApiReply<ThemeConfig> {
    match ThemeService::new(&state.db).get().await {
        Ok(config) => ApiResponse::ok(config),
        Err(error) => ApiResponse::from_err(&error),
    }
}
#[utoipa::path(
    post,
    path = "/theme",
    operation_id = "theme_save_theme",
    tag = "theme",
    request_body = super::docs::ThemeSettings,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::Empty>))
)]
pub async fn save_theme(
    State(state): State<AppState>,
    Json(body): Json<ThemeConfig>,
) -> crate::api::ApiReply<()> {
    match ThemeService::new(&state.db).save(&body).await {
        Ok(()) => ApiResponse::ok(()),
        Err(error) => ApiResponse::from_err(&error),
    }
}
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_theme, save_theme))
}
