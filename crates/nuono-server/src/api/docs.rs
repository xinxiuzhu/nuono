//! 在组装 HTTP 路由的同时自动收集并生成 OpenAPI 文档。
use axum::{Router, http::header, routing::get};
use utoipa::openapi::{
    Content, Info, OpenApi,
    response::ResponseBuilder,
    security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityRequirement, SecurityScheme},
};
use utoipa_swagger_ui::{Config, SwaggerUi};

use crate::middleware;

/// Multipart 图片上传表单；具体图片格式与体积限制由对应 handler 校验。
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub(super) struct ImageUpload {
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
}

/// 角色图片或参考音频上传表单，需附带最近一次保存的角色版本号（revision）用于并发校验。
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub(super) struct PersonaAssetUpload {
    #[schema(value_type = String, format = Binary)]
    file: Vec<u8>,
    revision: i64,
}

pub(super) fn configure(mut doc: OpenApi) -> OpenApi {
    doc.info = Info::new("Nuono API", env!("CARGO_PKG_VERSION"));
    doc.info.description = Some(
        "Nuono 管理 HTTP API。JSON 响应使用 success/data/error 信封。内置管理接口失败使用对应的 4xx/5xx；JSON、路径、查询和上传解析失败也返回错误信封，内部细节只写入日志。SSE 开流前失败返回 JSON 错误，开流后失败以 error 事件结束；文件下载成功保留二进制响应。登录两步验证挑战保留 HTTP 401 与 totp_required/totp_invalid 载荷。\n\n\
         认证默认启用：先在管理界面登录，再打开 /api/docs/。浏览器沿用 session_token Cookie；\
         所有受保护的写请求还必须携带与 csrf_token Cookie 一致的 X-CSRF-Token，文档页面会自动填写。\
         Bearer 会话令牌同样需要 CSRF Cookie 与请求头。接口还会按账号能力与数据归属检查授权。\n\n\
         POST /api/setup 仅在数据库无账号时开放，初始化完成后关闭。\
         本文档覆盖内置管理 HTTP API；插件动态 Web 路由、平台 Webhook 和 WebSocket 使用各自协议。"
            .into(),
    );
    let components = doc.components.get_or_insert_with(Default::default);
    components.add_security_scheme(
        "sessionCookie",
        SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("session_token"))),
    );
    components.add_security_scheme(
        "bearerSession",
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
    components.add_security_scheme(
        "csrfToken",
        SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-CSRF-Token"))),
    );
    for (path, item) in &mut doc.paths.paths {
        for (method, operation) in [
            (axum::http::Method::GET, item.get.as_mut()),
            (axum::http::Method::POST, item.post.as_mut()),
            (axum::http::Method::PUT, item.put.as_mut()),
            (axum::http::Method::PATCH, item.patch.as_mut()),
            (axum::http::Method::DELETE, item.delete.as_mut()),
        ] {
            let Some(operation) = operation else { continue };
            if let Some(id) = &mut operation.operation_id {
                id.push('_');
                id.push_str(&method.as_str().to_ascii_lowercase());
            }
            let public = middleware::is_public_api_request(&method, path);
            if public || path == "/api/setup" {
                operation.security = Some(vec![]);
            } else {
                let mut cookie = SecurityRequirement::new("sessionCookie", Vec::<String>::new());
                let mut bearer = SecurityRequirement::new("bearerSession", Vec::<String>::new());
                if method != axum::http::Method::GET {
                    cookie = cookie.add("csrfToken", Vec::<String>::new());
                    bearer = bearer.add("csrfToken", Vec::<String>::new());
                }
                operation.security = Some(vec![cookie, bearer]);
                let capability = middleware::route_capability(&method, path);
                let note = format!("启用认证时需要 {} 能力。", capability.as_str());
                operation.description = Some(match operation.description.take() {
                    Some(description) => format!("{description}\n\n{note}"),
                    None => note,
                });
            }
            for (status, description) in [
                ("400", "请求格式或字段校验失败"),
                ("401", "缺少认证令牌，或令牌已失效"),
                ("403", "能力、数据归属或 CSRF 校验拒绝请求"),
                ("429", "请求超过限流或账号尝试次数限制"),
                ("404", "资源不存在"),
                ("409", "资源状态冲突，请重新读取后重试"),
                ("405", "接口不支持此请求方法"),
                ("413", "请求或上传文件超过大小限制"),
                ("415", "请求内容类型或文件类型不受支持"),
                ("500", "内部操作失败，技术细节记录在服务日志"),
                ("502", "上游服务请求失败"),
                ("503", "服务暂时不可用"),
                ("504", "上游服务响应超时"),
            ] {
                operation
                    .responses
                    .responses
                    .entry(status.into())
                    .or_insert_with(|| {
                        ResponseBuilder::new()
                            .description(description)
                            .content(
                                "application/json",
                                Content::new(Some(
                                    <super::ApiResponse<Empty> as utoipa::PartialSchema>::schema(),
                                )),
                            )
                            .build()
                            .into()
                    });
            }
        }
    }
    doc
}

pub(super) fn router<S: Clone + Send + Sync + 'static>(doc: OpenApi) -> Router<S> {
    Router::from(
        SwaggerUi::new("/api/docs")
            .url("/api/openapi.json", doc)
            .config(Config::default().filter(true).with_credentials(true)),
    )
    .route("/api/docs/swagger-initializer.js", get(initializer))
}

async fn initializer() -> impl axum::response::IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("swagger-initializer.js"),
    )
}

// Utoipa 的 path 宏要求单元类型 `()` 和元组元素必须使用具名类型别名。
pub(super) type Empty = ();

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(super) struct AnyJson(serde_json::Value);

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(super) struct CountEntry(String, u64);

// 将 OpenAPI schema 派生留在 HTTP 层，避免侵入应用层结构体。
// 下方的穷尽结构体解构与字段类型断言可确保：一旦上游结构体字段增删或类型变更，此处投影会在编译期立即报错。
macro_rules! schema_projection {
    ($name:ident, $source:ident, { $($(#[$attr:meta])* $field:ident: $ty:ty),* $(,)? }) => {
        #[derive(serde::Serialize, utoipa::ToSchema)]
        #[allow(dead_code)]
        pub(super) struct $name { $($(#[$attr])* $field: $ty),* }
        const _: fn(&$source) = |source| {
            let $source { $($field),* } = source;
            $(let _: &$ty = $field;)*
        };
    };
}

use nuono_app::{
    adapter::types::{AdapterCapabilitiesInfo, AdapterInfo, AdapterStatus},
    agent::store::ProviderHealthRecord,
    preferences::ThemeConfig,
};

schema_projection!(ThemeSettings, ThemeConfig, {
    wallpaper_enabled: bool,
    wallpaper_url: String,
    wallpaper_blur: u32,
    wallpaper_opacity: f32,
    performance_mode: bool,
    primary_color: String,
    glass_blur: u32,
    #[schema(default)]
    extra: std::collections::HashMap<String, String>,
});

schema_projection!(AdapterInfoSchema, AdapterInfo, {
    id: String,
    adapter_type: String,
    name: String,
    #[schema(value_type = AdapterStatusSchema)]
    status: AdapterStatus,
    enabled: bool,
    platform_name: String,
    #[schema(value_type = AdapterCapabilitiesSchema)]
    capabilities: AdapterCapabilitiesInfo,
});

#[derive(utoipa::ToSchema)]
#[schema(rename_all = "lowercase")]
#[allow(dead_code)]
pub(super) enum AdapterStatusSchema {
    Stopped,
    Running,
    Error,
}
const _: fn(AdapterStatus) = |status| match status {
    AdapterStatus::Stopped | AdapterStatus::Running | AdapterStatus::Error => (),
};

schema_projection!(AdapterCapabilitiesSchema, AdapterCapabilitiesInfo, {
    send_message: bool,
    delete_message: bool,
    send_image: bool,
    send_voice: bool,
    send_video: bool,
    group_ban: bool,
});

schema_projection!(ProviderHealthSchema, ProviderHealthRecord, {
    provider: String,
    model: String,
    attempts: u64,
    successes: u64,
    failures: u64,
    timeouts: u64,
    selected: u64,
    success_rate: f64,
    last_status: Option<String>,
    last_error: Option<String>,
    last_elapsed_ms: Option<u64>,
    last_seen: Option<String>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use tower::ServiceExt;

    fn document() -> Value {
        serde_json::to_value(configure(super::super::documented_router().into_openapi())).unwrap()
    }

    #[test]
    fn complete_document_has_unique_operations_and_resolved_schema_references() {
        let doc = document();
        let mut ids = HashSet::new();
        let paths = doc["paths"].as_object().unwrap();
        for (path, item) in paths {
            for method in ["get", "post", "put", "patch", "delete"] {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                let id = operation["operationId"].as_str().unwrap();
                assert!(ids.insert(id), "duplicate operation id: {id}");
                let expected: HashSet<_> = path
                    .split('{')
                    .skip(1)
                    .map(|part| part.split('}').next().unwrap())
                    .collect();
                let actual: HashSet<_> = operation["parameters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|param| param["in"] == "path")
                    .map(|param| {
                        assert_eq!(param["required"], true, "{method} {path}");
                        param["name"].as_str().unwrap()
                    })
                    .collect();
                assert_eq!(actual, expected, "path parameter mismatch: {method} {path}");
            }
        }
        assert!(
            ids.len() >= 181,
            "expected all built-in management operations"
        );
        fn validate_refs(value: &Value, doc: &Value) {
            match value {
                Value::Object(map) => {
                    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                        let pointer = reference.strip_prefix('#').expect("local schema reference");
                        assert!(
                            doc.pointer(pointer).is_some(),
                            "unresolved reference: {reference}"
                        );
                    }
                    for value in map.values() {
                        validate_refs(value, doc);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        validate_refs(value, doc);
                    }
                }
                _ => {}
            }
        }
        validate_refs(&doc, &doc);
    }

    #[test]
    fn plugin_trigger_policy_requires_complete_bounded_fields_and_admin_csrf_security() {
        let doc = document();
        let path = &doc["paths"]["/api/plugins/{name}/trigger-policy"];
        assert!(path["get"].is_object());
        let patch = &path["patch"];
        let reference = patch["requestBody"]["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap();
        let schema = doc.pointer(reference.strip_prefix('#').unwrap()).unwrap();
        let mut required = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        required.sort_unstable();
        assert_eq!(
            required,
            [
                "group_burst",
                "group_requests_per_minute",
                "max_in_flight_per_user",
                "user_cooldown_ms"
            ]
        );
        for (field, minimum, maximum) in [
            ("user_cooldown_ms", 0, 3_600_000),
            ("max_in_flight_per_user", 0, 32),
            ("group_requests_per_minute", 0, 60_000),
            ("group_burst", 1, 1000),
        ] {
            assert_eq!(schema["properties"][field]["minimum"], minimum);
            assert_eq!(schema["properties"][field]["maximum"], maximum);
        }
        assert_eq!(
            patch["security"],
            serde_json::json!([
                {"sessionCookie": [], "csrfToken": []}, {"bearerSession": [], "csrfToken": []}
            ])
        );
        for method in ["get", "patch"] {
            assert!(
                path[method]["description"]
                    .as_str()
                    .unwrap()
                    .contains("manage_runtime")
            );
        }
    }

    #[test]
    fn plugin_diagnostics_policy_has_bounded_payload_and_management_security() {
        let doc = document();
        let operation = &doc["paths"]["/api/plugins/{name}/diagnostics"]["post"];
        let request_ref = operation["requestBody"]["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap();
        let schema = doc.pointer(request_ref.strip_prefix('#').unwrap()).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["slow_threshold_ms"]));
        assert_eq!(schema["properties"]["slow_threshold_ms"]["minimum"], 0);
        assert_eq!(
            schema["properties"]["slow_threshold_ms"]["maximum"],
            nuono_core::core::plugin_host::MAX_SLOW_PLUGIN_THRESHOLD_MS
        );
        assert_eq!(
            operation["security"],
            serde_json::json!([
                {"sessionCookie": [], "csrfToken": []},
                {"bearerSession": [], "csrfToken": []}
            ])
        );
        assert!(
            operation["description"]
                .as_str()
                .unwrap()
                .contains("manage_runtime")
        );
        assert!(operation["responses"]["200"]["content"]["application/json"]["schema"].is_object());
    }

    #[test]
    fn unused_host_hook_routes_and_schemas_are_not_published() {
        let doc = document();
        assert!(
            doc["paths"]
                .as_object()
                .unwrap()
                .keys()
                .all(|path| path != "/api/hooks" && !path.starts_with("/api/hooks/"))
        );
        assert!(
            doc["components"]["schemas"]
                .as_object()
                .unwrap()
                .keys()
                .all(|name| !name.contains("api.hooks.")
                    && !name.contains("HookInfo")
                    && !name.contains("HookStats"))
        );
    }

    #[test]
    fn schemas_describe_json_uploads_queries_and_streams() {
        let doc = document();
        let login = &doc["paths"]["/api/auth/login"]["post"];
        let reference = login["requestBody"]["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap();
        let schema = doc.pointer(reference.strip_prefix('#').unwrap()).unwrap();
        assert_eq!(schema["properties"]["password"]["writeOnly"], true);
        assert_eq!(schema["required"], json!(["username", "password"]));
        assert!(doc["paths"]["/api/upload/wallpaper"]["post"]["requestBody"]["content"]["multipart/form-data"].is_object());
        assert!(doc["paths"]["/api/ai/chat/stream"]["post"]["responses"]["200"]["content"]["text/event-stream"].is_object());
        assert!(doc["paths"]["/api/audit/event-logs/export"]["get"]["responses"]["200"]["content"]["text/csv"].is_object());
        let parameters = doc["paths"]["/api/ai/memories"]["get"]["parameters"]
            .as_array()
            .unwrap();
        assert!(parameters.iter().any(|param| param["name"] == "limit"
            && param["in"] == "query"
            && param["required"] == false));
        assert!(doc["paths"]["/api/config"]["post"].is_object());
        assert!(doc["paths"]["/api/config"]["patch"].is_object());
    }

    #[test]
    fn log_file_contract_is_read_only_and_protected() {
        let doc = document();
        for path in ["/api/logs/files", "/api/logs/files/content"] {
            let route = &doc["paths"][path];
            assert!(route["get"].is_object());
            assert!(route["post"].is_null());
            assert_eq!(
                route["get"]["security"],
                json!([{"sessionCookie": []}, {"bearerSession": []}])
            );
            assert!(
                route["get"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("view_observability")
            );
        }
        let query = doc["paths"]["/api/logs/files/content"]["get"]["parameters"]
            .as_array()
            .unwrap();
        assert!(
            query
                .iter()
                .any(|parameter| parameter["name"] == "name" && parameter["required"] == true)
        );
        for name in ["before", "limit", "q", "around", "plugin"] {
            assert!(
                query
                    .iter()
                    .any(|parameter| parameter["name"] == name && parameter["required"] == false)
            );
        }
    }

    fn resolve<'a>(schema: &'a Value, doc: &'a Value) -> &'a Value {
        if let Some(reference) = schema["$ref"].as_str() {
            doc.pointer(reference.strip_prefix('#').unwrap()).unwrap()
        } else {
            schema
        }
    }
    fn response_contains(schema: &Value, doc: &Value, property: &str, depth: usize) -> bool {
        if depth > 16 {
            return false;
        }
        let schema = resolve(schema, doc);
        if schema["properties"].get(property).is_some() {
            return true;
        }
        match schema {
            Value::Object(map) => map
                .values()
                .any(|value| response_contains(value, doc, property, depth + 1)),
            Value::Array(values) => values
                .iter()
                .any(|value| response_contains(value, doc, property, depth + 1)),
            _ => false,
        }
    }

    #[test]
    fn git_version_routes_publish_queries_schemas_and_management_security() {
        let doc = document();
        for (path, method, concrete) in [
            ("/api/store/git/{id}", "get", "/api/store/git/demo"),
            (
                "/api/store/git/by-name/{name}",
                "get",
                "/api/store/git/by-name/NuonoBridgeAdapter",
            ),
            (
                "/api/store/git/{id}/refresh",
                "post",
                "/api/store/git/demo/refresh",
            ),
            (
                "/api/store/git/{id}/checkout",
                "post",
                "/api/store/git/demo/checkout",
            ),
        ] {
            let operation = &doc["paths"][path][method];
            assert!(operation.is_object(), "missing Git route: {method} {path}");
            let http_method = if method == "get" {
                axum::http::Method::GET
            } else {
                axum::http::Method::POST
            };
            assert!(!middleware::is_public_api_request(&http_method, concrete));
            assert_eq!(
                middleware::route_capability(&http_method, concrete),
                nuono_core::auth::Capability::ManageRuntime
            );
            assert!(
                operation["description"]
                    .as_str()
                    .unwrap()
                    .contains("manage_runtime")
            );
            let security = if method == "get" {
                json!([{"sessionCookie": []}, {"bearerSession": []}])
            } else {
                json!([{"sessionCookie": [], "csrfToken": []}, {"bearerSession": [], "csrfToken": []}])
            };
            assert_eq!(
                operation["security"], security,
                "security contract: {method} {path}"
            );
            for status in ["401", "403"] {
                assert!(operation["responses"][status].is_object());
            }
            let parameters = operation["parameters"].as_array().unwrap();
            let path_parameter = if path.contains("{name}") {
                "name"
            } else {
                "id"
            };
            assert!(parameters.iter().any(|p| p["name"] == path_parameter
                && p["in"] == "path"
                && p["required"] == true));
            if method == "get" {
                for name in ["reference", "offset"] {
                    assert!(
                        parameters.iter().any(|p| p["name"] == name
                            && p["in"] == "query"
                            && p["required"] == false),
                        "missing optional {name} query for {path}"
                    );
                }
                let response =
                    &operation["responses"]["200"]["content"]["application/json"]["schema"];
                assert!(response_contains(response, &doc, "plugin_id", 0));
                assert!(response_contains(response, &doc, "allowed_commit", 0));
                assert!(response_contains(response, &doc, "next_offset", 0));
            }
        }
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let history = schemas
            .values()
            .find(|s| {
                s["properties"].get("refs").is_some() && s["properties"].get("commits").is_some()
            })
            .unwrap();
        for field in [
            "source_commit",
            "running_commit",
            "source_version",
            "running_version",
            "pending_restart",
            "dirty",
            "can_switch",
            "switch_limitation",
            "allowed_commit",
            "reference",
            "offset",
            "next_offset",
        ] {
            assert!(
                history["properties"].get(field).is_some(),
                "missing history field: {field}"
            );
        }
        let commit = resolve(&history["properties"]["commits"]["items"], &doc);
        for field in ["commit", "subject", "author", "date"] {
            assert!(commit["properties"].get(field).is_some());
        }
        let git_ref = resolve(&history["properties"]["refs"]["items"], &doc);
        for field in ["name", "kind", "commit"] {
            assert!(git_ref["properties"].get(field).is_some());
        }
        let checkout = &doc["paths"]["/api/store/git/{id}/checkout"]["post"];
        let body = resolve(
            &checkout["requestBody"]["content"]["application/json"]["schema"],
            &doc,
        );
        let required = body["required"].as_array().unwrap();
        for field in ["commit", "expected_commit"] {
            assert_eq!(body["properties"][field]["type"], "string");
            assert!(required.iter().any(|value| value == field));
        }
        let response = &checkout["responses"]["200"]["content"]["application/json"]["schema"];
        assert!(response_contains(response, &doc, "target_commit", 0));
        assert!(response_contains(
            response,
            &doc,
            "expected_source_commit",
            0
        ));
    }

    #[test]
    fn persona_library_bindings_and_assets_are_runtime_protected() {
        let doc = document();
        for (path, concrete, methods) in [
            ("/api/ai/personas", "/api/ai/personas", vec!["get", "post"]),
            (
                "/api/ai/personas/{id}",
                "/api/ai/personas/example",
                vec!["get", "put", "delete"],
            ),
            (
                "/api/ai/persona-bindings",
                "/api/ai/persona-bindings",
                vec!["get", "put", "delete"],
            ),
            (
                "/api/ai/personas/{id}/assets/{kind}",
                "/api/ai/personas/example/assets/avatar",
                vec!["get", "post", "delete"],
            ),
        ] {
            for verb in methods {
                let method =
                    axum::http::Method::from_bytes(verb.to_uppercase().as_bytes()).unwrap();
                let operation = &doc["paths"][path][verb];
                assert!(operation.is_object(), "missing {verb} {path}");
                assert!(!middleware::is_public_api_request(&method, concrete));
                assert_eq!(
                    middleware::route_capability(&method, concrete),
                    nuono_core::Capability::ManageRuntime
                );
                let expected = if verb == "get" {
                    json!([{"sessionCookie": []}, {"bearerSession": []}])
                } else {
                    json!([{"sessionCookie": [], "csrfToken": []}, {"bearerSession": [], "csrfToken": []}])
                };
                assert_eq!(operation["security"], expected);
            }
        }
        let response = &doc["paths"]["/api/ai/personas/{id}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        for field in [
            "system_prompt",
            "avatar_url",
            "portrait_url",
            "reference_audio_url",
            "reference_text",
            "language",
            "revision",
        ] {
            assert!(response_contains(response, &doc, field, 0));
        }
        for field in [
            "avatar_filename",
            "portrait_filename",
            "reference_audio_filename",
        ] {
            assert!(!response_contains(response, &doc, field, 0));
        }
    }

    #[test]
    fn agent_run_list_and_detail_are_runtime_protected_with_bounded_list_contract() {
        let doc = document();
        for (path, concrete) in [
            ("/api/ai/runs", "/api/ai/runs"),
            ("/api/ai/runs/{id}", "/api/ai/runs/example"),
            ("/api/ai/tool-approvals/{id}", "/api/ai/tool-approvals/42"),
        ] {
            let operation = &doc["paths"][path]["get"];
            assert!(operation.is_object(), "missing task endpoint {path}");
            assert_eq!(
                operation["security"],
                json!([{"sessionCookie": []}, {"bearerSession": []}])
            );
            assert!(!middleware::is_public_api_request(
                &axum::http::Method::GET,
                concrete
            ));
            let capability = middleware::route_capability(&axum::http::Method::GET, concrete);
            assert_eq!(capability, nuono_core::auth::Capability::ManageRuntime);
            assert!(!nuono_core::auth::Role::User.has(capability));
            assert!(nuono_core::auth::Role::Admin.has(capability));
            for status in ["401", "403"] {
                assert!(operation["responses"][status].is_object());
            }
        }
        let approval = &doc["paths"]["/api/ai/tool-approvals/{id}"]["get"];
        let parameters = approval["parameters"].as_array().unwrap();
        assert!(
            parameters
                .iter()
                .any(|parameter| parameter["name"] == "id" && parameter["required"] == true)
        );
        let approval_response =
            &approval["responses"]["200"]["content"]["application/json"]["schema"];
        for field in ["tool_name", "input_json", "status", "risk_level"] {
            assert!(response_contains(approval_response, &doc, field, 0));
        }
        let list = &doc["paths"]["/api/ai/runs"]["get"];
        let params = list["parameters"].as_array().unwrap();
        for field in ["status", "limit"] {
            assert!(params.iter().any(|parameter| parameter["name"] == field));
        }
        let response = &list["responses"]["200"]["content"]["application/json"]["schema"];
        for field in ["title", "updated_at", "child_count", "reply_preview"] {
            assert!(
                response_contains(response, &doc, field, 0),
                "missing list field {field}"
            );
        }
        for field in ["task", "state_json", "messages", "reply"] {
            assert!(
                !response_contains(response, &doc, field, 0),
                "list leaked {field}"
            );
        }
        let detail = &doc["paths"]["/api/ai/runs/{id}"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        for field in ["title", "task", "platform", "bot_id", "depends_on"] {
            assert!(
                response_contains(detail, &doc, field, 0),
                "missing detail field {field}"
            );
        }
    }

    #[test]
    fn security_matches_public_endpoints_and_protected_writes() {
        let doc = document();
        assert_eq!(
            doc["paths"]["/api/auth/login"]["post"]["security"],
            json!([])
        );
        assert_eq!(doc["paths"]["/api/health"]["get"]["security"], json!([]));
        assert_eq!(doc["paths"]["/api/setup"]["post"]["security"], json!([]));
        assert!(doc["paths"]["/api/setup"]["post"]["responses"]["409"].is_object());
        let read = &doc["paths"]["/api/config"]["get"]["security"];
        assert_eq!(read, &json!([{"sessionCookie": []}, {"bearerSession": []}]));
        let write = &doc["paths"]["/api/config"]["patch"]["security"];
        assert_eq!(
            write,
            &json!([{"sessionCookie": [], "csrfToken": []}, {"bearerSession": [], "csrfToken": []}])
        );
        for path in [
            "/api/docs",
            "/api/docs/",
            "/api/docs/swagger-ui-bundle.js",
            "/api/openapi.json",
        ] {
            assert!(!middleware::is_public_api_request(
                &axum::http::Method::GET,
                path
            ));
            assert_eq!(
                middleware::route_capability(&axum::http::Method::GET, path),
                nuono_core::auth::Capability::ManageRuntime
            );
        }
    }

    #[tokio::test]
    async fn swagger_assets_and_document_are_served_without_spa_fallback() {
        let router = router::<()>(configure(super::super::documented_router().into_openapi()));
        for (path, content_type, marker) in [
            ("/api/docs/", "text/html", "Swagger UI"),
            (
                "/api/docs/swagger-initializer.js",
                "application/javascript",
                "requestInterceptor",
            ),
            (
                "/api/docs/swagger-ui-bundle.js",
                "text/javascript",
                "SwaggerUIBundle",
            ),
            ("/api/openapi.json", "application/json", "Nuono API"),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with(content_type),
                "{path}"
            );
            let body = to_bytes(response.into_body(), 10 * 1024 * 1024)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&body).contains(marker), "{path}");
        }
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/docs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/api/docs/");
    }
}
