//! 磁盘持久化任务与构建产物的故障恢复模块。必须在扫描源码目录前执行，且调用方此时不得持有应用构建锁。
use crate::{
    git,
    source_build::{self, SourceKind, SourcePlugin},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
};
pub mod staging;
pub const HISTORY_LIMIT: usize = 20;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreJob {
    #[serde(default)]
    pub kind: SourceKind,
    pub id: String,
    pub plugin_id: String,
    pub operation: String,
    pub phase: String,
    pub message: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub repo_url: Option<String>,
    pub previous_commit: Option<String>,
    /// 显式指定的目标 Commit SHA；旧版任务记录中无此字段。
    #[serde(default)]
    pub target_commit: Option<String>,
    #[serde(default)]
    pub expected_source_commit: Option<String>,
    /// 标记源码目录是否已完成版本切换；旧版任务记录为 `None` 时保留原有恢复语义。
    #[serde(default)]
    pub source_switched: Option<bool>,
    pub staged_name: Option<String>,
    pub runtime_name: Option<String>,
    pub previously_enabled: bool,
    pub expected: Option<SourcePlugin>,
    /// 本次任务构建出的不可变候选应用产物信息（含完整的插件组合与二进制校验和）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<source_build::BuildArtifact>,
    #[serde(default)]
    pub owner_pid: u32,
    #[serde(default)]
    pub log: String,
}
impl StoreJob {
    pub fn active(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "queued" | "downloading" | "building" | "removing"
        )
    }
    pub fn terminal(&self) -> bool {
        matches!(self.phase.as_str(), "failed" | "applied" | "superseded")
    }
}

pub fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && !nuono_domain::deployment::is_reserved_plugin_name(id)
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
pub fn is_valid_commit(commit: &str) -> bool {
    commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn job_path(directory: &FsPath, id: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(id).map_err(|_| "任务编号无效".to_string())?;
    Ok(directory.join(format!("{id}.json")))
}

pub fn persist(directory: &FsPath, job: &StoreJob) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let path = job_path(directory, &job.id)?;
    let mut stored = job.clone();
    stored.log.clear();
    source_build::durable_json(&path, &stored).map_err(|e| e.to_string())
}

pub fn read_job(directory: &FsPath, id: &str) -> Result<StoreJob, String> {
    let path = job_path(directory, id)?;
    let bytes = fs::read(path).map_err(|_| "找不到该任务".to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("任务记录过大".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("任务记录损坏: {e}"))
}

pub fn read_jobs(directory: &FsPath) -> Result<Vec<StoreJob>, String> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut jobs = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().extension().is_some_and(|e| e == "json")
            && let Some(id) = entry.path().file_stem().and_then(|s| s.to_str())
        {
            jobs.push(read_job(directory, id)?);
        }
    }
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(jobs)
}

pub fn phase(
    directory: &FsPath,
    job: &mut StoreJob,
    phase: &str,
    message: &str,
) -> Result<(), String> {
    job.phase = phase.into();
    job.message = message.into();
    job.updated_at = chrono::Utc::now().to_rfc3339();
    persist(directory, job)
}

pub fn publication_authorized(publication: &source_build::Publication, jobs: &[StoreJob]) -> bool {
    publication.job_id.as_deref().is_none_or(|id| {
        jobs.iter().any(|job| {
            job.id == id
                && matches!(job.phase.as_str(), "awaiting_restart" | "applied")
                && job.artifact.as_ref().is_some_and(|artifact| {
                    source_build::same_artifact(artifact, &publication.artifact)
                })
        })
    })
}

/// 恢复中断的插件更新任务；若用户在任务中断后又手动提交了新版本或有本地未提交修改，则拒绝回退以免丢失用户代码。
pub fn recover_update(target: &FsPath, job: &StoreJob) -> Result<(), String> {
    let Some(previous) = &job.previous_commit else {
        return Ok(());
    };
    if !is_valid_commit(previous) {
        return Err("恢复版本记录无效，已停止操作".into());
    }
    git::independent_repository_sync(target)?;
    let run = |args: &[&str]| {
        git::git_sync(
            target,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            std::time::Duration::from_secs(15),
        )
    };
    let current = run(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    let target = job.target_commit.as_deref().or_else(|| {
        job.expected
            .as_ref()
            .and_then(|source| source.commit.as_deref())
    });
    if !current.trim().eq_ignore_ascii_case(previous)
        && target.is_none_or(|target| !current.trim().eq_ignore_ascii_case(target))
    {
        return Err("插件 HEAD 已被其他操作修改，已保留该版本；请在终端核对源码".into());
    }
    if run(&["status", "--porcelain", "--untracked-files=all"])?
        .lines()
        .any(|line| !line.is_empty() && line != "?? .nuono-pin")
    {
        return Err("源码存在本地修改，已保留修改；请在终端核对后恢复版本".into());
    }
    if !current.trim().eq_ignore_ascii_case(previous) {
        run(&["checkout", "--detach", previous])?;
    }
    Ok(())
}

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        pid == std::process::id()
    }
}

/// 在扫描源码目录前恢复意外中断的插件任务与发布事务。
/// 内部会主动获取应用构建锁；若检测到仍有活跃的 Cargo 构建则拒绝执行，并保护用户的本地源码修改不被覆盖。
pub fn recover_before_build(root: &FsPath) -> Result<(), String> {
    let directory = root.join("data/.plugin-jobs");
    let build_lock = source_build::application_lock(root).map_err(|e| e.to_string())?;
    let mut jobs = read_jobs(&directory)?;
    let authorization = jobs.clone();
    let interrupted: Vec<_> = jobs
        .iter_mut()
        .filter(|job| job.active() && !process_alive(job.owner_pid))
        .collect();
    // 意外强退的服务可能会遗留仍在后台运行的孤儿 Cargo 进程；在构建目录尚被占用时绝不能修改源码。
    let target = source_build::target_dir(root).map_err(|e| e.to_string())?;
    let _cargo_locks = crate::cargo_cache::lock_build_directories(&target)
        .map_err(|error| format!("启动恢复未执行：{error:#}"))?;
    source_build::recover_publications(&build_lock.target, |publication| {
        publication_authorized(publication, &authorization)
    })
    .map_err(|e| e.to_string())?;
    for job in interrupted {
        if let Some(artifact) = &job.artifact {
            source_build::rollback_candidate(&build_lock.target, artifact)
                .map_err(|e| e.to_string())?;
        }
        let source = root.join(job.kind.directory()).join(&job.plugin_id);
        if job.operation == "update" && job.source_switched != Some(false) {
            if let Some(commit) = &job.previous_commit {
                recover_update(&source, job)
                    .map_err(|e| format!("恢复 {} 的旧源码失败: {e}", job.plugin_id))?;
                fs::write(source.join(".nuono-pin"), format!("{commit}\n"))
                    .map_err(|e| e.to_string())?;
            }
        } else if let Some(staged_name) = &job.staged_name {
            let staged = root.join(job.kind.directory()).join(staged_name);
            if job.operation == "uninstall" && staged.exists() && !source.exists() {
                fs::rename(staged, source).map_err(|e| e.to_string())?;
            } else if job.operation == "install" && source.exists() && !staged.exists() {
                fs::rename(source, staged).map_err(|e| e.to_string())?;
            }
        }
        job.error = Some(
            "上次操作意外中断，启动前已恢复可用源码。卸载中的插件保持停用，可检查后重新启用。"
                .into(),
        );
        phase(
            &directory,
            job,
            "failed",
            "已恢复中断任务，请查看日志后重试",
        )?;
    }
    Ok(())
}
