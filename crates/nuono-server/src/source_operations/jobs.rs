//! 持久化源码插件操作任务（安装、更新、版本切换、卸载）与中断恢复；后台构建任务独立运行，不绑定 HTTP 请求生命周期。
use super::{
    OperationContext, find_store_plugin, git, installed_commit, is_valid_plugin_id,
    is_valid_repo_url, select_repo,
};
use crate::operation_error::{OperationError as OpError, Result as OpResult};
use crate::source_build::{self, SourceKind, SourcePlugin};
use nuono_tooling::recovery::staging;
use serde::Serialize;
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

const MAX_LOG_BYTES: usize = 256 * 1024;
use nuono_tooling::recovery::{
    job_path, persist, phase, process_alive, publication_authorized, read_jobs, recover_update,
};

pub(crate) use nuono_tooling::recovery::StoreJob;
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct StoreJobs {
    #[schema(value_type = Vec<crate::source_schemas::StoreJobSchema>)]
    jobs: Vec<StoreJob>,
    source_available: bool,
    tools_available: bool,
    restart_supported: bool,
    limitation: String,
    #[schema(value_type = Vec<crate::source_schemas::SourcePluginSchema>)]
    running: Vec<SourcePlugin>,
}

fn directory(state: &OperationContext) -> PathBuf {
    state.data.join(".plugin-jobs")
}

fn requested_job(directory: &FsPath, id: &str) -> OpResult<StoreJob> {
    let path = job_path(directory, id).map_err(OpError::invalid)?;
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OpError::missing("任务不存在")
        } else {
            OpError::from(error)
        }
    })?;
    if bytes.len() > 1024 * 1024 {
        return Err(OpError::from("任务记录过大"));
    }
    serde_json::from_slice(&bytes).map_err(|e| OpError::from(format!("任务记录损坏: {e}")))
}

fn sanitize_build_line(line: &str, secrets: &[String]) -> String {
    let mut line = line.to_string();
    for secret in secrets {
        line = line.replace(secret, "[REDACTED]");
    }
    nuono_core::telemetry::sanitize_file_line(&line).0
}

fn log_output(directory: PathBuf, id: String) -> source_build::BuildOutput {
    let secrets: Vec<String> = std::env::vars()
        .filter(|(key, value)| {
            let key = key.to_ascii_uppercase();
            value.len() >= 4
                && [
                    "TOKEN",
                    "PASSWORD",
                    "SECRET",
                    "KEY",
                    "CREDENTIAL",
                    "AUTH",
                    "COOKIE",
                    "DATABASE_URL",
                ]
                .iter()
                .any(|word| key.contains(word))
        })
        .map(|(_, value)| value)
        .collect();
    let lock = std::sync::Mutex::new(());
    Arc::new(move |line| {
        let Ok(_guard) = lock.lock() else {
            return;
        };
        // 复用日志查看器的脱敏规则：抹除 URL 凭据、环境变量中的敏感密钥/Token 以及 ANSI 终端控制序列。
        let mut line = sanitize_build_line(line, &secrets);
        if line.len() > 4096 {
            let mut end = 4096;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        line.push('\n');
        let path = directory.join(format!("{id}.log"));
        let mut bytes = fs::read(&path).unwrap_or_default();
        if bytes.len() + line.len() > MAX_LOG_BYTES {
            let start = bytes.len().saturating_sub(MAX_LOG_BYTES / 2);
            let start = bytes[start..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| start + i + 1);
            bytes.drain(..start);
        }
        bytes.extend_from_slice(line.as_bytes());
        let _ = fs::write(path, bytes);
    })
}
pub(crate) fn tool_exists(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            path.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
                .is_file()
        })
    })
}
fn supervised() -> bool {
    std::env::var("NUONO_SUPERVISED").is_ok_and(|v| v == "1")
}
pub(crate) fn source_root() -> OpResult<PathBuf> {
    let current = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = crate::find_nuono_repository_root(&current)
        .ok_or_else(|| OpError::conflict("当前是预编译部署，请在构建机器调整插件并重新部署"))?;
    let executable = source_build::running_executable()
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)
        .map_err(|e| e.to_string())?;
    // Windows 上 `current_exe` 返回普通盘符路径，`canonicalize` 却会加 `\\?\` 逐字前缀；统一形态后再比较。
    let executable = executable.canonicalize().unwrap_or(executable);
    let target = source_build::target_dir(&root).map_err(|e| e.to_string())?;
    let target = target.canonicalize().unwrap_or(target);
    // 优先使用启动时缓存的可执行文件路径：在 Linux 上，若运行中的二进制被新构建产物原子替换，`/proc/self/exe` 会追加 ` (deleted)` 后缀。
    let expected_names = [
        format!("nuono{}", std::env::consts::EXE_SUFFIX),
        format!("nuono-application{}", std::env::consts::EXE_SUFFIX),
    ];
    if !executable.starts_with(&target)
        || !executable.file_name().is_some_and(|name| {
            expected_names
                .iter()
                .any(|expected| name == expected.as_str())
        })
    {
        return Err(OpError::conflict(
            "当前运行的是导出程序；在线构建不会替换它。请在构建机器调整插件并重新导出，再替换部署程序。",
        ));
    }
    Ok(root)
}

/// 在受理新任务前先核对并恢复上次中断的遗留任务（必须严格核对二进制哈希与运行清单，绝不盲目假定构建已生效）。
async fn reconcile(state: &OperationContext) -> Result<(), String> {
    let Ok(_guard) = source_build::operation_lock().try_lock_owned() else {
        return Ok(());
    };
    reconcile_locked(state).await
}
fn job_is_applied(job: &StoreJob, running: &[SourcePlugin]) -> bool {
    let actual = running
        .iter()
        .find(|p| p.kind == job.kind && p.id == job.plugin_id);
    if job.operation == "uninstall" {
        actual.is_none()
    } else {
        actual
            .zip(job.expected.as_ref())
            .is_some_and(|(actual, expected)| {
                actual.kind == expected.kind
                    && actual.kind == job.kind
                    && actual.source_hash == expected.source_hash
                    && (expected.commit.is_none() || actual.commit == expected.commit)
            })
    }
}

fn artifact_is_applied(
    job: &StoreJob,
    running: &[SourcePlugin],
    publication: Option<&source_build::Publication>,
) -> bool {
    artifact_is_applied_by_binary(
        job,
        running,
        publication,
        source_build::running_binary_sha256(),
    )
}
fn artifact_is_applied_by_binary(
    job: &StoreJob,
    running: &[SourcePlugin],
    publication: Option<&source_build::Publication>,
    running_sha256: Option<&str>,
) -> bool {
    if job.artifact.is_none() {
        return job_is_applied(job, running);
    }
    publication.is_some_and(|publication| {
        !publication.deferred_binary
            && running_sha256 == Some(publication.artifact.sha256.as_str())
            && source_build::same_sources(running, &publication.artifact.sources)
            && job_is_applied(job, running)
    })
}

async fn reconcile_locked(state: &OperationContext) -> Result<(), String> {
    let directory = directory(state);
    let root = source_root().ok();
    let lock_root = root.clone();
    let build_lock = tokio::task::spawn_blocking(move || {
        lock_root
            .as_deref()
            .map(source_build::try_application_lock)
            .transpose()
            .map(|lock| lock.flatten())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if root.is_some() && build_lock.is_none() {
        return Ok(());
    }
    let running = source_build::running_manifest();
    let records = read_jobs(&directory)?;
    for mut job in read_jobs(&directory)? {
        if job.active() {
            if job.owner_pid != std::process::id() && process_alive(job.owner_pid) {
                continue;
            }
            if let (Some(root), Some(lock)) = (root.as_deref(), build_lock.as_ref()) {
                if let Err(error) = recover_source_locked(root, &job, lock).await {
                    job.error = Some(format!(
                        "上次运行意外中断；恢复源码失败: {error}。请保留源码目录并联系管理员。"
                    ));
                } else {
                    job.error = Some("上次运行意外中断，已恢复可用源码；请查看日志并重试。".into());
                }
            } else {
                job.error = Some("上次操作中断；请在原构建机器检查源码后重试。".into());
            }
            phase(&directory, &mut job, "failed", "操作中断，未自动继续")?;
        } else if job.phase == "awaiting_restart" {
            let publication = build_lock
                .as_ref()
                .and_then(|lock| {
                    source_build::read_publication(&lock.target, env!("NUONO_BUILD_PROFILE"))
                        .ok()
                        .flatten()
                })
                .filter(|publication| publication_authorized(publication, &records));
            let applied = artifact_is_applied(&job, &running, publication.as_ref());
            if applied {
                phase(&directory, &mut job, "applied", "当前程序已应用这次变更")?;
            }
        }
    }
    let records = read_jobs(&directory)?;
    let maintain_directory = directory.clone();
    tokio::task::spawn_blocking(move || {
        let protected = root
            .as_deref()
            .map(|root| {
                source_build::target_dir(root)
                    .and_then(|target| source_build::protected_job_ids(&target))
            })
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        staging::maintain(root.as_deref(), &maintain_directory, &records, &protected)?;
        if let Some(root) = root.as_deref()
            && let Ok(target) = source_build::target_dir(root)
            && let Err(error) = source_build::maintain_artifacts(root, &target)
        {
            tracing::warn!(%error, "保留无法校验的历史构建产物");
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(())
}

pub(crate) async fn list_jobs(
    state: &OperationContext,
    log_id: Option<&str>,
) -> OpResult<StoreJobs> {
    reconcile(state).await?;
    let mut jobs = match read_jobs(&directory(state)) {
        Ok(jobs) => jobs,
        Err(e) => return Err(e.into()),
    };
    if let Some(id) = log_id {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(OpError::invalid("任务编号无效"));
        }
        if !jobs.iter().any(|job| job.id == id) {
            return Err(OpError::missing("任务不存在"));
        }
        if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
            use std::io::Read;
            let path = directory(state).join(format!("{id}.log"));
            if let Ok(file) = fs::File::open(path) {
                let mut bytes = Vec::new();
                if file
                    .take(MAX_LOG_BYTES as u64)
                    .read_to_end(&mut bytes)
                    .is_ok()
                {
                    job.log = String::from_utf8_lossy(&bytes).into();
                }
            }
        }
    }
    let source_available = source_root().is_ok();
    let tools_available = tool_exists("cargo") && tool_exists("git");
    let limitation = if !source_available {
        "当前是预编译部署：请在构建机器安装或移除插件，再导出并替换程序。"
    } else if !tools_available {
        "安装和更新需要 Git、Cargo。请先在服务器安装缺少的工具。"
    } else if !supervised() {
        "安装完成后，可继续拉取其他插件，全部部署完成后再手动重启 Nuono（开发模式运行 cargo run，发布模式运行 cargo run --release）。"
    } else {
        "部署完成后等待重启生效，可继续拉取其他插件后统一重启。卸载后将清理插件注册和群设置，配置文件、密钥及业务数据保留。"
    };
    Ok(StoreJobs {
        jobs: jobs
            .into_iter()
            .map(|mut job| {
                job.artifact = None;
                job
            })
            .collect(),
        source_available,
        tools_available,
        restart_supported: supervised(),
        limitation: limitation.into(),
        running: source_build::running_manifest(),
    })
}

pub(crate) async fn retry(state: OperationContext, id: String) -> OpResult<StoreJob> {
    let job = match requested_job(&directory(&state), &id) {
        Ok(job) => job,
        Err(e) => return Err(e),
    };
    if job.phase != "failed" {
        return Err(OpError::conflict("只有失败的任务可以重试"));
    }
    let operation = job.operation.clone();
    submit(
        state,
        super::source_key(job.kind, &job.plugin_id),
        &operation,
        job.repo_url,
        job.target_commit,
        job.expected_source_commit,
    )
    .await
}
pub(crate) async fn restart(state: OperationContext, id: String) -> OpResult<String> {
    if crate::backup::transfer::is_pending(&state.data)
        || state.data.join(crate::backup::RESTORE_MARKER).exists()
    {
        return Err(OpError::conflict("已有数据恢复安排，请按恢复流程重启"));
    }
    if !supervised() {
        return Err(OpError::conflict(
            "当前没有声明进程托管，请在终端手动重启 Nuono",
        ));
    }
    let Ok(_guard) = source_build::operation_lock().try_lock_owned() else {
        return Err(OpError::conflict(
            "还有源码操作或缓存维护正在运行，请稍后重启",
        ));
    };
    let root = match source_root() {
        Ok(root) => root,
        Err(error) => return Err(error),
    };
    let jobs_directory = directory(&state);
    let validation = tokio::task::spawn_blocking(move || {
        let lock = source_build::try_application_lock(&root)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| OpError::conflict("还有源码构建正在运行，请稍后重启"))?;
        let job = requested_job(&jobs_directory, &id)?;
        if job.phase != "awaiting_restart" {
            return Err(OpError::conflict("该任务尚未构建完成，暂时不能重启"));
        }
        let publication = source_build::read_publication(&lock.target, env!("NUONO_BUILD_PROFILE"))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| OpError::conflict("该任务缺少已提交构建凭据，请重新构建"))?;
        if job.artifact.is_none()
            || !publication_authorized(&publication, &read_jobs(&jobs_directory)?)
            || !job_is_applied(&job, &publication.artifact.sources)
        {
            return Err(OpError::conflict(
                "任务版本不在当前有效构建中，请刷新任务并重新构建",
            ));
        }
        source_build::verify_publication(
            &lock.target,
            &publication,
            &source_build::current_sources(&root).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok::<_, OpError>(lock)
    })
    .await
    .map_err(|e| OpError::from(e.to_string()))
    .and_then(|result| result);
    let build_lock = match validation {
        Ok(lock) => lock,
        Err(error) => return Err(error),
    };
    let control = state.framework_control.clone();
    tokio::spawn(async move {
        let _guard = _guard;
        let _build_lock = build_lock;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        control.request_restart("plugin source changes");
    });
    Ok("已安排重启，页面恢复连接后会核对生效版本。".into())
}

pub(crate) async fn submit_checkout(
    state: OperationContext,
    id: String,
    commit: String,
    expected: String,
) -> OpResult<StoreJob> {
    submit(state, id, "update", None, Some(commit), Some(expected)).await
}

/// 解析 Git 链接安装使用的扩展 ID：用户显式填写时校验并沿用该 ID，留空则由仓库地址自动推导。
fn resolve_link_install_id(repo_url: &str, requested_id: Option<&str>) -> Result<String, String> {
    if let Some(custom) = requested_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !is_valid_plugin_id(custom) {
            return Err("扩展 ID 只能包含字母、数字、连字符和下划线，且不能使用保留名称".into());
        }
        return Ok(custom.to_string());
    }
    super::link_install_id(repo_url)
}

pub(crate) async fn submit_link_install(
    state: OperationContext,
    repo_url: String,
    requested_id: Option<String>,
) -> OpResult<StoreJob> {
    let repo_url = repo_url.trim().to_string();
    if !is_valid_repo_url(&repo_url) {
        return Err(OpError::invalid(
            "仓库地址无效，请使用无密码的 HTTPS 或 SSH Git 地址",
        ));
    }
    let id =
        resolve_link_install_id(&repo_url, requested_id.as_deref()).map_err(OpError::invalid)?;
    submit(state, id, "install", Some(repo_url), None, None).await
}

pub(crate) async fn submit(
    state: OperationContext,
    id: String,
    operation: &str,
    repo_url: Option<String>,
    target_commit: Option<String>,
    expected_source_commit: Option<String>,
) -> OpResult<StoreJob> {
    let (kind, id) = super::parse_source_key(&id).map_err(OpError::invalid)?;
    if !is_valid_plugin_id(&id) {
        return Err(OpError::invalid(
            "扩展 ID 只能包含字母、数字、连字符和下划线，且不能使用保留名称",
        ));
    }
    if crate::backup::transfer::is_pending(&state.data)
        || state.data.join(crate::backup::RESTORE_MARKER).exists()
    {
        return Err(OpError::conflict(
            "已有数据恢复安排，完成恢复前不能变更插件源码",
        ));
    }
    let root = match source_root() {
        Ok(root) => root,
        Err(e) => return Err(e),
    };
    if !tool_exists("cargo") || !tool_exists("git") {
        return Err(OpError::conflict("缺少 Cargo 或 Git，无法执行源码插件操作"));
    }
    if repo_url.as_ref().is_some_and(|url| !is_valid_repo_url(url)) {
        return Err(OpError::invalid(
            "仓库地址无效，请使用无密码的 HTTPS 或 SSH Git 地址",
        ));
    }
    let Ok(guard) = source_build::operation_lock().try_lock_owned() else {
        return Err(OpError::conflict(
            "已有插件构建或缓存维护正在运行，请在任务列表查看进度",
        ));
    };
    // 与恢复安排持有同一把操作锁后重新核对，避免等待锁期间出现新恢复标记。
    if crate::backup::transfer::is_pending(&state.data)
        || state.data.join(crate::backup::RESTORE_MARKER).exists()
    {
        return Err(OpError::conflict(
            "已有数据恢复安排，完成恢复前不能变更插件源码",
        ));
    }
    reconcile_locked(&state).await?;
    if read_jobs(&directory(&state))?
        .iter()
        .any(|job| job.active() && process_alive(job.owner_pid))
    {
        return Err(OpError::conflict("已有插件任务仍在运行，请等待该任务结束"));
    }
    let target = root.join(kind.directory()).join(&id);
    // 通过链接安装时，扩展类型要等下载到仓库清单后才能确定；目录冲突留给 run_job 按实际类型检查。
    if operation == "install" && repo_url.is_none() && target.exists() {
        return Err(OpError::conflict("扩展目录已存在；已安装的扩展请使用更新"));
    }
    if operation != "install" && !target.join("Cargo.toml").is_file() {
        return Err(OpError::missing(
            "插件源码未安装；如果是导出程序内置的插件，请在构建机器调整后重新导出",
        ));
    }
    if operation != "install"
        && let Err(e) = ensure_clean_source(&target).await
    {
        return Err(OpError::conflict(e));
    }
    if let Some(commit) = &target_commit {
        let validation = async {
            let expected = expected_source_commit
                .as_deref()
                .ok_or("缺少当前源码版本，请刷新后重试")?;
            git::expected_head(&target, expected).await?;
            git::verify_commit(&target, commit).await?;
            let repo = git::origin(&target).await?;
            let config = state.config.snapshot().await;
            let plugin =
                find_store_plugin(&super::source_key(kind, &id), &config.plugin_store).await?;
            git::target_policy(plugin.as_ref(), &repo, commit)?;
            Ok::<(), String>(())
        }
        .await;
        validation.map_err(OpError::conflict)?;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let job = StoreJob {
        kind,
        id: uuid::Uuid::new_v4().to_string(),
        plugin_id: id,
        operation: operation.into(),
        phase: "queued".into(),
        message: "已创建任务，准备检查插件来源".into(),
        error: None,
        created_at: now.clone(),
        updated_at: now,
        repo_url,
        previous_commit: None,
        target_commit: target_commit.map(|s| s.to_ascii_lowercase()),
        expected_source_commit: expected_source_commit.map(|s| s.to_ascii_lowercase()),
        source_switched: Some(false),
        staged_name: None,
        runtime_name: None,
        previously_enabled: false,
        expected: None,
        artifact: None,
        owner_pid: std::process::id(),
        log: String::new(),
    };
    persist(&directory(&state), &job)?;
    let response = job.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let mut job = job;
        let lock_root = root.clone();
        let build_lock =
            match tokio::task::spawn_blocking(move || source_build::application_lock(&lock_root))
                .await
            {
                Ok(Ok(lock)) => Arc::new(lock),
                Ok(Err(error)) => {
                    job.error = Some(format!("无法获取构建锁: {error:#}"));
                    let _ = phase(
                        &directory(&state),
                        &mut job,
                        "failed",
                        "无法开始源码操作，请稍后重试",
                    );
                    return;
                }
                Err(error) => {
                    job.error = Some(format!("无法获取构建锁: {error}"));
                    let _ = phase(
                        &directory(&state),
                        &mut job,
                        "failed",
                        "无法开始源码操作，请稍后重试",
                    );
                    return;
                }
            };
        let result = run_job(&state, &root, &mut job, build_lock.clone()).await;
        if let Err(error) = result {
            let recovery = recover_source_locked(&root, &job, &build_lock).await;
            if job.operation == "uninstall"
                && recovery.is_ok()
                && job.previously_enabled
                && let Some(name) = &job.runtime_name
            {
                let _ = state.plugin_host.toggle(name, true).await;
            }
            let error = match recovery {
                Ok(()) => format!("{error}；原有运行程序和可用源码已保留"),
                Err(recovery) => {
                    format!("{error}；恢复源码失败: {recovery}，请保留源码目录并检查任务日志")
                }
            };
            let (error, _) = nuono_core::telemetry::sanitize_file_line(&error);
            job.error = Some(error.clone());
            log_output(directory(&state), job.id.clone())(&error);
            let _ = phase(
                &directory(&state),
                &mut job,
                "failed",
                "操作失败，请查看日志；修复后可重试",
            );
        }
    });
    Ok(response)
}

async fn ensure_clean_source(target: &FsPath) -> Result<(), String> {
    git::independent_repository(target).await?;
    if git::source_dirty(target).await? {
        return Err("插件有未提交的本地修改，已停止操作。请先备份或提交修改，再回来重试。".into());
    }
    Ok(())
}
async fn logged_git(
    root: &FsPath,
    args: Vec<String>,
    output: source_build::BuildOutput,
) -> Result<(), String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new("git");
        git::configure_command(&mut command);
        command
            .current_dir(root)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0");
        source_build::run_logged(&mut command, output).map_err(|e| format!("Git 操作失败: {e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn run_job(
    state: &OperationContext,
    root: &FsPath,
    job: &mut StoreJob,
    build_lock: Arc<source_build::ApplicationBuildLock>,
) -> Result<(), String> {
    let directory = directory(state);
    let mut target = root.join(job.kind.directory()).join(&job.plugin_id);
    let output = log_output(directory.clone(), job.id.clone());
    if job.operation == "uninstall" {
        // 此阶段只停用并移走源码，不清理 DB 注册或群设置。新程序启动确认完整
        // 插件名单后再事务清理；构建失败或尚未重启时保留这些记录以供恢复。
        ensure_clean_source(&target).await?;
        let running = source_build::running_manifest()
            .into_iter()
            .find(|p| p.kind == job.kind && p.id == job.plugin_id);
        if let Some(running) = running.filter(|_| job.kind == SourceKind::Plugin) {
            let name = running
                .runtime_name
                .ok_or("无法确认运行中的插件名称，请先从插件管理停用，再在终端卸载")?;
            job.previously_enabled = state
                .db
                .is_plugin_enabled(&name)
                .await
                .map_err(|e| e.to_string())?;
            job.runtime_name = Some(name.clone());
            persist(&directory, job)?;
            state
                .plugin_host
                .toggle(&name, false)
                .await
                .map_err(|e| format!("插件未能停用，已中止卸载: {e}"))?;
        }
        job.staged_name = Some(format!(".nuono-removed-{}", job.id));
        if let Err(error) = staging::seal(&directory, job, &target) {
            tracing::warn!(%error, "暂存源码封存失败，将保留目录");
        }
        phase(
            &directory,
            job,
            "removing",
            "已停用插件，准备移除源码；注册记录和群设置将在新程序启动后清理",
        )?;
        fs::rename(
            &target,
            root.join(job.kind.directory())
                .join(job.staged_name.as_ref().unwrap()),
        )
        .map_err(|e| e.to_string())?;
    } else {
        let config = state.config.snapshot().await;
        if job.operation == "install" {
            job.staged_name = Some(format!(".nuono-install-{}", job.id));
            phase(&directory, job, "downloading", "正在下载扩展源码")?;
            let staged = root
                .join(job.kind.directory())
                .join(job.staged_name.as_ref().unwrap());
            fs::create_dir_all(root.join(job.kind.directory())).map_err(|e| e.to_string())?;
            let (repo, pre_clone_commit) = if let Some(requested_repo) = job.repo_url.as_deref() {
                // 链接安装需等克隆并读取 Cargo.toml 确认真实类型后再查商店条目，
                // 避免适配器绕过商店策略，也避免与同名插件发生误拦截。
                (requested_repo.to_string(), None)
            } else {
                let plugin = find_store_plugin(
                    &super::source_key(job.kind, &job.plugin_id),
                    &config.plugin_store,
                )
                .await?;
                let repo = select_repo(None, plugin.as_ref())?;
                (repo, plugin.and_then(|plugin| plugin.pinned_commit))
            };
            if !is_valid_repo_url(&repo) {
                return Err("扩展仓库地址无效".into());
            }
            logged_git(
                root,
                vec![
                    "clone".into(),
                    "--".into(),
                    repo,
                    staged.to_string_lossy().into(),
                ],
                output.clone(),
            )
            .await?;
            // 链接安装的类型由仓库清单决定：适配器清单切到 adapters/ 暂存目录并同步任务记录。
            let staged = adopt_declared_kind(root, &directory, job, &staged)?;
            target = root.join(job.kind.directory()).join(&job.plugin_id);
            if let Err(error) = staging::seal(&directory, job, &staged) {
                tracing::warn!(%error, "暂存源码封存失败，将保留目录");
            }
            let pinned_commit = if job.repo_url.is_some() {
                let plugin = find_store_plugin(
                    &super::source_key(job.kind, &job.plugin_id),
                    &config.plugin_store,
                )
                .await?;
                enforce_link_install_store_policy(job, plugin.as_ref())?
            } else {
                pre_clone_commit
            };
            if let Some(commit) = pinned_commit {
                logged_git(
                    &staged,
                    vec!["checkout".into(), "--detach".into(), commit],
                    output.clone(),
                )
                .await?;
            }
            if target.exists() {
                return Err("扩展目录已存在；已安装的扩展请使用更新".into());
            }
            fs::rename(&staged, &target).map_err(|e| e.to_string())?;
        } else {
            let plugin = find_store_plugin(
                &super::source_key(job.kind, &job.plugin_id),
                &config.plugin_store,
            )
            .await?;
            let origin = git::origin(&target).await?;
            let explicit = job.target_commit.is_some();
            let commit = if let Some(commit) = &job.target_commit {
                git::target_policy(plugin.as_ref(), &origin, commit)?;
                git::verify_commit(&target, commit).await?;
                commit.clone()
            } else {
                let plugin = plugin.as_ref().ok_or(
                    "该插件没有商店发布记录，请在终端更新来源，或配置提供该插件的商店目录",
                )?;
                if origin != plugin.repo_url {
                    return Err("已安装插件的仓库与商店发布记录不同，已停止更新".into());
                }
                plugin
                    .pinned_commit
                    .clone()
                    .ok_or("商店尚未发布固定版本，暂不支持在线更新")?
            };
            ensure_clean_source(&target).await?;
            if let Some(expected) = &job.expected_source_commit {
                git::expected_head(&target, expected).await?;
            }
            job.previous_commit = Some(git::current_commit(&target).await?);
            phase(
                &directory,
                job,
                "downloading",
                if explicit {
                    "正在准备所选 Git 版本"
                } else {
                    "正在获取商店发布的固定版本"
                },
            )?;
            if !explicit {
                logged_git(
                    &target,
                    vec![
                        "fetch".into(),
                        "--tags".into(),
                        "--no-recurse-submodules".into(),
                        "origin".into(),
                    ],
                    output.clone(),
                )
                .await?;
            }
            ensure_clean_source(&target).await?;
            git::expected_head(&target, job.previous_commit.as_deref().unwrap()).await?;
            git::verify_commit(&target, &commit).await?;
            job.target_commit = Some(commit.clone());
            if job.expected_source_commit.is_none() {
                job.expected_source_commit = job.previous_commit.clone();
            }
            job.source_switched = Some(true);
            persist(&directory, job)?;
            logged_git(
                &target,
                vec!["checkout".into(), "--detach".into(), commit.clone()],
                output.clone(),
            )
            .await?;
            if !git::current_commit(&target)
                .await?
                .eq_ignore_ascii_case(&commit)
            {
                return Err("切换后 HEAD 与所选版本不一致，已停止构建".into());
            }
        }
        let manifest: toml::Value = toml::from_str(
            &fs::read_to_string(target.join("Cargo.toml")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if source_build::manifest_kind(&manifest).map_err(|e| e.to_string())? != job.kind {
            return Err("扩展声明的类型与安装目标不一致".into());
        }
        match job.kind {
            SourceKind::Plugin => crate::plugin_dev::validate_manifest(&manifest),
            SourceKind::Adapter => source_build::validate_adapter_manifest(&manifest),
        }
        .map_err(|e| e.to_string())?;
        if let Some(commit) = installed_commit(&target) {
            fs::write(target.join(".nuono-pin"), format!("{commit}\n"))
                .map_err(|e| e.to_string())?;
        }
        job.expected = Some(source_build::source_identity(&target).map_err(|e| e.to_string())?);
    }
    phase(
        &directory,
        job,
        "building",
        "正在统一构建，当前机器人继续使用原有程序",
    )?;
    let root_build = root.to_path_buf();
    let config_path = state.config.config_path().to_path_buf();
    let candidate_lock = build_lock.clone();
    let artifact = tokio::task::spawn_blocking(move || {
        let artifact = source_build::build_candidate_locked(
            &root_build,
            env!("NUONO_BUILD_PROFILE"),
            Some(output),
            &candidate_lock,
            &config_path,
        )
        .map_err(|e| format!("构建失败: {e:#}"))?;
        Ok::<_, String>(artifact)
    })
    .await
    .map_err(|e| e.to_string())??;
    if !job_is_applied(job, &artifact.sources) {
        return Err("构建清单没有包含任务选择的版本，未发布程序".into());
    }
    job.artifact = Some(artifact.clone());
    persist(&directory, job)?;
    let publication = source_build::prepare_job_candidate(root, &artifact, &job.id, &build_lock)
        .map_err(|e| format!("{e:#}"))?;
    let restart_message = if job.operation == "uninstall" {
        "卸载部署已完成，等待重启后生效（可继续调整其他插件后统一重启）；配置文件、密钥及业务数据保留"
    } else {
        "部署已完成，等待重启后生效（可继续安装或更新其他插件后统一重启）；配置文件、密钥及业务数据保留"
    };
    phase(&directory, job, "awaiting_restart", restart_message)?;
    source_build::complete_job_candidate(root, &publication, &build_lock)
        .map_err(|e| format!("{e:#}"))?;
    if job.kind == SourceKind::Adapter && job.operation == "uninstall" {
        state
            .bot_instances
            .source
            .suspend_type(&job.plugin_id)
            .await;
    }
    let earlier_jobs = match read_jobs(&directory) {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::warn!(%error, "新程序已提交，旧任务列表暂时无法读取");
            return Ok(());
        }
    };
    for mut earlier in earlier_jobs {
        if earlier.id != job.id
            && earlier.kind == job.kind
            && earlier.plugin_id == job.plugin_id
            && earlier.phase == "awaiting_restart"
            && let Err(error) = phase(
                &directory,
                &mut earlier,
                "superseded",
                "该插件已有更新的构建任务，以最新任务为准",
            )
        {
            tracing::warn!(%error, "已提交新程序，但旧任务状态尚未更新");
        }
    }
    Ok(())
}

/// 链接安装确认真实类型后，按最终 `(kind, id)` 核对商店策略：
/// 若同类型同 ID 已上架商店，禁止使用非商店仓库覆盖，并返回商店固定的 commit（若有）。
fn enforce_link_install_store_policy(
    job: &StoreJob,
    store_plugin: Option<&super::StorePlugin>,
) -> Result<Option<String>, String> {
    select_repo(job.repo_url.as_deref(), store_plugin)?;
    Ok(store_plugin.and_then(|plugin| plugin.pinned_commit.clone()))
}

/// 链接安装按仓库清单决定类型：清单声明为适配器时把暂存源码移入 `adapters/`
/// 并改写任务记录。显式 `adapter:` 任务和商店安装不参与推断，插件不会被降级。
fn adopt_declared_kind(
    root: &FsPath,
    directory: &FsPath,
    job: &mut StoreJob,
    staged: &FsPath,
) -> Result<PathBuf, String> {
    if job.kind != SourceKind::Plugin || job.repo_url.is_none() {
        return Ok(staged.to_path_buf());
    }
    let manifest: toml::Value = toml::from_str(
        &fs::read_to_string(staged.join("Cargo.toml")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if source_build::manifest_kind(&manifest).map_err(|error| error.to_string())?
        != SourceKind::Adapter
    {
        return Ok(staged.to_path_buf());
    }
    let staged_name = job
        .staged_name
        .as_deref()
        .ok_or("安装任务缺少暂存目录")?
        .to_string();
    let parent = root.join(SourceKind::Adapter.directory());
    fs::create_dir_all(&parent).map_err(|error| error.to_string())?;
    let moved = parent.join(staged_name);
    fs::rename(staged, &moved).map_err(|error| error.to_string())?;
    job.kind = SourceKind::Adapter;
    if let Err(error) = persist(directory, job) {
        // 任务记录写不进去时把源码移回原处，避免记录与实际目录类型不一致。
        let _ = fs::rename(&moved, staged);
        job.kind = SourceKind::Plugin;
        return Err(error);
    }
    log_output(directory.to_path_buf(), job.id.clone())(
        "仓库清单声明为平台适配器，已切换到 adapters/ 目录安装",
    );
    Ok(moved)
}

#[cfg(test)]
async fn recover_source(root: &FsPath, job: &StoreJob) -> Result<(), String> {
    let lock_root = root.to_path_buf();
    let build_lock =
        tokio::task::spawn_blocking(move || source_build::application_lock(&lock_root))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    recover_source_locked(root, job, &build_lock).await
}
async fn recover_source_locked(
    root: &FsPath,
    job: &StoreJob,
    build_lock: &source_build::ApplicationBuildLock,
) -> Result<(), String> {
    if let Some(artifact) = &job.artifact {
        source_build::rollback_candidate(&build_lock.target, artifact)
            .map_err(|e| e.to_string())?;
    }
    let target = root.join(job.kind.directory()).join(&job.plugin_id);
    if job.operation == "update" && job.source_switched != Some(false) {
        if let Some(previous) = &job.previous_commit {
            let target_recover = target.clone();
            let job_recover = job.clone();
            tokio::task::spawn_blocking(move || recover_update(&target_recover, &job_recover))
                .await
                .map_err(|_| "源码恢复检查失败".to_string())??;
            fs::write(target.join(".nuono-pin"), format!("{previous}\n"))
                .map_err(|e| e.to_string())?;
        }
    } else if let Some(staged_name) = &job.staged_name {
        let staged = root.join(job.kind.directory()).join(staged_name);
        if job.operation == "uninstall" && staged.exists() && !target.exists() {
            fs::rename(staged, target).map_err(|e| e.to_string())?;
        } else if job.operation == "install" && target.exists() && !staged.exists() {
            fs::rename(target, staged).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn health_attention(data: &FsPath) -> Result<Vec<(String, String, String)>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    let jobs = read_jobs(&data.join(".plugin-jobs"))?;
    let publication = source_root()
        .ok()
        .and_then(|root| source_build::target_dir(&root).ok())
        .and_then(|target| {
            source_build::read_publication(&target, env!("NUONO_BUILD_PROFILE"))
                .ok()
                .flatten()
        })
        .filter(|publication| publication_authorized(publication, &jobs));
    for job in jobs {
        if !seen.insert((job.kind, job.plugin_id.clone())) {
            continue;
        }
        if job.phase == "failed" {
            result.push((
                format!("plugin-job:{}", job.id),
                format!("{} 的插件操作失败", job.plugin_id),
                "原有程序继续运行，请打开插件商店查看任务日志，修复后重试。".into(),
            ));
        } else if job.phase == "awaiting_restart"
            && !artifact_is_applied(
                &job,
                &source_build::running_manifest(),
                publication.as_ref(),
            )
        {
            result.push((
                format!("plugin-job:{}", job.id),
                format!("{} 等待重启生效", job.plugin_id),
                "源码已部署完成，等待重启服务后生效；可继续拉取其他插件后统一重启。".into(),
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_tooling::recovery::{read_job, recover_before_build};
    fn job(operation: &str) -> StoreJob {
        StoreJob {
            kind: SourceKind::Plugin,
            id: uuid::Uuid::new_v4().to_string(),
            plugin_id: "demo".into(),
            operation: operation.into(),
            phase: "building".into(),
            message: String::new(),
            error: None,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            repo_url: None,
            previous_commit: None,
            target_commit: None,
            expected_source_commit: None,
            source_switched: Some(false),
            staged_name: Some(".nuono-removed-fixture".into()),
            runtime_name: None,
            previously_enabled: false,
            expected: None,
            artifact: None,
            owner_pid: std::process::id(),
            log: String::new(),
        }
    }
    #[test]
    fn same_id_in_another_source_kind_cannot_apply_or_satisfy_an_uninstall() {
        let mut job = job("install");
        job.kind = SourceKind::Adapter;
        let mut source = SourcePlugin {
            kind: SourceKind::Adapter,
            id: "demo".into(),
            source_hash: "selected".into(),
            ..Default::default()
        };
        job.expected = Some(source.clone());
        assert!(job_is_applied(&job, &[source.clone()]));
        source.kind = SourceKind::Plugin;
        assert!(!job_is_applied(&job, &[source.clone()]));
        job.operation = "uninstall".into();
        assert!(job_is_applied(&job, &[source]));
    }
    #[test]
    fn interrupted_adapter_install_recovers_only_the_adapter_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("adapters/demo")).unwrap();
        fs::create_dir_all(root.path().join("plugins/demo")).unwrap();
        fs::write(
            root.path().join("adapters/demo/Cargo.toml"),
            "broken candidate",
        )
        .unwrap();
        fs::write(
            root.path().join("plugins/demo/Cargo.toml"),
            "unrelated plugin",
        )
        .unwrap();
        let mut job = job("install");
        job.kind = SourceKind::Adapter;
        job.owner_pid = 0;
        persist(&root.path().join("data/.plugin-jobs"), &job).unwrap();
        recover_before_build(root.path()).unwrap();
        assert!(!root.path().join("adapters/demo").exists());
        assert!(
            root.path()
                .join("adapters/.nuono-removed-fixture/Cargo.toml")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(root.path().join("plugins/demo/Cargo.toml")).unwrap(),
            "unrelated plugin"
        );
    }

    const ADAPTER_MANIFEST: &str = "[package]\nname = \"demo-adapter\"\nversion = \"0.1.0\"\n\n[workspace]\n\n[package.metadata.nuono]\nkind = \"adapter\"\n";
    const PLUGIN_MANIFEST: &str =
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[workspace]\n";

    fn staged_install(root: &FsPath, job: &StoreJob, manifest: &str) -> PathBuf {
        let staged = root
            .join(SourceKind::Plugin.directory())
            .join(job.staged_name.as_ref().unwrap());
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("Cargo.toml"), manifest).unwrap();
        staged
    }

    #[test]
    fn link_install_adopts_adapter_manifest_and_moves_source() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("data/.plugin-jobs");
        let mut job = job("install");
        job.repo_url = Some("https://example.invalid/nuono-adapter-demo.git".into());
        job.staged_name = Some(".nuono-install-fixture".into());
        let staged = staged_install(root.path(), &job, ADAPTER_MANIFEST);

        let moved = adopt_declared_kind(root.path(), &jobs, &mut job, &staged).unwrap();

        assert_eq!(job.kind, SourceKind::Adapter);
        assert_eq!(moved, root.path().join("adapters/.nuono-install-fixture"));
        assert!(moved.join("Cargo.toml").is_file());
        assert!(!staged.exists());
        assert_eq!(read_job(&jobs, &job.id).unwrap().kind, SourceKind::Adapter);
        assert!(
            fs::read_to_string(jobs.join(format!("{}.log", job.id)))
                .unwrap()
                .contains("adapters/")
        );
    }

    #[test]
    fn link_install_keeps_plugin_and_store_installs_never_switch() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("data/.plugin-jobs");
        let mut link = job("install");
        link.repo_url = Some("https://example.invalid/demo.git".into());
        link.staged_name = Some(".nuono-install-plugin".into());
        let staged = staged_install(root.path(), &link, PLUGIN_MANIFEST);
        let kept = adopt_declared_kind(root.path(), &jobs, &mut link, &staged).unwrap();
        assert_eq!(kept, staged);
        assert_eq!(link.kind, SourceKind::Plugin);
        assert!(!jobs.join(format!("{}.json", link.id)).exists());

        // 商店安装没有仓库地址，类型以商店清单为准，不根据源码清单改类型。
        let mut store = job("install");
        store.staged_name = Some(".nuono-install-store".into());
        let staged = staged_install(root.path(), &store, ADAPTER_MANIFEST);
        let kept = adopt_declared_kind(root.path(), &jobs, &mut store, &staged).unwrap();
        assert_eq!(kept, staged);
        assert_eq!(store.kind, SourceKind::Plugin);
        assert!(!root.path().join("adapters/.nuono-install-store").exists());
    }

    #[test]
    fn link_install_id_respects_custom_or_derives_from_repo() {
        assert_eq!(
            resolve_link_install_id("https://github.com/xinxiuzhu/nuono-adapter-qq.git", None)
                .unwrap(),
            "qq"
        );
        assert_eq!(
            resolve_link_install_id(
                "https://github.com/xinxiuzhu/nuono-adapter-qq.git",
                Some("   "),
            )
            .unwrap(),
            "qq"
        );
        assert_eq!(
            resolve_link_install_id(
                "https://github.com/xinxiuzhu/nuono-adapter-qq.git",
                Some("qq-fork"),
            )
            .unwrap(),
            "qq-fork"
        );
        // 仓库名撞保留字时，用户可通过显式 ID 正常安装。
        assert_eq!(
            resolve_link_install_id("https://github.com/xinxiuzhu/hub", Some("my-hub")).unwrap(),
            "my-hub"
        );
        // 留空且无法从仓库名推导合法 ID 时报错并引导显式填写。
        assert!(
            resolve_link_install_id("https://github.com/xinxiuzhu/nuono-adapter-", None).is_err()
        );
        // 显式 ID 不允许包含非法字符、保留名或 adapter: 前缀。
        assert!(
            resolve_link_install_id(
                "https://github.com/xinxiuzhu/nuono-adapter-qq.git",
                Some("hub"),
            )
            .is_err()
        );
        assert!(
            resolve_link_install_id(
                "https://github.com/xinxiuzhu/nuono-adapter-qq.git",
                Some("adapter:qq"),
            )
            .is_err()
        );
    }

    #[test]
    fn link_install_store_policy_checks_adopted_adapter_kind() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("data/.plugin-jobs");
        let mut job = job("install");
        job.plugin_id = "qq".into();
        job.repo_url = Some("https://evil.example/nuono-adapter-qq.git".into());
        job.staged_name = Some(".nuono-install-adapter-policy".into());
        let staged = staged_install(root.path(), &job, ADAPTER_MANIFEST);

        adopt_declared_kind(root.path(), &jobs, &mut job, &staged).unwrap();
        assert_eq!(
            super::super::source_key(job.kind, &job.plugin_id),
            "adapter:qq"
        );

        let official_adapter = super::super::StorePlugin {
            kind: SourceKind::Adapter,
            id: "qq".into(),
            name: "QQ".into(),
            version: "0.1.0".into(),
            description: String::new(),
            author: String::new(),
            repo_url: "https://github.com/xinxiuzhu/nuono-adapter-qq.git".into(),
            pinned_commit: Some("a".repeat(40)),
            license: None,
            installed: false,
            installed_version: None,
            installed_commit: None,
            update_available: false,
            running_version: None,
            running_commit: None,
            pending_restart: false,
        };
        assert!(enforce_link_install_store_policy(&job, Some(&official_adapter)).is_err());

        job.repo_url = Some(official_adapter.repo_url.clone());
        assert_eq!(
            enforce_link_install_store_policy(&job, Some(&official_adapter)).unwrap(),
            official_adapter.pinned_commit
        );
        assert_eq!(enforce_link_install_store_policy(&job, None).unwrap(), None);
    }

    #[test]
    fn failed_kind_switch_persist_restores_source_and_kind() {
        let root = tempfile::tempdir().unwrap();
        let jobs = root.path().join("data/.plugin-jobs");
        fs::create_dir_all(jobs.parent().unwrap()).unwrap();
        fs::write(&jobs, "blocked").unwrap();
        let mut job = job("install");
        job.repo_url = Some("https://example.invalid/nuono-adapter-demo.git".into());
        job.staged_name = Some(".nuono-install-fixture".into());
        let staged = staged_install(root.path(), &job, ADAPTER_MANIFEST);

        assert!(adopt_declared_kind(root.path(), &jobs, &mut job, &staged).is_err());
        assert_eq!(job.kind, SourceKind::Plugin);
        assert!(staged.join("Cargo.toml").is_file());
        assert!(!root.path().join("adapters/.nuono-install-fixture").exists());
    }
    #[test]
    fn build_logs_hide_bare_sensitive_environment_values_and_credentials() {
        let result = sanitize_build_line(
            "build script printed bare-sensitive-value and https://user:pass@example.invalid/repo",
            &["bare-sensitive-value".into()],
        );
        assert!(!result.contains("bare-sensitive-value"));
        assert!(!result.contains("user:pass"));
    }
    #[test]
    fn job_survives_reload_and_log_is_bounded_and_redacted() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let job = job("install");
        persist(&root, &job).unwrap();
        assert_eq!(read_job(&root, &job.id).unwrap().phase, "building");
        let log = log_output(root.clone(), job.id.clone());
        log("token=secret-value password=hidden-value https://alice:secret@example.invalid/path");
        let text = fs::read_to_string(root.join(format!("{}.log", job.id))).unwrap();
        assert!(!text.contains("secret-value"));
        assert!(!text.contains("hidden-value"));
        assert!(!text.contains("alice:secret"));
        for _ in 0..100 {
            log(&"x".repeat(4000));
        }
        assert!(
            fs::metadata(root.join(format!("{}.log", job.id)))
                .unwrap()
                .len()
                <= MAX_LOG_BYTES as u64
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn interrupted_uninstall_restores_source_without_touching_data() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let job = job("uninstall");
        fs::create_dir_all(root.join("plugins/.nuono-removed-fixture")).unwrap();
        fs::create_dir_all(root.join("data/demo")).unwrap();
        fs::write(
            root.join("plugins/.nuono-removed-fixture/Cargo.toml"),
            "source",
        )
        .unwrap();
        fs::write(root.join("data/demo/config.toml"), "keep").unwrap();
        recover_source(&root, &job).await.unwrap();
        assert!(root.join("plugins/demo/Cargo.toml").is_file());
        assert_eq!(
            fs::read_to_string(root.join("data/demo/config.toml")).unwrap(),
            "keep"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pending_restart_requires_compiled_source_hash_not_version_number() {
        let mut job = job("update");
        let previous = SourcePlugin {
            id: "demo".into(),
            version: "1.0.0".into(),
            source_hash: "old".into(),
            ..Default::default()
        };
        let next = SourcePlugin {
            source_hash: "new".into(),
            ..previous.clone()
        };
        job.expected = Some(next.clone());
        assert!(!job_is_applied(&job, &[previous]));
        assert!(!job_is_applied(&job, &[]));
        assert!(job_is_applied(&job, std::slice::from_ref(&next)));
        job.operation = "uninstall".into();
        assert!(!job_is_applied(&job, &[next]));
        assert!(job_is_applied(&job, &[]));
    }

    fn artifact(sources: Vec<SourcePlugin>) -> source_build::BuildArtifact {
        source_build::BuildArtifact {
            id: uuid::Uuid::new_v4().to_string(),
            profile: "debug".into(),
            sha256: "a".repeat(64),
            sources,
        }
    }
    fn publication(job: &StoreJob) -> source_build::Publication {
        serde_json::from_value(serde_json::json!({
            "artifact": job.artifact,
            "job_id": job.id,
            "previous_sha256": null,
            "previous_receipt": false
        }))
        .unwrap()
    }

    #[test]
    fn one_verified_restart_applies_multiple_plugin_jobs_using_the_latest_complete_manifest() {
        let first = SourcePlugin {
            id: "demo".into(),
            source_hash: "first changed".into(),
            ..Default::default()
        };
        let second = SourcePlugin {
            id: "second".into(),
            source_hash: "second changed".into(),
            ..Default::default()
        };
        let mut earlier = job("update");
        earlier.expected = Some(first.clone());
        earlier.phase = "awaiting_restart".into();
        earlier.artifact = Some(artifact(vec![first.clone()]));
        let mut latest = job("update");
        latest.plugin_id = second.id.clone();
        latest.expected = Some(second.clone());
        latest.phase = "awaiting_restart".into();
        latest.artifact = Some(artifact(vec![first.clone(), second.clone()]));
        let published = publication(&latest);
        let running = vec![first, second];
        for selected in [&earlier, &latest] {
            assert!(artifact_is_applied_by_binary(
                selected,
                &running,
                Some(&published),
                Some(&published.artifact.sha256)
            ));
            assert!(!artifact_is_applied_by_binary(
                selected,
                &running,
                Some(&published),
                Some("previous binary")
            ));
            assert!(!artifact_is_applied_by_binary(
                selected,
                &running[..1],
                Some(&published),
                Some(&published.artifact.sha256)
            ));
        }
    }

    #[test]
    fn publication_requires_a_durable_matching_awaiting_job_and_persists_the_full_manifest() {
        let mut selected = job("uninstall");
        selected.artifact = Some(artifact(vec![]));
        let publication = publication(&selected);
        assert!(!publication_authorized(&publication, &[selected.clone()]));
        selected.phase = "awaiting_restart".into();
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        persist(&root, &selected).unwrap();
        let restored = read_job(&root, &selected.id).unwrap();
        assert!(publication_authorized(
            &publication,
            std::slice::from_ref(&restored)
        ));
        assert!(artifact_is_applied_by_binary(
            &restored,
            &[],
            Some(&publication),
            Some(&publication.artifact.sha256)
        ));
        let mut other = restored;
        other.artifact.as_mut().unwrap().sha256 = "b".repeat(64);
        assert!(!publication_authorized(&publication, &[other]));
        assert!(!publication_authorized(&publication, &[]));
        let mut deferred = publication;
        deferred.deferred_binary = true;
        assert!(!artifact_is_applied_by_binary(
            &selected,
            &[],
            Some(&deferred),
            Some(&deferred.artifact.sha256)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_version_is_applied_only_when_the_running_commit_matches() {
        let mut job = job("update");
        let running = SourcePlugin {
            source_hash: "same tree".into(),
            commit: Some("1111111111111111111111111111111111111111".into()),
            ..Default::default()
        };
        let selected = SourcePlugin {
            commit: Some("2222222222222222222222222222222222222222".into()),
            ..running.clone()
        };
        job.plugin_id = running.id.clone();
        job.expected = Some(selected.clone());
        assert!(!job_is_applied(&job, &[running]));
        assert!(job_is_applied(&job, &[selected]));
    }

    #[test]
    fn version_selection_survives_job_persistence_and_legacy_jobs_remain_readable() {
        let mut selected = job("update");
        selected.target_commit = Some("1111111111111111111111111111111111111111".into());
        selected.expected_source_commit = Some("2222222222222222222222222222222222222222".into());
        let mut value = serde_json::to_value(&selected).unwrap();
        let restored: StoreJob = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored.target_commit, selected.target_commit);
        assert_eq!(
            restored.expected_source_commit,
            selected.expected_source_commit
        );
        assert_eq!(restored.source_switched, Some(false));
        for key in ["target_commit", "expected_source_commit", "source_switched"] {
            value.as_object_mut().unwrap().remove(key);
        }
        let legacy: StoreJob = serde_json::from_value(value).unwrap();
        assert!(
            legacy.target_commit.is_none()
                && legacy.expected_source_commit.is_none()
                && legacy.source_switched.is_none()
        );
    }

    #[tokio::test]
    async fn failed_update_recovers_its_switch_but_preserves_external_edits() {
        let root =
            std::env::temp_dir().join(format!("nuono-git-job-test-{}", uuid::Uuid::new_v4()));
        let target = root.join("plugins/demo");
        fs::create_dir_all(&target).unwrap();
        let run = |args: &[&str]| {
            git::git_sync(
                &target,
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                std::time::Duration::from_secs(15),
            )
            .unwrap()
            .trim()
            .to_string()
        };
        run(&["init", "--initial-branch=main"]);
        fs::write(target.join("source.rs"), "old").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "old",
        ]);
        let previous = run(&["rev-parse", "HEAD"]);
        fs::write(target.join("source.rs"), "selected").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "selected",
        ]);
        let selected = run(&["rev-parse", "HEAD"]);
        let mut selected_job = job("update");
        selected_job.previous_commit = Some(previous.clone());
        selected_job.target_commit = Some(selected.clone());
        // 若任务在尚未执行 git checkout（`source_switched != Some(true)`）前就失败，恢复时不得重置用户外部手动切换过的 HEAD。
        recover_source(&root, &selected_job).await.unwrap();
        assert_eq!(run(&["rev-parse", "HEAD"]), selected);
        selected_job.source_switched = Some(true);
        fs::write(target.join("source.rs"), "external uncommitted edit").unwrap();
        assert!(recover_source(&root, &selected_job).await.is_err());
        assert_eq!(
            fs::read_to_string(target.join("source.rs")).unwrap(),
            "external uncommitted edit"
        );
        fs::write(target.join("source.rs"), "selected").unwrap();
        fs::write(target.join("other.rs"), "external commit").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "external",
        ]);
        let external = run(&["rev-parse", "HEAD"]);
        assert!(recover_source(&root, &selected_job).await.is_err());
        assert_eq!(run(&["rev-parse", "HEAD"]), external);
        run(&["checkout", "--detach", &selected]);
        recover_source(&root, &selected_job).await.unwrap();
        assert_eq!(run(&["rev-parse", "HEAD"]), previous);
        // 启动前的 `recover_before_build` 同样走这一套受保护的源码恢复路径。
        run(&["checkout", "--detach", &selected]);
        selected_job.owner_pid = 0;
        selected_job.phase = "building".into();
        persist(&root.join("data/.plugin-jobs"), &selected_job).unwrap();
        recover_before_build(&root).unwrap();
        assert_eq!(run(&["rev-parse", "HEAD"]), previous);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn startup_recovers_interrupted_install_before_build_scans_plugins() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(root.join("plugins/demo")).unwrap();
        fs::write(root.join("plugins/demo/Cargo.toml"), "broken new source").unwrap();
        let mut job = job("install");
        job.owner_pid = 0;
        persist(&root.join("data/.plugin-jobs"), &job).unwrap();
        recover_before_build(&root).unwrap();
        assert!(!root.join("plugins/demo").exists());
        assert!(
            root.join("plugins/.nuono-removed-fixture/Cargo.toml")
                .exists()
        );
        assert_eq!(
            read_job(&root.join("data/.plugin-jobs"), &job.id)
                .unwrap()
                .phase,
            "failed"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
