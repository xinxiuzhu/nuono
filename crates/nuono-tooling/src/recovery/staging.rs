//! 暂存源码目录清理模块：仅清理内容指纹与初始快照完全一致的暂存目录，防止误删用户手动修改过的文件。
use super::{HISTORY_LIMIT, StoreJob, is_valid_plugin_id, job_path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path as FsPath, PathBuf},
};

const FAILED_INSTALL_LIMIT: usize = 3;
const RETENTION_DAYS: i64 = 7;
const MAX_SEAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SEAL_ENTRIES: usize = 50_000;

#[derive(Serialize, Deserialize)]
struct Seal {
    #[serde(default)]
    kind: crate::source_build::SourceKind,
    job_id: String,
    plugin_id: String,
    operation: String,
    staged_name: String,
    fingerprint: String,
}

fn managed_name(job: &StoreJob) -> Option<String> {
    uuid::Uuid::parse_str(&job.id).ok()?;
    if !is_valid_plugin_id(&job.plugin_id) {
        return None;
    }
    let prefix = match job.operation.as_str() {
        "install" => ".nuono-install-",
        "uninstall" => ".nuono-removed-",
        _ => return None,
    };
    let name = format!("{prefix}{}", job.id);
    (job.staged_name.as_deref() == Some(name.as_str())).then_some(name)
}

fn marker(directory: &FsPath, job: &StoreJob) -> Result<PathBuf, String> {
    Ok(job_path(directory, &job.id)?.with_extension("staging"))
}

fn plain_directory(path: &FsPath) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("暂存源码不是普通目录，已保留供人工检查".into());
    }
    Ok(())
}

fn fingerprint(root: &FsPath) -> Result<String, String> {
    fn visit(
        root: &FsPath,
        path: &FsPath,
        hasher: &mut Sha256,
        bytes: &mut u64,
        count: &mut usize,
    ) -> Result<(), String> {
        let mut entries = fs::read_dir(path)
            .map_err(|e| e.to_string())?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            // `.nuono-pin` 由宿主在 Git 检出后写入，不属于原始下载的代码树内容。
            if path == root && entry.file_name() == ".nuono-pin" {
                continue;
            }
            *count += 1;
            if *count > MAX_SEAL_ENTRIES {
                return Err("暂存源码文件过多，已保留供人工检查".into());
            }
            let file = entry.path();
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() || !(kind.is_dir() || kind.is_file()) {
                return Err("暂存源码含链接或特殊文件，已保留供人工检查".into());
            }
            let relative = file.strip_prefix(root).map_err(|e| e.to_string())?;
            let relative = relative.to_str().ok_or("暂存源码文件名无效")?;
            hasher.update((relative.len() as u64).to_le_bytes());
            hasher.update(relative.as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                hasher.update(
                    entry
                        .metadata()
                        .map_err(|e| e.to_string())?
                        .mode()
                        .to_le_bytes(),
                );
            }
            if kind.is_dir() {
                hasher.update(b"directory");
                visit(root, &file, hasher, bytes, count)?;
            } else {
                hasher.update(b"file");
                let mut reader = fs::File::open(&file).map_err(|e| e.to_string())?;
                let mut file_hash = Sha256::new();
                let mut buffer = [0; 16 * 1024];
                loop {
                    let size = reader.read(&mut buffer).map_err(|e| e.to_string())?;
                    if size == 0 {
                        break;
                    }
                    *bytes += size as u64;
                    if *bytes > MAX_SEAL_BYTES {
                        return Err("暂存源码过大，已保留供人工检查".into());
                    }
                    file_hash.update(&buffer[..size]);
                }
                hasher.update(file_hash.finalize());
            }
        }
        Ok(())
    }
    plain_directory(root)?;
    let mut hasher = Sha256::new();
    visit(root, root, &mut hasher, &mut 0, &mut 0)?;
    Ok(hex::encode(hasher.finalize()))
}

/// 在移入暂存区或卸载隐藏源码前先记录目录指纹；若指纹生成失败，后续清理时将始终保留源码目录。
pub fn seal(directory: &FsPath, job: &StoreJob, source: &FsPath) -> Result<(), String> {
    let staged_name = managed_name(job).ok_or("暂存目录名称与任务不匹配")?;
    plain_directory(source.parent().ok_or("暂存目录没有父目录")?)?;
    let seal = Seal {
        kind: job.kind,
        job_id: job.id.clone(),
        plugin_id: job.plugin_id.clone(),
        operation: job.operation.clone(),
        staged_name,
        fingerprint: fingerprint(source)?,
    };
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let path = marker(directory, job)?;
    let temporary = path.with_extension("staging-tmp");
    fs::write(
        &temporary,
        serde_json::to_vec(&seal).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}

fn discard(root: &FsPath, directory: &FsPath, job: &StoreJob) -> Result<bool, String> {
    let Some(name) = managed_name(job) else {
        return Ok(false);
    };
    let plugins = root.join(job.kind.directory());
    plain_directory(&plugins)?;
    let staged = plugins.join(&name);
    match fs::symlink_metadata(&staged) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    plain_directory(&staged)?;
    // Git 克隆失败时可能在尚未写入指纹文件前留下一个空目录，可直接安全移除。
    if fs::read_dir(&staged)
        .map_err(|e| e.to_string())?
        .next()
        .is_none()
    {
        fs::remove_dir(&staged).map_err(|e| e.to_string())?;
        return Ok(true);
    }
    let Ok(file) = fs::File::open(marker(directory, job)?) else {
        return Ok(false);
    };
    let mut contents = Vec::new();
    file.take(4097)
        .read_to_end(&mut contents)
        .map_err(|e| e.to_string())?;
    if contents.len() > 4096 {
        return Ok(false);
    }
    let Ok(seal) = serde_json::from_slice::<Seal>(&contents) else {
        return Ok(false);
    };
    if seal.kind != job.kind
        || seal.job_id != job.id
        || seal.plugin_id != job.plugin_id
        || seal.operation != job.operation
        || seal.staged_name != name
        || seal.fingerprint != fingerprint(&staged)?
    {
        return Ok(false);
    }
    fs::remove_dir_all(staged).map_err(|e| e.to_string())?;
    Ok(true)
}

/// 需在持有源码操作锁时调用。来历不明或被用户修改过的暂存目录会连同其任务记录一并保留。
pub fn maintain(
    root: Option<&FsPath>,
    directory: &FsPath,
    jobs: &[StoreJob],
    protected_ids: &[String],
) -> Result<(), String> {
    use std::{
        sync::{LazyLock, Mutex},
        time::{Duration, Instant},
    };
    // 前端界面每 2 秒轮询一次任务列表；当已结束任务的状态未发生变化时，最多每 10 分钟执行一次磁盘清理检查。
    static LAST: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
    let signature = format!(
        "{root:?}:{directory:?}:{protected_ids:?}:{:?}",
        jobs.iter()
            .map(|job| { (&job.id, &job.phase, &job.updated_at) })
            .collect::<Vec<_>>()
    );
    let mut last = LAST.lock().unwrap_or_else(|error| error.into_inner());
    if last.as_ref().is_some_and(|(previous, checked)| {
        *previous == signature && checked.elapsed() < Duration::from_secs(600)
    }) {
        return Ok(());
    }
    maintain_inner(root, directory, jobs, protected_ids)?;
    *last = Some((signature, Instant::now()));
    Ok(())
}

fn maintain_inner(
    root: Option<&FsPath>,
    directory: &FsPath,
    jobs: &[StoreJob],
    protected_ids: &[String],
) -> Result<(), String> {
    let mut failed_install_count = 0;
    let mut terminal_count = 0;
    for job in jobs {
        if !job.terminal() {
            continue;
        }
        terminal_count += 1;
        let failed_install = job.operation == "install" && job.phase == "failed";
        if failed_install {
            failed_install_count += 1;
        }
        let old = chrono::DateTime::parse_from_rfc3339(&job.updated_at).is_ok_and(|updated| {
            chrono::Utc::now().signed_duration_since(updated).num_days() >= RETENTION_DAYS
        });
        let can_discard = (job.operation == "uninstall" && job.phase == "applied")
            || (failed_install
                && (old
                    || failed_install_count > FAILED_INSTALL_LIMIT
                    || terminal_count > HISTORY_LIMIT));
        let has_stage = job.staged_name.is_some();
        let stage_gone = if !has_stage {
            true
        } else if let Some(root) = root {
            let path = job
                .staged_name
                .as_ref()
                .map(|name| root.join(job.kind.directory()).join(name));
            let absent = managed_name(job).is_some()
                && path.is_some_and(|path| {
                    fs::symlink_metadata(path)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                });
            absent || (can_discard && discard(root, directory, job).unwrap_or(false))
        } else {
            false
        };
        if stage_gone {
            let _ = fs::remove_file(marker(directory, job)?);
            let publication_unknown = root.is_none() && job.artifact.is_some();
            if terminal_count > HISTORY_LIMIT
                && !publication_unknown
                && !protected_ids.contains(&job.id)
            {
                fs::remove_file(job_path(directory, &job.id)?).map_err(|e| e.to_string())?;
                let _ = fs::remove_file(directory.join(format!("{}.log", job.id)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::persist;
    use super::*;
    use crate::source_build;

    fn maintain(
        root: Option<&FsPath>,
        directory: &FsPath,
        jobs: &[StoreJob],
    ) -> Result<(), String> {
        super::maintain(root, directory, jobs, &[])
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("nuono-staging-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("plugins")).unwrap();
            Self(root)
        }
        fn directory(&self) -> PathBuf {
            self.0.join("data/.plugin-jobs")
        }
        fn job(&self, operation: &str, phase: &str) -> StoreJob {
            let id = uuid::Uuid::new_v4().to_string();
            let prefix = if operation == "install" {
                "install"
            } else {
                "removed"
            };
            let job: StoreJob = serde_json::from_value(serde_json::json!({
                "id": id, "plugin_id": "demo", "operation": operation, "phase": phase,
                "message": "", "error": null, "created_at": "2026-01-01T00:00:00Z",
                "updated_at": chrono::Utc::now().to_rfc3339(), "repo_url": null,
                "previous_commit": null, "staged_name": format!(".nuono-{prefix}-{id}"),
                "runtime_name": null, "previously_enabled": false, "expected": null
            }))
            .unwrap();
            let staged = self.staged(&job);
            fs::create_dir_all(staged.join(".git")).unwrap();
            fs::write(staged.join(".git/HEAD"), "fixture commit").unwrap();
            fs::write(staged.join("Cargo.toml"), "fixture source").unwrap();
            seal(&self.directory(), &job, &staged).unwrap();
            persist(&self.directory(), &job).unwrap();
            job
        }
        fn staged(&self, job: &StoreJob) -> PathBuf {
            self.0
                .join("plugins")
                .join(job.staged_name.as_ref().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn applied_uninstall_cleans_only_unchanged_staged_source_and_keeps_data() {
        let fixture = Fixture::new();
        let job = fixture.job("uninstall", "applied");
        fs::create_dir_all(fixture.0.join("data/demo")).unwrap();
        fs::write(fixture.0.join("data/demo/config.toml"), "keep").unwrap();
        maintain(
            Some(&fixture.0),
            &fixture.directory(),
            std::slice::from_ref(&job),
        )
        .unwrap();
        assert!(!fixture.staged(&job).exists());
        assert_eq!(
            fs::read_to_string(fixture.0.join("data/demo/config.toml")).unwrap(),
            "keep"
        );
        assert!(job_path(&fixture.directory(), &job.id).unwrap().exists());
    }

    #[test]
    fn failures_keep_three_recent_copies_and_expire_old_copies() {
        let fixture = Fixture::new();
        let mut jobs: Vec<_> = (0..4).map(|_| fixture.job("install", "failed")).collect();
        maintain(Some(&fixture.0), &fixture.directory(), &jobs).unwrap();
        assert!(jobs[..3].iter().all(|job| fixture.staged(job).exists()));
        assert!(!fixture.staged(&jobs[3]).exists());
        jobs[0].updated_at = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
        maintain(Some(&fixture.0), &fixture.directory(), &jobs).unwrap();
        assert!(!fixture.staged(&jobs[0]).exists());
    }

    #[test]
    fn local_edits_git_changes_and_unknown_trees_survive_history_pruning() {
        let fixture = Fixture::new();
        let mut jobs: Vec<_> = (0..HISTORY_LIMIT)
            .map(|_| fixture.job("uninstall", "applied"))
            .collect();
        let edited = fixture.job("uninstall", "applied");
        fs::write(fixture.staged(&edited).join("local.txt"), "user edits").unwrap();
        let git_changed = fixture.job("uninstall", "applied");
        fs::write(
            fixture.staged(&git_changed).join(".git/HEAD"),
            "user commit",
        )
        .unwrap();
        let unknown = fixture.job("uninstall", "applied");
        fs::remove_file(marker(&fixture.directory(), &unknown).unwrap()).unwrap();
        let removable = fixture.job("uninstall", "applied");
        jobs.extend([
            edited.clone(),
            git_changed.clone(),
            unknown.clone(),
            removable.clone(),
        ]);
        maintain(Some(&fixture.0), &fixture.directory(), &jobs).unwrap();
        for job in [edited, git_changed, unknown] {
            assert!(fixture.staged(&job).exists());
            assert!(job_path(&fixture.directory(), &job.id).unwrap().exists());
        }
        assert!(!fixture.staged(&removable).exists());
        assert!(
            !job_path(&fixture.directory(), &removable.id)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn active_pending_and_mismatched_paths_are_never_removed() {
        let fixture = Fixture::new();
        let active = fixture.job("install", "building");
        let pending = fixture.job("uninstall", "awaiting_restart");
        let mut unrelated = fixture.job("uninstall", "applied");
        let original = fixture.staged(&unrelated);
        unrelated.staged_name = Some("../data/demo".into());
        maintain(
            Some(&fixture.0),
            &fixture.directory(),
            &[active.clone(), pending.clone(), unrelated],
        )
        .unwrap();
        assert!(fixture.staged(&active).exists());
        assert!(fixture.staged(&pending).exists());
        assert!(original.exists());
    }

    #[test]
    fn failed_uninstall_retains_recovery_source_even_when_history_is_full() {
        let fixture = Fixture::new();
        let mut jobs: Vec<_> = (0..HISTORY_LIMIT)
            .map(|_| fixture.job("uninstall", "applied"))
            .collect();
        let failed = fixture.job("uninstall", "failed");
        jobs.push(failed.clone());
        maintain(Some(&fixture.0), &fixture.directory(), &jobs).unwrap();
        assert!(fixture.staged(&failed).exists());
        assert!(job_path(&fixture.directory(), &failed.id).unwrap().exists());
    }

    #[test]
    fn current_publication_keeps_its_authorizing_job_beyond_history_limit() {
        let fixture = Fixture::new();
        let mut jobs: Vec<_> = (0..HISTORY_LIMIT)
            .map(|_| fixture.job("uninstall", "applied"))
            .collect();
        let published = fixture.job("uninstall", "applied");
        jobs.push(published.clone());
        super::maintain(
            Some(&fixture.0),
            &fixture.directory(),
            &jobs,
            std::slice::from_ref(&published.id),
        )
        .unwrap();
        assert!(!fixture.staged(&published).exists());
        assert!(
            job_path(&fixture.directory(), &published.id)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn exported_runtime_keeps_build_authorization_when_source_root_is_unavailable() {
        let fixture = Fixture::new();
        let mut jobs: Vec<_> = (0..HISTORY_LIMIT)
            .map(|_| fixture.job("uninstall", "applied"))
            .collect();
        let mut published = fixture.job("install", "applied");
        fs::remove_dir_all(fixture.staged(&published)).unwrap();
        published.operation = "update".into();
        published.staged_name = None;
        published.artifact = Some(source_build::BuildArtifact {
            id: uuid::Uuid::new_v4().to_string(),
            profile: "debug".into(),
            sha256: "0".repeat(64),
            sources: vec![],
        });
        persist(&fixture.directory(), &published).unwrap();
        jobs.push(published.clone());
        maintain(None, &fixture.directory(), &jobs).unwrap();
        assert!(
            job_path(&fixture.directory(), &published.id)
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn empty_failed_download_can_expire_without_a_seal() {
        let fixture = Fixture::new();
        let mut job = fixture.job("install", "failed");
        let staged = fixture.staged(&job);
        fs::remove_dir_all(&staged).unwrap();
        fs::create_dir(&staged).unwrap();
        fs::remove_file(marker(&fixture.directory(), &job).unwrap()).unwrap();
        job.updated_at = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
        maintain(Some(&fixture.0), &fixture.directory(), &[job]).unwrap();
        assert!(!staged.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_never_deletes_its_target() {
        let fixture = Fixture::new();
        let job = fixture.job("uninstall", "applied");
        let staged = fixture.staged(&job);
        let outside = fixture.0.join("keep");
        fs::rename(&staged, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &staged).unwrap();
        maintain(Some(&fixture.0), &fixture.directory(), &[job]).unwrap();
        assert!(outside.join("Cargo.toml").exists());
        assert!(
            fs::symlink_metadata(staged)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
