//! 声明与分发合一的插件 Web 路由器与类型安全提取器。
//!
//! 路由器内部不对外暴露 `axum` 类型以保护 `nuono-plugin` 的 SemVer 契约。
//! 插件只需通过 [`PluginWebRouter`] 声明一次路由表，即可同时导出宿主路由声明
//! ([`PluginWebRoute`]) 并完成类型安全的请求分发。

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use http::Method;
use serde::Serialize;
use serde::de::{self, DeserializeOwned, IntoDeserializer, MapAccess, Visitor};
use serde_json::json;

use crate::error::{NuonoError, Result as NuonoResult};
use crate::plugin::{
    PluginWebAccessPolicy, PluginWebRequest, PluginWebResponse, PluginWebRoute, PluginWebUser,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type PathParams = Vec<(String, String)>;
type HandlerFn<S> =
    Arc<dyn Fn(WebContext<'_>, S) -> BoxFuture<'static, PluginWebResponse> + Send + Sync>;
type ErasedDispatcher =
    Arc<dyn Fn(&PluginWebRequest) -> BoxFuture<'_, PluginWebResponse> + Send + Sync>;

/// 单次 Web 请求的只读分发上下文，包含原始请求与已提取的路径参数。
#[derive(Debug)]
pub struct WebContext<'a> {
    request: &'a PluginWebRequest,
    params: &'a [(String, String)],
}

impl<'a> WebContext<'a> {
    /// 原始请求引用。
    pub fn request(&self) -> &'a PluginWebRequest {
        self.request
    }

    /// 按名称获取路径参数（如 `/groups/:group_id` 中的 `group_id`）。
    pub fn param(&self, name: &str) -> Option<&'a str> {
        self.params
            .iter()
            .find_map(|(k, v)| (k == name).then_some(v.as_str()))
    }

    /// 全部路径参数切片。
    pub fn params(&self) -> &'a [(String, String)] {
        self.params
    }
}

/// 路由状态提取器。
#[derive(Debug, Clone)]
pub struct State<S>(pub S);

impl<S> Deref for State<S> {
    type Target = S;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 路径参数提取器。
///
/// 支持单值标量（如 `Path<String>`、`Path<i64>`）、命名结构体（`#[derive(Deserialize)]`）
/// 以及多段元组（`Path<(String, i64)>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path<T>(pub T);

impl<T> Deref for Path<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// URL 查询字符串提取器（支持 `String`、整数、布尔及 `Option<T>` 字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query<T>(pub T);

impl<T> Deref for Query<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 原始查询参数集合（轻量键值访问，无需定义结构体）。
#[derive(Debug, Clone, Default)]
pub struct RawQuery(Vec<(String, String)>);

impl RawQuery {
    /// 读取指定查询参数的第一个值。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find_map(|(k, v)| (k == key).then_some(v.as_str()))
    }

    /// 读取并解析指定查询参数。
    pub fn parse<T: FromStr>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|v| v.parse::<T>().ok())
    }

    /// 返回所有解码后的键值对。
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

/// JSON 请求体提取器 / 原始 JSON 响应封装器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 标准 Nuono API 信封响应：`{ "success": true, "data": T, "error": null }`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiJson<T>(pub T);

impl<T> Deref for ApiJson<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// HTML 响应封装器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Html(pub String);

/// 显式状态码与标准 JSON 错误信封响应。
pub fn api_error_response(status: u16, message: impl AsRef<str>) -> PluginWebResponse {
    PluginWebResponse::Http {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: json!({
            "success": false,
            "data": null,
            "error": message.as_ref(),
        })
        .to_string()
        .into_bytes(),
    }
}

/// 将 [`NuonoError`] 映射为标准 Nuono HTTP JSON 错误响应。
pub fn error_to_response(error: NuonoError) -> PluginWebResponse {
    match error {
        NuonoError::Validation(msg)
        | NuonoError::InvalidInput(msg)
        | NuonoError::ConfigValidation(msg)
        | NuonoError::Deserialization(msg) => api_error_response(400, msg),
        NuonoError::Unauthorized(msg) => api_error_response(401, msg),
        NuonoError::PermissionDenied(msg) | NuonoError::PermissionValidation(msg) => {
            api_error_response(403, msg)
        }
        NuonoError::NotFound(msg) | NuonoError::PluginNotFound(msg) => api_error_response(404, msg),
        NuonoError::Conflict(msg) => api_error_response(409, msg),
        NuonoError::Overloaded(msg) => api_error_response(429, msg),
        NuonoError::Timeout(msg) => api_error_response(504, msg),
        other => {
            tracing::error!(error = %other, "插件 Web 请求处理失败");
            api_error_response(500, "服务暂时不可用")
        }
    }
}

/// 已认证的 Dashboard 用户提取器；未登录时自动返回 401。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUser(pub PluginWebUser);

impl Deref for WebUser {
    type Target = PluginWebUser;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 可选的 Dashboard 用户提取器（适用于公开接口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalWebUser(pub Option<PluginWebUser>);

/// 原始请求头提取器。
#[derive(Debug, Clone)]
pub struct WebHeaders(pub http::HeaderMap);

impl Deref for WebHeaders {
    type Target = http::HeaderMap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 原始请求体字节提取器。
#[derive(Debug, Clone)]
pub struct WebBody(pub Vec<u8>);

/// 从请求上下文提取参数的 Trait。
pub trait FromWebRequest<S>: Sized + Send {
    fn from_web_request(
        ctx: &WebContext<'_>,
        state: &S,
    ) -> std::result::Result<Self, PluginWebResponse>;
}

impl<S: Clone + Send + Sync> FromWebRequest<S> for State<S> {
    fn from_web_request(
        _ctx: &WebContext<'_>,
        state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        Ok(Self(state.clone()))
    }
}

impl<S> FromWebRequest<S> for WebUser {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        ctx.request()
            .user
            .clone()
            .map(Self)
            .ok_or_else(|| api_error_response(401, "需要登录"))
    }
}

impl<S> FromWebRequest<S> for OptionalWebUser {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        Ok(Self(ctx.request().user.clone()))
    }
}

impl<S> FromWebRequest<S> for WebHeaders {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        Ok(Self(ctx.request().headers.clone()))
    }
}

impl<S> FromWebRequest<S> for WebBody {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        Ok(Self(ctx.request().body.clone()))
    }
}

impl<S> FromWebRequest<S> for RawQuery {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        let pairs = url::form_urlencoded::parse(ctx.request().query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Ok(Self(pairs))
    }
}

impl<S, T: DeserializeOwned + Send> FromWebRequest<S> for Json<T> {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        serde_json::from_slice(&ctx.request().body)
            .map(Self)
            .map_err(|_| api_error_response(400, "请检查必填字段和格式"))
    }
}

impl<S, T: DeserializeOwned + Send> FromWebRequest<S> for Path<T> {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        let params = ctx.params();
        // 先尝试按命名结构体反序列化（如 struct { group_id: String }）
        if let Ok(parsed) = T::deserialize(PairsDeserializer::new(params)) {
            return Ok(Self(parsed));
        }
        // 单参数支持直接提取标量（如 Path<String> / Path<i64>）
        if params.len() == 1 {
            let value = &params[0].1;
            if let Ok(parsed) = T::deserialize(ScalarStrDeserializer(value)) {
                return Ok(Self(parsed));
            }
        }
        // 多参数支持元组提取（如 Path<(String, i64)>）
        if params.len() > 1 {
            let values: Vec<&str> = params.iter().map(|(_, v)| v.as_str()).collect();
            if let Ok(parsed) = T::deserialize(SeqStrDeserializer::new(&values)) {
                return Ok(Self(parsed));
            }
        }
        Err(api_error_response(400, "路径参数无效"))
    }
}

impl<S, T: DeserializeOwned + Send> FromWebRequest<S> for Query<T> {
    fn from_web_request(
        ctx: &WebContext<'_>,
        _state: &S,
    ) -> std::result::Result<Self, PluginWebResponse> {
        let pairs: Vec<(String, String)> =
            url::form_urlencoded::parse(ctx.request().query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        T::deserialize(PairsDeserializer::new(&pairs))
            .map(Self)
            .map_err(|_| api_error_response(400, "查询参数无效"))
    }
}

/// 将处理器返回值转换为 [`PluginWebResponse`] 的 Trait。
pub trait IntoWebResponse {
    fn into_web_response(self) -> PluginWebResponse;
}

impl IntoWebResponse for PluginWebResponse {
    fn into_web_response(self) -> PluginWebResponse {
        self
    }
}

impl IntoWebResponse for Html {
    fn into_web_response(self) -> PluginWebResponse {
        PluginWebResponse::Html(self.0)
    }
}

impl IntoWebResponse for () {
    fn into_web_response(self) -> PluginWebResponse {
        PluginWebResponse::Json(json!({ "success": true, "data": null, "error": null }))
    }
}

impl<T: Serialize> IntoWebResponse for Json<T> {
    fn into_web_response(self) -> PluginWebResponse {
        match serde_json::to_value(self.0) {
            Ok(value) => PluginWebResponse::Json(value),
            Err(error) => error_to_response(NuonoError::Serialization(error.to_string())),
        }
    }
}

impl<T: Serialize> IntoWebResponse for ApiJson<T> {
    fn into_web_response(self) -> PluginWebResponse {
        match serde_json::to_value(self.0) {
            Ok(data) => PluginWebResponse::Json(json!({
                "success": true,
                "data": data,
                "error": null,
            })),
            Err(error) => error_to_response(NuonoError::Serialization(error.to_string())),
        }
    }
}

impl<T: IntoWebResponse> IntoWebResponse for NuonoResult<T> {
    fn into_web_response(self) -> PluginWebResponse {
        match self {
            Ok(response) => response.into_web_response(),
            Err(error) => error_to_response(error),
        }
    }
}

/// 类型安全 Web 处理器 Trait，自动为 0–6 个提取器参数的异步函数实现。
pub trait WebHandler<S, Args>: Clone + Send + Sync + 'static {
    fn call(&self, ctx: WebContext<'_>, state: S) -> BoxFuture<'static, PluginWebResponse>;
}

impl<S, F, Fut, Res> WebHandler<S, ()> for F
where
    S: Send + Sync + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Res> + Send + 'static,
    Res: IntoWebResponse + 'static,
{
    fn call(&self, _ctx: WebContext<'_>, _state: S) -> BoxFuture<'static, PluginWebResponse> {
        let fut = (self)();
        Box::pin(async move { fut.await.into_web_response() })
    }
}

macro_rules! impl_web_handler {
    ($($ty:ident),+) => {
        impl<S, F, Fut, Res, $($ty),+> WebHandler<S, ($($ty,)+)> for F
        where
            S: Send + Sync + 'static,
            F: Fn($($ty),+) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send + 'static,
            Res: IntoWebResponse + 'static,
            $($ty: FromWebRequest<S> + 'static,)+
        {
            #[expect(non_snake_case, reason = "宏复用泛型类型名作为提取后的局部变量名")]
            fn call(&self, ctx: WebContext<'_>, state: S) -> BoxFuture<'static, PluginWebResponse> {
                $(
                    let $ty = match $ty::from_web_request(&ctx, &state) {
                        Ok(value) => value,
                        Err(response) => return Box::pin(async move { response }),
                    };
                )+
                let fut = (self)($($ty),+);
                Box::pin(async move { fut.await.into_web_response() })
            }
        }
    };
}

impl_web_handler!(T1);
impl_web_handler!(T1, T2);
impl_web_handler!(T1, T2, T3);
impl_web_handler!(T1, T2, T3, T4);
impl_web_handler!(T1, T2, T3, T4, T5);
impl_web_handler!(T1, T2, T3, T4, T5, T6);

struct MethodBinding<S> {
    method: Method,
    policy: PluginWebAccessPolicy,
    max_body_bytes: usize,
    required_capability: Option<String>,
    forbidden_message: Option<String>,
    handler: HandlerFn<S>,
}

/// 绑定在同一路径下的 HTTP 方法路由集合。
pub struct MethodRouter<S> {
    bindings: Vec<MethodBinding<S>>,
}

impl<S> std::fmt::Debug for MethodRouter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MethodRouter")
            .field("binding_count", &self.bindings.len())
            .finish()
    }
}

impl<S: Clone + Send + Sync + 'static> MethodRouter<S> {
    fn with_method<H, Args>(method: Method, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        let handler: HandlerFn<S> = Arc::new(move |ctx, state| handler.call(ctx, state));
        Self {
            bindings: vec![MethodBinding {
                method,
                policy: PluginWebAccessPolicy::ManageRuntime,
                max_body_bytes: 2 * 1024 * 1024,
                required_capability: None,
                forbidden_message: None,
                handler,
            }],
        }
    }

    fn push_method<H, Args>(mut self, method: Method, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        let handler: HandlerFn<S> = Arc::new(move |ctx, state| handler.call(ctx, state));
        self.bindings.push(MethodBinding {
            method,
            policy: PluginWebAccessPolicy::ManageRuntime,
            max_body_bytes: 2 * 1024 * 1024,
            required_capability: None,
            forbidden_message: None,
            handler,
        });
        self
    }

    pub fn get<H, Args>(self, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        self.push_method(Method::GET, handler)
    }

    pub fn post<H, Args>(self, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        self.push_method(Method::POST, handler)
    }

    pub fn put<H, Args>(self, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        self.push_method(Method::PUT, handler)
    }

    pub fn patch<H, Args>(self, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        self.push_method(Method::PATCH, handler)
    }

    pub fn delete<H, Args>(self, handler: H) -> Self
    where
        H: WebHandler<S, Args>,
    {
        self.push_method(Method::DELETE, handler)
    }

    /// 设置最近注册方法的宿主访问策略（默认 `ManageRuntime`）。
    pub fn access(mut self, policy: PluginWebAccessPolicy) -> Self {
        if let Some(last) = self.bindings.last_mut() {
            last.policy = policy;
        }
        self
    }

    /// 设置最近注册方法的最大请求体字节数（默认 2 MiB）。
    pub fn max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        if let Some(last) = self.bindings.last_mut() {
            last.max_body_bytes = max_body_bytes;
        }
        self
    }

    /// 为最近注册方法添加插件扩展能力校验；校验不通过时返回 403。
    pub fn require_capability(mut self, capability: impl Into<String>) -> Self {
        if let Some(last) = self.bindings.last_mut() {
            last.required_capability = Some(capability.into());
        }
        self
    }

    /// 自定义能力不足时的 403 提示文案。
    pub fn forbidden_message(mut self, message: impl Into<String>) -> Self {
        if let Some(last) = self.bindings.last_mut() {
            last.forbidden_message = Some(message.into());
        }
        self
    }
}

pub fn get<S, H, Args>(handler: H) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    H: WebHandler<S, Args>,
{
    MethodRouter::with_method(Method::GET, handler)
}

pub fn post<S, H, Args>(handler: H) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    H: WebHandler<S, Args>,
{
    MethodRouter::with_method(Method::POST, handler)
}

pub fn put<S, H, Args>(handler: H) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    H: WebHandler<S, Args>,
{
    MethodRouter::with_method(Method::PUT, handler)
}

pub fn patch<S, H, Args>(handler: H) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    H: WebHandler<S, Args>,
{
    MethodRouter::with_method(Method::PATCH, handler)
}

pub fn delete<S, H, Args>(handler: H) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    H: WebHandler<S, Args>,
{
    MethodRouter::with_method(Method::DELETE, handler)
}

#[derive(Debug, Clone)]
enum SegmentPattern {
    Literal(String),
    Param(String),
}

#[derive(Debug, Clone)]
struct CompiledPattern {
    raw: String,
    segments: Vec<SegmentPattern>,
}

impl CompiledPattern {
    fn compile(path: &str) -> Self {
        let normalized = normalize_path(path);
        let segments = if normalized == "/" {
            Vec::new()
        } else {
            normalized
                .trim_matches('/')
                .split('/')
                .map(|seg| match seg.strip_prefix(':') {
                    Some(name) if !name.is_empty() => SegmentPattern::Param(name.to_owned()),
                    _ => SegmentPattern::Literal(seg.to_owned()),
                })
                .collect()
        };
        Self {
            raw: normalized,
            segments,
        }
    }

    fn matches(&self, request_path: &str) -> Option<Vec<(String, String)>> {
        let normalized = normalize_path(request_path);
        let req_segments: Vec<&str> = if normalized == "/" {
            Vec::new()
        } else {
            normalized.trim_matches('/').split('/').collect()
        };
        if req_segments.len() != self.segments.len() {
            return None;
        }
        let mut params = Vec::new();
        for (pattern, actual) in self.segments.iter().zip(req_segments.iter()) {
            match pattern {
                SegmentPattern::Literal(expected) => {
                    if expected != actual {
                        return None;
                    }
                }
                SegmentPattern::Param(name) => {
                    params.push((name.clone(), (*actual).to_owned()));
                }
            }
        }
        Some(params)
    }

    fn specificity(&self) -> usize {
        self.segments
            .iter()
            .filter(|seg| matches!(seg, SegmentPattern::Literal(_)))
            .count()
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_owned();
    }
    let inner = trimmed.trim_matches('/');
    if inner.is_empty() {
        "/".to_owned()
    } else {
        format!("/{inner}")
    }
}

fn join_paths(prefix: &str, path: &str) -> String {
    let prefix = normalize_path(prefix);
    let path = normalize_path(path);
    match (prefix.as_str(), path.as_str()) {
        ("/", p) => p.to_owned(),
        (pre, "/") => pre.to_owned(),
        (pre, p) => format!("{pre}{p}"),
    }
}

struct RegisteredRoute<S> {
    pattern: CompiledPattern,
    binding: MethodBinding<S>,
}

/// 声明与分发合一的插件 Web 路由器。
pub struct PluginWebRouter<S = ()> {
    state: S,
    entries: Vec<RegisteredRoute<S>>,
}

impl<S> std::fmt::Debug for PluginWebRouter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginWebRouter")
            .field("route_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Default for PluginWebRouter<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginWebRouter<()> {
    /// 创建无状态插件 Web 路由器。
    pub fn new() -> Self {
        Self {
            state: (),
            entries: Vec::new(),
        }
    }
}

impl<S: Clone + Send + Sync + 'static> PluginWebRouter<S> {
    /// 创建携带共享状态 `S` 的插件 Web 路由器。
    pub fn with_state(state: S) -> Self {
        Self {
            state,
            entries: Vec::new(),
        }
    }

    /// 在 `path` 上挂载一个或多个 HTTP 方法处理器。
    pub fn route(mut self, path: &str, methods: MethodRouter<S>) -> Self {
        let pattern = CompiledPattern::compile(path);
        for binding in methods.bindings {
            self.entries.push(RegisteredRoute {
                pattern: pattern.clone(),
                binding,
            });
        }
        self
    }

    /// 在 `prefix` 下挂载子路由器（如 `.nest("/admin", admin::router(state))`）。
    pub fn nest(mut self, prefix: &str, sub: PluginWebRouter<S>) -> Self {
        for entry in sub.entries {
            let joined = join_paths(prefix, &entry.pattern.raw);
            self.entries.push(RegisteredRoute {
                pattern: CompiledPattern::compile(&joined),
                binding: entry.binding,
            });
        }
        self
    }

    /// 合并另一个同状态类型的路由器。
    pub fn merge(mut self, other: PluginWebRouter<S>) -> Self {
        self.entries.extend(other.entries);
        self
    }

    /// 导出给宿主的全部 [`PluginWebRoute`] 声明。
    pub fn routes(&self) -> Vec<PluginWebRoute> {
        self.entries
            .iter()
            .map(|entry| {
                PluginWebRoute::new(
                    entry.binding.method.clone(),
                    entry.pattern.raw.clone(),
                    entry.binding.policy,
                )
                .with_max_body_bytes(entry.binding.max_body_bytes)
            })
            .collect()
    }

    /// 按路径与方法分发请求；字面量段优先于 `:param` 参数段。
    pub async fn handle(&self, req: &PluginWebRequest) -> PluginWebResponse {
        let mut best: Option<(&RegisteredRoute<S>, PathParams, usize)> = None;
        for entry in &self.entries {
            if entry.binding.method != req.method {
                continue;
            }
            if let Some(params) = entry.pattern.matches(&req.path) {
                let specificity = entry.pattern.specificity();
                if best.as_ref().is_none_or(|(_, _, prev)| specificity > *prev) {
                    best = Some((entry, params, specificity));
                }
            }
        }

        let Some((entry, params, _)) = best else {
            return PluginWebResponse::NotFound;
        };

        if let Some(cap) = entry.binding.required_capability.as_deref() {
            let Some(user) = req.user.as_ref() else {
                return api_error_response(401, "需要登录");
            };
            if !user.has_capability(cap) {
                let message = entry
                    .binding
                    .forbidden_message
                    .as_deref()
                    .unwrap_or("无权执行此操作");
                return api_error_response(403, message);
            }
        }

        let ctx = WebContext {
            request: req,
            params: &params,
        };
        (entry.binding.handler)(ctx, self.state.clone()).await
    }

    /// 擦除状态泛型 `S`，便于放入 [`crate::MountedPlugin`] 或无泛型结构体中。
    pub fn into_erased(self) -> ErasedWebRouter {
        let routes = self.routes();
        let router = Arc::new(self);
        ErasedWebRouter {
            routes,
            dispatcher: Arc::new(move |req| {
                let router = router.clone();
                Box::pin(async move { router.handle(req).await })
            }),
        }
    }
}

/// 擦除状态类型的插件 Web 路由器。
#[derive(Clone)]
pub struct ErasedWebRouter {
    routes: Vec<PluginWebRoute>,
    dispatcher: ErasedDispatcher,
}

impl std::fmt::Debug for ErasedWebRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedWebRouter")
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

impl Default for ErasedWebRouter {
    fn default() -> Self {
        PluginWebRouter::new().into_erased()
    }
}

impl ErasedWebRouter {
    /// 返回所有声明的 [`PluginWebRoute`]。
    pub fn routes(&self) -> Vec<PluginWebRoute> {
        self.routes.clone()
    }

    /// 分发单次 Web 请求。
    pub async fn handle(&self, req: &PluginWebRequest) -> PluginWebResponse {
        (self.dispatcher)(req).await
    }
}

// 轻量级字符串键值 Serde 反序列化器（用于 Path<T> 与 Query<T>）

#[derive(Debug)]
struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

impl de::Error for ParseError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

struct ScalarStrDeserializer<'a>(&'a str);

macro_rules! forward_parse_num {
    ($fn_name:ident, $visit_fn:ident, $ty:ty) => {
        fn $fn_name<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            let parsed = self
                .0
                .parse::<$ty>()
                .map_err(|e| ParseError(e.to_string()))?;
            visitor.$visit_fn(parsed)
        }
    };
}

impl<'de> de::Deserializer<'de> for ScalarStrDeserializer<'_> {
    type Error = ParseError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_str(self.0)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            "true" | "1" => visitor.visit_bool(true),
            "false" | "0" => visitor.visit_bool(false),
            _ => Err(ParseError(format!("invalid bool: {}", self.0))),
        }
    }

    forward_parse_num!(deserialize_i8, visit_i8, i8);
    forward_parse_num!(deserialize_i16, visit_i16, i16);
    forward_parse_num!(deserialize_i32, visit_i32, i32);
    forward_parse_num!(deserialize_i64, visit_i64, i64);
    forward_parse_num!(deserialize_u8, visit_u8, u8);
    forward_parse_num!(deserialize_u16, visit_u16, u16);
    forward_parse_num!(deserialize_u32, visit_u32, u32);
    forward_parse_num!(deserialize_u64, visit_u64, u64);
    forward_parse_num!(deserialize_f32, visit_f32, f32);
    forward_parse_num!(deserialize_f64, visit_f64, f64);

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct
        map struct enum identifier ignored_any
    }
}

struct PairsDeserializer<'a> {
    pairs: &'a [(String, String)],
}

impl<'a> PairsDeserializer<'a> {
    fn new(pairs: &'a [(String, String)]) -> Self {
        Self { pairs }
    }
}

impl<'de> de::Deserializer<'de> for PairsDeserializer<'_> {
    type Error = ParseError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_map(PairsMapAccess {
            iter: self.pairs.iter(),
            current_value: None,
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct
        enum identifier ignored_any
    }
}

struct PairsMapAccess<'a> {
    iter: std::slice::Iter<'a, (String, String)>,
    current_value: Option<&'a str>,
}

impl<'de> MapAccess<'de> for PairsMapAccess<'_> {
    type Error = ParseError;

    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some((key, value)) = self.iter.next() else {
            return Ok(None);
        };
        self.current_value = Some(value.as_str());
        seed.deserialize(key.as_str().into_deserializer()).map(Some)
    }

    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .current_value
            .take()
            .ok_or_else(|| ParseError("missing map value".into()))?;
        seed.deserialize(ScalarStrDeserializer(value))
    }
}

struct SeqStrDeserializer<'a> {
    values: &'a [&'a str],
}

impl<'a> SeqStrDeserializer<'a> {
    fn new(values: &'a [&'a str]) -> Self {
        Self { values }
    }
}

impl<'de> de::Deserializer<'de> for SeqStrDeserializer<'_> {
    type Error = ParseError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_seq(StrSeqAccess {
            iter: self.values.iter(),
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct
        enum identifier ignored_any
    }
}

struct StrSeqAccess<'a> {
    iter: std::slice::Iter<'a, &'a str>,
}

impl<'de> de::SeqAccess<'de> for StrSeqAccess<'_> {
    type Error = ParseError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some(value) = self.iter.next() else {
            return Ok(None);
        };
        seed.deserialize(ScalarStrDeserializer(value)).map(Some)
    }
}

/// 便捷工具：将查询参数转为 `HashMap<String, String>`（供需要遍历的调用方使用）。
pub fn parse_query_map(query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Clone)]
    struct AppState {
        prefix: String,
    }

    #[derive(Deserialize)]
    struct ListQuery {
        days: Option<i64>,
        after: Option<String>,
        active: Option<bool>,
    }

    #[derive(Deserialize)]
    struct Payload {
        amount: u32,
    }

    #[tokio::test]
    async fn router_declares_matches_extracts_and_nests() {
        async fn get_group(
            State(state): State<AppState>,
            Path(group_id): Path<i64>,
            Query(query): Query<ListQuery>,
            user: WebUser,
        ) -> NuonoResult<ApiJson<Value>> {
            Ok(ApiJson(json!({
                "prefix": state.prefix,
                "group_id": group_id,
                "days": query.days.unwrap_or(7),
                "after": query.after,
                "active": query.active,
                "user_id": user.user_id,
            })))
        }

        async fn create_payment(
            Path((group_id, op)): Path<(String, String)>,
            Json(body): Json<Payload>,
        ) -> NuonoResult<ApiJson<Value>> {
            Ok(ApiJson(json!({
                "group_id": group_id,
                "op": op,
                "amount": body.amount,
            })))
        }

        let admin = PluginWebRouter::with_state(AppState {
            prefix: "billing".into(),
        })
        .route(
            "/groups/:group_id",
            get(get_group).access(PluginWebAccessPolicy::Authenticated),
        );

        let router = PluginWebRouter::with_state(AppState {
            prefix: "billing".into(),
        })
        .route(
            "/groups/:group_id/:op",
            post(create_payment).access(PluginWebAccessPolicy::ManageRuntime),
        )
        .nest("/admin", admin)
        .into_erased();

        let routes = router.routes();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, "/groups/:group_id/:op");
        assert_eq!(routes[0].policy, PluginWebAccessPolicy::ManageRuntime);
        assert_eq!(routes[1].path, "/admin/groups/:group_id");
        assert_eq!(routes[1].policy, PluginWebAccessPolicy::Authenticated);

        // 未登录请求在提取 WebUser 时返回 401
        let unauth = PluginWebRequest::new(Method::GET, "/admin/groups/42");
        assert!(matches!(
            router.handle(&unauth).await,
            PluginWebResponse::Http { status: 401, .. }
        ));

        // 已登录 GET 请求：提取强类型路径参数与查询参数
        let req = PluginWebRequest::new(Method::GET, "/admin/groups/42")
            .with_query("days=14&after=cursor_9&active=true")
            .with_user(Some(PluginWebUser {
                user_id: 99,
                username: "alice".into(),
                role: "user".into(),
            }));
        let PluginWebResponse::Json(resp) = router.handle(&req).await else {
            panic!("expected json response");
        };
        assert_eq!(resp["success"], true);
        assert_eq!(resp["data"]["prefix"], "billing");
        assert_eq!(resp["data"]["group_id"], 42);
        assert_eq!(resp["data"]["days"], 14);
        assert_eq!(resp["data"]["after"], "cursor_9");
        assert_eq!(resp["data"]["active"], true);
        assert_eq!(resp["data"]["user_id"], 99);

        // 元组路径参数 + JSON 请求体
        let post_req = PluginWebRequest::new(Method::POST, "/groups/g1/pay")
            .with_json_body(&json!({ "amount": 500 }))
            .unwrap();
        let PluginWebResponse::Json(resp) = router.handle(&post_req).await else {
            panic!("expected json response");
        };
        assert_eq!(resp["data"]["group_id"], "g1");
        assert_eq!(resp["data"]["op"], "pay");
        assert_eq!(resp["data"]["amount"], 500);
    }

    #[tokio::test]
    async fn require_capability_checks_builtin_capabilities_and_rejects_unknown_roles() {
        let router = PluginWebRouter::new().route(
            "/admin/data",
            get(|| async { ApiJson(json!({ "ok": true })) }).require_capability("manage_runtime"),
        );

        let user_with_role = |role: &str| {
            PluginWebRequest::new(Method::GET, "/admin/data").with_user(Some(PluginWebUser {
                user_id: 1,
                username: "tester".into(),
                role: role.into(),
            }))
        };

        assert!(matches!(
            router
                .handle(&PluginWebRequest::new(Method::GET, "/admin/data"))
                .await,
            PluginWebResponse::Http { status: 401, .. }
        ));
        assert!(matches!(
            router.handle(&user_with_role("user")).await,
            PluginWebResponse::Http { status: 403, .. }
        ));
        assert!(matches!(
            router.handle(&user_with_role("super_admin")).await,
            PluginWebResponse::Http { status: 403, .. }
        ));
        assert!(matches!(
            router.handle(&user_with_role("admin")).await,
            PluginWebResponse::Json(_)
        ));
        assert!(matches!(
            router.handle(&user_with_role("owner")).await,
            PluginWebResponse::Json(_)
        ));
    }
}
