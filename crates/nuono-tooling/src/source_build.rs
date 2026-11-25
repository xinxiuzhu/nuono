//! 将宿主与当前已安装的源码插件、适配器统一编译链接为单个本地应用可执行文件。
//!
//! 调用方负责管理宿主权限校验与进程生命周期策略。若需同时获取两个锁，必须先获取 `operation_lock`
//! 再获取 `application_lock`。启动恢复（`recover_before_build`）必须在获取 `application_lock` 或扫描扩展目录之前执行。
//! 所有构建函数均要求调用方已持有 `application_lock`；候选产物在发布时还会再次核对源码指纹与二进制哈希。
//! 取消构建时会强杀所属子进程组，同时保留构建日志供排障恢复。

use anyhow::{Context, Result, ensure};
use fs2::FileExt;

// 通过 `include_str!` 内嵌构建辅助源码，确保辅助脚本变更时 Cargo 也会自动重编启动器；
// 生成的应用工程会写入同一份辅助脚本，并读取应用自身独立的 `Cargo.lock`。
const BUILD_METADATA_HELPER: &str = include_str!("../build_support.rs");
const BUILD_PROFILE_HELPER: &str = include_str!("../build_profile.rs");
const APPLICATION_BUILD_SCRIPT: &str = "mod build_support;\nfn main() { build_support::emit_build_metadata(\"nuono_server::BuildMetadata\", \"nuono_server::BuildDependencyMetadata\"); }\n";
mod artifacts;
mod cargo_build;
mod icons;
pub use artifacts::{
    BuildArtifact, Publication, durable_json, export_validated_candidate, hash_file,
    maintain_artifacts, maintain_artifacts_cancellable, protected_job_ids, public_binary,
    read_publication, recover_publications, rollback_candidate, same_artifact, same_sources,
    verify_publication,
};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, Instant},
};

/// 源码变更操作与后台产物维护任务在完整执行周期内共用的互斥锁。
pub fn operation_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
        LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));
    LOCK.clone()
}

/// 清理外部继承的 `GIT_*` 环境变量，确保 Git 命令始终按显式传入的工作目录解析仓库。
pub fn configure_git_command(command: &mut Command) {
    command.env("GIT_NO_LAZY_FETCH", "1");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(variable);
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Plugin,
    Adapter,
}

impl SourceKind {
    pub fn directory(self) -> &'static str {
        match self {
            Self::Plugin => "plugins",
            Self::Adapter => "adapters",
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SourcePlugin {
    #[serde(default)]
    pub kind: SourceKind,
    pub id: String,
    pub package: String,
    pub version: String,
    pub commit: Option<String>,
    pub source_hash: String,
    pub runtime_name: Option<String>,
}
static RUNNING: LazyLock<RwLock<Vec<SourcePlugin>>> = LazyLock::new(|| RwLock::new(Vec::new()));
static RUNNING_EXECUTABLE: std::sync::OnceLock<RunningExecutable> = std::sync::OnceLock::new();

struct RunningExecutable {
    path: PathBuf,
    sha256: std::sync::OnceLock<Option<String>>,
}

pub fn register_running_manifest(manifest: &str) {
    if let Ok(value) = serde_json::from_str(manifest) {
        *RUNNING.write().unwrap_or_else(|e| e.into_inner()) = value;
    }
}
/// 记录重启和状态查询使用的程序路径，无需读取程序内容。
pub fn remember_running_executable_path() {
    if RUNNING_EXECUTABLE.get().is_none()
        && let Ok(path) = std::env::current_exe()
    {
        let _ = RUNNING_EXECUTABLE.set(RunningExecutable {
            path: cached_running_path(path),
            sha256: std::sync::OnceLock::new(),
        });
    }
}

/// 在源码发布恢复可能替换入口之前，记录当前运行程序的摘要。
pub fn remember_running_executable() {
    remember_running_executable_path();
    if let Some(executable) = RUNNING_EXECUTABLE.get() {
        executable.sha256.get_or_init(|| {
            #[cfg(target_os = "linux")]
            let path = Path::new("/proc/self/exe");
            #[cfg(not(target_os = "linux"))]
            let path = executable.path.as_path();
            artifacts::hash_file(path).ok()
        });
    }
}
pub fn cached_running_path(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "linux")]
    if let Some(name) = path.file_name().and_then(|name| name.to_str())
        && let Some(original) = name.strip_suffix(" (deleted)")
        && matches!(
            original,
            "nuono" | "nuono-application" | "nuono-application-candidate"
        )
    {
        return path.with_file_name(original);
    }
    path
}
pub fn running_executable() -> Option<PathBuf> {
    RUNNING_EXECUTABLE
        .get()
        .map(|executable| executable.path.clone())
}
pub fn running_binary_sha256() -> Option<&'static str> {
    RUNNING_EXECUTABLE.get()?.sha256.get()?.as_deref()
}
pub fn register_runtime_name(id: &str, name: &str) {
    if let Some(plugin) = RUNNING
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .iter_mut()
        .find(|p| p.kind == SourceKind::Plugin && p.id == id)
    {
        plugin.runtime_name = Some(name.into());
    }
}
pub fn running_manifest() -> Vec<SourcePlugin> {
    RUNNING.read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn running_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut plugins = running_manifest();
    plugins.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    let values: Vec<_> = plugins
        .into_iter()
        .map(|p| (p.kind, p.id, p.package, p.version, p.commit, p.source_hash))
        .collect();
    hex::encode(Sha256::digest(
        serde_json::to_vec(&values).unwrap_or_default(),
    ))
}

pub fn source_identity(path: &Path) -> Result<SourcePlugin> {
    use sha2::{Digest, Sha256};
    fn hash_tree(root: &Path, path: &Path, hasher: &mut Sha256) -> Result<()> {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == ".git" || name == "target" || name == ".nuono-pin" || name == ".DS_Store" {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            let file = entry.path();
            if kind.is_dir() {
                hash_tree(root, &file, hasher)?;
            } else if kind.is_file() {
                hasher.update(file.strip_prefix(root)?.to_string_lossy().as_bytes());
                let mut reader = fs::File::open(file)?;
                let mut buffer = [0; 16 * 1024];
                loop {
                    let size = reader.read(&mut buffer)?;
                    if size == 0 {
                        break;
                    }
                    hasher.update(&buffer[..size]);
                }
            }
        }
        Ok(())
    }
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(path.join("Cargo.toml"))?)?;
    let mut hasher = Sha256::new();
    hash_tree(path, path, &mut hasher)?;
    let commit = path
        .join(".git")
        .exists()
        .then(|| {
            let mut command = Command::new("git");
            configure_git_command(&mut command);
            command
                .args(["-C", &path.to_string_lossy(), "rev-parse", "HEAD"])
                .output()
                .ok()
        })
        .flatten()
        .filter(|v| v.status.success())
        .map(|v| String::from_utf8_lossy(&v.stdout).trim().to_string());
    Ok(SourcePlugin {
        kind: manifest_kind(&manifest)?,
        id: path
            .file_name()
            .context("missing plugin directory")?
            .to_string_lossy()
            .into(),
        package: manifest["package"]["name"]
            .as_str()
            .unwrap_or_default()
            .into(),
        version: manifest["package"]["version"]
            .as_str()
            .unwrap_or_default()
            .into(),
        commit,
        source_hash: hex::encode(hasher.finalize()),
        runtime_name: None,
    })
}

static STOP_COMMANDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn shutdown_operations() {
    STOP_COMMANDS.store(true, std::sync::atomic::Ordering::Release);
}
fn stop_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub type BuildOutput = Arc<dyn Fn(&str) + Send + Sync>;
/// 使用受限的单行缓冲区读取子进程 stdout 和 stderr，避免在内存中积压完整的 Cargo 输出。
pub fn run_logged(command: &mut Command, output: BuildOutput) -> Result<()> {
    run_logged_with_outputs(command, output.clone(), output, 4096, true)
}

fn run_logged_with_outputs(
    command: &mut Command,
    stdout_output: BuildOutput,
    stderr_output: BuildOutput,
    stdout_line_limit: usize,
    manage_lifecycle: bool,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if manage_lifecycle {
            command.process_group(0);
        }
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("无法启动外部命令，请检查 Git / Cargo 是否已安装")?;
    fn drain(
        reader: impl Read + Send + 'static,
        output: BuildOutput,
        line_limit: usize,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            let mut truncated = false;
            loop {
                let available = match reader.fill_buf() {
                    Ok(v) if !v.is_empty() => v,
                    _ => break,
                };
                let end = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |i| i + 1);
                let remaining = line_limit.saturating_sub(line.len());
                if end > remaining {
                    truncated = true;
                }
                line.extend_from_slice(&available[..end.min(remaining)]);
                let newline = available[end - 1] == b'\n';
                reader.consume(end);
                if newline {
                    if truncated {
                        output(&format!("[输出行超过 {line_limit} 字节，已隐藏]"));
                    } else {
                        output(String::from_utf8_lossy(&line).trim_end());
                    }
                    line.clear();
                    truncated = false;
                }
            }
            if !line.is_empty() {
                if truncated {
                    output(&format!("[输出行超过 {line_limit} 字节，已隐藏]"));
                } else {
                    output(&String::from_utf8_lossy(&line));
                }
            }
        })
    }
    let stdout = drain(
        child.stdout.take().context("missing stdout")?,
        stdout_output,
        stdout_line_limit,
    );
    let stderr = drain(
        child.stderr.take().context("missing stderr")?,
        stderr_output,
        4096,
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if manage_lifecycle && STOP_COMMANDS.load(std::sync::atomic::Ordering::Acquire) {
            stop_child(&mut child);
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(anyhow::anyhow!(
                "服务正在停止，已结束构建；下次启动会检查并恢复中断任务"
            ));
        }
        if manage_lifecycle && started.elapsed() > Duration::from_secs(45 * 60) {
            stop_child(&mut child);
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(anyhow::anyhow!(
                "操作超过 45 分钟，已停止；请检查网络和机器资源后重试"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = stdout.join();
    let _ = stderr.join();
    ensure!(
        status.success(),
        "命令执行失败（{}），请查看任务日志",
        status
    );
    Ok(())
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).ok().as_deref() != Some(bytes) {
        fs::write(path, bytes)?;
    }
    Ok(())
}

pub fn target_dir(root: &Path) -> Result<PathBuf> {
    let path = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

pub fn validate_adapter_manifest(manifest: &toml::Value) -> Result<()> {
    ensure!(
        manifest_kind(manifest)? == SourceKind::Adapter,
        "适配器需要 [package.metadata.nuono] kind = \"adapter\""
    );
    ensure!(
        manifest.get("workspace").is_some(),
        "适配器需要独立 [workspace]"
    );
    ensure!(
        manifest["package"]["name"].as_str().is_some(),
        "适配器缺少 package.name"
    );
    ensure!(
        manifest
            .get("dependencies")
            .and_then(|dependencies| dependencies.get("nuono-adapter"))
            .is_some(),
        "适配器缺少 nuono-adapter 契约依赖"
    );
    if let Some(types) = manifest
        .get("lib")
        .and_then(|lib| lib.get("crate-type"))
        .and_then(toml::Value::as_array)
    {
        ensure!(
            types
                .iter()
                .any(|kind| matches!(kind.as_str(), Some("rlib" | "lib")))
                && !types
                    .iter()
                    .any(|kind| matches!(kind.as_str(), Some("dylib" | "cdylib"))),
            "适配器必须提供普通 Rust 库"
        );
    }
    Ok(())
}

pub fn plugins(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    discover_sources(root, SourceKind::Plugin)
}

pub fn extensions(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut sources = plugins(root)?;
    sources.extend(discover_sources(root, SourceKind::Adapter)?);
    Ok(sources)
}

pub fn manifest_kind(manifest: &toml::Value) -> Result<SourceKind> {
    match manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("nuono"))
        .and_then(|nuono| nuono.get("kind"))
        .and_then(toml::Value::as_str)
    {
        None | Some("plugin") => Ok(SourceKind::Plugin),
        Some("adapter") => Ok(SourceKind::Adapter),
        Some(_) => anyhow::bail!("未知 Nuono 源码扩展类型"),
    }
}

fn discover_sources(root: &Path, kind: SourceKind) -> Result<Vec<(String, PathBuf)>> {
    let directory = root.join(kind.directory());
    // 发现结果会成为已安装插件清单；访问失败不能被误认为插件已卸载。
    let directory_metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("无法检查插件目录: {}", directory.display()));
        }
    };
    let directory_metadata = if directory_metadata.is_symlink() {
        fs::metadata(&directory)
            .with_context(|| format!("无法访问插件目录: {}", directory.display()))?
    } else {
        directory_metadata
    };
    ensure!(
        directory_metadata.is_dir(),
        "插件路径不是目录: {}",
        directory.display()
    );
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("无法读取插件目录: {}", directory.display()))?
        .map(|v| v.map(|v| v.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("无法枚举插件目录: {}", directory.display()))?;
    entries.sort();
    let mut result = Vec::new();
    for path in entries {
        if path
            .file_name()
            .is_some_and(|v| v.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if !fs::metadata(&path)
            .with_context(|| format!("无法检查插件候选路径: {}", path.display()))?
            .is_dir()
        {
            continue;
        }
        let manifest_path = path.join("Cargo.toml");
        let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 候选目录若在扫描期间消失，也不能当成正常的无清单目录。
                ensure!(
                    fs::metadata(&path)
                        .with_context(|| format!("无法检查插件候选路径: {}", path.display()))?
                        .is_dir(),
                    "插件候选路径已不再是目录: {}",
                    path.display()
                );
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法检查插件清单: {}", manifest_path.display()));
            }
        };
        let manifest_metadata = if manifest_metadata.is_symlink() {
            fs::metadata(&manifest_path)
                .with_context(|| format!("无法访问插件清单: {}", manifest_path.display()))?
        } else {
            manifest_metadata
        };
        ensure!(
            manifest_metadata.is_file(),
            "插件清单不是普通文件: {}",
            manifest_path.display()
        );
        let path = path.canonicalize()?;
        let manifest: toml::Value = toml::from_str(&fs::read_to_string(path.join("Cargo.toml"))?)?;
        let declared = manifest_kind(&manifest)?;
        ensure!(
            declared == kind,
            "源码扩展类型与安装目录不一致: {}",
            path.display()
        );
        match kind {
            SourceKind::Plugin => crate::plugin_dev::validate_manifest(&manifest),
            SourceKind::Adapter => validate_adapter_manifest(&manifest),
        }
        .with_context(|| format!("无效源码扩展工程: {}", path.display()))?;
        result.push((
            manifest["package"]["name"]
                .as_str()
                .context("plugin package name missing")?
                .to_string(),
            path,
        ));
    }
    Ok(result)
}

#[cfg(test)]
fn generate(root: &Path, selected: &[(String, PathBuf)]) -> Result<(String, String)> {
    let sources = selected
        .iter()
        .map(|(name, path)| {
            if path.join("Cargo.toml").is_file() {
                source_identity(path)
            } else {
                Ok(SourcePlugin {
                    id: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into(),
                    package: name.clone(),
                    ..Default::default()
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    generate_with_sources(root, selected, &sources)
}

const SHARED_SOURCE_CRATES: &[&str] = &[
    "nuono-adapter",
    "nuono-plugin",
    "nuono-plugin-macros",
    "nuono-domain",
    "nuono-app",
    "nuono-ai",
    "nuono-rag",
    "nuono-memory",
    "nuono-error",
];

fn shared_dependency_sources(value: &toml::Value) -> std::collections::BTreeSet<String> {
    let mut sources = std::collections::BTreeSet::new();
    let mut collect = |section: &toml::Value| {
        for kind in ["dependencies", "build-dependencies"] {
            if let Some(dependencies) = section.get(kind).and_then(toml::Value::as_table) {
                for (alias, dependency) in dependencies {
                    let dependency = if dependency.get("workspace").and_then(toml::Value::as_bool)
                        == Some(true)
                    {
                        value
                            .get("workspace")
                            .and_then(|workspace| workspace.get("dependencies"))
                            .and_then(|dependencies| dependencies.get(alias.as_str()))
                            .unwrap_or(dependency)
                    } else {
                        dependency
                    };
                    let package = dependency
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(alias);
                    if !SHARED_SOURCE_CRATES.contains(&package) {
                        continue;
                    }
                    if let Some(git) = dependency.get("git").and_then(toml::Value::as_str) {
                        sources.insert(git.to_owned());
                    } else if dependency.get("registry").is_none()
                        && dependency.get("path").is_none()
                        && (dependency.as_str().is_some() || dependency.get("version").is_some())
                    {
                        sources.insert("crates-io".to_owned());
                    }
                }
            }
        }
    };
    collect(value);
    if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect(target);
        }
    }
    sources
}

fn generate_with_sources(
    root: &Path,
    selected: &[(String, PathBuf)],
    sources: &[SourcePlugin],
) -> Result<(String, String)> {
    let quote = |value: &str| serde_json::to_string(value);
    let mut manifest = format!(
        "[package]\nname=\"nuono-application\"\nversion=\"0.0.1\"\nedition=\"2024\"\npublish=false\n[workspace]\nresolver=\"3\"\n[dependencies]\nnuono-server={{path={}}}\n",
        quote(&root.join("crates/nuono-server").to_string_lossy())?
    );
    let mut factories = Vec::new();
    let mut adapters = Vec::new();
    let mut shared_sources = std::collections::BTreeSet::new();
    for (i, (name, path)) in selected.iter().enumerate() {
        let source = sources.get(i).context("源码清单与发现结果不一致")?;
        manifest.push_str(&format!(
            "plugin_{i}={{package={},path={}}}\n",
            quote(name)?,
            quote(&path.to_string_lossy())?
        ));
        let id = path
            .file_name()
            .context("missing plugin id")?
            .to_string_lossy();
        // 在生成代码时直接内嵌已校验通过的图标字节字面量，而不使用 `include_bytes!` 在编译期延迟读取源码路径。
        // 构建前后的源码一致性校验同样覆盖图标文件。
        if source.kind == SourceKind::Adapter {
            adapters.push(format!("plugin_{i}::nuono_create_adapter"));
        } else {
            let icon_registration = icons::load(path)?
                .map(|icon| icon.registration())
                .unwrap_or_default();
            factories.push(format!("|| {{ let plugin = plugin_{i}::nuono_create_plugin(); nuono_server::register_source_runtime_name({}, plugin.name()); {icon_registration} plugin }}", quote(&id)?));
        }
        if let Ok(content) = fs::read_to_string(path.join("Cargo.toml")) {
            let value: toml::Value = toml::from_str(&content)?;
            shared_sources.extend(shared_dependency_sources(&value));
        }
    }
    // 即使插件通过外部 Git 仓库或 crates.io 别名引入了共享基础 crate，
    // 也通过 `[patch]` 统一重定向到本地仓库源码，确保宿主、SDK 与插件中的公共类型完全一致。
    for source in shared_sources {
        manifest.push_str(&format!("[patch.{}]\n", quote(&source)?));
        for name in SHARED_SOURCE_CRATES {
            manifest.push_str(&format!(
                "{name}={{path={}}}\n",
                quote(&root.join("crates").join(name).to_string_lossy())?
            ));
        }
    }
    // 生成的顶层应用工程统一掌管整个依赖图的编译 profile 配置。
    let workspace: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    let toml_dependency = workspace
        .get("workspace")
        .and_then(|value| value.get("dependencies"))
        .and_then(|value| value.get("toml"))
        .context("workspace toml dependency missing")?
        .clone();
    let mut build_dependencies = toml::map::Map::new();
    build_dependencies.insert("toml".into(), toml_dependency);
    build_dependencies.insert(
        "sha2".into(),
        workspace["workspace"]["dependencies"]["sha2"].clone(),
    );
    let mut build_values = toml::map::Map::new();
    build_values.insert(
        "build-dependencies".into(),
        toml::Value::Table(build_dependencies),
    );
    manifest.push_str(&toml::to_string(&toml::Value::Table(build_values))?);
    let mut profiles = workspace
        .get("profile")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    for name in ["dev", "release"] {
        let profile = profiles
            .entry(name)
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .context("invalid build profile")?;
        profile.insert("incremental".into(), toml::Value::Boolean(true));
        profile.insert("debug".into(), toml::Value::Integer(0));
    }
    let mut values = toml::map::Map::new();
    values.insert("profile".into(), toml::Value::Table(profiles));
    manifest.push_str(&toml::to_string(&toml::Value::Table(values))?);
    let sources = quote(&serde_json::to_string(&sources)?)?;
    let main = format!(
        "static BUILD_METADATA: nuono_server::BuildMetadata = include!(concat!(env!(\"OUT_DIR\"), \"/build_metadata.rs\"));\nfn main() {{ nuono_server::register_build_metadata(&BUILD_METADATA); nuono_server::register_source_manifest({sources}); if let Err(error) = nuono_server::run_with_extensions(vec![{}], vec![{}]) {{ eprintln!(\"{{error:#}}\"); std::process::exit(1); }} }}\n",
        factories.join(","),
        adapters.join(",")
    );
    Ok((manifest, main))
}

pub fn build_locked(
    root: &Path,
    profile: &str,
    output: Option<BuildOutput>,
    lock: &ApplicationBuildLock,
    config_path: &Path,
) -> Result<PathBuf> {
    let candidate =
        build_candidate_prepared_locked(root, profile, output, lock, config_path, true)?;
    let publication_started = crate::startup_progress::Stage::new("校验源码并发布应用");
    ensure!(
        same_sources(&current_sources(root)?, &candidate.artifact().sources),
        "构建后插件源码已变化，未发布程序"
    );
    let reused = candidate.is_reused();
    let binary = artifacts::publish_validated_candidate(&lock.target, candidate, None)?;
    if reused {
        eprintln!("应用产物未变化，校验通过；沿用已发布程序，无需再次复制或备份");
    }
    publication_started.finish();
    Ok(binary)
}

pub struct ApplicationBuildLock {
    pub target: PathBuf,
    _file: fs::File,
}
impl Drop for ApplicationBuildLock {
    fn drop(&mut self) {
        // fork/exec 出的子进程可能会短暂继承该文件描述符；在关闭句柄前显式解锁，避免锁被子进程意外延续。
        let _ = FileExt::unlock(&self._file);
    }
}
pub fn application_lock(root: &Path) -> Result<ApplicationBuildLock> {
    application_lock_mode(root, false)?.context("无法获得应用构建锁")
}
pub fn try_application_lock(root: &Path) -> Result<Option<ApplicationBuildLock>> {
    application_lock_mode(root, true)
}
fn application_lock_mode(root: &Path, nonblocking: bool) -> Result<Option<ApplicationBuildLock>> {
    let target = target_dir(root)?;
    let project = target.join("nuono-app");
    fs::create_dir_all(&project)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(project.join("build.lock"))?;
    if nonblocking {
        if let Err(error) = lock.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
    } else {
        lock.lock_exclusive()?;
    }
    Ok(Some(ApplicationBuildLock {
        target,
        _file: lock,
    }))
}
pub fn current_sources(root: &Path) -> Result<Vec<SourcePlugin>> {
    extensions(root)?
        .into_iter()
        .map(|(_, path)| source_identity(&path))
        .collect()
}
pub fn build_candidate_locked(
    root: &Path,
    profile: &str,
    output: Option<BuildOutput>,
    lock: &ApplicationBuildLock,
    config_path: &Path,
) -> Result<BuildArtifact> {
    build_candidate_prepared_locked(root, profile, output, lock, config_path, false)
        .map(artifacts::ValidatedCandidate::into_artifact)
}

pub fn build_candidate_prepared_locked(
    root: &Path,
    profile: &str,
    output: Option<BuildOutput>,
    lock: &ApplicationBuildLock,
    config_path: &Path,
    reuse: bool,
) -> Result<artifacts::ValidatedCandidate> {
    let build_started = Instant::now();
    let preparation = crate::startup_progress::Stage::new("扫描插件和适配器源码并准备应用工程");
    let selected = extensions(root)?;
    let sources = selected
        .iter()
        .map(|(_, path)| source_identity(path))
        .collect::<Result<Vec<_>>>()?;
    let target = &lock.target;
    let project = target.join("nuono-app");
    fs::create_dir_all(project.join("src"))?;
    let (mut manifest, main) = generate_with_sources(root, &selected, &sources)?;
    manifest.push_str("\n[[bin]]\nname=\"nuono-application-candidate\"\npath=\"src/main.rs\"\n");
    write_if_changed(&project.join("Cargo.toml"), manifest.as_bytes())?;
    write_if_changed(&project.join("src/main.rs"), main.as_bytes())?;
    write_if_changed(
        &project.join("build.rs"),
        APPLICATION_BUILD_SCRIPT.as_bytes(),
    )?;
    write_if_changed(
        &project.join("build_support.rs"),
        BUILD_METADATA_HELPER.as_bytes(),
    )?;
    write_if_changed(
        &project.join("build_profile.rs"),
        BUILD_PROFILE_HELPER.as_bytes(),
    )?;
    cargo_build::sync_lockfile(root, &project)?;
    let cargo_profile = if profile == "debug" { "dev" } else { profile };
    let mut command = Command::new("cargo");
    let incremental = crate::build_config::apply(&mut command, config_path)?;
    let jobs =
        std::env::var("CARGO_BUILD_JOBS").unwrap_or_else(|_| "自动（Cargo 配置或可用 CPU）".into());
    eprintln!(
        "构建应用：{} 个已安装源码扩展，增量编译：{}，并行任务：{}（配置：{}；正在编译宿主及源码扩展，尚未启动服务）",
        selected.len(),
        incremental,
        jobs,
        config_path.display()
    );
    command
        .args(["build", "--manifest-path"])
        .arg(project.join("Cargo.toml"))
        .args(["--bin", "nuono-application-candidate"])
        .args(["--profile", cargo_profile, "--target-dir"])
        .arg(target)
        .current_dir(root);
    if std::env::var_os("NUONO_CARGO_OFFLINE").is_some() {
        command.arg("--offline");
    }
    preparation.finish();
    let compilation = crate::startup_progress::Stage::visible("Cargo 编译宿主与插件");
    let binary = cargo_build::run(&mut command, &project.join("Cargo.toml"), output)
        .context("应用构建失败")?;
    compilation.finish();
    let validation = crate::startup_progress::Stage::new("校验构建来源并冻结候选产物");
    let profile_directory = if cargo_profile == "dev" {
        "debug"
    } else {
        cargo_profile
    };
    ensure!(
        same_sources(&sources, &current_sources(root)?),
        "构建期间插件集合或源码已变化，候选程序未发布"
    );
    let candidate =
        artifacts::freeze_candidate_validated(target, profile_directory, &binary, sources, reuse)?;
    validation.finish();
    eprintln!(
        "应用编译与候选校验完成（耗时 {:.2} 秒；发布及服务初始化尚未完成）",
        build_started.elapsed().as_secs_f64()
    );
    Ok(candidate)
}
pub fn prepare_job_candidate(
    root: &Path,
    artifact: &BuildArtifact,
    id: &str,
    lock: &ApplicationBuildLock,
) -> Result<Publication> {
    ensure!(
        same_sources(&current_sources(root)?, &artifact.sources),
        "插件源码与构建候选版本不一致，已停止发布"
    );
    artifacts::prepare_publication(&lock.target, artifact, Some(id))
}
pub fn complete_job_candidate(
    root: &Path,
    publication: &Publication,
    lock: &ApplicationBuildLock,
) -> Result<PathBuf> {
    ensure!(
        same_sources(&current_sources(root)?, &publication.artifact.sources),
        "任务提交期间插件源码已变化，未发布程序"
    );
    let binary = artifacts::finish_publication(&lock.target, publication)?;
    Ok(binary)
}

pub fn launch(binary: &Path) -> Result<()> {
    let mut command = Command::new(binary);
    command.args(std::env::args_os().skip(1));
    crate::startup_progress::forward_to(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("无法启动已构建应用")
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("无法启动已构建应用")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

pub const MAX_ICON_BYTES: usize = 512 * 1024;
pub const ICON_FILES: [(&str, &str); 4] = [
    ("icon.svg", "image/svg+xml"),
    ("icon.png", "image/png"),
    ("icon.webp", "image/webp"),
    ("icon.ico", "image/x-icon"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn write_discovery_plugin(root: &Path) -> PathBuf {
        let plugin = root.join("plugins/discovery-probe");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join("Cargo.toml"),
            "[package]\nname='discovery-probe'\nversion='0.1.0'\n[workspace]\n[dependencies]\nnuono-plugin='0.4'\n",
        )
        .unwrap();
        plugin
    }

    #[test]
    fn plugin_discovery_accepts_missing_directory_and_ignores_non_plugin_entries() {
        let directory = tempfile::tempdir().unwrap();
        assert!(plugins(directory.path()).unwrap().is_empty());
        let plugin = write_discovery_plugin(directory.path());
        fs::write(
            directory.path().join("plugins/PLUGIN_DEVELOPMENT.md"),
            "Plugin development documentation",
        )
        .unwrap();
        fs::create_dir(directory.path().join("plugins/no-manifest")).unwrap();
        assert_eq!(
            plugins(directory.path()).unwrap(),
            vec![("discovery-probe".into(), plugin.canonicalize().unwrap())]
        );
    }

    #[test]
    fn plugin_discovery_rejects_an_existing_non_directory_plugin_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("plugins"), "not a directory").unwrap();
        let error = plugins(directory.path()).unwrap_err();
        assert!(error.to_string().contains("插件路径不是目录"));
    }

    #[test]
    fn plugin_discovery_rejects_a_manifest_that_is_a_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("plugins/broken/Cargo.toml")).unwrap();
        let error = plugins(directory.path()).unwrap_err();
        assert!(error.to_string().contains("插件清单不是普通文件"));
    }

    #[cfg(unix)]
    #[test]
    fn plugin_discovery_rejects_dangling_directory_candidate_and_manifest_symlinks() {
        use std::os::unix::fs::symlink;

        for relative in ["plugins", "plugins/broken", "plugins/broken/Cargo.toml"] {
            let directory = tempfile::tempdir().unwrap();
            let link = directory.path().join(relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(directory.path().join("missing-target"), &link).unwrap();
            let error = plugins(directory.path()).unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::NotFound,
                "断链不能被视为未安装插件: {relative}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn plugin_discovery_keeps_working_directory_and_manifest_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let plugin = write_discovery_plugin(directory.path());
        fs::rename(plugin.join("Cargo.toml"), plugin.join("manifest.toml")).unwrap();
        symlink("manifest.toml", plugin.join("Cargo.toml")).unwrap();
        fs::rename(
            directory.path().join("plugins"),
            directory.path().join("sources"),
        )
        .unwrap();
        symlink("sources", directory.path().join("plugins")).unwrap();
        fs::rename(
            directory.path().join("sources/discovery-probe"),
            directory.path().join("external-probe"),
        )
        .unwrap();
        symlink("../external-probe", plugin).unwrap();
        assert_eq!(
            plugins(directory.path()).unwrap(),
            vec![(
                "discovery-probe".into(),
                directory
                    .path()
                    .join("external-probe")
                    .canonicalize()
                    .unwrap()
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn plugin_discovery_propagates_an_unsearchable_candidate_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let plugin = write_discovery_plugin(directory.path());
        let original_permissions = fs::metadata(&plugin).unwrap().permissions();
        fs::set_permissions(&plugin, fs::Permissions::from_mode(0o600)).unwrap();
        let manifest_access = fs::symlink_metadata(plugin.join("Cargo.toml"));
        let discovered = plugins(directory.path());
        fs::set_permissions(&plugin, original_permissions).unwrap();

        // root 等具有额外权限的执行用户不会触发搜索权限失败。
        if let Err(error) = manifest_access {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(
                discovered
                    .unwrap_err()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn logged_command_discards_oversized_lines_instead_of_retaining_secret_fragments() {
        let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let observed = lines.clone();
        let output: BuildOutput = Arc::new(move |line| observed.lock().unwrap().push(line.into()));
        run_logged(
            Command::new("printf").arg("private-token-fragment".repeat(500)),
            output,
        )
        .unwrap();
        assert_eq!(
            *lines.lock().unwrap(),
            vec!["[输出行超过 4096 字节，已隐藏]"]
        );
    }
    #[cfg(unix)]
    #[test]
    fn stopping_a_command_terminates_its_owned_process_group_promptly() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 & wait"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        stop_child(&mut child);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(child.try_wait().unwrap().is_some());
    }
    #[test]
    fn source_identity_changes_with_code_but_not_build_cache_pin_or_finder_metadata() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='1.0.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "fn first() {}").unwrap();
        let first = source_identity(&root).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/file"), "cache").unwrap();
        fs::write(root.join(".nuono-pin"), "pin").unwrap();
        fs::write(root.join(".DS_Store"), "Finder root metadata").unwrap();
        fs::write(root.join("src/.DS_Store"), "Finder nested metadata").unwrap();
        assert_eq!(
            first.source_hash,
            source_identity(&root).unwrap().source_hash
        );
        fs::write(root.join(".DS_Store"), "updated Finder root metadata").unwrap();
        fs::write(root.join("src/.DS_Store"), "updated Finder nested metadata").unwrap();
        assert_eq!(
            first.source_hash,
            source_identity(&root).unwrap().source_hash
        );
        fs::write(root.join("src/lib.rs"), "fn second() {}").unwrap();
        assert_ne!(
            first.source_hash,
            source_identity(&root).unwrap().source_hash
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn application_registers_only_selected_plugins_and_defaults_to_incremental() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let (manifest, main) = generate(
            &root,
            &[("chosen-plugin".into(), root.join("plugins/chosen"))],
        )
        .unwrap();
        let value: toml::Value = toml::from_str(&manifest).unwrap();
        assert_eq!(value["dependencies"].as_table().unwrap().len(), 2);
        assert!(main.contains("plugin_0::nuono_create_plugin"));
        assert!(!main.contains("plugin_1"));
        assert_eq!(
            value["build-dependencies"]["toml"],
            toml::from_str::<toml::Value>(&fs::read_to_string(root.join("Cargo.toml")).unwrap())
                .unwrap()["workspace"]["dependencies"]["toml"]
        );
        assert!(main.contains("include!(concat!(env!(\"OUT_DIR\"), \"/build_metadata.rs\"))"));
        assert!(
            main.find("register_build_metadata").unwrap()
                < main.find("run_with_extensions").unwrap()
        );
        assert!(APPLICATION_BUILD_SCRIPT.contains("emit_build_metadata"));
        assert_eq!(value["profile"]["dev"]["incremental"].as_bool(), Some(true));
        assert_eq!(
            value["profile"]["release"]["incremental"].as_bool(),
            Some(true)
        );
        assert!(!main.contains("register_plugin_icon"));
    }

    #[test]
    fn managed_service_policy_framework_build_needs_no_private_plugins() {
        let empty = tempfile::tempdir().unwrap();
        assert!(extensions(empty.path()).unwrap().is_empty());
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let (manifest, main) = generate(&root, &[]).unwrap();
        let manifest: toml::Value = toml::from_str(&manifest).unwrap();
        let dependencies = manifest["dependencies"].as_table().unwrap();
        assert_eq!(dependencies.len(), 1);
        assert!(dependencies.contains_key("nuono-server"));
        assert!(main.contains("run_with_extensions(vec![], vec![])"));
        assert!(!main.contains("nuono_create_plugin"));
    }

    #[test]
    fn adapter_only_workspace_generates_an_adapter_factory_without_business_plugins() {
        let framework = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("adapters/fixture");
        fs::create_dir_all(adapter.join("src")).unwrap();
        fs::write(adapter.join("Cargo.toml"), "[workspace]\n[package]\nname='fixture-adapter'\nversion='0.1.0'\n[package.metadata.nuono]\nkind='adapter'\n[dependencies]\nnuono-adapter={path='/fixture/sdk'}\n").unwrap();
        fs::write(adapter.join("src/lib.rs"), "// code generation fixture\n").unwrap();
        let selected = extensions(directory.path()).unwrap();
        assert_eq!(selected.len(), 1);
        let sources = current_sources(directory.path()).unwrap();
        assert_eq!(sources[0].kind, SourceKind::Adapter);
        let (manifest, main) = generate(&framework, &selected).unwrap();
        let manifest: toml::Value = toml::from_str(&manifest).unwrap();
        assert_eq!(manifest["dependencies"].as_table().unwrap().len(), 2);
        assert!(main.contains("run_with_extensions(vec![], vec![plugin_0::nuono_create_adapter])"));
        assert!(!main.contains("nuono_create_plugin"));
    }

    #[test]
    fn source_kind_is_part_of_publication_identity_and_old_records_are_business_plugins() {
        let plugin: SourcePlugin = serde_json::from_value(serde_json::json!({"id":"same","package":"same","version":"0.1.0","source_hash":"same"})).unwrap();
        assert_eq!(plugin.kind, SourceKind::Plugin);
        let mut adapter = plugin.clone();
        adapter.kind = SourceKind::Adapter;
        assert!(!same_sources(&[plugin], &[adapter]));
    }

    #[test]
    fn an_adapter_cannot_be_installed_through_the_business_plugin_directory() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("plugins/fixture");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("Cargo.toml"), "[workspace]\n[package]\nname='fixture-adapter'\nversion='0.1.0'\n[package.metadata.nuono]\nkind='adapter'\n[dependencies]\nnuono-adapter={path='/fixture/sdk'}\n").unwrap();
        assert!(extensions(directory.path()).is_err());
    }

    #[test]
    fn external_shared_sources_use_the_local_ai_and_rag_type_definitions() {
        let framework = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let directory = tempfile::tempdir().unwrap();
        for (source, dependency) in [
            ("crates-io", "nuono-plugin='0.4'"),
            (
                "https://example.com/framework.git",
                "nuono-plugin={git='https://example.com/framework.git'}",
            ),
            (
                "https://example.com/contracts.git",
                "nuono-plugin={path='/fixture/sdk'}\nai-contracts={package='nuono-ai',git='https://example.com/contracts.git'}",
            ),
            (
                "crates-io",
                "nuono-plugin={path='/fixture/sdk'}\nretrieval={package='nuono-rag',version='0.4'}",
            ),
            (
                "https://example.com/contracts.git",
                "nuono-plugin={path='/fixture/sdk'}\n[target.'cfg(unix)'.dependencies]\nai-contracts={package='nuono-ai',git='https://example.com/contracts.git'}",
            ),
            (
                "https://example.com/contracts.git",
                "nuono-plugin={path='/fixture/sdk'}\nai-contracts={workspace=true}\n[workspace.dependencies]\nai-contracts={package='nuono-ai',git='https://example.com/contracts.git'}",
            ),
        ] {
            fs::write(
                directory.path().join("Cargo.toml"),
                format!(
                    "[package]\nname='ai-fixture'\nversion='0.1.0'\n[dependencies]\n{dependency}\n"
                ),
            )
            .unwrap();
            let (manifest, _) = generate(
                &framework,
                &[("ai-fixture".into(), directory.path().to_path_buf())],
            )
            .unwrap();
            let value: toml::Value = toml::from_str(&manifest).unwrap();
            for name in ["nuono-plugin", "nuono-app", "nuono-ai", "nuono-rag"] {
                assert_eq!(
                    value["patch"][source][name]["path"].as_str(),
                    Some(framework.join("crates").join(name).to_str().unwrap()),
                );
            }
        }
    }

    #[test]
    fn application_embeds_package_icon_under_actual_runtime_plugin_name() {
        let framework = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("outer-directory");
        fs::create_dir(&plugin).unwrap();
        fs::write(
            plugin.join("Cargo.toml"),
            "[package]\nname=\"icon-fixture\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(plugin.join("icon.ico"), [0, 0, 1, 0]).unwrap();
        let before = source_identity(&plugin).unwrap();
        let (_, main) = generate(&framework, &[("icon-fixture".into(), plugin.clone())]).unwrap();
        assert!(main.contains("register_source_runtime_name(\"outer-directory\", plugin.name())"));
        assert!(main.contains(
            "register_plugin_icon(plugin.name(), \"image/x-icon\", b\"\\x00\\x00\\x01\\x00\")"
        ));
        assert!(!main.contains("include_bytes!"));
        // 图标文件变更也会改变插件的 `source_hash`，与插件源码享受同等级别的发布一致性保护。
        fs::write(plugin.join("icon.ico"), [0, 0, 1, 1]).unwrap();
        assert_ne!(
            before.source_hash,
            source_identity(&plugin).unwrap().source_hash
        );
    }
}
