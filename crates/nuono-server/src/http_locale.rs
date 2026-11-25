//! HTTP 请求级语言协商中间件（仅作用于当前请求上下文，绝不改写聊天语言偏好或持久化系统设置）。
use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, header::ACCEPT_LANGUAGE},
    middleware::Next,
    response::Response,
};
use nuono_app::localization::{Locale, LocaleContext, LocaleSource};
use nuono_core::config::ConfigManager;
use std::sync::Arc;

pub(crate) async fn middleware(
    State(config): State<Arc<ConfigManager>>,
    mut request: Request,
    next: Next,
) -> Response {
    let default = config.snapshot().await.localization.default_locale;
    let context = negotiate(request.headers(), default);
    request.extensions_mut().insert(context);
    let mut response = next.run(request).await;
    // 使用 `x-nuono-locale` 响应头标识本次请求协商出的语言上下文；
    // 不直接设置标准 `Content-Language` 头，因为目前并非所有 API 错误文案都已完成多语言翻译。
    response.headers_mut().insert(
        "x-nuono-locale",
        HeaderValue::from_static(context.locale.as_str()),
    );
    response
}

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !matches!(whole, "0" | "1")
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (whole == "1" && fraction.bytes().any(|byte| byte != b'0'))
    {
        return None;
    }
    if whole == "1" {
        Some(1000)
    } else {
        Some(fraction.parse::<u16>().unwrap_or(0) * 10u16.pow(3 - fraction.len() as u32))
    }
}

fn negotiate(headers: &HeaderMap, default: Locale) -> LocaleContext {
    let mut ranges = Vec::new();
    for header in headers.get_all(ACCEPT_LANGUAGE) {
        let Ok(value) = header.to_str() else { continue };
        for entry in value.split(',') {
            let mut parts = entry.trim().split(';');
            let tag = parts.next().unwrap_or_default().trim();
            let mut weight = Some(1000);
            if let Some(parameter) = parts.next() {
                weight = parameter.trim().strip_prefix("q=").and_then(quality);
            }
            if parts.next().is_some() {
                continue;
            }
            let Some(weight) = weight else { continue };
            if tag == "*" {
                ranges.push((None, weight));
            } else if let Some(locale) = Locale::from_language_tag(tag) {
                ranges.push((Some(locale), weight));
            }
        }
    }
    let explicit_locales: std::collections::HashSet<Locale> =
        ranges.iter().filter_map(|(locale, _)| *locale).collect();
    let mut best = None;
    for (locale, weight) in &ranges {
        if *weight == 0 {
            continue;
        }
        let locale = match locale {
            Some(locale) => *locale,
            None => {
                // 显式指定的语言标签（包含 `q=0` 拒绝项）优先使用其自身权重；
                // 通配符 `*` 仅匹配未在请求头中显式出现的语言。
                let allowed = |locale| !explicit_locales.contains(&locale);
                if allowed(default) {
                    default
                } else if let Some(locale) = Locale::ALL.into_iter().find(|locale| allowed(*locale))
                {
                    locale
                } else {
                    continue;
                }
            }
        };
        // 权重相同时保留客户端请求头中的先后顺序。
        if best.is_none_or(|(_, previous)| *weight > previous) {
            best = Some((locale, *weight));
        }
    }
    match best {
        Some((locale, _)) => LocaleContext {
            locale,
            source: LocaleSource::Explicit,
        },
        None => LocaleContext {
            locale: default,
            source: LocaleSource::System,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Extension, Router, body::Body, http::StatusCode, routing::get};
    use tower::ServiceExt;

    fn selected(value: &str, default: Locale) -> LocaleContext {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_LANGUAGE, value.parse().unwrap());
        negotiate(&headers, default)
    }

    #[test]
    fn negotiates_weights_regions_and_safe_fallback() {
        assert_eq!(
            selected("en-US;q=0.7, ja-JP;q=0.9, ko;q=0", Locale::ZhCn).locale,
            Locale::Ja
        );
        assert_eq!(selected("ko-KR,ja;q=1", Locale::ZhCn).locale, Locale::Ko);
        assert_eq!(selected("zh-Hans-CN", Locale::En).locale, Locale::ZhCn);
        for invalid in [
            "",
            "fr-FR",
            "en;q=NaN",
            "ja;q=1.1",
            "ko;q=-1",
            "ja;q=0",
            "ko;q=0.1234",
            "en;q=0.7;bad=1",
        ] {
            assert_eq!(
                selected(invalid, Locale::ZhCn),
                LocaleContext::default(),
                "{invalid}"
            );
        }
        assert_eq!(selected("*", Locale::Ja).locale, Locale::Ja);
        assert_eq!(
            selected("zh-CN;q=0,*;q=0.5", Locale::ZhCn).locale,
            Locale::En
        );
        assert_eq!(
            selected("en;q=0.2,*;q=0.8", Locale::En).locale,
            Locale::ZhCn
        );
        let repeated = format!("zh-CN;q=0,{}", vec!["*"; 2_000].join(","));
        assert_eq!(selected(&repeated, Locale::ZhCn).locale, Locale::En);
        assert_eq!(selected("fr-FR", Locale::Ko).source, LocaleSource::System);
        let mut headers = HeaderMap::new();
        headers.append(ACCEPT_LANGUAGE, "en;q=0.5".parse().unwrap());
        headers.append(ACCEPT_LANGUAGE, "ja".parse().unwrap());
        assert_eq!(negotiate(&headers, Locale::ZhCn).locale, Locale::Ja);
    }

    #[tokio::test]
    async fn contexts_are_isolated_and_available_before_an_unauthorized_response() {
        let directory = tempfile::tempdir().unwrap();
        let config = Arc::new(
            ConfigManager::load(Some(directory.path().join("nuono.toml")))
                .await
                .unwrap(),
        );
        let app = Router::new()
            .route(
                "/",
                get(|Extension(context): Extension<LocaleContext>| async move {
                    (StatusCode::UNAUTHORIZED, context.locale.as_str())
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                config.clone(),
                middleware,
            ));
        for locale in ["ja", "en", "ko", "zh-CN"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(ACCEPT_LANGUAGE, locale)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()["x-nuono-locale"], locale);
            assert!(response.headers().get("content-language").is_none());
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 32)
                    .await
                    .unwrap(),
                locale
            );
        }
        assert_eq!(
            config.snapshot().await.localization.default_locale,
            Locale::ZhCn
        );
    }
}
