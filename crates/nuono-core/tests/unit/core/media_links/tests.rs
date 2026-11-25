use std::time::Duration;

use super::{HostMediaLinks, TOKEN_PREFIX};

struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(name: &str, content: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("nuono-media-link-{}-{name}", uuid::Uuid::new_v4()));
        std::fs::write(&path, content).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn links() -> HostMediaLinks {
    // `new` 返回 Arc 包装，测试中直接解包获取内部实例即可。
    std::sync::Arc::try_unwrap(HostMediaLinks::new("http://127.0.0.1:9923".into()))
        .ok()
        .expect("fresh Arc is uniquely owned")
}

#[test]
fn issued_token_verifies_to_the_canonical_file() {
    let file = TempFile::new("clip.mp4", b"video-bytes");
    let links = links();
    let link = links.issue(file.path(), Duration::from_secs(60)).unwrap();
    assert!(link.path.starts_with(TOKEN_PREFIX));
    assert!(!link.path.contains(file.path().to_str().unwrap()));
    assert!(link.url.starts_with("http://127.0.0.1:9923/media/"));
    let token = link.path.trim_start_matches(TOKEN_PREFIX);
    assert_eq!(
        links.verify(token).unwrap(),
        file.path().canonicalize().unwrap()
    );
}

#[test]
fn tampered_truncated_and_reencoded_tokens_are_rejected() {
    let file = TempFile::new("clip.mp4", b"video-bytes");
    let links = links();
    let link = links.issue(file.path(), Duration::from_secs(60)).unwrap();
    let token = link.path.trim_start_matches(TOKEN_PREFIX).to_string();

    let mut tampered = token.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'a' { 'b' } else { 'a' });
    assert!(links.verify(&tampered).is_none());

    assert!(links.verify(&token[..token.len() - 4]).is_none());
    assert!(links.verify("").is_none());
    assert!(links.verify("no-dots").is_none());
}

#[test]
fn expired_tokens_and_absurd_ttls_are_rejected() {
    let file = TempFile::new("clip.mp4", b"video-bytes");
    let links = links();
    assert!(links.issue(file.path(), Duration::ZERO).is_err());
    assert!(
        links.issue(file.path(), Duration::from_secs(7200)).is_err(),
        "links must stay short-lived"
    );
}

#[test]
fn directories_and_missing_files_are_rejected() {
    let links = links();
    assert!(
        links
            .issue(&std::env::temp_dir(), Duration::from_secs(60))
            .is_err()
    );
    let missing = std::env::temp_dir().join("nuono-media-link-does-not-exist");
    assert!(links.issue(&missing, Duration::from_secs(60)).is_err());
}

#[test]
fn a_deleted_file_stops_verifying() {
    let file = TempFile::new("clip.mp4", b"video-bytes");
    let links = links();
    let link = links.issue(file.path(), Duration::from_secs(60)).unwrap();
    let token = link.path.trim_start_matches(TOKEN_PREFIX).to_string();
    std::fs::remove_file(file.path()).unwrap();
    assert!(links.verify(&token).is_none());
}

#[tokio::test]
async fn route_specific_base_overrides_the_fallback() {
    use nuono_plugin::RuntimeBotRoute;
    use nuono_plugin::media_link::MediaLinkService;

    let file = TempFile::new("clip.mp4", b"video-bytes");
    let links = links();
    let route = RuntimeBotRoute::new("onebot", "inst-1", "bot-1");
    super::remember_route_base(&route, 7, "https://bot.example.com/");
    let issued = links
        .link(file.path(), Duration::from_secs(60), Some(&route))
        .await
        .unwrap();
    assert!(issued.url.starts_with("https://bot.example.com/media/"));
    assert_eq!(
        issued.path,
        issued.url.trim_start_matches("https://bot.example.com")
    );
    // 旧连接断开时不能误清当前活跃连接注册的网关基地址。
    super::forget_route_base(&route, 6);
    assert!(
        links
            .link(file.path(), Duration::from_secs(60), Some(&route))
            .await
            .is_ok()
    );
    super::forget_route_base(&route, 7);
    assert!(
        links
            .link(file.path(), Duration::from_secs(60), Some(&route))
            .await
            .is_err(),
        "a route with no learned origin must fail so the caller can fall back to Base64"
    );
}

#[test]
fn header_derived_bases_reject_authority_tricks() {
    use http::{HeaderMap, HeaderValue, header};

    let mut headers = HeaderMap::new();
    headers.insert(
        header::HOST,
        HeaderValue::from_static("bot.example.com:8443"),
    );
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    assert_eq!(
        super::base_from_headers(&headers).as_deref(),
        Some("https://bot.example.com:8443")
    );

    for host in [
        "evil.com/path",
        "user@evil.com",
        "evil.com?x=1",
        "evil.com#frag",
        "evil.com\\@trusted",
        "",
    ] {
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        assert!(
            super::base_from_headers(&headers).is_none(),
            "{host} must not become a signed media origin"
        );
    }
}
