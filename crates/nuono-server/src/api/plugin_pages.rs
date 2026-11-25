//! 插件自定义前端页面发现接口，以及绑定当前会话、带容量与过期限制的页面静态资源访问票据。
use super::{ApiReply, ApiResponse};
use crate::{AppState, middleware::CallerIdentity};
use axum::{
    Extension,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use nuono_core::{
    auth::{Capability, Role},
    core::plugin_host::PluginWebPageView,
    plugin::PluginWebPagePlacement,
};
use rand::TryRng;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

const GRANT_TTL: Duration = Duration::from_secs(3600);
const MAX_GRANTS: usize = 4096;
const BRIDGE: &str = include_str!("../plugin_view_bridge.js");

#[derive(Clone)]
struct PageGrant {
    session: String,
    role: Role,
    plugin: String,
    page: String,
    generation: u64,
    expires: Instant,
}

/// 页面授权映射仅保存在服务端内存中；浏览器只拿到一个仅限读取该页面静态资源的随机票据（ticket）。
#[derive(Default)]
pub(crate) struct PageSessions(Mutex<HashMap<String, PageGrant>>);

impl PageSessions {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn issue(
        &self,
        session: String,
        view: &PluginWebPageView,
        role: Role,
    ) -> anyhow::Result<String> {
        let now = Instant::now();
        let mut grants = self.0.lock().await;
        grants.retain(|_, grant| grant.expires > now);
        if let Some((ticket, grant)) = grants.iter_mut().find(|(_, grant)| {
            grant.session == session
                && grant.role == role
                && grant.plugin == view.plugin
                && grant.page == view.page.id
                && grant.generation == view.generation
        }) {
            grant.expires = now + GRANT_TTL;
            return Ok(ticket.clone());
        }
        anyhow::ensure!(
            grants.len() < MAX_GRANTS,
            "plugin page session capacity reached"
        );
        let mut bytes = [0u8; 32];
        rand::rngs::SysRng.try_fill_bytes(&mut bytes)?;
        let ticket = hex::encode(bytes);
        grants.insert(
            ticket.clone(),
            PageGrant {
                session,
                role,
                plugin: view.plugin.clone(),
                page: view.page.id.clone(),
                generation: view.generation,
                expires: now + GRANT_TTL,
            },
        );
        Ok(ticket)
    }

    async fn resolve(&self, ticket: &str) -> Option<PageGrant> {
        if ticket.len() != 64 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut grants = self.0.lock().await;
        let grant = grants.get(ticket)?;
        if grant.expires <= Instant::now() {
            grants.remove(ticket);
            return None;
        }
        Some(grant.clone())
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PageInfo {
    plugin: String,
    id: String,
    revision: String,
    title: String,
    icon: String,
    dock: bool,
    path: String,
}

impl PageInfo {
    fn from_view(view: &PluginWebPageView, role: Role) -> Self {
        let page = &view.page;
        Self {
            plugin: view.plugin.clone(),
            id: page.id.clone(),
            revision: page_revision(view.generation),
            title: if role.has(Capability::ManageRuntime) {
                page.title.clone()
            } else {
                page.user_title
                    .clone()
                    .unwrap_or_else(|| page.title.clone())
            },
            icon: page.icon.clone(),
            dock: page.placement == PluginWebPagePlacement::Dock,
            path: format!(
                "/plugin/{}/{}",
                urlencoding::encode(&view.plugin),
                urlencoding::encode(&page.id)
            ),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PageCatalog {
    version: u16,
    pages: Vec<PageInfo>,
}

fn page_revision(generation: u64) -> String {
    static BOOT: OnceLock<uuid::Uuid> = OnceLock::new();
    format!("{}-{generation}", BOOT.get_or_init(uuid::Uuid::new_v4))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PageEntry {
    version: u16,
    page: PageInfo,
    content_path: String,
}

pub(crate) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(entry))
        .route("/api/plugin-pages/bridge.js", get(bridge))
        .route("/api/plugin-pages/assets/{ticket}/{*path}", get(asset))
}

#[utoipa::path(get, path = "/api/plugin-pages", tag = "Plugin pages", responses((status = 200, body = ApiResponse<PageCatalog>)))]
async fn list(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerIdentity>,
) -> ApiReply<PageCatalog> {
    let views = state
        .manager
        .plugin_host
        .plugin_web_pages(caller.role)
        .await;
    ApiResponse::ok(PageCatalog {
        version: 1,
        pages: views
            .iter()
            .map(|view| PageInfo::from_view(view, caller.role))
            .collect(),
    })
}

#[utoipa::path(get, path = "/api/plugin-pages/{plugin}/{page}", tag = "Plugin pages", params(("plugin" = String, Path), ("page" = String, Path)), responses((status = 200, body = ApiResponse<PageEntry>), (status = 404, description = "页面不可用")))]
async fn entry(
    State(state): State<AppState>,
    Extension(caller): Extension<CallerIdentity>,
    Path((plugin, page)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiReply<PageEntry> {
    let Some(view) = state
        .manager
        .plugin_host
        .plugin_web_page(&plugin, &page, caller.role)
        .await
    else {
        return ApiResponse::not_found("插件页面不存在或未启用");
    };
    let Some(session) = super::auth::extract_token_from_headers(&headers) else {
        return ApiResponse::unauthenticated("需要真实登录会话");
    };
    let ticket = match state
        .plugin_page_sessions
        .issue(session, &view, caller.role)
        .await
    {
        Ok(ticket) => ticket,
        Err(error) => return ApiResponse::internal(error),
    };
    ApiResponse::ok(PageEntry {
        version: 1,
        page: PageInfo::from_view(&view, caller.role),
        content_path: format!("/api/plugin-pages/assets/{ticket}/{}", view.page.entry),
    })
}

async fn bridge() -> Response {
    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("access-control-allow-origin", "*"),
            ("cross-origin-resource-policy", "cross-origin"),
            ("cache-control", "no-cache"),
        ],
        BRIDGE,
    )
        .into_response()
}

async fn asset(
    State(state): State<AppState>,
    Path((ticket, path)): Path<(String, String)>,
) -> Response {
    let Some(grant) = state.plugin_page_sessions.resolve(&ticket).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let caller = match super::auth::authenticated_user_checked(&state.db, &grant.session).await {
        Ok(Some(user)) => CallerIdentity::from_user(&user),
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(error) => return ApiResponse::<()>::from_err(&error).into_response(),
    };
    let Some(view) = state
        .manager
        .plugin_host
        .plugin_web_page(&grant.plugin, &grant.page, caller.role)
        .await
        .filter(|view| view.generation == grant.generation)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(asset) = view.page.assets.iter().find(|asset| asset.path == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = if asset.content_type.starts_with("text/html") {
        let Ok(html) = std::str::from_utf8(asset.data) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let script = "<script src=\"/api/plugin-pages/bridge.js\"></script>";
        let html = if html.contains("</head>") {
            html.replacen("</head>", &format!("{script}</head>"), 1)
        } else {
            format!("{script}{html}")
        };
        ([("content-type", asset.content_type), ("cache-control", "no-store"),
          ("content-security-policy", "sandbox allow-scripts allow-forms allow-downloads; default-src 'self' data: blob:; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'self'")], html).into_response()
    } else {
        (
            [
                ("content-type", asset.content_type),
                ("cache-control", "private, max-age=300"),
            ],
            asset.data,
        )
            .into_response()
    };
    let headers = response.headers_mut();
    for (name, value) in [
        ("access-control-allow-origin", "*"),
        ("cross-origin-resource-policy", "cross-origin"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

/// 判断请求路径是否为插件页面静态资源接口（仅这类只读路径通过 URL 中的页面专属 ticket 鉴权）。
pub(crate) fn is_asset_path(path: &str) -> bool {
    path.strip_prefix("/api/plugin-pages/assets/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(ticket, file)| !ticket.is_empty() && !file.is_empty())
}

#[cfg(test)]
#[path = "../../tests/unit/api/plugin_pages.rs"]
mod tests;
