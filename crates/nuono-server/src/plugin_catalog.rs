//! 插件商店远端 HTTPS 目录拉取、格式校验与按源隔离的本地回退缓存（Last-Known-Good）。
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub(crate) struct Catalog {
    pub plugins: Vec<CatalogPlugin>,
}

impl Serialize for Catalog {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Catalog", 2)?;
        state.serialize_field("schema_version", &SCHEMA_VERSION)?;
        state.serialize_field("extensions", &self.plugins)?;
        state.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogPlugin {
    #[serde(default)]
    pub kind: crate::source_build::SourceKind,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub repo: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CatalogSnapshot {
    pub plugins: Vec<CatalogPlugin>,
    pub stale: bool,
    pub warning: Option<String>,
}

struct CachedCatalog {
    catalog: Catalog,
    modified: SystemTime,
}

/// 目录下载及其源代码仓库都必须使用 HTTPS。
pub(crate) fn validate_https_url(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "地址必须是合法 HTTPS URL".to_string())?;
    if value
        .bytes()
        .any(|value| value.is_ascii_whitespace() || value.is_ascii_control())
        || value.len() > 2048
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("地址必须是 HTTPS URL，且不能包含登录凭据或片段".into());
    }
    Ok(())
}

pub(crate) fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && !id.eq_ignore_ascii_case("hub")
        && id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
}

pub(crate) fn is_valid_commit(commit: &str) -> bool {
    commit.len() == 40 && commit.bytes().all(|value| value.is_ascii_hexdigit())
}

fn nonempty_field(value: &str, field: &str, id: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("插件 {id} 的 {field} 不能为空"));
    }
    Ok(())
}

fn numeric_version_part(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|value| value.is_ascii_digit())
        && (value.len() == 1 || !value.starts_with('0'))
}

/// 仅要求精确的 SemVer 版本号（可带预发布或构建后缀），绝不接受 `^`/`>=` 等版本范围表达式。
fn is_exact_version(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(version, label)| (version, Some(label)));
    let (base, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(version, label)| {
            (version, Some(label))
        });
    let parts: Vec<_> = base.split('.').collect();
    if parts.len() != 3 || !parts.into_iter().all(numeric_version_part) {
        return false;
    }
    for (label, is_prerelease) in [(build, false), (prerelease, true)] {
        if let Some(label) = label {
            for part in label.split('.') {
                if part.is_empty()
                    || !part
                        .bytes()
                        .all(|value| value.is_ascii_alphanumeric() || value == b'-')
                    || (is_prerelease
                        && part.bytes().all(|value| value.is_ascii_digit())
                        && !numeric_version_part(part))
                {
                    return false;
                }
            }
        }
    }
    true
}

impl CatalogPlugin {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !is_valid_plugin_id(&self.id) {
            return Err(format!("非法或保留的插件 ID: {}", self.id));
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("author", &self.author),
        ] {
            nonempty_field(value, field, &self.id)?;
        }
        if !is_exact_version(&self.version) {
            return Err(format!("插件 {} 的 version 必须是完整版本号", self.id));
        }
        validate_https_url(&self.repo)
            .map_err(|message| format!("插件 {} 的 repo 无效: {message}", self.id))?;
        if !is_valid_commit(&self.commit) {
            return Err(format!(
                "插件 {} 的 commit 必须是 40 位十六进制 SHA",
                self.id
            ));
        }
        if let Some(license) = &self.license {
            nonempty_field(license, "license", &self.id)?;
        }
        Ok(())
    }
}

pub(crate) fn parse_catalog(content: &[u8]) -> Result<Catalog, String> {
    if content.len() > MAX_CATALOG_BYTES {
        return Err("插件目录超过 2 MiB 限制".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(content).map_err(|error| format!("扩展目录 JSON 无效: {error}"))?;
    let version = value["schema_version"]
        .as_u64()
        .ok_or("目录缺少 schema_version")?;
    if version != u64::from(SCHEMA_VERSION) {
        return Err(format!(
            "不支持目录 schema_version={version}，需要 {SCHEMA_VERSION}"
        ));
    }
    if value
        .as_object()
        .is_none_or(|object| object.len() != 2 || !object.contains_key("extensions"))
    {
        return Err("目录包含未知字段或缺少条目".into());
    }
    let entries = value["extensions"].as_array().ok_or("目录条目必须是数组")?;
    if entries.iter().any(|entry| entry.get("kind").is_none()) {
        return Err("扩展条目缺少 kind 字段".into());
    }
    let catalog = Catalog {
        plugins: entries
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .map_err(|error| format!("扩展条目不符合清单格式: {error}"))?,
    };
    let mut ids = HashSet::new();
    for plugin in &catalog.plugins {
        plugin.validate()?;
        // 在大小写不敏感的文件系统上，仅大小写不同的插件 ID 会发生目录冲突，故按小写去重。
        if !ids.insert((plugin.kind, plugin.id.to_ascii_lowercase())) {
            return Err(format!("插件目录包含重复 ID: {}", plugin.id));
        }
    }
    Ok(catalog)
}

fn cache_path(root: &Path, index_url: &str) -> PathBuf {
    root.join("target")
        .join("nuono-plugin-store")
        .join(hex::encode(Sha256::digest(index_url.as_bytes())))
        .join("catalog.json")
}

/// 仅供本地 Git 历史查询使用的只读缓存读取（绝不在此触发远端目录下载或缓存刷新）。
pub(crate) async fn cached_plugins(
    root: &Path,
    index_url: &str,
) -> Result<Option<Vec<CatalogPlugin>>, String> {
    validate_https_url(index_url)?;
    Ok(read_cache(&cache_path(root, index_url))
        .await?
        .map(|cached| cached.catalog.plugins))
}

async fn read_cache(path: &Path) -> Result<Option<CachedCatalog>, String> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取插件目录缓存失败: {error}")),
    };
    if metadata.len() > MAX_CATALOG_BYTES as u64 {
        return Err("插件目录缓存超过 2 MiB 限制".into());
    }
    let content = tokio::fs::read(path)
        .await
        .map_err(|error| format!("读取插件目录缓存失败: {error}"))?;
    let catalog = parse_catalog(&content)?;
    let modified = metadata
        .modified()
        .map_err(|error| format!("读取插件目录缓存时间失败: {error}"))?;
    Ok(Some(CachedCatalog { catalog, modified }))
}

fn cache_is_fresh(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age < CACHE_TTL)
}

async fn write_cache(path: &Path, catalog: &Catalog) -> Result<(), String> {
    let directory = path.parent().expect("catalog cache has a parent directory");
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| format!("创建插件目录缓存失败: {error}"))?;
    let content =
        serde_json::to_vec(catalog).map_err(|error| format!("序列化插件目录缓存失败: {error}"))?;
    let temporary = directory.join(format!("catalog-{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        tokio::fs::write(&temporary, content)
            .await
            .map_err(|error| format!("写入插件目录缓存失败: {error}"))?;
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|error| format!("发布插件目录缓存失败: {error}"))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn download_catalog(index_url: &str) -> Result<Catalog, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("插件目录重定向次数过多");
            }
            if let Err(message) = validate_https_url(attempt.url().as_str()) {
                return attempt.error(message);
            }
            attempt.follow()
        }))
        .build()
        .map_err(|error| format!("创建插件目录 HTTP 客户端失败: {error}"))?;
    let mut response = client
        .get(index_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| format!("下载插件目录失败: {error}"))?
        .error_for_status()
        .map_err(|error| format!("下载插件目录失败: {error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err("插件目录超过 2 MiB 限制".into());
    }
    let mut content = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取插件目录响应失败: {error}"))?
    {
        if chunk.len() > MAX_CATALOG_BYTES.saturating_sub(content.len()) {
            return Err("插件目录超过 2 MiB 限制".into());
        }
        content.extend_from_slice(&chunk);
    }
    parse_catalog(&content)
}

fn stale_snapshot(
    cached: Option<CachedCatalog>,
    error: String,
    refresh: bool,
) -> Result<CatalogSnapshot, String> {
    if refresh {
        return Err(error);
    }
    match cached {
        Some(cached) => Ok(CatalogSnapshot {
            plugins: cached.catalog.plugins,
            stale: true,
            warning: Some(format!("{error}；暂用上次验证通过的插件目录")),
        }),
        None => Err(error),
    }
}

/// 常规读取优先使用未过期的本地缓存，远端拉取失败时可回退到上次校验通过的旧缓存；
/// 显式刷新（`refresh = true`）则强制拉取远端，一旦下载或校验失败直接报错，且不会覆盖已有可用缓存。
pub(crate) async fn load_remote(
    root: &Path,
    index_url: &str,
    refresh: bool,
) -> crate::operation_error::Result<CatalogSnapshot> {
    validate_https_url(index_url).map_err(crate::operation_error::OperationError::conflict)?;
    let path = cache_path(root, index_url);
    let (cached, cache_warning) = match read_cache(&path).await {
        Ok(cached) => (cached, None),
        Err(error) => (None, Some(error)),
    };
    if !refresh
        && let Some(cached) = &cached
        && cache_is_fresh(cached.modified, SystemTime::now())
    {
        return Ok(CatalogSnapshot {
            plugins: cached.catalog.plugins.clone(),
            stale: false,
            warning: None,
        });
    }
    match download_catalog(index_url).await {
        Ok(catalog) => {
            write_cache(&path, &catalog).await?;
            Ok(CatalogSnapshot {
                plugins: catalog.plugins,
                stale: false,
                warning: None,
            })
        }
        Err(error) => {
            let error = match cache_warning {
                Some(warning) => format!("{error}；{warning}"),
                None => error,
            };
            stale_snapshot(cached, error, refresh)
                .map_err(crate::operation_error::OperationError::upstream)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_value() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "extensions": [{
                "kind": "plugin",
                "id": "demo-adapter",
                "name": "Demo Adapter",
                "version": "0.1.0",
                "description": "Demo adapter for Nuono",
                "author": "example",
                "repo": "https://github.com/example/nuono-demo-adapter.git",
                "commit": "0123456789abcdef0123456789abcdef01234567",
                "license": "MIT"
            }]
        })
    }

    fn parse_value(value: &serde_json::Value) -> Result<Catalog, String> {
        parse_catalog(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn catalog_keeps_kinds_distinct_and_cache_round_trips() {
        let entry = catalog_value()["extensions"][0].clone();
        let mut plugin = entry.clone();
        plugin["kind"] = serde_json::json!("plugin");
        let mut adapter = entry;
        adapter["kind"] = serde_json::json!("adapter");
        let value = serde_json::json!({"schema_version":1,"extensions":[plugin.clone(),adapter]});
        let parsed = parse_value(&value).unwrap();
        assert_eq!(parsed.plugins.len(), 2);
        assert_eq!(
            parse_catalog(&serde_json::to_vec(&parsed).unwrap())
                .unwrap()
                .plugins
                .len(),
            2
        );
        let missing_kind = serde_json::json!({
            "schema_version": 1,
            "extensions": [{
                "id": "demo",
                "name": "Demo",
                "version": "0.1.0",
                "description": "Demo",
                "author": "example",
                "repo": "https://github.com/example/demo.git",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }]
        });
        assert!(parse_value(&missing_kind).is_err());
    }
    #[test]
    fn catalog_requires_supported_schema_and_complete_release_metadata() {
        assert!(parse_value(&catalog_value()).is_ok());
        for field in [
            "kind",
            "id",
            "name",
            "version",
            "description",
            "author",
            "repo",
            "commit",
        ] {
            let mut value = catalog_value();
            value["extensions"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(parse_value(&value).is_err(), "missing {field}");
        }
        let mut value = catalog_value();
        value["schema_version"] = 2.into();
        assert!(parse_value(&value).is_err());
        value = catalog_value();
        value["unrecognized"] = true.into();
        assert!(parse_value(&value).is_err());
        value = catalog_value();
        value["extensions"][0]["unrecognized"] = true.into();
        assert!(parse_value(&value).is_err());
    }

    #[test]
    fn invalid_release_identity_and_unpinned_repositories_are_rejected() {
        for (field, invalid) in [
            ("id", "../adapter"),
            ("id", "hub"),
            ("name", " "),
            ("repo", "http://github.com/x/plugin.git"),
            ("repo", "ssh://git@github.com/x/plugin.git"),
            ("repo", "https://token@github.com/x/plugin.git"),
            ("commit", "abcdef"),
            ("commit", "0123456789abcdef0123456789abcdef0123456g"),
        ] {
            let mut value = catalog_value();
            value["extensions"][0][field] = invalid.into();
            assert!(parse_value(&value).is_err(), "accepted {field}={invalid}");
        }
        let mut value = catalog_value();
        let duplicate = value["extensions"][0].clone();
        value["extensions"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_value(&value).is_err());
        value["extensions"][1]["id"] = "Demo-Adapter".into();
        assert!(parse_value(&value).is_err());
    }

    #[test]
    fn exact_versions_accept_semver_labels_but_reject_ranges_and_malformed_versions() {
        for valid in ["0.4.0", "1.0.0-rc.1", "1.0.0+build.01", "1.0.0-rc.1+build"] {
            assert!(is_exact_version(valid), "rejected {valid}");
        }
        for invalid in [
            "0.4", "^0.4.0", "01.0.0", "1.0.0-01", "1.0.0+", "1.0.0-", "1.0.0++x",
        ] {
            assert!(!is_exact_version(invalid), "accepted {invalid}");
        }
    }

    #[test]
    fn response_and_cache_limits_share_the_same_validation() {
        assert!(parse_catalog(&vec![b' '; MAX_CATALOG_BYTES + 1]).is_err());
        assert!(validate_https_url("https://example.com/index.json").is_ok());
        for invalid in [
            "http://example.com/index.json",
            "file:///tmp/index.json",
            "https://user:password@example.com/index.json",
            "https://example.com/index.json#fragment",
            " https://example.com/index.json",
            "https://exam\nple.com/index.json",
            "https://example.com/in\tdex.json",
        ] {
            assert!(validate_https_url(invalid).is_err());
        }
        let root = Path::new("/tmp/catalog-cache-test");
        assert_ne!(
            cache_path(root, "https://example.com/first.json"),
            cache_path(root, "https://example.com/second.json")
        );
        let now = SystemTime::now();
        assert!(cache_is_fresh(now - Duration::from_secs(299), now));
        assert!(!cache_is_fresh(now - CACHE_TTL, now));
        assert!(!cache_is_fresh(now + Duration::from_secs(1), now));
    }

    #[tokio::test]
    async fn fresh_cache_is_usable_offline_and_invalid_downloads_leave_it_untouched() {
        let root =
            std::env::temp_dir().join(format!("nuono-plugin-catalog-{}", uuid::Uuid::new_v4()));
        let index_url = "https://127.0.0.1:1/catalog.json";
        let path = cache_path(&root, index_url);
        let catalog = parse_value(&catalog_value()).unwrap();
        write_cache(&path, &catalog).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();

        let fresh = load_remote(&root, index_url, false).await.unwrap();
        assert!(!fresh.stale);
        assert!(fresh.warning.is_none());
        assert_eq!(fresh.plugins[0].id, "demo-adapter");
        assert!(load_remote(&root, index_url, true).await.is_err());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);

        std::fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - CACHE_TTL)
            .unwrap();
        let stale = load_remote(&root, index_url, false).await.unwrap();
        assert!(stale.stale);
        assert!(stale.warning.unwrap().contains("下载插件目录失败"));
        assert_eq!(stale.plugins.len(), 1);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(stale_snapshot(None, "network unavailable".into(), false).is_err());

        let other_source = cache_path(&root, "https://127.0.0.1:1/other.json");
        assert!(read_cache(&other_source).await.unwrap().is_none());
        tokio::fs::write(&path, b"{bad-json").await.unwrap();
        assert!(read_cache(&path).await.is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
