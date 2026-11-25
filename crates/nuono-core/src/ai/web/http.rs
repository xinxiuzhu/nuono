use super::{WebError, WebErrorCode, WebSettings, WebTransport, providers::ProviderRequest};
use futures::StreamExt;
use reqwest::{
    Client, Method, StatusCode,
    header::{LOCATION, RANGE},
};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    time::Duration,
};
use url::{Host, Url};

pub(super) struct SafeHttp;

pub(super) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && ((b == 18 || b == 19) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
                && segments[0] != 0x2002
                && !(segments[0] == 0x3fff && segments[1] < 0x1000)
        }
    }
}

pub(super) fn public_url(raw: &str) -> Result<Url, WebError> {
    if raw.len() > 4096 || raw.chars().any(char::is_control) {
        return Err(WebError::new(WebErrorCode::UnsafeUrl));
    }
    let mut url = Url::parse(raw).map_err(|_| WebError::new(WebErrorCode::UnsafeUrl))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.port_or_known_default(), Some(80 | 443))
    {
        return Err(WebError::new(WebErrorCode::UnsafeUrl));
    }
    match url
        .host()
        .ok_or_else(|| WebError::new(WebErrorCode::UnsafeUrl))?
    {
        Host::Ipv4(ip) if public_ip(IpAddr::V4(ip)) => {}
        Host::Ipv6(ip) if public_ip(IpAddr::V6(ip)) => {}
        Host::Domain(host) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if !host.contains('.')
                || [
                    "localhost",
                    "local",
                    "internal",
                    "lan",
                    "home",
                    "test",
                    "invalid",
                ]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
            {
                return Err(WebError::new(WebErrorCode::UnsafeUrl));
            }
        }
        _ => return Err(WebError::new(WebErrorCode::UnsafeUrl)),
    }
    url.set_fragment(None);
    Ok(url)
}

fn checked_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, WebError> {
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(WebError::new(WebErrorCode::Dns));
    }
    Ok(addresses)
}

fn reject_host_address(address: SocketAddr) -> Result<(), WebError> {
    // UDP connect 仅请求操作系统匹配路由并选定本地源地址，不会真正发出任何数据包。
    // 若目标地址恰好是本机网卡绑定的公网 IP，也能通过比对本地源地址识别并拦截（纯 RFC 私网段判断会漏放本机公网 IP）。
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).map_err(|_| WebError::new(WebErrorCode::Dns))?;
    if socket.connect(address).is_ok()
        && socket
            .local_addr()
            .map_err(|_| WebError::new(WebErrorCode::Dns))?
            .ip()
            == address.ip()
    {
        return Err(WebError::new(WebErrorCode::UnsafeUrl));
    }
    Ok(())
}

async fn client(url: &Url, settings: &WebSettings) -> Result<Client, WebError> {
    let host = url
        .host_str()
        .ok_or_else(|| WebError::new(WebErrorCode::UnsafeUrl))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WebError::new(WebErrorCode::UnsafeUrl))?;
    let addresses = checked_addresses(
        tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|_| WebError::new(WebErrorCode::Dns))?
            .collect(),
    )?;
    for address in &addresses {
        reject_host_address(*address)?;
    }
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .timeout(Duration::from_secs(settings.timeout_secs))
        .connect_timeout(Duration::from_secs(settings.timeout_secs.min(10)))
        .resolve_to_addrs(host, &addresses)
        .user_agent("Nuono-Web/0.4")
        .build()
        .map_err(|_| WebError::new(WebErrorCode::Network))
}

fn request_error(error: reqwest::Error) -> WebError {
    WebError::new(if error.is_timeout() {
        WebErrorCode::Timeout
    } else {
        WebErrorCode::Network
    })
}

fn status_error(status: StatusCode) -> WebError {
    WebError::new(match status.as_u16() {
        301 | 302 | 303 | 307 | 308 => WebErrorCode::Redirect,
        401 | 403 => WebErrorCode::Unauthorized,
        429 | 432 | 433 => WebErrorCode::RateLimited,
        _ => WebErrorCode::Http,
    })
}

fn provider_endpoint(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some("api.tavily.com" | "api.exa.ai" | "api.firecrawl.dev" | "r.jina.ai" | "s.jina.ai")
        )
}

pub(super) fn redirect_target(current: &Url, location: &str) -> Result<Url, WebError> {
    let next = current
        .join(location)
        .map_err(|_| WebError::new(WebErrorCode::Redirect))?;
    if current.scheme() == "https" && next.scheme() != "https" {
        return Err(WebError::new(WebErrorCode::Redirect));
    }
    public_url(next.as_str())
}

pub(super) async fn response_json(
    response: reqwest::Response,
    maximum: usize,
) -> Result<serde_json::Value, WebError> {
    if !response.status().is_success() {
        return Err(status_error(response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(WebError::new(WebErrorCode::ResponseTooLarge));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(request_error)?;
        if chunk.len() > maximum.saturating_sub(body.len()) {
            return Err(WebError::new(WebErrorCode::ResponseTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| WebError::new(WebErrorCode::InvalidResponse))
}

#[async_trait::async_trait]
impl WebTransport for SafeHttp {
    async fn request(
        &self,
        wire: ProviderRequest,
        settings: &WebSettings,
    ) -> Result<serde_json::Value, WebError> {
        if !provider_endpoint(&wire.url) {
            return Err(WebError::new(WebErrorCode::UnsafeUrl));
        }
        let client = client(&wire.url, settings).await?;
        let mut request = client.request(wire.method, wire.url).headers(wire.headers);
        if let Some(body) = wire.body {
            request = request.json(&body);
        }
        response_json(
            request.send().await.map_err(request_error)?,
            settings.max_response_bytes,
        )
        .await
    }

    async fn preflight(&self, mut url: Url, settings: &WebSettings) -> Result<Url, WebError> {
        for hop in 0..=5 {
            url = public_url(url.as_str())?;
            let client = client(&url, settings).await?;
            let mut response = client
                .request(Method::HEAD, url.clone())
                .send()
                .await
                .map_err(request_error)?;
            if matches!(response.status().as_u16(), 405 | 501) {
                drop(response);
                response = client
                    .request(Method::GET, url.clone())
                    .header(RANGE, "bytes=0-0")
                    .send()
                    .await
                    .map_err(request_error)?;
            }
            if response.status().is_redirection() {
                if hop == 5 {
                    return Err(WebError::new(WebErrorCode::Redirect));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| WebError::new(WebErrorCode::Redirect))?;
                // 逐跳重新校验并解析重定向目标地址，且预检请求绝不附带服务商 API 密钥。
                url = redirect_target(&url, location)?;
            } else {
                // 预检阶段仅发送 HEAD 请求，不下载任何响应正文；
                // 即使目标站点拒绝 HEAD 请求（如返回 405），只要确认其为合法公网地址即可交给外部提取服务抓取。
                return Ok(url);
            }
        }
        Err(WebError::new(WebErrorCode::Redirect))
    }
}

#[cfg(test)]
pub(super) fn validate_dns_for_test(addresses: Vec<SocketAddr>) -> Result<(), WebError> {
    checked_addresses(addresses).map(|_| ())
}
