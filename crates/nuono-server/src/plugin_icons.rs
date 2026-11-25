//! 编译期嵌入到应用二进制中的插件图标注册表。
//!
//! 注册表直接保存图标字节而非源码路径；管理端 HTTP 请求读取图标时绝不访问插件源码目录，
//! 也不要求已禁用的插件启动 Web 路由处理器。
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{LazyLock, RwLock},
};

use nuono_tooling::source_build::{ICON_FILES, MAX_ICON_BYTES};

#[derive(Clone)]
pub(crate) struct PluginIcon {
    content_type: &'static str,
    bytes: &'static [u8],
    hash: String,
}

impl PluginIcon {
    fn new(content_type: &'static str, bytes: &'static [u8]) -> Option<Self> {
        if bytes.is_empty()
            || bytes.len() > MAX_ICON_BYTES
            || !ICON_FILES.iter().any(|(_, mime)| *mime == content_type)
        {
            return None;
        }
        Some(Self {
            content_type,
            bytes,
            hash: hex::encode(Sha256::digest(bytes)),
        })
    }

    fn url(&self, name: &str) -> String {
        format!(
            "/api/plugins/{}/icon?v={}",
            urlencoding::encode(name),
            self.hash
        )
    }

    pub(crate) fn response(&self, request: &HeaderMap) -> Response {
        let etag = format!("\"{}\"", self.hash);
        // GET 对 If-None-Match 使用弱比较；接受列表和通配符。
        let unchanged = request.get_all(header::IF_NONE_MATCH).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value.split(',').any(|candidate| {
                    let candidate = candidate.trim();
                    candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
                })
            })
        });
        let mut response = Response::new(if unchanged {
            Body::empty()
        } else {
            Body::from(self.bytes)
        });
        if unchanged {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
        }
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(self.content_type),
        );
        headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if self.content_type == "image/svg+xml" {
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
            );
        }
        response
    }
}

static ICONS: LazyLock<RwLock<HashMap<String, PluginIcon>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) fn register(name: &str, content_type: &'static str, bytes: &'static [u8]) {
    if let Some(icon) = PluginIcon::new(content_type, bytes) {
        ICONS
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name.to_string(), icon);
    }
}

pub(crate) fn get(name: &str) -> Option<PluginIcon> {
    ICONS
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .get(name)
        .cloned()
}

pub(crate) fn url(name: &str) -> Option<String> {
    get(name).map(|icon| icon.url(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn plugin_icon_http_headers_and_conditional_requests() {
        let icon = PluginIcon::new(
            "image/svg+xml",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        )
        .unwrap();
        let response = icon.response(&HeaderMap::new());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=3600"
        );
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "sandbox; default-src 'none'; style-src 'unsafe-inline'"
        );
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            to_bytes(response.into_body(), MAX_ICON_BYTES)
                .await
                .unwrap()
                .as_ref(),
            icon.bytes
        );

        for matching in [etag.clone(), format!("\"other\", W/{etag}"), "*".into()] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::IF_NONE_MATCH,
                HeaderValue::from_str(&matching).unwrap(),
            );
            let response = icon.response(&headers);
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()[header::ETAG], etag.as_str());
            assert!(
                to_bytes(response.into_body(), MAX_ICON_BYTES)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        assert_eq!(icon.response(&headers).status(), StatusCode::OK);
    }

    #[test]
    fn plugin_icon_registry_uses_runtime_names_and_versioned_urls() {
        let name = "icon-test 示例/DM";
        assert!(get(name).is_none());
        register(name, "image/png", b"icon");
        let address = url(name).unwrap();
        assert!(address.starts_with("/api/plugins/icon-test%20%E7%A4%BA%E4%BE%8B%2FDM/icon?v="));
        let first = address;
        register(name, "image/png", b"changed-icon");
        assert_ne!(url(name).unwrap(), first);
        assert!(get("../icon-test").is_none());
        assert!(PluginIcon::new("text/html", b"<script/>").is_none());
    }
}
