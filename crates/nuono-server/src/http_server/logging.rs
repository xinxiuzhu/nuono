//! HTTP 诊断区分业务等待和请求故障，不记录查询参数或认证头。
use crate::business_http::ExpectedBusinessWait;
use axum::{
    extract::MatchedPath,
    http::{Method, Request, Response, StatusCode},
};
use std::{
    fmt,
    time::{Duration, Instant},
};
use tower_http::classify::{
    ClassifiedResponse, ClassifyResponse, MakeClassifier, NeverClassifyEos,
    ServerErrorsFailureClass,
};
use tracing::Span;

#[derive(Clone, Copy, Debug)]
pub(super) struct HttpRequests;

#[derive(Clone, Debug)]
pub(super) struct RequestClassifier {
    method: Method,
    path: String,
    started_at: Instant,
}

#[derive(Debug)]
pub(super) struct HttpFailure {
    request: RequestClassifier,
    classification: ServerErrorsFailureClass,
}

/// 优先记录路由模板；签名媒体的令牌和所有查询参数都不进入诊断日志。
fn request_path<B>(request: &Request<B>) -> &str {
    if let Some(path) = request.extensions().get::<MatchedPath>() {
        path.as_str()
    } else if request.uri().path().starts_with("/media/") {
        "/media/{token}"
    } else {
        request.uri().path()
    }
}

pub(super) fn make_span<B>(request: &Request<B>) -> Span {
    tracing::debug_span!("http_request", method = %request.method(), path = request_path(request))
}

impl MakeClassifier for HttpRequests {
    type FailureClass = HttpFailure;
    type ClassifyEos = NeverClassifyEos<HttpFailure>;
    type Classifier = RequestClassifier;

    fn make_classifier<B>(&self, request: &Request<B>) -> Self::Classifier {
        RequestClassifier {
            method: request.method().clone(),
            path: request_path(request).to_owned(),
            started_at: Instant::now(),
        }
    }
}

impl ClassifyResponse for RequestClassifier {
    type FailureClass = HttpFailure;
    type ClassifyEos = NeverClassifyEos<HttpFailure>;

    fn classify_response<B>(
        self,
        response: &Response<B>,
    ) -> ClassifiedResponse<Self::FailureClass, Self::ClassifyEos> {
        // 只有宿主在进程内显式写入 `ExpectedBusinessWait` 扩展标记的 503 响应，才视为预期的「业务未开放等待」并免于记录 ERROR 日志；
        // 单凭响应头、响应正文或状态码本身都不算数，且该标记也不能豁免其他 5xx 错误。
        if response.status() == StatusCode::SERVICE_UNAVAILABLE
            && let Some(wait) = response.extensions().get::<ExpectedBusinessWait>()
        {
            tracing::debug!(target: "nuono::http", method = %self.method, path = %self.path,
                status = response.status().as_u16(), code = wait.code(),
                latency_ms = self.started_at.elapsed().as_secs_f64() * 1000.0,
                "业务入口暂未开放");
            return ClassifiedResponse::Ready(Ok(()));
        }
        if response.status().is_server_error() {
            return ClassifiedResponse::Ready(Err(HttpFailure {
                request: self,
                classification: ServerErrorsFailureClass::StatusCode(response.status()),
            }));
        }
        tracing::debug!(target: "tower_http::trace::on_response", method = %self.method, path = %self.path,
            status = response.status().as_u16(),
            latency_ms = self.started_at.elapsed().as_secs_f64() * 1000.0,
            "HTTP 请求完成");
        ClassifiedResponse::Ready(Ok(()))
    }

    fn classify_error<E>(self, error: &E) -> Self::FailureClass
    where
        E: fmt::Display + 'static,
    {
        HttpFailure {
            request: self,
            classification: ServerErrorsFailureClass::Error(error.to_string()),
        }
    }
}

pub(super) fn log_failure(failure: HttpFailure, latency: Duration, _span: &Span) {
    tracing::error!(target: "nuono::http", method = %failure.request.method,
        path = %failure.request.path, classification = %failure.classification,
        latency_ms = latency.as_secs_f64() * 1000.0, "HTTP 请求失败");
}

#[cfg(test)]
#[path = "../../tests/unit/http_logging.rs"]
mod tests;
