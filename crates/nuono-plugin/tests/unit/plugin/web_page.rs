use super::*;

fn page() -> PluginWebPage {
    PluginWebPage::new(
        "overview",
        "Overview",
        vec![PluginWebAsset::new(
            "index.html",
            "text/html",
            b"<html></html>",
        )],
    )
}

#[test]
fn embedded_bundles_cannot_escape_or_ambiguously_address_their_root() {
    for path in [
        "../secret",
        "/secret",
        "a//b",
        "a/./b",
        "a/../b",
        "a\\b",
        "%2e%2e/file",
        "a?key=x",
        "a#x",
        "https://example.com",
    ] {
        let mut candidate = page();
        candidate
            .assets
            .push(PluginWebAsset::new(path, "text/plain", b"x"));
        assert!(
            PluginWebPage::validate_all(&[candidate]).is_err(),
            "accepted {path:?}"
        );
    }
    assert!(PluginWebPage::validate_all(&[page()]).is_ok());
}

#[test]
fn public_missing_and_duplicate_entries_are_rejected() {
    assert!(
        PluginWebPage::validate_all(&[page().with_access(PluginWebAccessPolicy::Public)]).is_err()
    );
    assert!(PluginWebPage::validate_all(&[page(), page()]).is_err());
    let mut candidate = page();
    candidate.entry = "missing.html".into();
    assert!(PluginWebPage::validate_all(&[candidate]).is_err());
}
