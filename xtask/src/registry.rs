use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEntry {
    #[serde(default)]
    pub kind: ExtensionKind,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub license: String,
    pub nuono_version: String,
    pub repo: String,
    pub commit: String,
}

impl ExtensionEntry {
    pub fn filename(&self) -> String {
        match self.kind {
            ExtensionKind::Plugin => format!("{}.json", self.id),
            ExtensionKind::Adapter => format!("adapter.{}.json", self.id),
        }
    }

    pub fn validate(&self, filename: Option<&str>) -> Result<()> {
        validate_id(&self.id)?;
        if let Some(filename) = filename {
            ensure!(
                filename == self.filename(),
                "条目文件名 {filename} 与期望的 {} 不一致",
                self.filename()
            );
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("author", &self.author),
            ("license", &self.license),
        ] {
            ensure!(
                !value.is_empty()
                    && value.trim() == value
                    && value.chars().count() <= MAX_FIELD_CHARS
                    && !value.contains('\0'),
                "{field} 必须为非空文本且不超过 {MAX_FIELD_CHARS} 字符"
            );
        }
        ensure!(
            is_exact_semver(&self.version),
            "{}: version 必须是完整 SemVer（如 0.1.0）",
            self.id
        );
        ensure!(
            is_exact_semver(&self.nuono_version),
            "{}: nuono_version 必须是完整 SemVer（如 0.0.1）",
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

pub fn normalize_github_repo(input: &str) -> Result<String> {
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
    Ok(format!("https://github.com/{owner}/{repo}.git"))
}

pub fn entries_dir(root: &Path) -> PathBuf {
    root.join("registry").join("entries")
}

pub fn load_entries(root: &Path) -> Result<Vec<ExtensionEntry>> {
    let dir = entries_dir(root);
    ensure!(dir.is_dir(), "缺少扩展目录 {}", dir.display());
    let mut files = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    files.sort_by_key(|entry| entry.file_name());
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
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
        let entry: ExtensionEntry = serde_json::from_str(&raw)
            .with_context(|| format!("解析 {} 失败", file.path().display()))?;
        entry.validate(Some(&name))?;
        ensure!(
            seen.insert((entry.kind, entry.id.to_ascii_lowercase())),
            "重复的扩展 ID: {}",
            entry.id
        );
        entries.push(entry);
    }
    entries.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    Ok(entries)
}

fn render_catalog(entries: &[ExtensionEntry], v2: bool) -> Result<Value> {
    let mut items = Vec::new();
    for entry in entries {
        if !v2 && entry.kind != ExtensionKind::Plugin {
            continue;
        }
        entry.validate(None)?;
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
    render_catalog(&entries, true)?;
    render_catalog(&entries, false)?;
    println!(
        "Extension registry validation passed ({} entries).",
        entries.len()
    );
    Ok(entries.len())
}

pub fn build(root: &Path, out_dir: &Path) -> Result<usize> {
    let entries = load_entries(root)?;
    let v2 = render_catalog(&entries, true)?;
    let v1 = render_catalog(&entries, false)?;
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
        entries.len(),
        out_dir.display()
    );
    Ok(entries.len())
}

pub fn parse_issue_body(body: &str) -> Result<ExtensionEntry> {
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
    let field = |label: &str| -> Result<String> {
        let value = fields
            .get(label)
            .with_context(|| format!("缺少表单字段「{label}」"))?
            .trim()
            .to_owned();
        ensure!(
            !value.is_empty() && value != "_No response_",
            "表单字段「{label}」不能为空"
        );
        Ok(value)
    };
    let kind = match field("扩展类型")?.as_str() {
        "plugin" => ExtensionKind::Plugin,
        "adapter" => ExtensionKind::Adapter,
        other => anyhow::bail!("扩展类型必须是 plugin 或 adapter，收到: {other}"),
    };
    let entry = ExtensionEntry {
        kind,
        id: field("扩展 ID")?,
        name: field("展示名称")?,
        author: field("作者")?,
        repo: normalize_github_repo(&field("GitHub 源码仓库")?)?,
        version: field("版本号")?,
        commit: field("固定 Commit SHA")?.to_ascii_lowercase(),
        license: field("开源协议")?,
        nuono_version: field("适用的 Nuono 版本")?,
        description: field("简介")?,
    };
    entry.validate(None)?;
    Ok(entry)
}

pub fn import_issue(root: &Path, issue_body_file: &Path) -> Result<PathBuf> {
    let body = fs::read_to_string(issue_body_file)
        .with_context(|| format!("读取 Issue 内容失败: {}", issue_body_file.display()))?;
    let entry = parse_issue_body(&body)?;
    let dir = entries_dir(root);
    fs::create_dir_all(&dir)?;
    let path = dir.join(entry.filename());
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&entry)?),
    )?;
    validate(root)?;
    println!("{}", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ISSUE: &str = r#"### 扩展类型

plugin

### 扩展 ID

demo-plugin

### 展示名称

Demo Plugin

### 作者

alice

### GitHub 源码仓库

https://github.com/alice/demo-plugin

### 版本号

0.1.0

### 固定 Commit SHA

0123456789ABCDEF0123456789abcdef01234567

### 开源协议

Apache-2.0

### 适用的 Nuono 版本

0.0.1

### 简介

示例插件简介
"#;

    #[test]
    fn issue_import_normalizes_and_builds_v1_and_v2_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(entries_dir(root)).unwrap();
        let issue_file = root.join("issue.md");
        fs::write(&issue_file, SAMPLE_ISSUE).unwrap();

        let written = import_issue(root, &issue_file).unwrap();
        assert_eq!(written, entries_dir(root).join("demo-plugin.json"));

        let out = root.join("out");
        assert_eq!(build(root, &out).unwrap(), 1);
        let v2: Value =
            serde_json::from_str(&fs::read_to_string(out.join("extensions-v2.json")).unwrap())
                .unwrap();
        assert_eq!(v2["schema_version"], 2);
        assert_eq!(v2["extensions"][0]["kind"], "plugin");
        assert_eq!(
            v2["extensions"][0]["repo"],
            "https://github.com/alice/demo-plugin.git"
        );
        let v1: Value =
            serde_json::from_str(&fs::read_to_string(out.join("catalog.json")).unwrap()).unwrap();
        assert_eq!(v1["schema_version"], 1);
        assert!(v1["plugins"][0].get("kind").is_none());
    }

    #[test]
    fn non_github_repo_and_invalid_commit_are_rejected() {
        assert!(normalize_github_repo("https://gitlab.com/alice/demo").is_err());
        assert!(normalize_github_repo("https://github.com/alice/demo/tree/main").is_err());
        let bad = SAMPLE_ISSUE.replace(
            "0123456789ABCDEF0123456789abcdef01234567",
            "not-a-40-char-sha",
        );
        assert!(parse_issue_body(&bad).is_err());
    }
}
