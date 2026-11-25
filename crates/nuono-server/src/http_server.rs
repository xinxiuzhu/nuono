//! HTTP 路由、中间件组合、静态资源和 Webhook 入口。
use crate::{AppState, api, middleware, source_ingress, ws};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{any, get, post},
};
use std::{sync::atomic::Ordering, time::Duration};
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{DefaultPredicate, NotForContentType, Predicate},
    },
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    sensitive_headers::{SetSensitiveRequestHeadersLayer, SetSensitiveResponseHeadersLayer},
    services::ServeDir,
    trace::TraceLayer,
};

mod logging;
mod plugin_web;
pub(crate) fn router(app_state: AppState) -> Router {
    // 构建路由。安全中间件保护 /api 管理面；/ws、插件 Web 路由和静态文件的
    // 暴露边界还必须由反向代理、防火墙和各自协议认证共同约束。
    let mut reverse = Router::new();
    for path in ["/ws", "/ws/{id}", "/onebot/v11/ws", "/onebot/v11/ws/{id}"] {
        reverse = if app_state
            .bot_instances
            .source
            .listener_token("onebot")
            .is_some()
        {
            reverse.route(path, any(ws::onebot_ws_handler))
        } else {
            reverse.route(path, any(|| async { StatusCode::NOT_FOUND }))
        };
    }
    let app = Router::new()
        .merge(reverse)
        .route("/api/plugin/{*path}", any(plugin_web::handle))
        .route("/webhook/feishu", post(source_ingress::legacy_feishu))
        .route("/webhook/qq/{instance_id}", post(source_ingress::legacy_qq))
        .route(
            "/ingress/{adapter}/{instance}",
            post(source_ingress::handle),
        )
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .merge(api::router())
        .nest_service(
            "/wallpapers",
            // 壁纸是管理员上传的原样字节（不重编码），按扩展名给 image/* 类型；
            // nosniff 让「类型正确」不依赖浏览器嗅探行为，与 SPA/头像服务对齐。
            axum::Router::new()
                .fallback_service(ServeDir::new(app_state.paths.wallpapers()))
                .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                    axum::http::header::X_CONTENT_TYPE_OPTIONS,
                    axum::http::HeaderValue::from_static("nosniff"),
                )),
        )
        .route("/ws/log-viewer", any(ws::log_ws_handler))
        .route("/metrics", get(metrics_handler))
        .route("/media/{*token}", get(media_link))
        .fallback(spa_fallback)
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            middleware::security_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            app_state.config.clone(),
            crate::http_locale::middleware,
        ))
        .with_state(app_state);

    let request_id_header = axum::http::HeaderName::from_static("x-request-id");
    app.layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(SetSensitiveResponseHeadersLayer::new(std::iter::once(
            axum::http::header::SET_COOKIE,
        )))
        .layer(SetSensitiveRequestHeadersLayer::new([
            axum::http::header::AUTHORIZATION,
            axum::http::header::COOKIE,
        ]))
        .layer(axum::middleware::from_fn(request_timeout))
        // 静态资源压缩。前端 wasm 包未压缩是 6.8MB，走内网穿透时带宽根本跑不完 ——
        // 表现是控制台白屏（wasm 请求挂住，Leptos 永远 mount 不了），而不是任何前端 bug。
        // gzip 后约 1/4 体积。
        //
        // 放在 TraceLayer 内侧：日志记的是压缩前的真实响应体积，否则排查时看到的都是压缩后的数字。
        // WebSocket 升级响应没有 body，这层会原样放过，不影响 `/ws`。
        .layer(
            CompressionLayer::new().gzip(true).br(true).compress_when(
                // 默认谓词只跳过 image/；签发的媒体常为视频/音频，压缩它们只烧 CPU
                // 且会让网关拿到无法流式写入的响应。未识别的容器走 octet-stream，
                // 同样不能压缩。
                DefaultPredicate::new()
                    .and(NotForContentType::const_new("video/"))
                    .and(NotForContentType::const_new("audio/"))
                    .and(NotForContentType::const_new("application/octet-stream")),
            ),
        )
        .layer(
            TraceLayer::new(logging::HttpRequests)
                .make_span_with(logging::make_span)
                .on_request(())
                .on_response(())
                .on_failure(logging::log_failure),
        )
}

async fn livez() -> StatusCode {
    StatusCode::OK
}

/// 下发宿主签名的临时媒体文件链接。
///
/// 路径中的签名 token 即访问凭证：除了该 token 外没有其他鉴权手段，因此该 handler 刻意挂在 `/api` 认证中间件之外。
/// 只有插件显式签发过的本地文件路径才允许读取；打开文件前会先校验 token 签名与过期时间。
async fn media_link(
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(links) = nuono_core::core::media_links::global() else {
        metrics::counter!("nuono_media_link_rejected_total", "reason" => "unsupported")
            .increment(1);
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(file) = links.verify(&token) else {
        metrics::counter!("nuono_media_link_rejected_total", "reason" => "token").increment(1);
        return StatusCode::NOT_FOUND.into_response();
    };
    // 采用流式读取下发：此端点的设计初衷就是避免把大体积音视频常驻内存；
    // 若在此处把整个文件读进 Vec，就失去了改成 URL 拉取的意义。
    // 打开路径最后一级时不跟随符号链接（O_NOFOLLOW），防止文件在签发后被替换成软链而导致仍在有效期内的 token 读到宿主机其他文件。
    let opened = match open_signed_file(&file) {
        Ok(opened) => opened,
        Err(error) => {
            // token 验签通过但底层文件已不存在或不可读；统一返回 404，使外部无法通过响应差异区分「token 无效」与「文件缺失」。
            tracing::info!(error = %error, path = %file.display(), "媒体链接读取失败");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let stream = tokio::fs::File::from_std(opened);
    let length = match stream.metadata().await {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                media_content_type(&file).to_string(),
            ),
            (axum::http::header::CONTENT_LENGTH, length.to_string()),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=300".to_string(),
            ),
            (
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                "nosniff".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", safe_download_name(&file)),
            ),
        ],
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(stream)),
    )
        .into_response()
}

fn media_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()) {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("m4a") => "audio/mp4",
        Some("mp3") => "audio/mpeg",
        Some("aac") => "audio/aac",
        Some("flac") => "audio/flac",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("flv") => "video/x-flv",
        Some("ts") => "video/mp2t",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

/// 以不跟随末端符号链接（`O_NOFOLLOW`）的方式打开已验签的媒体文件路径。
///
/// 路径在签发 token 时已完成规范化（canonicalize）；此处使用 `O_NOFOLLOW` 是为了堵住「验签与实际打开之间（TOCTOU）目标文件被替换成符号链接」的时间窗，
/// 确保仍在有效期内的 token 无法被重定向到宿主机上的其他文件。
fn open_signed_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        let file = open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(std::fs::File::from(file))
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

fn safe_download_name(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("media");
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if !state.readiness.load(Ordering::Acquire) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    if state.db.ping().await.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Prometheus 指标端点。两条放行路径：配置的 `metrics.token`，或管理员会话。
///
/// 会话这条要求管理员角色 —— 指标包含全站运行细节（各群消息量、插件调用、数据库耗时），
/// 不是普通用户该看的。该端点在 `/api` 之外，`security_middleware` 覆盖不到。
async fn metrics_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let cfg = state.config.get().await;
    if !cfg.metrics.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }

    let token = crate::api::auth::extract_token_from_headers(&headers);
    let metrics_token_valid = match (&token, cfg.metrics.token.as_deref()) {
        (Some(provided), Some(expected)) if !expected.is_empty() => {
            nuono_core::auth::constant_time_eq(provided.as_bytes(), expected.as_bytes())
        }
        _ => false,
    };
    let session_valid = match token.as_deref() {
        Some(token) => crate::api::auth::authenticated_with(
            &state.db,
            token,
            nuono_core::auth::Capability::ViewObservability,
        )
        .await
        .is_some(),
        None => false,
    };

    if !metrics_token_valid && !session_valid {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    state.prometheus.render().into_response()
}

async fn request_timeout(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path();
    let management_api =
        (path == "/api" || path.starts_with("/api/")) && !path.starts_with("/api/plugin/");
    match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) if management_api => api::ApiReply::<()>::error(
            StatusCode::GATEWAY_TIMEOUT,
            "请求处理超时，请先检查操作状态再重试",
        )
        .into_response(),
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

/// SPA fallback：先尝试提供静态文件，找不到则返回 index.html
async fn spa_fallback(State(state): State<AppState>, uri: axum::http::Uri) -> impl IntoResponse {
    if uri.path() == "/api" || uri.path().starts_with("/api/") {
        return api::not_found().await.into_response();
    }
    let static_dir = crate::release::static_dir()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| state.paths.web_dist.clone());
    serve_spa(&static_dir, uri).await
}

async fn serve_spa(
    static_root: &std::path::Path,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let path = uri.path().trim_start_matches('/');

    // 安全检查：防止目录遍历
    if path.contains("..") || path.starts_with('/') {
        return StatusCode::NOT_FOUND.into_response();
    }

    let file_path = static_root.join(path);

    // 尝试直接提供文件（简化逻辑，避免 canonicalize 的潜在问题）
    if !path.is_empty()
        && tokio::fs::metadata(&file_path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        && let Ok(content) = tokio::fs::read(&file_path).await
    {
        let ct = match file_path.extension().and_then(|e| e.to_str()) {
            Some("js") | Some("mjs") => "application/javascript",
            Some("css") => "text/css",
            Some("svg") => "image/svg+xml",
            Some("html") => "text/html",
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("ico") => "image/x-icon",
            Some("json") => "application/json",
            Some("woff2") => "font/woff2",
            Some("woff") => "font/woff",
            Some("ttf") => "font/ttf",
            // 必须显式给 `application/wasm`：漏掉会落到下面的 octet-stream，
            // 浏览器就走不了 `compileStreaming`，改成先整包下载再编译。
            Some("wasm") => "application/wasm",
            _ => "application/octet-stream",
        };
        let mut response = ([("content-type", ct)], content).into_response();
        response.headers_mut().insert(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
        // wasm-bindgen 生成的 snippets 路径仅包含 crate 名而不含内容哈希；
        // 必须要求浏览器每次重新校验，避免缓存中的旧 JS 片段无法通过新版 Trunk 构建的 SRI 完整性校验。
        if path.starts_with("snippets/") {
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
            );
        }
        return response;
    }

    // 否则返回 index.html（SPA 路由交由前端处理）
    let index = static_root.join("index.html");
    match tokio::fs::read_to_string(&index).await {
        Ok(html) => serve_index_with_csp_nonce(html).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 给 index.html 注入一次性 CSP nonce，并返回带 CSP 头的响应。
///
/// index.html 携带内联脚本（Trunk 引导 + 主题防闪白），其中一个的内容随前端构建
/// 变化，静态 `'sha256-…'` 例外会在每次更新后失效；按请求生成 nonce 并改写
/// `<script` 标签可以让 CSP 在任意构建产物上稳定生效。被注入的内联脚本没有
/// nonce，仍然被 `script-src` 拦下——CSP 因此成为 XSS 之外的第二层缓解。
fn serve_index_with_csp_nonce(html: String) -> impl IntoResponse {
    use rand::TryRng;
    let mut bytes = [0u8; 16];
    let nonce = match rand::rngs::SysRng.try_fill_bytes(&mut bytes) {
        Ok(()) => hex::encode(bytes),
        Err(e) => {
            tracing::warn!(error = %e, "生成 CSP nonce 失败，本次响应不放行内联脚本");
            String::new()
        }
    };

    let (html, script_src) = if nonce.is_empty() {
        // 无 nonce 时只放行外部脚本：内联脚本被禁，页面降级但不引入漏洞。
        (html, "'self' 'wasm-unsafe-eval'".to_string())
    } else {
        (
            html.replace("<script", &format!("<script nonce=\"{nonce}\"")),
            format!("'self' 'nonce-{nonce}' 'wasm-unsafe-eval'"),
        )
    };
    // `wasm-unsafe-eval` 供 WASM 实例化；样式放开内联是 Leptos 的行内样式所需；
    // 图片/媒体允许任意源（消息里会出现第三方平台的图）。
    let csp = format!(
        "default-src 'self'; script-src {script_src}; style-src 'self' 'unsafe-inline'; \
         img-src * data: blob:; media-src * blob:; connect-src 'self' ws: wss:; \
         font-src 'self' data:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
    );
    (
        [
            (axum::http::header::CONTENT_SECURITY_POLICY, csp),
            (
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                "nosniff".to_string(),
            ),
        ],
        Html(html),
    )
}

#[cfg(test)]
mod spa_fallback_tests {
    use super::serve_spa;
    use axum::http::{StatusCode, header::CONTENT_TYPE};
    use axum::response::IntoResponse;

    struct TestDirectory(std::path::PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn nested_mjs_is_served_as_javascript() {
        const SCRIPT: &str = "export const snippet = 'regression';\n";
        const PATH: &str = "/snippets/test-package-123/inline0.mjs";

        let directory = TestDirectory(
            std::env::temp_dir().join(format!("nuono-spa-mjs-test-{}", uuid::Uuid::new_v4())),
        );
        let snippet = directory.0.join(PATH.trim_start_matches('/'));
        std::fs::create_dir_all(snippet.parent().unwrap()).unwrap();
        std::fs::write(&snippet, SCRIPT).unwrap();
        std::fs::write(directory.0.join("index.html"), "<html>SPA fallback</html>").unwrap();

        let response = serve_spa(&directory.0, PATH.parse().unwrap())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/javascript");
        assert_eq!(
            response.headers()[axum::http::header::CACHE_CONTROL],
            "no-cache, must-revalidate"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), SCRIPT.as_bytes());
    }
}

#[cfg(test)]
mod csp_nonce_tests {
    use super::serve_index_with_csp_nonce;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn index_html_gets_per_request_nonce_in_scripts_and_csp() {
        let html = "<html><head><script type=\"module\">init()</script><script>theme()</script></head></html>".to_string();
        let res = serve_index_with_csp_nonce(html).into_response();

        let csp = res
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .expect("CSP 头必须存在")
            .to_string();
        assert!(csp.contains("'nonce-"), "CSP 应携带 nonce 例外");
        assert!(csp.contains("'wasm-unsafe-eval'"));

        // 从 CSP 里取出 nonce，两个内联 script 标签都必须带上同一个值。
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("nonce 值可解析")
            .to_string();
        assert!(!nonce.is_empty());

        let (parts, body) = res.into_parts();
        let body = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(
            body.matches(&format!("nonce=\"{nonce}\"")).count(),
            2,
            "两个内联脚本都要带 nonce"
        );
        assert!(
            !body.contains("<script>") && !body.contains("<script type="),
            "不允许残留未带 nonce 的 script 开标签: {}",
            parts.status
        );
    }
}

#[cfg(test)]
mod timeout_contract_tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test(start_paused = true)]
    async fn management_timeout_uses_json_504_without_changing_other_protocols() {
        let app = Router::new()
            .fallback(|| async { std::future::pending::<StatusCode>().await })
            .layer(axum::middleware::from_fn(request_timeout));
        for (path, expected) in [
            ("/api/tasks", StatusCode::GATEWAY_TIMEOUT),
            ("/webhook/feishu", StatusCode::REQUEST_TIMEOUT),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if path.starts_with("/api/") {
                let bytes = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["success"], false);
            }
        }
    }
}

#[cfg(test)]
mod media_link_tests {
    use super::media_link;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use tower::ServiceExt;

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, content: &[u8]) -> Self {
            let path = std::env::temp_dir()
                .join(format!("nuono-http-media-{}-{name}", uuid::Uuid::new_v4()));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn router() -> axum::Router {
        axum::Router::new().route("/media/{*token}", axum::routing::get(media_link))
    }

    async fn get(uri: String) -> axum::response::Response {
        router()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn signed_token_streams_the_file_with_media_content_type() {
        let links = nuono_core::core::media_links::init_global("http://127.0.0.1:9".into());
        let file = TempFile::new("clip.mp4", b"ftypmp42-payload");
        let link = links
            .issue(&file.0, std::time::Duration::from_secs(60))
            .unwrap();

        let response = get(link.path).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "video/mp4"
        );
        assert_eq!(response.headers()[axum::http::header::CONTENT_LENGTH], "16");
        assert_eq!(
            response.headers()[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"ftypmp42-payload");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_swapped_in_after_signing_is_not_followed() {
        let links = nuono_core::core::media_links::init_global("http://127.0.0.1:9".into());
        let file = TempFile::new("clip.mp4", b"original");
        let link = links
            .issue(&file.0, std::time::Duration::from_secs(60))
            .unwrap();
        let decoy = TempFile::new("decoy.mp4", b"secret");
        std::fs::remove_file(&file.0).unwrap();
        std::os::unix::fs::symlink(&decoy.0, &file.0).unwrap();
        let response = get(link.path).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a link swapped in after signing must not redirect the token"
        );
    }

    #[tokio::test]
    async fn unsigned_and_tampered_tokens_are_indistinguishable_404s() {
        let links = nuono_core::core::media_links::init_global("http://127.0.0.1:9".into());
        let file = TempFile::new("clip.mp4", b"payload");
        let link = links
            .issue(&file.0, std::time::Duration::from_secs(60))
            .unwrap();
        let mut tampered = link.path.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });

        for uri in ["/media/garbage".to_string(), tampered] {
            let response = get(uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn a_valid_token_for_a_removed_file_is_also_404() {
        let links = nuono_core::core::media_links::init_global("http://127.0.0.1:9".into());
        let file = TempFile::new("clip.mp4", b"payload");
        let link = links
            .issue(&file.0, std::time::Duration::from_secs(60))
            .unwrap();
        std::fs::remove_file(&file.0).unwrap();
        let response = get(link.path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn content_types_cover_the_media_the_plugin_produces() {
        use super::media_content_type;
        for (name, expected) in [
            ("a.mp4", "video/mp4"),
            ("a.m4a", "audio/mp4"),
            ("a.mp3", "audio/mpeg"),
            ("a.flac", "audio/flac"),
            ("a.aac", "audio/aac"),
            ("a.mkv", "video/x-matroska"),
            ("a.webm", "video/webm"),
            ("a.png", "image/png"),
            ("a.bin", "application/octet-stream"),
        ] {
            assert_eq!(media_content_type(std::path::Path::new(name)), expected);
        }
    }

    #[test]
    fn download_names_never_carry_path_separators() {
        use super::safe_download_name;
        let name = safe_download_name(std::path::Path::new("/tmp/a b/..evil\"name.mp4"));
        assert!(!name.contains(['/', '\\', '"']));
        assert!(name.ends_with(".mp4"));
    }
}
