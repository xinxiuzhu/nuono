//! HTTP 管理接口与启动流程共用的源码插件商店策略及安装/更新编排。
//!
//! 本模块单向依赖底层的 `source_build` 构建原语；`source_build` 不会反过来读取任务账本或回调本模块。
pub(crate) mod git;
pub(crate) mod jobs;
use crate::source_build::SourceKind;
use nuono_core::{
    config::ConfigManager,
    core::{FrameworkControl, plugin_host::PluginHost},
    db::Database,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

/// 源码插件操作所需的宿主依赖上下文（与具体 HTTP 请求状态解耦）。
#[derive(Clone)]
pub(crate) struct OperationContext {
    pub data: PathBuf,
    pub config: Arc<ConfigManager>,
    pub db: Database,
    pub plugin_host: Arc<PluginHost>,
    pub framework_control: FrameworkControl,
    pub bot_instances: Arc<crate::bot_instances::BotInstanceRuntimeManager>,
}

use nuono_tooling::recovery::is_valid_plugin_id;

/// 解析扩展标识：普通插件保持原 ID，平台适配器带 `adapter:` 前缀以防命名冲突。
pub(crate) fn parse_source_key(key: &str) -> Result<(SourceKind, String), String> {
    let (kind, id) = key
        .strip_prefix("adapter:")
        .map_or((SourceKind::Plugin, key), |id| (SourceKind::Adapter, id));
    if !is_valid_plugin_id(id) {
        return Err("扩展 ID 无效".into());
    }
    Ok((kind, id.into()))
}
pub(crate) fn source_key(kind: SourceKind, id: &str) -> String {
    match kind {
        SourceKind::Plugin => id.into(),
        SourceKind::Adapter => format!("adapter:{id}"),
    }
}
pub(crate) fn plugin_target(key: &str) -> Result<std::path::PathBuf, String> {
    let (kind, id) = parse_source_key(key)?;
    Ok(store_root()?.join(kind.directory()).join(id))
}

fn store_root() -> Result<std::path::PathBuf, String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(crate::find_nuono_repository_root(&directory).unwrap_or(directory))
}

/// 只允许 http(s) 与 ssh 形式的 git 仓库地址。
///
/// 拒绝以 `-` 开头的值（会被 git 解释为命令行选项）以及 `ext::`/`file://` 等
/// 可导致执行任意命令或读取本地文件的 transport。
fn is_valid_repo_url(url: &str) -> bool {
    if url.is_empty()
        || url.len() > 2048
        || url.starts_with('-')
        || url
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    if let Some(address) = url.strip_prefix("git@") {
        return address
            .split_once(':')
            .is_some_and(|(host, path)| !host.is_empty() && !path.is_empty());
    }
    url::Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "https" | "http" | "ssh")
            && parsed.host_str().is_some()
            && parsed.password().is_none()
            && parsed.fragment().is_none()
            && (parsed.scheme() == "ssh" || parsed.username().is_empty())
    })
}

#[derive(Debug, Serialize, Deserialize, Clone, utoipa::ToSchema)]
#[schema(as = api::plugin_store::StorePlugin)]
pub struct StorePlugin {
    #[serde(default)]
    #[schema(value_type = crate::source_schemas::SourceKindSchema)]
    pub kind: SourceKind,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub repo_url: String,
    /// 商店清单固定要安装的 git commit（40 位十六进制）。None = 清单未固定。
    pub pinned_commit: Option<String>,
    pub license: Option<String>,
    pub installed: bool,
    pub installed_version: Option<String>,
    /// 当前安装目录 HEAD 的 commit。
    pub installed_commit: Option<String>,
    pub update_available: bool,
    #[serde(default)]
    pub running_version: Option<String>,
    #[serde(default)]
    pub running_commit: Option<String>,
    #[serde(default)]
    pub pending_restart: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugin_store::StoreCatalog)]
pub struct StoreCatalog {
    pub plugins: Vec<StorePlugin>,
    pub index_url: Option<String>,
    pub stale: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::plugin_store::StoreSettingsBody)]
pub struct StoreSettingsBody {
    pub index_url: String,
}

/// 校验并保存插件商店设置补丁（交由 `ConfigManager` 持久化）。
pub(crate) async fn save_settings(
    config: &ConfigManager,
    body: StoreSettingsBody,
) -> nuono_core::error::Result<()> {
    let index_url = body.index_url.trim().to_string();
    let index_url = (!index_url.is_empty()
        && index_url != nuono_core::config::OFFICIAL_PLUGIN_STORE_INDEX_URL)
        .then_some(index_url);
    let patch = nuono_core::db::RuntimeConfigPatch {
        settings: serde_json::json!({"plugin_store": {
            "index_url": index_url,
        }}),
        secrets: Vec::new(),
    };
    config.update_patch(patch).await
}

impl From<crate::plugin_catalog::CatalogPlugin> for StorePlugin {
    fn from(plugin: crate::plugin_catalog::CatalogPlugin) -> Self {
        Self {
            kind: plugin.kind,
            id: plugin.id,
            name: plugin.name,
            version: plugin.version,
            description: plugin.description,
            author: plugin.author,
            repo_url: plugin.repo,
            pinned_commit: Some(plugin.commit),
            license: plugin.license,
            installed: false,
            installed_version: None,
            installed_commit: None,
            update_available: false,
            running_version: None,
            running_commit: None,
            pending_restart: false,
        }
    }
}

/// 合法 commit：40 位十六进制。manifest 只接受完整 SHA，避免短 SHA 歧义。
use nuono_tooling::recovery::is_valid_commit;

/// 读取插件安装目录当前的 HEAD commit。
fn installed_commit(target: &std::path::Path) -> Option<String> {
    let mut command = std::process::Command::new("git");
    git::configure_command(&mut command);
    let out = command
        .args(["-C", &target.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    is_valid_commit(&commit).then_some(commit)
}

/// 合并已发布、已安装和正在运行的插件元数据。
pub(crate) async fn store_catalog(
    cfg: &nuono_core::config::PluginStoreConfig,
    force: bool,
) -> crate::operation_error::Result<StoreCatalog> {
    let root = store_root()?;
    let source_available = jobs::source_root().is_ok();
    let mut view = StoreCatalog {
        plugins: Vec::new(),
        index_url: cfg.index_url.clone(),
        stale: false,
        warning: None,
    };
    let snapshot =
        crate::plugin_catalog::load_remote(&root, cfg.effective_index_url(), force).await?;
    view.plugins = snapshot
        .plugins
        .into_iter()
        .map(StorePlugin::from)
        .collect();
    view.stale = snapshot.stale;
    view.warning = snapshot.warning;
    tokio::task::spawn_blocking(move || {
        let directory = root.join("plugins_store");
        if directory.is_dir() {
            for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                if !entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir()
                {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().to_string();
                if !is_valid_plugin_id(&id) {
                    return Err(crate::operation_error::OperationError::conflict(format!(
                        "商店插件 ID 无效: {id}"
                    )));
                }
                if let Some(plugin) = read_store_plugin(&entry.path(), &id)? {
                    if view.plugins.iter().any(|existing| {
                        existing.kind == plugin.kind && existing.id.eq_ignore_ascii_case(&id)
                    }) {
                        return Err(crate::operation_error::OperationError::conflict(format!(
                            "本地与远端商店插件 ID 冲突: {id}"
                        )));
                    }
                    view.plugins.push(plugin);
                }
            }
        }
        for kind in [SourceKind::Plugin, SourceKind::Adapter] {
            if let Ok(entries) = std::fs::read_dir(root.join(kind.directory())) {
                for entry in entries.flatten() {
                    let id = entry.file_name().to_string_lossy().to_string();
                    if is_valid_plugin_id(&id)
                        && !view.plugins.iter().any(|p| p.kind == kind && p.id == id)
                        && let Some(mut item) = source_catalog_item(&entry.path(), &id)
                    {
                        item.kind = kind;
                        view.plugins.push(item);
                    }
                }
            }
        }
        let running = crate::source_build::running_manifest();
        for running in &running {
            if !view
                .plugins
                .iter()
                .any(|p| p.kind == running.kind && p.id == running.id)
            {
                let mut plugin = empty_catalog_item(&running.id);
                plugin.kind = running.kind;
                plugin.name = running
                    .runtime_name
                    .clone()
                    .unwrap_or(running.package.clone());
                plugin.version = running.version.clone();
                view.plugins.push(plugin);
            }
        }
        view.plugins
            .sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
        for plugin in &mut view.plugins {
            let target = root.join(plugin.kind.directory()).join(&plugin.id);
            let compiled = running
                .iter()
                .find(|p| p.kind == plugin.kind && p.id == plugin.id);
            mark_catalog_state(plugin, &target, compiled, source_available);
        }
        Ok(view)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn mark_catalog_state(
    plugin: &mut StorePlugin,
    target: &std::path::Path,
    running: Option<&crate::source_build::SourcePlugin>,
    source_available: bool,
) {
    mark_installed(plugin, target);
    if let Some(running) = running {
        plugin.running_version = Some(running.version.clone());
        plugin.running_commit = running.commit.clone();
    }
    // 独立导出的预编译程序已静态包含所有插件，无需依赖本地源码目录；
    // 而在源码工作区部署模式下，若运行中插件的源码目录被删除或版本不一致，则标记为需要重启。
    plugin.pending_restart = source_available
        && match running {
            Some(running) => {
                !plugin.installed
                    || crate::source_build::source_identity(target)
                        .map(|source| {
                            source.source_hash != running.source_hash
                                || source.commit != running.commit
                        })
                        .unwrap_or(true)
            }
            None => plugin.installed,
        };
}

fn empty_catalog_item(id: &str) -> StorePlugin {
    StorePlugin {
        kind: SourceKind::Plugin,
        id: id.into(),
        name: id.into(),
        version: String::new(),
        description: String::new(),
        author: String::new(),
        repo_url: String::new(),
        pinned_commit: None,
        license: None,
        installed: false,
        installed_version: None,
        installed_commit: None,
        update_available: false,
        running_version: None,
        running_commit: None,
        pending_restart: false,
    }
}
fn source_catalog_item(path: &std::path::Path, id: &str) -> Option<StorePlugin> {
    let value: toml::Value =
        toml::from_str(&std::fs::read_to_string(path.join("Cargo.toml")).ok()?).ok()?;
    let mut plugin = empty_catalog_item(id);
    plugin.name = value["package"]["name"].as_str()?.into();
    plugin.version = value["package"]["version"].as_str()?.into();
    plugin.description = value["package"]
        .get("description")
        .and_then(toml::Value::as_str)
        .unwrap_or("本机已安装的源码插件")
        .into();
    plugin.license = value["package"]
        .get("license")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    Some(plugin)
}

fn mark_installed(plugin: &mut StorePlugin, target: &std::path::Path) {
    plugin.installed = target.join("Cargo.toml").is_file();
    if !plugin.installed {
        return;
    }
    plugin.installed_commit = installed_commit(target);
    plugin.update_available = match (&plugin.installed_commit, &plugin.pinned_commit) {
        (Some(actual), Some(expected)) => !actual.eq_ignore_ascii_case(expected),
        _ => false,
    };
    plugin.installed_version = std::fs::read_to_string(target.join("Cargo.toml"))
        .ok()
        .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
        .and_then(|value| {
            value
                .get("package")?
                .get("version")?
                .as_str()
                .map(str::to_string)
        });
}

fn read_store_plugin(dir: &std::path::Path, id: &str) -> Result<Option<StorePlugin>, String> {
    let json_file = dir.join("plugin.json");
    let toml_file = dir.join("plugin.toml");
    let p: Value = if json_file.is_file() {
        serde_json::from_str(&std::fs::read_to_string(json_file).map_err(|e| e.to_string())?)
            .map_err(|e| format!("插件 {id} 的 JSON 清单无效: {e}"))?
    } else if toml_file.is_file() {
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(toml_file).map_err(|e| e.to_string())?)
                .map_err(|e| format!("插件 {id} 的 TOML 清单无效: {e}"))?;
        serde_json::to_value(value).map_err(|e| e.to_string())?
    } else {
        return Ok(None);
    };
    let p = p.get("plugin").unwrap_or(&p);
    if p.get("id").is_some_and(|value| value.as_str() != Some(id)) {
        return Err(format!("插件清单 ID 与目录不一致: {id}"));
    }
    let text = |key: &str| p.get(key).and_then(Value::as_str).map(str::to_string);
    let repo_url = text("repo").ok_or_else(|| format!("插件 {id} 清单缺少 repo"))?;
    if !is_valid_repo_url(&repo_url) {
        return Err(format!("插件 {id} 清单 repo 无效"));
    }
    let commit = text("commit");
    if commit
        .as_deref()
        .is_some_and(|value| !is_valid_commit(value))
    {
        return Err(format!("插件 {id} 清单需要完整的 40 位 commit"));
    }
    Ok(Some(StorePlugin {
        kind: p
            .get("kind")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| "扩展 kind 无效")?
            .unwrap_or_default(),
        id: id.to_string(),
        name: text("name").unwrap_or_else(|| id.to_string()),
        version: text("version").unwrap_or_else(|| "0.1.0".into()),
        description: text("description").unwrap_or_default(),
        author: text("author").unwrap_or_else(|| "unknown".into()),
        repo_url,
        pinned_commit: commit,
        license: text("license"),
        installed: false,
        installed_version: None,
        installed_commit: None,
        update_available: false,
        running_version: None,
        running_commit: None,
        pending_restart: false,
    }))
}

fn select_repo(body_repo: Option<&str>, plugin: Option<&StorePlugin>) -> Result<String, String> {
    match (body_repo, plugin) {
        (Some(requested), Some(plugin)) if requested != plugin.repo_url => {
            Err("商店安装不允许覆盖清单中的仓库地址".into())
        }
        (_, Some(plugin)) => Ok(plugin.repo_url.clone()),
        (Some(requested), None) => Ok(requested.to_string()),
        (None, None) => Err("插件未在商店中找到，请提供 repo_url".into()),
    }
}

#[cfg(test)]
async fn git_command(args: Vec<String>) -> Result<std::process::Output, String> {
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new("git");
        git::configure_command(&mut command);
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
async fn git_at(target: &std::path::Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut arguments = vec!["-C".into(), target.to_string_lossy().into_owned()];
    arguments.extend(args.iter().map(|value| value.to_string()));
    git_command(arguments).await
}

#[cfg(test)]
fn successful_git(result: Result<std::process::Output, String>) -> Result<String, String> {
    let output = result?;
    if !output.status.success() {
        return Err(nuono_core::telemetry::sanitize_file_line(
            String::from_utf8_lossy(&output.stderr).trim(),
        )
        .0);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
async fn checkout_and_build<F, Fut>(
    target: &std::path::Path,
    commit: &str,
    build: F,
) -> Result<(), String>
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let previous = successful_git(git_at(target, &["rev-parse", "HEAD"]).await)?
        .trim()
        .to_string();
    if !is_valid_commit(&previous) {
        return Err("当前插件 HEAD 无效".into());
    }
    successful_git(
        git_at(
            target,
            &["checkout", "--detach", &commit.to_ascii_lowercase()],
        )
        .await,
    )?;
    let actual = successful_git(git_at(target, &["rev-parse", "HEAD"]).await);
    let result = match actual {
        Ok(actual) if actual.trim().eq_ignore_ascii_case(commit) => {
            build(target.to_path_buf()).await
        }
        _ => Err("更新后 HEAD 与固定 commit 不一致".into()),
    };
    if let Err(error) = result {
        return match successful_git(git_at(target, &["checkout", "--detach", &previous]).await) {
            Ok(_) => Err(format!("{error}；已恢复旧源码版本")),
            Err(rollback) => Err(format!("{error}；恢复旧源码失败: {rollback}")),
        };
    }
    if let Err(error) = std::fs::write(target.join(".nuono-pin"), format!("{commit}\n")) {
        tracing::warn!(%error, "写入 .nuono-pin 失败");
    }
    Ok(())
}

/// 按扩展标识在商店目录中查找对应条目。
async fn find_store_plugin(
    id: &str,
    cfg: &nuono_core::config::PluginStoreConfig,
) -> Result<Option<StorePlugin>, String> {
    let (kind, id) = parse_source_key(id)?;
    Ok(store_catalog(cfg, false)
        .await?
        .plugins
        .into_iter()
        .find(|plugin| plugin.kind == kind && plugin.id == id))
}

/// 提取近期需要管理员关注的源码插件操作异常摘要（刻意省略冗长的编译器原始输出）。
pub(crate) fn health_attention(
    data: &std::path::Path,
) -> Result<Vec<(String, String, String)>, String> {
    jobs::health_attention(data)
}

/// 在源码启动器调用 Cargo 编译前执行，回滚或清理上次中断遗留的半截插件源码变更。
pub(crate) fn recover_before_build(root: &std::path::Path) -> Result<(), String> {
    nuono_tooling::recovery::recover_before_build(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_store_plugin() -> StorePlugin {
        StorePlugin {
            kind: SourceKind::Plugin,
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new(),
            repo_url: "https://example.com/demo.git".into(),
            pinned_commit: None,
            license: None,
            installed: false,
            installed_version: None,
            installed_commit: None,
            update_available: false,
            running_version: None,
            running_commit: None,
            pending_restart: false,
        }
    }

    #[test]
    fn precompiled_catalog_keeps_running_plugins_without_source() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("demo");
        let running = crate::source_build::SourcePlugin {
            version: "1.2.3".into(),
            commit: Some("compiled-commit".into()),
            ..Default::default()
        };
        let mut plugin = sample_store_plugin();
        mark_catalog_state(&mut plugin, &target, Some(&running), false);
        assert!(!plugin.installed);
        assert_eq!(plugin.running_version.as_deref(), Some("1.2.3"));
        assert_eq!(plugin.running_commit.as_deref(), Some("compiled-commit"));
        assert!(!plugin.pending_restart);

        mark_catalog_state(&mut plugin, &target, Some(&running), true);
        assert!(
            plugin.pending_restart,
            "source removal still requires a restart"
        );
    }

    #[test]
    fn catalog_compares_local_source_only_for_source_deployments() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("demo");
        std::fs::create_dir_all(target.join("src")).unwrap();
        std::fs::write(
            target.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='1.0.0'\n",
        )
        .unwrap();
        let source = target.join("src/lib.rs");
        std::fs::write(&source, "pub const VALUE: u8 = 1;\n").unwrap();
        let running = crate::source_build::source_identity(&target).unwrap();
        let mut plugin = sample_store_plugin();
        mark_catalog_state(&mut plugin, &target, Some(&running), true);
        assert!(!plugin.pending_restart);

        std::fs::write(&source, "pub const VALUE: u8 = 2;\n").unwrap();
        mark_catalog_state(&mut plugin, &target, Some(&running), true);
        assert!(
            plugin.pending_restart,
            "same-version source edits must be detected"
        );
        mark_catalog_state(&mut plugin, &target, Some(&running), false);
        assert!(
            !plugin.pending_restart,
            "exported programs cannot apply local source edits"
        );

        mark_catalog_state(&mut plugin, &target, None, true);
        assert!(
            plugin.pending_restart,
            "new source plugins need to be loaded"
        );
        mark_catalog_state(&mut plugin, &target, None, false);
        assert!(!plugin.pending_restart);
    }

    #[test]
    fn commit_validation_requires_full_hex_sha() {
        assert!(is_valid_commit("0123456789abcdef0123456789abcdef01234567"));
        assert!(
            !is_valid_commit("0123456789abcdef"),
            "短 SHA 有歧义，必须拒绝"
        );
        assert!(!is_valid_commit("0123456789abcdef0123456789abcdef0123456g"));
        assert!(!is_valid_commit(""));
    }

    #[test]
    fn store_source_cannot_be_replaced_by_install_body() {
        let plugin = sample_store_plugin();
        assert!(select_repo(Some("https://evil.example/demo.git"), Some(&plugin)).is_err());
        assert_eq!(select_repo(None, Some(&plugin)).unwrap(), plugin.repo_url);
        assert_eq!(
            select_repo(Some(&plugin.repo_url), Some(&plugin)).unwrap(),
            plugin.repo_url
        );
    }

    #[test]
    fn local_toml_is_parsed_as_toml_and_invalid_json_is_rejected() {
        let root =
            std::env::temp_dir().join(format!("nuono-local-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("plugin.toml"),
            "[plugin]\nname = 'A = B' # a comment\nrepo = 'https://example.com/plugin.git'\n",
        )
        .unwrap();
        assert_eq!(
            read_store_plugin(&root, "demo").unwrap().unwrap().name,
            "A = B"
        );
        std::fs::write(root.join("plugin.json"), "{invalid = 'not json'}").unwrap();
        assert!(read_store_plugin(&root, "demo").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn update_without_tags_detects_commit_and_failed_build_restores_source_and_pin() {
        let root =
            std::env::temp_dir().join(format!("nuono-store-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        successful_git(git_at(&root, &["-c", "init.templateDir=", "init"]).await).unwrap();
        let commit = || {
            [
                "-c",
                "user.name=Nuono Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "fixture",
            ]
        };
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='1.0.0'\n",
        )
        .unwrap();
        successful_git(git_at(&root, &["add", "Cargo.toml"]).await).unwrap();
        successful_git(git_at(&root, &commit()).await).unwrap();
        let old = installed_commit(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='1.0.1'\n",
        )
        .unwrap();
        successful_git(git_at(&root, &["add", "Cargo.toml"]).await).unwrap();
        successful_git(git_at(&root, &commit()).await).unwrap();
        let new = installed_commit(&root).unwrap();
        successful_git(git_at(&root, &["checkout", "--detach", &old]).await).unwrap();
        std::fs::write(root.join(".nuono-pin"), format!("{old}\n")).unwrap();
        let mut plugin = sample_store_plugin();
        plugin.pinned_commit = Some(new.clone());
        mark_installed(&mut plugin, &root);
        assert!(
            plugin.update_available,
            "update detection must work without tags"
        );
        assert_eq!(plugin.installed_version.as_deref(), Some("1.0.0"));
        let error = checkout_and_build(&root, &new, |path| async move {
            assert!(
                std::fs::read_to_string(path.join("Cargo.toml"))
                    .unwrap()
                    .contains("1.0.1")
            );
            Err("fixture compile failure".into())
        })
        .await
        .unwrap_err();
        assert!(error.contains("已恢复旧源码"));
        assert_eq!(installed_commit(&root).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(root.join(".nuono-pin")).unwrap(),
            format!("{old}\n")
        );
        checkout_and_build(&root, &new, |_| async { Ok(()) })
            .await
            .unwrap();
        mark_installed(&mut plugin, &root);
        assert!(!plugin.update_available);
        assert_eq!(
            std::fs::read_to_string(root.join(".nuono-pin")).unwrap(),
            format!("{new}\n")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
