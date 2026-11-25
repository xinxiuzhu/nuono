use super::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    pub kind: String,
    pub id: String,
    pub repository: Option<String>,
    pub commit: String,
    pub tree: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    pub checks: Vec<String>,
    pub required_plugins: Vec<String>,
    pub required_adapters: Vec<String>,
    pub external_connections: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub release_id: String,
    pub created_at: String,
    pub executable: String,
    pub application: ApplicationDescription,
    pub sources: Vec<SourceRecord>,
    pub readiness: Readiness,
    pub build_parameters: BTreeMap<String, String>,
    pub files: BTreeMap<String, FileRecord>,
}
impl ReleaseManifest {
    pub fn identity(&self) -> Result<String> {
        let mut value = self.clone();
        value.release_id.clear();
        Ok(digest(&serde_json::to_vec(&value)?))
    }
}
pub fn safe_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && !value.contains(['\\', '\0', ':'])
            && !value.starts_with('/')
            && !value.ends_with('/'),
        "非法发行路径: {value:?}"
    );
    ensure!(
        value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
            && Path::new(value)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "非法发行路径: {value:?}"
    );
    Ok(())
}
fn sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn inventory(root: &Path) -> Result<BTreeMap<String, FileRecord>> {
    fn walk(root: &Path, path: &Path, files: &mut BTreeMap<String, FileRecord>) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "发行包不允许符号链接: {}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(root, &entry?.path(), files)?;
            }
        } else {
            ensure!(metadata.is_file(), "发行包包含非普通文件");
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("发行文件名必须为 UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            safe_path(&name)?;
            if name == "release.json" {
                return Ok(());
            }
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = name.ends_with(".exe");
            files.insert(
                name,
                FileRecord {
                    size: metadata.len(),
                    sha256: source_build::hash_file(path)?,
                    executable,
                },
            );
        }
        Ok(())
    }
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "发行路径必须是实际目录"
    );
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

pub fn dashboard(root: &Path) -> Result<()> {
    let index = fs::read_to_string(root.join("index.html")).context("Dashboard 缺少 index.html")?;
    for attribute in ["src=\"", "href=\"", "src='", "href='"] {
        let quote = attribute.chars().last().unwrap();
        for tail in index.split(attribute).skip(1) {
            let value = tail.split(quote).next().unwrap_or_default();
            if value.starts_with(['#', '?']) || value.starts_with("data:") {
                continue;
            }
            ensure!(
                !value.contains("://") && !value.starts_with("//"),
                "Dashboard 发行资产不得依赖远程 URL"
            );
            let value = value
                .trim_start_matches('/')
                .split(['?', '#'])
                .next()
                .unwrap_or_default();
            if value.is_empty() {
                continue;
            }
            safe_path(value)?;
            ensure!(root.join(value).is_file(), "Dashboard 引用缺失: {value}");
        }
    }
    let files = inventory(root)?;
    for suffix in [".js", ".wasm", ".css"] {
        ensure!(
            files.keys().any(|name| name.ends_with(suffix)),
            "Dashboard 缺少 {suffix} 产物"
        );
    }
    Ok(())
}

pub fn verify(root: &Path) -> Result<ReleaseManifest> {
    let path = root.join("release.json");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 16 * 1024 * 1024,
        "无效或过大的 release.json"
    );
    let manifest: ReleaseManifest = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        manifest.schema_version == 1 && manifest.application.schema_version == 1,
        "不支持的发行清单版本"
    );
    ensure!(
        sha(&manifest.release_id) && manifest.release_id == manifest.identity()?,
        "发行清单摘要不一致"
    );
    ensure!(
        matches!(manifest.executable.as_str(), "nuono" | "nuono.exe"),
        "无效程序入口"
    );
    for (name, file) in &manifest.files {
        safe_path(name)?;
        ensure!(
            name == &manifest.executable
                || name == "LICENSE"
                || name.starts_with("nuono-hub/dist/")
                || matches!(
                    name.as_str(),
                    "build-info/Cargo.lock"
                        | "build-info/sources.json"
                        | "build-info/application.Cargo.toml"
                ),
            "发行包包含非发行数据: {name}"
        );
        ensure!(name != "release.json" && sha(&file.sha256), "无效文件摘要");
    }
    let actual = inventory(root)?;
    // Unix 平台严格校验可执行权限位；Windows 文件复制不会保留 Unix 权限模式，因此仅核对文件集合、大小与哈希。
    #[cfg(unix)]
    ensure!(
        actual == manifest.files,
        "发行文件缺失、多余、权限或内容已变化"
    );
    #[cfg(not(unix))]
    ensure!(
        actual.keys().eq(manifest.files.keys())
            && actual
                .iter()
                .all(|(name, file)| file.size == manifest.files[name].size
                    && file.sha256 == manifest.files[name].sha256),
        "发行文件已变化"
    );
    for name in [
        &manifest.executable,
        "build-info/Cargo.lock",
        "build-info/sources.json",
        "build-info/application.Cargo.toml",
    ] {
        ensure!(actual.contains_key(name), "发行文件缺失: {name}");
    }
    #[cfg(unix)]
    ensure!(actual[&manifest.executable].executable, "程序不可执行");
    ensure!(
        manifest.application.build.lock_sha256.as_deref()
            == Some(actual["build-info/Cargo.lock"].sha256.as_str()),
        "程序与应用锁文件不匹配"
    );
    let sources: Vec<SourceRecord> =
        serde_json::from_slice(&fs::read(root.join("build-info/sources.json"))?)?;
    ensure!(sources == manifest.sources, "源码清单不匹配");
    validate_sources(&manifest)?;
    dashboard(&root.join("nuono-hub/dist"))?;
    Ok(manifest)
}

fn validate_sources(manifest: &ReleaseManifest) -> Result<()> {
    let build = &manifest.application.build;
    ensure!(
        build.rust_version.is_some()
            && build.compiler_commit.is_some()
            && build.target_triple.is_some(),
        "缺少编译工具链信息"
    );
    ensure!(
        manifest.readiness.checks
            == [
                "database",
                "framework_migrations",
                "required_extension_initialization"
            ]
            && manifest.readiness.external_connections == "reported_separately",
        "不支持的就绪条件"
    );
    let mut keys = std::collections::BTreeSet::new();
    for source in &manifest.sources {
        ensure!(
            keys.insert((source.kind.as_str(), source.id.as_str())),
            "重复源码身份"
        );
        ensure!(
            matches!(source.kind.as_str(), "framework" | "plugin" | "adapter")
                && matches!(source.commit.len(), 40 | 64)
                && source.commit.bytes().all(|b| b.is_ascii_hexdigit())
                && matches!(source.tree.len(), 40 | 64)
                && source.tree.bytes().all(|b| b.is_ascii_hexdigit()),
            "无效源码修订"
        );
    }
    ensure!(keys.contains(&("framework", "nuono")), "缺少框架源码身份");
    ensure!(
        keys.len() == manifest.application.extensions.len() + 1,
        "源码与扩展清单数量不一致"
    );
    let mut extension_keys = std::collections::BTreeSet::new();
    for extension in &manifest.application.extensions {
        ensure!(
            extension_keys.insert((extension.kind, &extension.id)),
            "重复应用扩展身份"
        );
        ensure!(sha(&extension.source_hash), "无效扩展源码摘要");
        let kind = match extension.kind {
            SourceKind::Plugin => "plugin",
            SourceKind::Adapter => "adapter",
        };
        let source = manifest
            .sources
            .iter()
            .find(|s| s.kind == kind && s.id == extension.id)
            .context("扩展缺少源码身份")?;
        ensure!(
            extension.commit.as_deref() == Some(source.commit.as_str()),
            "扩展修订不一致"
        );
    }
    for (kind, ids) in [
        ("plugin", &manifest.readiness.required_plugins),
        ("adapter", &manifest.readiness.required_adapters),
    ] {
        for id in ids {
            ensure!(
                keys.contains(&(kind, id.as_str())),
                "必需扩展未编入应用: {kind}:{id}"
            );
        }
    }
    let mut namespaces = std::collections::BTreeSet::new();
    for set in &manifest.application.migrations {
        ensure!(
            namespaces.insert(set.namespace.as_str()),
            "重复迁移命名空间"
        );
        ensure!(
            set.namespace == "framework"
                || set
                    .namespace
                    .strip_prefix("plugin:")
                    .is_some_and(|id| keys.contains(&("plugin", id))),
            "迁移命名空间没有对应插件"
        );
        let mut versions = std::collections::BTreeSet::new();
        for migration in &set.migrations {
            ensure!(
                versions.insert(&migration.version)
                    && !migration.version.is_empty()
                    && !migration.description.trim().is_empty()
                    && sha(&migration.postgres_sha256)
                    && sha(&migration.sqlite_sha256),
                "无效迁移清单"
            );
        }
    }
    ensure!(namespaces.contains("framework"), "缺少框架迁移清单");
    for extension in manifest
        .application
        .extensions
        .iter()
        .filter(|s| s.kind == SourceKind::Plugin)
    {
        ensure!(
            namespaces.contains(format!("plugin:{}", extension.id).as_str()),
            "缺少插件迁移清单"
        );
    }
    Ok(())
}
