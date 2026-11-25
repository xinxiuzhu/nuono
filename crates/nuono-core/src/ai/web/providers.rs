//! 官方协议参考，检查于2026-10-14：
//! <https://docs.tavily.com/documentation/api-reference/endpoint/search>
//! <https://docs.tavily.com/documentation/api-reference/endpoint/extract>
//! <https://exa.ai/docs/reference/search>
//! <https://exa.ai/docs/reference/get-contents>
//! <https://docs.firecrawl.dev/api-reference/endpoint/search>
//! <https://docs.firecrawl.dev/api-reference/endpoint/scrape>
//! <https://github.com/jina-ai/reader#usage>

use super::{WebError, WebErrorCode, WebProvider, WebResult, WebSettings, WebSource};
use reqwest::{
    Method,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use url::Url;

pub(super) struct ProviderRequest {
    pub url: Url,
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Option<Value>,
}

fn request(
    source: &WebSource,
    endpoint: &str,
    method: Method,
    body: Option<Value>,
) -> Result<ProviderRequest, WebError> {
    let key = source
        .api_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| WebError::new(WebErrorCode::NoSource))?;
    let auth = if source.provider == WebProvider::Exa {
        key.to_owned()
    } else {
        format!("Bearer {key}")
    };
    let mut value =
        HeaderValue::from_str(&auth).map_err(|_| WebError::new(WebErrorCode::InvalidSettings))?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    if source.provider == WebProvider::Exa {
        headers.insert("x-api-key", value);
    } else {
        headers.insert(AUTHORIZATION, value);
    }
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if body.is_some() {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    Ok(ProviderRequest {
        url: Url::parse(endpoint).map_err(|_| WebError::new(WebErrorCode::InvalidSettings))?,
        method,
        headers,
        body,
    })
}

pub(super) fn search_request(
    source: &WebSource,
    query: &str,
    limit: usize,
    settings: &WebSettings,
) -> Result<ProviderRequest, WebError> {
    match source.provider {
        WebProvider::Tavily => request(
            source,
            "https://api.tavily.com/search",
            Method::POST,
            Some(
                json!({"query":query,"max_results":limit,"search_depth":settings.search_depth,
                "include_answer":false,"include_raw_content":false,"include_images":false,"auto_parameters":false}),
            ),
        ),
        WebProvider::Exa => request(
            source,
            "https://api.exa.ai/search",
            Method::POST,
            Some(json!({"query":query,"numResults":limit,"contents":{"text":true}})),
        ),
        WebProvider::Firecrawl => {
            if query.chars().count() > 500 {
                return Err(WebError::new(WebErrorCode::InvalidInput));
            }
            request(
                source,
                "https://api.firecrawl.dev/v2/search",
                Method::POST,
                Some(
                    json!({"query":query,"limit":limit,"sources":["web"],"timeout":settings.timeout_secs * 1000}),
                ),
            )
        }
        WebProvider::Jina => {
            let mut wire = request(source, "https://s.jina.ai/", Method::GET, None)?;
            wire.url
                .path_segments_mut()
                .map_err(|_| WebError::new(WebErrorCode::InvalidSettings))?
                .push(query);
            wire.headers
                .insert("x-respond-with", HeaderValue::from_static("markdown"));
            Ok(wire)
        }
    }
}

pub(super) fn fetch_request(
    source: &WebSource,
    url: &Url,
    settings: &WebSettings,
) -> Result<ProviderRequest, WebError> {
    match source.provider {
        WebProvider::Tavily => request(
            source,
            "https://api.tavily.com/extract",
            Method::POST,
            Some(
                json!({"urls":[url.as_str()],"extract_depth":"basic","format":"markdown","include_images":false}),
            ),
        ),
        WebProvider::Exa => request(
            source,
            "https://api.exa.ai/contents",
            Method::POST,
            Some(json!({"urls":[url.as_str()],"text":true})),
        ),
        WebProvider::Firecrawl => request(
            source,
            "https://api.firecrawl.dev/v2/scrape",
            Method::POST,
            Some(
                json!({"url":url.as_str(),"formats":["markdown"],"onlyMainContent":true,"timeout":settings.timeout_secs * 1000}),
            ),
        ),
        WebProvider::Jina => request(
            source,
            "https://r.jina.ai/",
            Method::POST,
            Some(json!({"url":url.as_str()})),
        ),
    }
}

fn first_string(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default()
        .to_owned()
}

fn exa_text(value: &Value) -> String {
    let text = first_string(value, &["text", "summary"]);
    if !text.is_empty() {
        return text;
    }
    value
        .get("highlights")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn array_at<'a>(value: &'a Value, path: &str) -> Result<&'a [Value], WebError> {
    value
        .pointer(path)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| WebError::new(WebErrorCode::InvalidResponse))
}

pub(super) fn search_results(
    provider: WebProvider,
    value: &Value,
) -> Result<Vec<WebResult>, WebError> {
    let rows = match provider {
        WebProvider::Tavily | WebProvider::Exa => array_at(value, "/results")?,
        WebProvider::Firecrawl => {
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                return Err(WebError::new(WebErrorCode::InvalidResponse));
            }
            array_at(value, "/data/web")?
        }
        WebProvider::Jina => {
            if value
                .get("code")
                .and_then(Value::as_u64)
                .is_some_and(|code| code != 200)
            {
                return Err(WebError::new(WebErrorCode::InvalidResponse));
            }
            array_at(value, "/data")?
        }
    };
    rows.iter()
        .map(|row| {
            let url = row
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| WebError::new(WebErrorCode::InvalidResponse))?;
            let content = match provider {
                WebProvider::Tavily => first_string(row, &["content", "raw_content"]),
                WebProvider::Exa => exa_text(row),
                WebProvider::Firecrawl => {
                    first_string(row, &["description", "markdown", "snippet"])
                }
                WebProvider::Jina => first_string(row, &["content", "description"]),
            };
            Ok(WebResult {
                title: first_string(row, &["title"]),
                url: url.into(),
                content,
            })
        })
        .collect()
}

pub(super) fn fetch_result(
    provider: WebProvider,
    value: &Value,
    requested: &Url,
) -> Result<WebResult, WebError> {
    let row = match provider {
        WebProvider::Tavily | WebProvider::Exa => array_at(value, "/results")?
            .first()
            .ok_or_else(|| WebError::new(WebErrorCode::InvalidResponse))?,
        WebProvider::Firecrawl => {
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                return Err(WebError::new(WebErrorCode::InvalidResponse));
            }
            value
                .get("data")
                .filter(|data| data.is_object())
                .ok_or_else(|| WebError::new(WebErrorCode::InvalidResponse))?
        }
        WebProvider::Jina => {
            if value
                .get("code")
                .and_then(Value::as_u64)
                .is_some_and(|code| code != 200)
            {
                return Err(WebError::new(WebErrorCode::InvalidResponse));
            }
            value
                .get("data")
                .filter(|data| data.is_object())
                .ok_or_else(|| WebError::new(WebErrorCode::InvalidResponse))?
        }
    };
    let content = match provider {
        WebProvider::Tavily => first_string(row, &["raw_content", "content"]),
        WebProvider::Exa => exa_text(row),
        WebProvider::Firecrawl => first_string(row, &["markdown"]),
        WebProvider::Jina => first_string(row, &["content"]),
    };
    let (title, url) = if provider == WebProvider::Firecrawl {
        let metadata = &row["metadata"];
        (
            first_string(metadata, &["title"]),
            first_string(metadata, &["sourceURL", "url"]),
        )
    } else {
        (first_string(row, &["title"]), first_string(row, &["url"]))
    };
    if content.trim().is_empty() {
        return Err(WebError::new(WebErrorCode::InvalidResponse));
    }
    Ok(WebResult {
        title,
        url: if url.is_empty() {
            requested.as_str().into()
        } else {
            url
        },
        content,
    })
}
