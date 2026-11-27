//! 已安装源码插件的本地 Git 历史查询与版本校验（实际切换版本与重新编译仍走持久化构建任务）。
use super::{
    OperationContext, StorePlugin, find_store_plugin, is_valid_commit, is_valid_repo_url, jobs,
    plugin_target, read_store_plugin, store_root,
};
use crate::operation_error::{OperationError as OpError, Result as OpResult};
use crate::source_build::{self, SourcePlugin};
use serde::{Deserialize, Serialize};
use std::{path::Path as FsPath, time::Duration};

#[cfg(test)]
#[path = "../../tests/unit/source_operations/git_tests.rs"]
mod tests;

const PAGE_SIZE: usize = 20;
const MAX_OFFSET: usize = 2000;

#[derive(Default, Deserialize)]
pub(crate) struct HistoryQuery {
    pub reference: Option<String>,
    #[serde(default)]
    pub offset: usize,
}
#[derive(Serialize, utoipa::ToSchema)]
struct GitRef {
    name: String,
    kind: String,
    commit: String,
}
#[derive(Serialize, utoipa::ToSchema)]
struct GitCommit {
    commit: String,
    subject: String,
    author: String,
    date: String,
}
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct GitHistory {
    plugin_id: String,
    available: bool,
    reason: Option<String>,
    repo_url: Option<String>,
    branch: Option<String>,
    source_commit: Option<String>,
    running_commit: Option<String>,
    source_version: Option<String>,
    running_version: Option<String>,
    pending_restart: bool,
    dirty: bool,
    can_switch: bool,
    switch_limitation: Option<String>,
    refs: Vec<GitRef>,
    commits: Vec<GitCommit>,
    offset: usize,
    next_offset: Option<usize>,
    reference: Option<String>,
}
/// 直接调用 `git` 子进程并限制输出大小与执行超时（含 `git fetch`），绝不通过 shell 执行。
pub(crate) fn git_sync(
    target: &FsPath,
    args: &[String],
    timeout: Duration,
) -> Result<String, String> {
    nuono_tooling::git::run(target, args, timeout).map_err(|error| {
        let detail = nuono_core::telemetry::sanitize_file_line(error.stderr()).0;
        if detail.is_empty() {
            error.to_string()
        } else {
            detail
        }
    })
}

pub(crate) use nuono_tooling::git::configure_command;

pub(crate) async fn run_git(target: &FsPath, args: &[&str]) -> Result<String, String> {
    let target = target.to_path_buf();
    let timeout = if args.first() == Some(&"fetch") {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(15)
    };
    let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || git_sync(&target, &args, timeout))
        .await
        .map_err(|_| "Git 操作执行失败".to_string())?
}

pub(crate) async fn independent_repository(target: &FsPath) -> Result<(), String> {
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || independent_repository_sync(&target))
        .await
        .map_err(|_| "Git 仓库检查失败".to_string())?
}

pub(crate) use nuono_tooling::git::independent_repository_sync;

pub(crate) async fn source_dirty(target: &FsPath) -> Result<bool, String> {
    Ok(
        run_git(target, &["status", "--porcelain", "--untracked-files=all"])
            .await?
            .lines()
            .any(|line| !line.is_empty() && line != "?? .nuono-pin"),
    )
}

pub(crate) async fn current_commit(target: &FsPath) -> Result<String, String> {
    let commit = run_git(target, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await?
        .trim()
        .to_string();
    if !is_valid_commit(&commit) {
        return Err("当前插件 HEAD 无效".into());
    }
    Ok(commit)
}

pub(crate) async fn verify_commit(target: &FsPath, commit: &str) -> Result<(), String> {
    if !is_valid_commit(commit) {
        return Err("版本必须是完整的 40 位 Git commit".into());
    }
    let resolved = run_git(
        target,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )
    .await?;
    if !resolved.trim().eq_ignore_ascii_case(commit) {
        return Err("所选版本不是有效的 commit".into());
    }
    Ok(())
}

pub(crate) async fn expected_head(target: &FsPath, expected: &str) -> Result<(), String> {
    if !is_valid_commit(expected) {
        return Err("当前版本必须是完整的 40 位 Git commit".into());
    }
    if !current_commit(target).await?.eq_ignore_ascii_case(expected) {
        return Err("插件源码版本已经变化，请刷新版本记录后重新选择".into());
    }
    Ok(())
}

pub(crate) async fn origin(target: &FsPath) -> Result<String, String> {
    let url = run_git(target, &["remote", "get-url", "origin"])
        .await?
        .trim()
        .to_string();
    if !is_valid_repo_url(&url) {
        return Err("插件 origin 地址无效，请在终端设置无密码的 HTTPS 或 SSH Git 地址".into());
    }
    Ok(url)
}

fn safe_repo_url(repo: &str) -> Option<String> {
    if !is_valid_repo_url(repo) {
        return None;
    }
    if repo.starts_with("git@") {
        return Some(repo.into());
    }
    let mut parsed = url::Url::parse(repo).ok()?;
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

/// 校验目标仓库地址与 commit 是否有效，并确认与商店记录仓库一致。
pub(crate) fn target_policy(
    plugin: Option<&StorePlugin>,
    repo: &str,
    commit: &str,
) -> Result<(), String> {
    if !is_valid_repo_url(repo) || !is_valid_commit(commit) {
        return Err("仓库地址或目标版本无效".into());
    }
    let plugin = plugin.filter(|p| !p.repo_url.is_empty());
    if plugin.is_some_and(|p| p.repo_url != repo) {
        return Err("已安装插件的仓库与商店发布记录不同，已停止操作".into());
    }
    Ok(())
}

fn text_field(value: &str) -> String {
    nuono_core::telemetry::sanitize_file_line(value)
        .0
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}
fn parse_refs(output: &str) -> Vec<GitRef> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split('\0').collect::<Vec<_>>();
            if fields.len() != 5 {
                return None;
            }
            let (kind, name) = if let Some(name) = fields[0].strip_prefix("refs/tags/") {
                ("tag", name)
            } else if let Some(name) = fields[0].strip_prefix("refs/heads/") {
                ("branch", name)
            } else {
                let name = fields[0].strip_prefix("refs/remotes/")?;
                if name.ends_with("/HEAD") {
                    return None;
                }
                ("branch", name)
            };
            let commit = if fields[3] == "commit" {
                fields[1]
            } else if fields[4] == "commit" {
                fields[2]
            } else {
                return None;
            };
            is_valid_commit(commit).then(|| GitRef {
                name: text_field(name),
                kind: kind.into(),
                commit: commit.into(),
            })
        })
        .collect()
}
fn parse_commits(output: &str) -> Vec<GitCommit> {
    let fields = output.split('\0').collect::<Vec<_>>();
    fields
        .as_chunks::<4>()
        .0
        .iter()
        .filter_map(|row| {
            let commit = row[0].trim();
            is_valid_commit(commit).then(|| GitCommit {
                commit: commit.into(),
                subject: text_field(row[1]),
                author: text_field(row[2]),
                date: text_field(row[3]),
            })
        })
        .collect()
}

async fn local_history(
    id: &str,
    target: &FsPath,
    running: Option<&SourcePlugin>,
    query: &HistoryQuery,
) -> OpResult<GitHistory> {
    if query.offset > MAX_OFFSET {
        return Err(OpError::invalid("历史记录页码超出范围"));
    }
    if query
        .reference
        .as_deref()
        .is_some_and(|value| !is_valid_commit(value))
    {
        return Err(OpError::invalid("历史范围必须是完整的 40 位 Git commit"));
    }
    let mut view = GitHistory {
        plugin_id: id.into(),
        available: false,
        reason: None,
        repo_url: None,
        branch: None,
        source_commit: None,
        running_commit: running.and_then(|p| p.commit.clone()),
        source_version: None,
        running_version: running.map(|p| p.version.clone()),
        pending_restart: false,
        dirty: false,
        can_switch: false,
        switch_limitation: None,
        refs: Vec::new(),
        commits: Vec::new(),
        offset: query.offset,
        next_offset: None,
        reference: None,
    };
    view.source_version = std::fs::read_to_string(target.join("Cargo.toml"))
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|manifest| {
            manifest
                .get("package")?
                .get("version")?
                .as_str()
                .map(str::to_string)
        });
    if let Err(error) = independent_repository(target).await {
        view.reason = Some(error.clone());
        view.switch_limitation = Some(error);
        return Ok(view);
    }
    if let Ok(source) = source_build::source_identity(target) {
        view.pending_restart = running
            .is_none_or(|p| p.source_hash != source.source_hash || p.commit != source.commit);
    }
    let head = current_commit(target).await?;
    let reference = if let Some(reference) = &query.reference {
        verify_commit(target, reference)
            .await
            .map_err(OpError::invalid)?;
        reference.to_ascii_lowercase()
    } else {
        run_git(
            target,
            &["rev-parse", "--verify", "refs/remotes/origin/HEAD^{commit}"],
        )
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_valid_commit(s))
        .unwrap_or_else(|| head.clone())
    };
    view.available = true;
    view.source_commit = Some(head);
    view.reference = Some(reference.clone());
    view.branch = run_git(target, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .map(|s| text_field(s.trim()));
    view.repo_url = origin(target).await.ok().and_then(|s| safe_repo_url(&s));
    view.dirty = source_dirty(target).await?;
    view.refs = parse_refs(&run_git(target, &["for-each-ref", "--count=256", "--sort=refname", "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(objecttype)%00%(*objecttype)", "refs/heads/", "refs/remotes/", "refs/tags/"]).await?);
    let log = run_git(
        target,
        &[
            "log",
            "--max-count=21",
            &format!("--skip={}", query.offset),
            "--format=%H%x00%s%x00%an%x00%cI%x00",
            &reference,
            "--",
        ],
    )
    .await?;
    view.commits = parse_commits(&log);
    if view.commits.len() > PAGE_SIZE && query.offset + PAGE_SIZE <= MAX_OFFSET {
        view.next_offset = Some(query.offset + PAGE_SIZE);
    }
    view.commits.truncate(PAGE_SIZE);
    Ok(view)
}

pub(crate) async fn history_view(
    state: &OperationContext,
    id: &str,
    query: &HistoryQuery,
) -> OpResult<GitHistory> {
    let target = plugin_target(id).map_err(OpError::invalid)?;
    let running = source_build::running_manifest()
        .into_iter()
        .find(|p| super::source_key(p.kind, &p.id) == id);
    let mut view = local_history(id, &target, running.as_ref(), query).await?;
    if !view.available {
        return Ok(view);
    }
    let limitation = if let Err(error) = jobs::source_root() {
        Some(error.to_string())
    } else if !jobs::tool_exists("git") || !jobs::tool_exists("cargo") {
        Some("切换版本需要 Git 和 Cargo".into())
    } else if view.dirty {
        Some("插件有未提交的本地修改，请先备份或提交修改".into())
    } else {
        let cfg = state.config.snapshot().await;
        match (
            origin(&target).await,
            local_published_plugin(id, &cfg.plugin_store).await,
        ) {
            (Err(error), _) | (_, Err(error)) => Some(error),
            (Ok(repo), Ok(plugin)) => {
                let candidate = plugin
                    .as_ref()
                    .and_then(|p| p.pinned_commit.as_deref())
                    .or(view.source_commit.as_deref())
                    .unwrap_or_default();
                target_policy(plugin.as_ref(), &repo, candidate).err()
            }
        }
    };
    view.can_switch = limitation.is_none();
    view.switch_limitation = limitation;
    Ok(view)
}

async fn local_published_plugin(
    id: &str,
    cfg: &nuono_core::config::PluginStoreConfig,
) -> Result<Option<StorePlugin>, String> {
    let root = store_root()?;
    let remote = if let Some(url) = cfg.index_url.as_deref() {
        let cached = crate::plugin_catalog::cached_plugins(&root, url)
            .await?
            .ok_or("商店发布记录尚未缓存，请先刷新商店目录")?;
        cached
            .into_iter()
            .find(|p| super::source_key(p.kind, &p.id) == id)
            .map(StorePlugin::from)
    } else {
        None
    };
    let (kind, raw_id) = super::parse_source_key(id)?;
    let local = if kind == source_build::SourceKind::Plugin {
        read_store_plugin(&root.join("plugins_store").join(&raw_id), &raw_id)?
    } else {
        None
    };
    if remote.is_some() && local.is_some() {
        return Err("本地与远端商店插件 ID 冲突".into());
    }
    Ok(remote.or(local))
}

pub(crate) async fn refresh_git(state: &OperationContext, id: &str) -> OpResult<String> {
    async {
        if super::parse_source_key(id).is_err() {
            return Err(OpError::invalid("插件 ID 无效"));
        }
        jobs::source_root()?;
        let _guard = source_build::operation_lock()
            .try_lock_owned()
            .map_err(|_| OpError::conflict("已有源码操作正在运行，请稍后刷新"))?;
        let target = plugin_target(id).map_err(OpError::invalid)?;
        independent_repository(&target)
            .await
            .map_err(OpError::conflict)?;
        let repo = origin(&target).await?;
        let cfg = state.config.snapshot().await;
        if find_store_plugin(id, &cfg.plugin_store)
            .await?
            .filter(|p| !p.repo_url.is_empty())
            .is_some_and(|p| p.repo_url != repo)
        {
            return Err(OpError::conflict(
                "已安装插件的仓库与商店发布记录不同，已停止刷新",
            ));
        }
        run_git(
            &target,
            &["fetch", "--tags", "--no-recurse-submodules", "origin"],
        )
        .await
        .map_err(OpError::upstream)?;
        Ok("Git 版本记录已刷新，插件源码未切换".to_string())
    }
    .await
}

pub(crate) async fn history_by_name(
    state: &OperationContext,
    name: &str,
    query: &HistoryQuery,
) -> OpResult<GitHistory> {
    let source = source_build::running_manifest()
        .into_iter()
        .find(|source| source.runtime_name.as_deref() == Some(name))
        .ok_or_else(|| OpError::missing("运行插件没有可对应的源码版本记录"))?;
    history_view(state, &source.id, query).await
}
