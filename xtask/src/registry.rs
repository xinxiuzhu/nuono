use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_FIELD_CHARS: usize = 2048;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    #[default]
    Plugin,
    Adapter,
}

/// 存放在 `registry/entries/` 中的极简仓库登记条目。
///
/// 必填项只有公开 GitHub 仓库地址，其余字段均可在构建目录时从仓库 `Cargo.toml`
/// 自动提取，也可按需填写用于覆盖默认值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ExtensionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// 发布到 `catalog` 分支目录文件中的完整扩展元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    #[serde(default)]
    pub kind: ExtensionKind,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub repo: String,
    pub commit: String,
}

impl RegistryEntry {
    pub fn validate(&self, filename: Option<&str>) -> Result<String> {
        let normalized_repo = normalize_github_repo(&self.repo)?;
        if let Some(id) = &self.id {
            validate_id(id)?;
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("author", &self.author),
            ("license", &self.license),
        ] {
            if let Some(value) = value {
                validate_text(field, value)?;
            }
        }
        if let Some(commit) = &self.commit {
            ensure!(
                is_lowercase_sha(commit),
                "commit 必须是 40 位小写十六进制 SHA: {commit}"
            );
        }
        if let Some(filename) = filename {
            let stem = filename
                .strip_suffix(".json")
                .with_context(|| format!("条目文件必须以 .json 结尾: {filename}"))?;
            let (file_kind, file_id) = stem
                .strip_prefix("adapter.")
                .map_or((ExtensionKind::Plugin, stem), |id| {
                    (ExtensionKind::Adapter, id)
                });
            validate_id(file_id)?;
            if let Some(kind) = self.kind {
                ensure!(
                    kind == file_kind,
                    "条目文件名 {filename} 与 kind={kind:?} 不一致"
                );
            }
            if let Some(id) = &self.id {
                ensure!(id == file_id, "条目文件名 {filename} 与 id={id} 不一致");
            }
        }
        Ok(normalized_repo)
    }
}

impl CatalogEntry {
    pub fn filename(&self) -> String {
        match self.kind {
            ExtensionKind::Plugin => format!("{}.json", self.id),
            ExtensionKind::Adapter => format!("adapter.{}.json", self.id),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("author", &self.author),
        ] {
            validate_text(field, value)?;
        }
        if let Some(license) = &self.license {
            validate_text("license", license)?;
        }
        ensure!(
            is_exact_semver(&self.version),
            "{}: version 必须是完整 SemVer（如 0.1.0）",
            self.id
        );
        ensure!(
            is_lowercase_sha(&self.commit),
            "{}: commit 必须是 40 位小写十六进制 SHA",
            self.id
        );
        normalize_github_repo(&self.repo)?;
        Ok(())
    }
}

fn validate_text(field: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.trim() == value
            && value.chars().count() <= MAX_FIELD_CHARS
            && !value.contains('\0'),
        "{field} 必须为非空文本且不超过 {MAX_FIELD_CHARS} 字符"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    let mut bytes = id.bytes();
    ensure!(
        !id.is_empty()
            && id.len() <= 80
            && !id.eq_ignore_ascii_case("hub")
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bytes
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')),
        "扩展 ID `{id}` 仅允许以小写字母或数字开头，包含小写字母、数字、`-`、`_`，且不能为 hub"
    );
    Ok(())
}

fn numeric_part(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && (part.len() == 1 || !part.starts_with('0'))
}

fn is_exact_semver(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(v, b)| (v, Some(b)));
    let (base, pre) = without_build
        .split_once('-')
        .map_or((without_build, None), |(v, p)| (v, Some(p)));
    let parts: Vec<_> = base.split('.').collect();
    if parts.len() != 3 || !parts.into_iter().all(numeric_part) {
        return false;
    }
    for (label, is_pre) in [(build, false), (pre, true)] {
        if let Some(label) = label {
            for segment in label.split('.') {
                if segment.is_empty()
                    || !segment
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || (is_pre
                        && segment.bytes().all(|b| b.is_ascii_digit())
                        && !numeric_part(segment))
                {
                    return false;
                }
            }
        }
    }
    true
}

fn is_lowercase_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse_github_owner_repo(input: &str) -> Result<(&str, &str)> {
    let trimmed = input.trim().trim_end_matches('/');
    let path = trimmed
        .strip_prefix("https://github.com/")
        .context("只接受 https://github.com/<owner>/<repo> 格式的公开 GitHub 仓库地址")?;
    let (owner, raw_repo) = path
        .split_once('/')
        .context("GitHub 仓库地址缺少 owner/repo")?;
    ensure!(
        !raw_repo.contains('/'),
        "GitHub 仓库地址只能包含 owner/repo，不能附带子路径"
    );
    let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
    ensure!(
        !owner.is_empty()
            && owner.len() <= 39
            && owner.as_bytes()[0].is_ascii_alphanumeric()
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !repo.is_empty()
            && !matches!(repo, "." | "..")
            && repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')),
        "无效的 GitHub 仓库地址: {input}"
    );
    Ok((owner, repo))
}

pub fn normalize_github_repo(input: &str) -> Result<String> {
    let (owner, repo) = parse_github_owner_repo(input)?;
    Ok(format!("https://github.com/{owner}/{repo}.git"))
}

pub fn entries_dir(root: &Path) -> PathBuf {
    root.join("registry").join("entries")
}

pub fn load_entries(root: &Path) -> Result<Vec<(String, RegistryEntry)>> {
    let dir = entries_dir(root);
    ensure!(dir.is_dir(), "缺少扩展目录 {}", dir.display());
    let mut files = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    files.sort_by_key(|entry| entry.file_name());
    let mut entries = Vec::new();
    let mut seen_repos = BTreeSet::new();
    for file in files {
        let name = file
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("文件名必须是 UTF-8"))?;
        if name == ".gitkeep" {
            continue;
        }
        ensure!(
            name.ends_with(".json") && !name.starts_with('.'),
            "registry/entries 中不支持的文件: {name}"
        );
        let raw = fs::read_to_string(file.path())
            .with_context(|| format!("读取 {} 失败", file.path().display()))?;
        let mut entry: RegistryEntry = serde_json::from_str(&raw)
            .with_context(|| format!("解析 {} 失败", file.path().display()))?;
        let normalized_repo = entry.validate(Some(&name))?;
        ensure!(
            seen_repos.insert(normalized_repo.to_ascii_lowercase()),
            "重复登记了同一个仓库: {normalized_repo}"
        );
        entry.repo = normalized_repo;
        entries.push((name, entry));
    }
    Ok(entries)
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .with_context(|| format!("执行 git {:?} 失败", args))?;
    ensure!(
        output.status.success(),
        "git {:?} 执行失败: {}",
        args,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn clone_repo(source: &str, commit: Option<&str>, dest: &Path) -> Result<String> {
    let dest_str = dest.to_str().context("临时目录路径包含非法 UTF-8 字符")?;
    if let Some(commit) = commit {
        run_git(None, &["init", "--quiet", dest_str])?;
        run_git(Some(dest), &["remote", "add", "origin", source])?;
        if run_git(Some(dest), &["fetch", "--depth", "1", "origin", commit]).is_err() {
            run_git(Some(dest), &["fetch", "--no-tags", "origin"])?;
        }
        run_git(Some(dest), &["checkout", "--detach", commit])?;
    } else {
        run_git(
            None,
            &[
                "clone",
                "--depth",
                "1",
                "--no-recurse-submodules",
                "--",
                source,
                dest_str,
            ],
        )?;
    }
    let head = run_git(Some(dest), &["rev-parse", "HEAD"])?
        .trim()
        .to_ascii_lowercase();
    ensure!(
        is_lowercase_sha(&head),
        "仓库 HEAD 不是有效 40 位 SHA: {head}"
    );
    Ok(head)
}

fn clean_author(raw: &str) -> Option<String> {
    let base = raw.split('<').next().unwrap_or(raw).trim();
    (!base.is_empty()).then(|| base.to_owned())
}

fn detect_kind(manifest: &toml::Value) -> Result<ExtensionKind> {
    if let Some(kind) = manifest
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("nuono"))
        .and_then(|n| n.get("kind"))
        .and_then(toml::Value::as_str)
    {
        return match kind {
            "plugin" => Ok(ExtensionKind::Plugin),
            "adapter" => Ok(ExtensionKind::Adapter),
            other => bail!(
                "Cargo.toml [package.metadata.nuono].kind 必须是 plugin 或 adapter，收到: {other}"
            ),
        };
    }
    let deps = manifest.get("dependencies").and_then(toml::Value::as_table);
    let has_adapter = deps.is_some_and(|d| d.contains_key("nuono-adapter"));
    let has_plugin = deps.is_some_and(|d| d.contains_key("nuono-plugin"));
    match (has_plugin, has_adapter) {
        (false, true) => Ok(ExtensionKind::Adapter),
        (true, false) => Ok(ExtensionKind::Plugin),
        (true, true) => bail!(
            "Cargo.toml 同时依赖了 nuono-plugin 与 nuono-adapter，请在 [package.metadata.nuono] 中显式声明 kind"
        ),
        (false, false) => bail!(
            "无法识别扩展类型：仓库 Cargo.toml 未依赖 nuono-plugin / nuono-adapter，也未声明 [package.metadata.nuono].kind"
        ),
    }
}

fn resolve_entry_from_source(entry: &RegistryEntry, clone_source: &str) -> Result<CatalogEntry> {
    let normalized_repo = entry.validate(None)?;
    let (owner, _) = parse_github_owner_repo(&normalized_repo)?;
    let temp = tempfile::tempdir().context("创建临时目录失败")?;
    let checkout_dir = temp.path().join("repo");
    let commit = clone_repo(clone_source, entry.commit.as_deref(), &checkout_dir)
        .with_context(|| format!("拉取仓库失败: {normalized_repo}"))?;

    let manifest_path = checkout_dir.join("Cargo.toml");
    let manifest_raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("仓库根目录缺少 Cargo.toml: {normalized_repo}"))?;
    let manifest: toml::Value = toml::from_str(&manifest_raw)
        .with_context(|| format!("解析仓库 Cargo.toml 失败: {normalized_repo}"))?;
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .with_context(|| format!("仓库 Cargo.toml 缺少 [package]: {normalized_repo}"))?;
    let nuono_meta = package
        .get("metadata")
        .and_then(|m| m.get("nuono"))
        .and_then(toml::Value::as_table);

    let meta_str = |key: &str| {
        nuono_meta
            .and_then(|m| m.get(key))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let pkg_str = |key: &str| {
        package
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    let package_name = pkg_str("name")
        .with_context(|| format!("仓库 Cargo.toml 缺少 package.name: {normalized_repo}"))?;
    let version = pkg_str("version")
        .with_context(|| format!("仓库 Cargo.toml 缺少 package.version: {normalized_repo}"))?;
    let kind = match entry.kind {
        Some(kind) => kind,
        None => detect_kind(&manifest)?,
    };
    let id = entry
        .id
        .clone()
        .or_else(|| meta_str("id"))
        .unwrap_or_else(|| package_name.to_ascii_lowercase());
    let name = entry
        .name
        .clone()
        .or_else(|| meta_str("name"))
        .unwrap_or(package_name);
    let description = entry
        .description
        .clone()
        .or_else(|| meta_str("description"))
        .or_else(|| pkg_str("description"))
        .unwrap_or_else(|| match kind {
            ExtensionKind::Adapter => "Nuono 平台适配器".to_owned(),
            ExtensionKind::Plugin => "Nuono 业务插件".to_owned(),
        });
    let author = entry
        .author
        .clone()
        .or_else(|| {
            package
                .get("authors")
                .and_then(toml::Value::as_array)
                .and_then(|items| items.first())
                .and_then(toml::Value::as_str)
                .and_then(clean_author)
        })
        .unwrap_or_else(|| owner.to_owned());
    let license = entry.license.clone().or_else(|| pkg_str("license"));

    let resolved = CatalogEntry {
        kind,
        id,
        name,
        version,
        description,
        author,
        license,
        repo: normalized_repo,
        commit,
    };
    resolved.validate()?;
    Ok(resolved)
}

fn render_catalog(entries: &[CatalogEntry], v2: bool) -> Result<Value> {
    let mut items = Vec::new();
    for entry in entries {
        if !v2 && entry.kind != ExtensionKind::Plugin {
            continue;
        }
        entry.validate()?;
        let mut value = serde_json::to_value(entry)?;
        if !v2 && let Some(map) = value.as_object_mut() {
            map.remove("kind");
        }
        items.push(value);
    }
    let catalog = if v2 {
        json!({
            "schema_version": 2,
            "extensions": items,
        })
    } else {
        json!({
            "schema_version": 1,
            "plugins": items,
        })
    };
    ensure!(
        serde_json::to_string_pretty(&catalog)?.len() < MAX_CATALOG_BYTES,
        "生成的扩展目录超过 2 MiB 上限"
    );
    Ok(catalog)
}

pub fn validate(root: &Path) -> Result<usize> {
    let entries = load_entries(root)?;
    println!(
        "Extension registry validation passed ({} entries).",
        entries.len()
    );
    Ok(entries.len())
}

fn build_with_resolver<F>(root: &Path, out_dir: &Path, mut resolve: F) -> Result<usize>
where
    F: FnMut(&RegistryEntry) -> Result<CatalogEntry>,
{
    let raw_entries = load_entries(root)?;
    let mut resolved = Vec::with_capacity(raw_entries.len());
    let mut seen = BTreeSet::new();
    for (filename, entry) in raw_entries {
        let item =
            resolve(&entry).with_context(|| format!("同步条目 {filename} 对应的仓库元数据失败"))?;
        ensure!(
            filename == item.filename(),
            "条目文件名 {filename} 与解析出的 {} 不一致",
            item.filename()
        );
        ensure!(
            seen.insert((item.kind, item.id.to_ascii_lowercase())),
            "重复的扩展 ID: {}",
            item.id
        );
        resolved.push(item);
    }
    resolved.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    let v2 = render_catalog(&resolved, true)?;
    let v1 = render_catalog(&resolved, false)?;
    fs::create_dir_all(out_dir)?;
    fs::write(
        out_dir.join("extensions-v2.json"),
        format!("{}\n", serde_json::to_string_pretty(&v2)?),
    )?;
    fs::write(
        out_dir.join("catalog.json"),
        format!("{}\n", serde_json::to_string_pretty(&v1)?),
    )?;
    println!(
        "Built extension catalog ({} entries) into {}",
        resolved.len(),
        out_dir.display()
    );
    Ok(resolved.len())
}

pub fn build(root: &Path, out_dir: &Path) -> Result<usize> {
    build_with_resolver(root, out_dir, |entry| {
        resolve_entry_from_source(entry, &entry.repo)
    })
}

pub fn parse_issue_body(body: &str) -> Result<RegistryEntry> {
    ensure!(body.chars().count() <= 20_000, "Issue 表单内容过大");
    let normalized = body.replace("\r\n", "\n");
    let mut fields = BTreeMap::<String, String>::new();
    let mut current: Option<String> = None;
    for line in normalized.lines() {
        if let Some(heading) = line.strip_prefix("### ") {
            let key = heading.trim().to_owned();
            ensure!(!fields.contains_key(&key), "重复的表单字段: {key}");
            fields.insert(key.clone(), String::new());
            current = Some(key);
        } else if let Some(key) = &current {
            let value = fields.get_mut(key).context("表单字段缺失")?;
            value.push_str(line);
            value.push('\n');
        }
    }
    let optional_field = |label: &str| -> Option<String> {
        let value = fields.get(label)?.trim();
        if value.is_empty() || value == "_No response_" {
            None
        } else {
            Some(value.to_owned())
        }
    };
    let repo_raw =
        optional_field("GitHub 源码仓库").context("表单字段「GitHub 源码仓库」不能为空")?;
    let entry = RegistryEntry {
        repo: normalize_github_repo(&repo_raw)?,
        kind: None,
        id: None,
        name: optional_field("展示名称"),
        description: optional_field("简介"),
        author: None,
        license: None,
        commit: None,
    };
    entry.validate(None)?;
    Ok(entry)
}

fn import_issue_with_source(
    root: &Path,
    issue_body_file: &Path,
    clone_source: Option<&str>,
) -> Result<PathBuf> {
    let body = fs::read_to_string(issue_body_file)
        .with_context(|| format!("读取 Issue 内容失败: {}", issue_body_file.display()))?;
    let entry = parse_issue_body(&body)?;
    let source = clone_source.unwrap_or(entry.repo.as_str());
    let resolved = resolve_entry_from_source(&entry, source)?;
    let dir = entries_dir(root);
    fs::create_dir_all(&dir)?;
    let path = dir.join(resolved.filename());
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&entry)?),
    )?;
    validate(root)?;
    println!("{}", path.display());
    Ok(path)
}

pub fn import_issue(root: &Path, issue_body_file: &Path) -> Result<PathBuf> {
    import_issue_with_source(root, issue_body_file, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_git_repo(dir: &Path, cargo_toml: &str) -> String {
        fs::create_dir_all(dir).unwrap();
        run_git(None, &["init", "--quiet", dir.to_str().unwrap()]).unwrap();
        fs::write(dir.join("Cargo.toml"), cargo_toml).unwrap();
        run_git(Some(dir), &["add", "Cargo.toml"]).unwrap();
        run_git(
            Some(dir),
            &[
                "-c",
                "user.name=Nuono Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "init",
            ],
        )
        .unwrap();
        run_git(Some(dir), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned()
    }

    const SAMPLE_PLUGIN_ISSUE: &str = r#"### GitHub 源码仓库

https://github.com/alice/demo-plugin

### 展示名称

_No response_

### 简介

_No response_
"#;

    const SAMPLE_ADAPTER_ISSUE: &str = r#"### GitHub 源码仓库

https://github.com/bob/nuono-adapter-onebot

### 展示名称

OneBot 适配器

### 简介

连接 OneBot v11 协议端
"#;

    #[test]
    fn minimal_issue_auto_fetches_plugin_and_adapter_metadata_and_builds_catalogs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir_all(entries_dir(&root)).unwrap();

        let plugin_repo = temp.path().join("demo-plugin-repo");
        let plugin_sha = create_git_repo(
            &plugin_repo,
            r#"[package]
name = "demo-plugin"
version = "0.2.0"
description = "自动从 Cargo.toml 读取的简介"
license = "MIT"
authors = ["Alice <alice@example.com>"]

[dependencies]
nuono-plugin = { path = "../../crates/nuono-plugin" }
"#,
        );

        let adapter_repo = temp.path().join("onebot-adapter-repo");
        let adapter_sha = create_git_repo(
            &adapter_repo,
            r#"[package]
name = "onebot"
version = "0.1.0"
license = "GPL-3.0-only"

[package.metadata.nuono]
kind = "adapter"

[dependencies]
nuono-adapter = { path = "../../crates/nuono-adapter" }
"#,
        );

        let plugin_issue = temp.path().join("plugin-issue.md");
        fs::write(&plugin_issue, SAMPLE_PLUGIN_ISSUE).unwrap();
        let written_plugin =
            import_issue_with_source(&root, &plugin_issue, Some(plugin_repo.to_str().unwrap()))
                .unwrap();
        assert_eq!(written_plugin, entries_dir(&root).join("demo-plugin.json"));

        let adapter_issue = temp.path().join("adapter-issue.md");
        fs::write(&adapter_issue, SAMPLE_ADAPTER_ISSUE).unwrap();
        let written_adapter =
            import_issue_with_source(&root, &adapter_issue, Some(adapter_repo.to_str().unwrap()))
                .unwrap();
        assert_eq!(
            written_adapter,
            entries_dir(&root).join("adapter.onebot.json")
        );

        let out = temp.path().join("out");
        let count = build_with_resolver(&root, &out, |entry| {
            let source = if entry.repo.contains("demo-plugin") {
                plugin_repo.to_str().unwrap()
            } else {
                adapter_repo.to_str().unwrap()
            };
            resolve_entry_from_source(entry, source)
        })
        .unwrap();
        assert_eq!(count, 2);

        let v2: Value =
            serde_json::from_str(&fs::read_to_string(out.join("extensions-v2.json")).unwrap())
                .unwrap();
        assert_eq!(v2["schema_version"], 2);
        assert_eq!(v2["extensions"].as_array().unwrap().len(), 2);

        let p = &v2["extensions"][0];
        assert_eq!(p["kind"], "plugin");
        assert_eq!(p["id"], "demo-plugin");
        assert_eq!(p["name"], "demo-plugin");
        assert_eq!(p["version"], "0.2.0");
        assert_eq!(p["author"], "Alice");
        assert_eq!(p["license"], "MIT");
        assert_eq!(p["description"], "自动从 Cargo.toml 读取的简介");
        assert_eq!(p["commit"], plugin_sha);
        assert_eq!(p["repo"], "https://github.com/alice/demo-plugin.git");
        assert!(p.get("nuono_version").is_none());

        let a = &v2["extensions"][1];
        assert_eq!(a["kind"], "adapter");
        assert_eq!(a["id"], "onebot");
        assert_eq!(a["name"], "OneBot 适配器");
        assert_eq!(a["author"], "bob");
        assert_eq!(a["description"], "连接 OneBot v11 协议端");
        assert_eq!(a["commit"], adapter_sha);

        let v1: Value =
            serde_json::from_str(&fs::read_to_string(out.join("catalog.json")).unwrap()).unwrap();
        assert_eq!(v1["schema_version"], 1);
        assert_eq!(v1["plugins"].as_array().unwrap().len(), 1);
        assert!(v1["plugins"][0].get("kind").is_none());
    }

    #[test]
    fn non_github_repo_and_non_nuono_crate_are_rejected() {
        assert!(normalize_github_repo("https://gitlab.com/alice/demo").is_err());
        assert!(normalize_github_repo("https://github.com/alice/demo/tree/main").is_err());
        assert!(parse_issue_body("### GitHub 源码仓库\n\n_No response_\n").is_err());

        let temp = tempfile::tempdir().unwrap();
        let bad_repo = temp.path().join("unrelated");
        create_git_repo(
            &bad_repo,
            "[package]\nname = \"unrelated\"\nversion = \"0.1.0\"\n",
        );
        let entry = RegistryEntry {
            repo: "https://github.com/alice/unrelated.git".into(),
            kind: None,
            id: None,
            name: None,
            description: None,
            author: None,
            license: None,
            commit: None,
        };
        assert!(resolve_entry_from_source(&entry, bad_repo.to_str().unwrap()).is_err());
    }
}
