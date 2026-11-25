use super::{manifest::*, *};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    source_build::configure_git_command(&mut command);
    let result = command
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("无法读取发行源码 Git 信息")?;
    ensure!(
        result.status.success(),
        "发行源码需要有效的独立 Git 仓库: {}",
        root.display()
    );
    Ok(String::from_utf8(result.stdout)?.trim().into())
}
pub fn source(root: &Path, kind: &str, id: &str) -> Result<SourceRecord> {
    ensure!(
        Path::new(&git(root, &["rev-parse", "--show-toplevel"])?).canonicalize()?
            == root.canonicalize()?,
        "扩展源码必须有自己的 Git 仓库: {}",
        root.display()
    );
    let status = git(root, &["status", "--porcelain", "--untracked-files=all"])?;
    ensure!(
        status
            .lines()
            .all(|line| line.is_empty() || line == "?? .nuono-pin"),
        "发行源码存在未提交修改，请先记录源码版本: {}",
        root.display()
    );
    let repository = git(root, &["remote", "get-url", "origin"])
        .ok()
        .map(|remote| {
            if let Ok(mut value) = url::Url::parse(&remote) {
                let _ = value.set_username("");
                let _ = value.set_password(None);
                value.set_query(None);
                value.set_fragment(None);
                value.into()
            } else {
                remote
                    .rsplit_once('@')
                    .map_or(remote.clone(), |(_, path)| path.to_owned())
            }
        });
    Ok(SourceRecord {
        kind: kind.into(),
        id: id.into(),
        repository,
        commit: git(root, &["rev-parse", "HEAD^{commit}"])?,
        tree: git(root, &["rev-parse", "HEAD^{tree}"])?,
    })
}
fn sources(root: &Path, selected: &[(String, PathBuf)]) -> Result<Vec<SourceRecord>> {
    let mut result = vec![source(root, "framework", "nuono")?];
    for (_, path) in selected {
        let identity = source_build::source_identity(path)?;
        result.push(source(
            path,
            if identity.kind == SourceKind::Plugin {
                "plugin"
            } else {
                "adapter"
            },
            &identity.id,
        )?);
    }
    result.sort_by(|a, b| (&a.kind, &a.id).cmp(&(&b.kind, &b.id)));
    Ok(result)
}

fn description(binary: &Path) -> Result<ApplicationDescription> {
    let mut child = Command::new(binary)
        .args(["release", "describe"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("无法读取候选应用清单；完整发行目前需要本机目标平台")?;
    let output = child.stdout.take().context("缺少应用描述输出")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let result = (|| {
        let bytes = receiver
            .recv_timeout(Duration::from_secs(30))
            .context("读取应用迁移声明超过 30 秒")??;
        ensure!(bytes.len() <= 16 * 1024 * 1024, "应用声明超过大小上限");
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            ensure!(std::time::Instant::now() < deadline, "应用描述进程退出超时");
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure!(status.success(), "候选应用无法提供完整声明");
        Ok(serde_json::from_slice(&bytes)?)
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// 在目标父目录所属的同一文件系统上构建临时暂存目录，待全部文件写完并校验通过后再原子重命名发布。
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "发行资产不允许符号链接: {}",
        source.display()
    );
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        ensure!(metadata.is_file(), "发行资产不是普通文件");
        fs::copy(source, target)?;
        fs::File::open(target)?.sync_all()?;
    }
    Ok(())
}
fn sync_directories(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            sync_directories(&path)?;
        }
    }
    #[cfg(unix)]
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn audit_local_dependencies(
    root: &Path,
    project: &Path,
    selected: &[(String, PathBuf)],
) -> Result<()> {
    let mut command = Command::new("cargo");
    crate::build_config::clear_parent_package_environment(
        &mut command,
        std::env::vars_os().map(|(key, _)| key),
    );
    let output = command
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(project.join("Cargo.toml"))
        .current_dir(root)
        .output()?;
    ensure!(
        output.status.success(),
        "无法确认应用锁文件中的本地依赖来源"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut allowed = vec![root.join("crates").canonicalize()?, project.canonicalize()?];
    let fixtures = root.join("xtask/fixtures");
    if fixtures.exists() {
        allowed.push(fixtures.canonicalize()?);
    }
    for (_, path) in selected {
        allowed.push(path.canonicalize()?);
    }
    for package in value["packages"]
        .as_array()
        .context("无效 Cargo metadata")?
    {
        if !package["source"].is_null() {
            continue;
        }
        let path = Path::new(
            package["manifest_path"]
                .as_str()
                .context("缺少本地依赖路径")?,
        )
        .canonicalize()?;
        ensure!(
            allowed.iter().any(|base| path.starts_with(base)),
            "本地依赖未纳入发行源码清单: {}",
            path.display()
        );
        // 嵌套的独立 Git 仓库必须纳入扩展源码清单，不能假借外层框架仓库的版本号蒙混过关。
        if !path.starts_with(project) {
            let owner = PathBuf::from(git(
                path.parent().unwrap(),
                &["rev-parse", "--show-toplevel"],
            )?)
            .canonicalize()?;
            ensure!(
                owner == root.canonicalize()?
                    || selected
                        .iter()
                        .any(|(_, path)| path.canonicalize().ok().as_ref() == Some(&owner)),
                "本地依赖属于未记录的独立仓库"
            );
        }
    }
    Ok(())
}

pub fn build(
    output: &Path,
    profile: &str,
    config: Option<&Path>,
    mut required_plugins: Vec<String>,
    mut required_adapters: Vec<String>,
) -> Result<()> {
    let root = crate::find_nuono_repository_root(&std::env::current_dir()?)
        .context("请在 Nuono 源码仓库构建发行包")?;
    let root = root.canonicalize()?;
    let target = source_build::target_dir(&root)?;
    fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let output = destination(output, &target)?;
    let parent = output.parent().context("发行目录缺少父目录")?;
    crate::recovery::recover_before_build(&root).map_err(anyhow::Error::msg)?;
    let lock = source_build::application_lock(&root)?;
    let selected = source_build::extensions(&root)?;
    let source_records = sources(&root, &selected)?;
    required_plugins.sort();
    required_plugins.dedup();
    required_adapters.sort();
    required_adapters.dedup();
    for (kind, ids) in [
        ("plugin", &required_plugins),
        ("adapter", &required_adapters),
    ] {
        for id in ids {
            ensure!(
                source_records
                    .iter()
                    .any(|source| source.kind == kind && source.id == *id),
                "必需扩展未安装: {kind}:{id}"
            );
        }
    }
    let dashboard_before = inventory(&root.join("nuono-hub/dist"))?;
    dashboard(&root.join("nuono-hub/dist"))?;
    let default = PathBuf::from("nuono.toml");
    let config = config.unwrap_or(&default);
    let candidate =
        source_build::build_candidate_prepared_locked(&root, profile, None, &lock, config, true)?;
    let project = target.join("nuono-app");
    audit_local_dependencies(&root, &project, &selected)?;
    ensure!(
        source_records == sources(&root, &source_build::extensions(&root)?)?,
        "构建期间源码修订发生变化"
    );
    ensure!(
        source_build::same_sources(
            &source_build::current_sources(&root)?,
            &candidate.artifact().sources
        ),
        "构建后扩展源码已变化"
    );
    ensure!(
        dashboard_before == inventory(&root.join("nuono-hub/dist"))?,
        "构建期间 Dashboard 已变化"
    );
    fs::create_dir_all(parent)?;
    let staging = Staging(parent.join(format!(".nuono-release-{}", uuid::Uuid::new_v4())));
    fs::create_dir(&staging.0)?;
    let executable = format!("nuono{}", std::env::consts::EXE_SUFFIX);
    source_build::export_validated_candidate(&target, candidate, &staging.0.join(&executable))?;
    let application = description(&staging.0.join(&executable))?;
    ensure!(
        application.build.target_triple.is_some()
            && application.build.target_triple == application.build.build_host,
        "完整发行目前只支持本机编译目标"
    );
    copy_tree(
        &root.join("nuono-hub/dist"),
        &staging.0.join("nuono-hub/dist"),
    )?;
    ensure!(
        dashboard_before == inventory(&staging.0.join("nuono-hub/dist"))?,
        "复制 Dashboard 时资产发生变化"
    );
    fs::create_dir(staging.0.join("build-info"))?;
    for (source, target_name) in [
        (project.join("Cargo.lock"), "Cargo.lock"),
        (project.join("Cargo.toml"), "application.Cargo.toml"),
    ] {
        copy_tree(&source, &staging.0.join("build-info").join(target_name))?;
    }
    copy_tree(&root.join("LICENSE"), &staging.0.join("LICENSE"))?;
    source_build::durable_json(&staging.0.join("build-info/sources.json"), &source_records)?;
    let mut parameters = BTreeMap::new();
    let mut command = Command::new("cargo");
    crate::build_config::apply(&mut command, config)?;
    for (key, value) in command.get_envs() {
        if matches!(key.to_str(), Some("CARGO_INCREMENTAL" | "CARGO_BUILD_JOBS"))
            && let Some(value) = value
        {
            parameters.insert(
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            );
        }
    }
    for (key, value) in std::env::vars() {
        let allowed = matches!(
            key.as_str(),
            "RUSTFLAGS"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_BUILD_RUSTFLAGS"
                | "CARGO_BUILD_TARGET"
                | "CC"
                | "CXX"
                | "AR"
                | "CFLAGS"
                | "CXXFLAGS"
                | "LDFLAGS"
                | "SDKROOT"
                | "MACOSX_DEPLOYMENT_TARGET"
        ) || key.starts_with("CARGO_PROFILE_")
            || (key.starts_with("CARGO_TARGET_")
                && (key.ends_with("_LINKER") || key.ends_with("_RUSTFLAGS")));
        if allowed {
            parameters.insert(key, value);
        }
    }
    let mut manifest = ReleaseManifest {
        schema_version: 1,
        release_id: String::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        executable,
        application,
        sources: source_records,
        readiness: Readiness {
            checks: vec![
                "database".into(),
                "framework_migrations".into(),
                "required_extension_initialization".into(),
            ],
            required_plugins,
            required_adapters,
            external_connections: "reported_separately".into(),
        },
        build_parameters: parameters,
        files: inventory(&staging.0)?,
    };
    manifest.release_id = manifest.identity()?;
    source_build::durable_json(&staging.0.join("release.json"), &manifest)?;
    verify(&staging.0)?;
    sync_directories(&staging.0)?;
    publish(&staging.0, &output)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    eprintln!(
        "完整发行已生成: {}\n发行编号: {}",
        output.display(),
        manifest.release_id
    );
    Ok(())
}

/// 在受支持的平台上以原子且不覆盖已有目录的方式发布发行目录。
pub fn publish(staging: &Path, output: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staging,
        rustix::fs::CWD,
        output,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(windows)]
    fs::rename(staging, output)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    anyhow::bail!("当前平台尚未实现发行目录的不可覆盖发布");
    Ok(())
}

/// 先解析父目录的真实路径（消除软链接别名），再校验输出目录确实位于 Cargo target 缓存目录之外。
pub fn destination(output: &Path, target: &Path) -> Result<PathBuf> {
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        std::env::current_dir()?.join(output)
    };
    let parent = output.parent().context("发行目录缺少父目录")?;
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("发行父目录不存在")?;
    }
    ensure!(
        !ancestor.canonicalize()?.starts_with(target),
        "发行目录必须位于 target 缓存之外"
    );
    fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    ensure!(
        !parent.starts_with(target),
        "发行目录必须位于 target 缓存之外"
    );
    let output = parent.join(output.file_name().context("无效发行目录")?);
    match fs::symlink_metadata(&output) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(output),
        Ok(_) => anyhow::bail!("发行目录已存在，拒绝覆盖: {}", output.display()),
        Err(error) => Err(error.into()),
    }
}
