//! 业务尚未开放时的统一 HTTP 响应；管理入口不经过此检查。
use crate::{AppState, business_startup::BusinessPhase};
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// 仅由业务启动门控写入的进程内标记，客户端不能通过请求或响应头伪造。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExpectedBusinessWait {
    code: &'static str,
}

impl ExpectedBusinessWait {
    pub(crate) fn code(self) -> &'static str {
        self.code
    }
}

/// 为支持重试提示的客户端提供短暂等待；业务开放后无需重新配置客户端。
const RETRY_AFTER_SECONDS: HeaderValue = HeaderValue::from_static("5");

#[derive(Serialize)]
struct UnavailableBody {
    code: &'static str,
    message: &'static str,
    business_state: BusinessPhase,
}

pub(crate) fn reject_unavailable(state: &AppState) -> Option<Response> {
    if state.manager.business_is_running() {
        return None;
    }
    Some(unavailable_response(state.business_startup.phase()))
}

fn unavailable_response(phase: BusinessPhase) -> Response {
    let (code, message) = match phase {
        BusinessPhase::WaitingSetup => ("setup_required", "请先完成首次管理员设置"),
        BusinessPhase::WaitingHttp | BusinessPhase::Starting => {
            ("business_starting", "业务正在启动，请稍后重试")
        }
        BusinessPhase::Failed => (
            "business_start_failed",
            "业务启动失败，请通过管理页面检查配置和日志",
        ),
        BusinessPhase::Stopped => ("business_stopped", "业务已停止"),
        BusinessPhase::Running => ("business_unavailable", "业务暂不可用，请稍后重试"),
    };
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(UnavailableBody {
            code,
            message,
            business_state: phase,
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if matches!(
        phase,
        BusinessPhase::WaitingHttp | BusinessPhase::WaitingSetup | BusinessPhase::Starting
    ) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, RETRY_AFTER_SECONDS);
        response
            .extensions_mut()
            .insert(ExpectedBusinessWait { code });
    }
    response
}

#[cfg(test)]
#[path = "../tests/unit/business_http.rs"]
mod tests;
