//! 不可变构建候选产物的冻结、校验，以及公开应用二进制文件的原子发布与故障回滚。
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
#[path = "../../tests/unit/source_build/artifacts_tests.rs"]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildArtifact {
    pub id: String,
    pub profile: String,
    pub sha256: String,
    pub sources: Vec<SourcePlugin>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub artifact: BuildArtifact,
    pub job_id: Option<String>,
    previous_sha256: Option<String>,
    previous_receipt: bool,
}

/// 单次构建流程内的已校验候选产物凭据，仅在当前进程内有效，绝不落盘或跨进程继承。
/// 后续执行发布时仍会再次核对候选二进制字节或现有公开程序的完整性。
pub struct ValidatedCandidate {
    target: PathBuf,
    artifact: BuildArtifact,
    reused_publication: Option<Publication>,
}
impl ValidatedCandidate {
    pub fn artifact(&self) -> &BuildArtifact {
        &self.artifact
    }
    pub fn into_artifact(self) -> BuildArtifact {
        self.artifact
    }
    pub fn is_reused(&self) -> bool {
        self.reused_publication.is_some()
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut reader = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn hash_reader_cancellable(
    reader: &mut impl Read,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<String>> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Ok(None);
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if cancelled() {
        return Ok(None);
    }
    Ok(Some(hex::encode(hash.finalize())))
}

fn hash_file_cancellable(path: &Path, cancelled: &impl Fn() -> bool) -> Result<Option<String>> {
    if cancelled() {
        return Ok(None);
    }
    hash_reader_cancellable(&mut fs::File::open(path)?, cancelled)
}
fn valid_profile(profile: &str) -> bool {
    !profile.is_empty()
        && profile.len() < 64
        && profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn artifact_directory(target: &Path, artifact: &BuildArtifact) -> Result<PathBuf> {
    uuid::Uuid::parse_str(&artifact.id).context("无效构建产物编号")?;
    ensure!(valid_profile(&artifact.profile), "无效构建 profile");
    ensure!(
        artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效构建产物校验和"
    );
    Ok(target.join("nuono-app/artifacts").join(&artifact.id))
}
pub fn public_binary(target: &Path, profile: &str) -> Result<PathBuf> {
    ensure!(valid_profile(profile), "无效构建 profile");
    Ok(target
        .join(profile)
        .join(format!("nuono-application{}", std::env::consts::EXE_SUFFIX)))
}
fn receipt_path(target: &Path, profile: &str) -> Result<PathBuf> {
    ensure!(valid_profile(profile), "无效构建 profile");
    Ok(target
        .join("nuono-app")
        .join(format!("published-{profile}.json")))
}
fn journal_path(target: &Path, profile: &str) -> Result<PathBuf> {
    ensure!(valid_profile(profile), "无效构建 profile");
    Ok(target
        .join("nuono-app")
        .join(format!("publishing-{profile}.json")))
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()?;
    }
    // Windows 没有目录 fsync 的等价语义。
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("缺少持久化目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "构建凭据不是普通文件"
    );
    ensure!(fs::metadata(path)?.len() <= 1024 * 1024, "构建凭据过大");
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
#[cfg(test)]
fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    copy_atomic_hashed(source, destination, None).map(|_| ())
}

/// 边复制边计算实际写入字节的 SHA-256 哈希；若复制期间源文件发生变化或与期望哈希不符，则直接中止并保留原目标文件不变。
pub fn copy_atomic_hashed(
    source: &Path,
    destination: &Path,
    expected: Option<&str>,
) -> Result<String> {
    let parent = destination.parent().context("缺少产物目录")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".application-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        ensure!(
            fs::symlink_metadata(source)?.file_type().is_file(),
            "待复制程序不是普通文件"
        );
        let mut reader = fs::File::open(source)?;
        let initial = reader.metadata()?;
        let modified = initial.modified()?;
        let permissions = initial.permissions();
        let mut writer = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            std::io::Write::write_all(&mut writer, &buffer[..count])?;
            hash.update(&buffer[..count]);
        }
        let final_metadata = reader.metadata()?;
        ensure!(
            initial.len() == final_metadata.len() && modified == final_metadata.modified()?,
            "程序在复制期间发生变化，未发布候选产物"
        );
        let sha256 = hex::encode(hash.finalize());
        ensure!(
            expected.is_none_or(|expected| expected == sha256.as_str()),
            "程序复制校验失败，未替换已发布程序"
        );
        fs::set_permissions(&temporary, permissions)?;
        writer.sync_all()?;
        fs::rename(&temporary, destination)?;
        sync_directory(parent)?;
        Ok(sha256)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn candidate_path(target: &Path, artifact: &BuildArtifact) -> Result<PathBuf> {
    Ok(artifact_directory(target, artifact)?.join("application"))
}
#[cfg(test)]
pub fn freeze_candidate(
    target: &Path,
    profile: &str,
    binary: &Path,
    sources: Vec<SourcePlugin>,
) -> Result<BuildArtifact> {
    freeze_candidate_validated(target, profile, binary, sources, false)
        .map(ValidatedCandidate::into_artifact)
}

/// 冻结并校验构建候选产物。插件安装/更新等后台任务必须传 `reuse = false`，以确保每个任务拥有独立的发布授权与回滚快照。
pub fn freeze_candidate_validated(
    target: &Path,
    profile: &str,
    binary: &Path,
    sources: Vec<SourcePlugin>,
    reuse: bool,
) -> Result<ValidatedCandidate> {
    ensure!(valid_profile(profile), "无效构建 profile");
    if reuse {
        ensure!(
            !journal_path(target, profile)?.exists(),
            "上次产物发布尚未恢复，请先重新启动源码启动器"
        );
        if let Some(publication) = read_publication(target, profile)?
            && same_sources(&sources, &publication.artifact.sources)
            && hash_file(binary)? == publication.artifact.sha256
        {
            // 公开程序可执行文件会在后续执行无操作复用发布时单独校验。
            verify_candidate(target, &publication.artifact)?;
            return Ok(ValidatedCandidate {
                target: target.into(),
                artifact: publication.artifact.clone(),
                reused_publication: Some(publication),
            });
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let directory = target.join("nuono-app/artifacts").join(&id);
    fs::create_dir_all(&directory)?;
    let sha256 = copy_atomic_hashed(binary, &directory.join("application"), None)?;
    let artifact = BuildArtifact {
        id,
        profile: profile.into(),
        sha256,
        sources,
    };
    durable_json(&directory.join("manifest.json"), &artifact)?;
    Ok(ValidatedCandidate {
        target: target.into(),
        artifact,
        reused_publication: None,
    })
}
fn verify_candidate_manifest(target: &Path, artifact: &BuildArtifact) -> Result<PathBuf> {
    let directory = artifact_directory(target, artifact)?;
    ensure!(
        !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
        "构建产物目录是符号链接"
    );
    let manifest: BuildArtifact = read_json(&directory.join("manifest.json"))?;
    ensure!(
        same_artifact(artifact, &manifest),
        "构建凭据与候选产物不一致"
    );
    let binary = candidate_path(target, artifact)?;
    ensure!(
        fs::symlink_metadata(&binary)?.file_type().is_file(),
        "候选产物不是普通文件"
    );
    Ok(binary)
}
fn verify_candidate(target: &Path, artifact: &BuildArtifact) -> Result<()> {
    let binary = verify_candidate_manifest(target, artifact)?;
    ensure!(
        hash_file(&binary)? == artifact.sha256,
        "候选构建产物校验失败"
    );
    Ok(())
}
pub fn same_sources(left: &[SourcePlugin], right: &[SourcePlugin]) -> bool {
    let key = |source: &SourcePlugin| {
        (
            source.kind,
            source.id.clone(),
            source.package.clone(),
            source.version.clone(),
            source.commit.clone(),
            source.source_hash.clone(),
        )
    };
    let mut left = left.iter().map(key).collect::<Vec<_>>();
    let mut right = right.iter().map(key).collect::<Vec<_>>();
    left.sort();
    right.sort();
    left == right
}
pub fn same_artifact(left: &BuildArtifact, right: &BuildArtifact) -> bool {
    left.id == right.id
        && left.profile == right.profile
        && left.sha256 == right.sha256
        && same_sources(&left.sources, &right.sources)
}
pub fn read_publication(target: &Path, profile: &str) -> Result<Option<Publication>> {
    let path = receipt_path(target, profile)?;
    if !path.exists() {
        return Ok(None);
    }
    let publication: Publication = read_json(&path)?;
    ensure!(
        publication.artifact.profile == profile,
        "发布凭据 profile 不一致"
    );
    artifact_directory(target, &publication.artifact)?;
    Ok(Some(publication))
}
fn remove_journal(target: &Path, profile: &str) -> Result<()> {
    let path = journal_path(target, profile)?;
    if path.exists() {
        fs::remove_file(&path)?;
        sync_directory(path.parent().unwrap())?;
    }
    Ok(())
}
fn complete_publication(target: &Path, publication: &Publication) -> Result<()> {
    verify_candidate(target, &publication.artifact)?;
    complete_verified_publication(target, publication)
}

/// 仅在当前操作已完成候选产物校验后调用；复制过程中仍会严格核对输入文件的 SHA-256。
fn complete_verified_publication(target: &Path, publication: &Publication) -> Result<()> {
    copy_atomic_hashed(
        &candidate_path(target, &publication.artifact)?,
        &public_binary(target, &publication.artifact.profile)?,
        Some(&publication.artifact.sha256),
    )?;
    durable_json(
        &receipt_path(target, &publication.artifact.profile)?,
        publication,
    )?;
    if let Err(error) = remove_journal(target, &publication.artifact.profile) {
        // 写入 published-*.json 回执即代表事务已提交成功；残留的 publishing-*.json 日志可在下次启动维护时安全清理。
        tracing::warn!(%error, "程序已提交，发布日志将在下次维护时恢复");
    }
    Ok(())
}
fn restore_publication(target: &Path, publication: &Publication) -> Result<()> {
    let artifact = &publication.artifact;
    let binary = public_binary(target, &artifact.profile)?;
    let actual = binary.is_file().then(|| hash_file(&binary)).transpose()?;
    ensure!(
        actual.as_deref() == Some(artifact.sha256.as_str())
            || actual == publication.previous_sha256,
        "公开程序已被其他操作修改，已保留该文件"
    );
    let directory = artifact_directory(target, artifact)?;
    if let Some(previous) = &publication.previous_sha256 {
        let backup = directory.join("previous-application");
        copy_atomic_hashed(&backup, &binary, Some(previous)).context("旧程序备份校验失败")?;
    } else if binary.exists() {
        fs::remove_file(&binary)?;
        sync_directory(binary.parent().unwrap())?;
    }
    let receipt = receipt_path(target, &artifact.profile)?;
    if publication.previous_receipt {
        let previous: Publication = read_json(&directory.join("previous-receipt.json"))?;
        durable_json(&receipt, &previous)?;
    } else if receipt.exists() {
        fs::remove_file(&receipt)?;
        sync_directory(receipt.parent().unwrap())?;
    }
    remove_journal(target, &artifact.profile)
}

/// 准备发布事务并备份旧程序（调用方须已持有应用构建锁），用于在任务标记为待重启前落盘回滚凭据。
pub fn prepare_publication(
    target: &Path,
    artifact: &BuildArtifact,
    job_id: Option<&str>,
) -> Result<Publication> {
    verify_candidate(target, artifact)?;
    prepare_verified_publication(target, artifact, job_id)
}

fn prepare_verified_publication(
    target: &Path,
    artifact: &BuildArtifact,
    job_id: Option<&str>,
) -> Result<Publication> {
    ensure!(
        !journal_path(target, &artifact.profile)?.exists(),
        "上次产物发布尚未恢复，请先重新启动源码启动器"
    );
    let directory = artifact_directory(target, artifact)?;
    let binary = public_binary(target, &artifact.profile)?;
    ensure!(
        read_publication(target, &artifact.profile)?
            .is_none_or(|publication| publication.artifact.id != artifact.id),
        "已发布产物不能重复准备发布事务，请创建独立候选产物"
    );
    let previous_sha256 = if binary.exists() {
        ensure!(
            fs::symlink_metadata(&binary)?.file_type().is_file(),
            "公开程序不是普通文件"
        );
        Some(copy_atomic_hashed(
            &binary,
            &directory.join("previous-application"),
            None,
        )?)
    } else {
        None
    };
    let previous_receipt = read_publication(target, &artifact.profile)?;
    if let Some(previous) = &previous_receipt {
        durable_json(&directory.join("previous-receipt.json"), previous)?;
    }
    let publication = Publication {
        artifact: artifact.clone(),
        job_id: job_id.map(str::to_string),
        previous_sha256,
        previous_receipt: previous_receipt.is_some(),
    };
    durable_json(&directory.join("publication.json"), &publication)?;
    durable_json(&journal_path(target, &artifact.profile)?, &publication)?;
    Ok(publication)
}
pub fn finish_publication(target: &Path, publication: &Publication) -> Result<PathBuf> {
    if let Err(error) = complete_publication(target, publication) {
        return match restore_publication(target, publication) {
            Ok(()) => Err(error.context("发布失败，已恢复原有程序")),
            Err(recovery) => Err(error.context(format!("发布失败，恢复程序失败: {recovery:#}"))),
        };
    }
    public_binary(target, &publication.artifact.profile)
}

/// 消费进程内的 `ValidatedCandidate` 凭据并发布产物。
/// 磁盘上的序列化回执无法直接走此快速路径；从任务持久化记录中恢复或提交时请使用上方完整的校验包装函数。
pub fn publish_validated_candidate(
    target: &Path,
    candidate: ValidatedCandidate,
    job_id: Option<&str>,
) -> Result<PathBuf> {
    ensure!(candidate.target == target, "候选产物属于其他构建目录");
    let artifact = candidate.artifact;
    verify_candidate_manifest(target, &artifact)?;
    if let Some(previous) = candidate.reused_publication {
        ensure!(job_id.is_none(), "插件任务必须使用独立候选产物");
        ensure!(
            !journal_path(target, &artifact.profile)?.exists(),
            "上次产物发布尚未恢复，请先重新启动源码启动器"
        );
        let publication =
            read_publication(target, &artifact.profile)?.context("已验证的发布凭据已被移除")?;
        ensure!(
            same_artifact(&publication.artifact, &artifact)
                && publication.job_id == previous.job_id
                && publication.previous_sha256 == previous.previous_sha256
                && publication.previous_receipt == previous.previous_receipt,
            "已验证的发布凭据已变化，未复用程序"
        );
        verify_public_binary(target, &artifact)?;
        return public_binary(target, &artifact.profile);
    }
    let publication = prepare_verified_publication(target, &artifact, job_id)?;
    finish_verified_publication(target, &publication)
}

/// 将已校验的候选二进制产物导出到指定路径，不修改本地运行的应用入口或发布回执（支持跨 target 导出）。
pub fn export_validated_candidate(
    target: &Path,
    candidate: ValidatedCandidate,
    output: &Path,
) -> Result<()> {
    ensure!(candidate.target == target, "候选产物属于其他构建目录");
    let binary = verify_candidate_manifest(target, &candidate.artifact)?;
    copy_atomic_hashed(&binary, output, Some(&candidate.artifact.sha256))?;
    Ok(())
}

fn finish_verified_publication(target: &Path, publication: &Publication) -> Result<PathBuf> {
    if let Err(error) = complete_verified_publication(target, publication) {
        return match restore_publication(target, publication) {
            Ok(()) => Err(error.context("发布失败，已恢复原有程序")),
            Err(recovery) => Err(error.context(format!("发布失败，恢复程序失败: {recovery:#}"))),
        };
    }
    public_binary(target, &publication.artifact.profile)
}

#[cfg(test)]
pub fn publish_candidate(
    target: &Path,
    artifact: &BuildArtifact,
    job_id: Option<&str>,
) -> Result<PathBuf> {
    verify_candidate(target, artifact)?;
    if job_id.is_none()
        && let Some(publication) = read_publication(target, &artifact.profile)?
        && publication.artifact.sha256 == artifact.sha256
        && same_sources(&publication.artifact.sources, &artifact.sources)
    {
        ensure!(
            !journal_path(target, &artifact.profile)?.exists(),
            "上次产物发布尚未恢复，请先重新启动源码启动器"
        );
        // 传入的 `artifact` 已在上方完成校验；若已发布的回执指向另一个产物 ID（即使哈希相同），也必须单独校验该已发布产物目录的完整性。
        if publication.artifact.id != artifact.id {
            verify_candidate(target, &publication.artifact)?;
        }
        verify_public_binary(target, &publication.artifact)?;
        return public_binary(target, &artifact.profile);
    }
    let publication = prepare_verified_publication(target, artifact, job_id)?;
    finish_verified_publication(target, &publication)
}

fn verify_public_binary(target: &Path, artifact: &BuildArtifact) -> Result<()> {
    let binary = public_binary(target, &artifact.profile)?;
    ensure!(
        fs::symlink_metadata(&binary)?.file_type().is_file(),
        "公开程序不是普通文件"
    );
    ensure!(
        hash_file(&binary)? == artifact.sha256,
        "公开程序与任务构建校验和不一致，已停止重启"
    );
    Ok(())
}

pub fn verify_publication(
    target: &Path,
    publication: &Publication,
    sources: &[SourcePlugin],
) -> Result<()> {
    verify_candidate(target, &publication.artifact)?;
    ensure!(
        same_sources(sources, &publication.artifact.sources),
        "全部插件源码与已发布构建不一致，请重新构建后再重启"
    );
    verify_public_binary(target, &publication.artifact)
}

/// 恢复未完成的发布事务日志：仅当对应的持久化任务记录仍处于已授权状态时才继续完成发布，否则回滚到上一版程序。
pub fn recover_publications(target: &Path, authorize: impl Fn(&Publication) -> bool) -> Result<()> {
    let directory = target.join("nuono-app");
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if !name.starts_with("publishing-") || !name.ends_with(".json") {
            continue;
        }
        let publication: Publication = read_json(&path)?;
        ensure!(
            path == journal_path(target, &publication.artifact.profile)?,
            "产物发布日志名称不一致"
        );
        let binary = public_binary(target, &publication.artifact.profile)?;
        let actual = binary.is_file().then(|| hash_file(&binary)).transpose()?;
        ensure!(
            actual.as_deref() == Some(publication.artifact.sha256.as_str())
                || actual == publication.previous_sha256,
            "公开程序已经变化，发布恢复不会覆盖它"
        );
        if authorize(&publication) {
            complete_publication(target, &publication)?;
        } else {
            restore_publication(target, &publication)?;
        }
    }
    Ok(())
}

pub fn rollback_candidate(target: &Path, artifact: &BuildArtifact) -> Result<()> {
    let journal = journal_path(target, &artifact.profile)?;
    if journal.exists() {
        let publication: Publication = read_json(&journal)?;
        if publication.artifact.id == artifact.id {
            return restore_publication(target, &publication);
        }
    }
    if let Some(publication) = read_publication(target, &artifact.profile)?
        && publication.artifact.id == artifact.id
    {
        return restore_publication(target, &publication);
    }
    Ok(())
}

fn referenced_publications(target: &Path) -> Result<Vec<Publication>> {
    let mut result = Vec::new();
    let directory = target.join("nuono-app");
    if !directory.is_dir() {
        return Ok(result);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if (name.starts_with("published-") || name.starts_with("publishing-"))
            && name.ends_with(".json")
        {
            let publication: Publication = read_json(&path)?;
            let previous =
                artifact_directory(target, &publication.artifact)?.join("previous-receipt.json");
            if publication.previous_receipt {
                result.push(read_json(&previous)?);
            }
            result.push(publication);
        }
    }
    Ok(result)
}
pub fn protected_job_ids(target: &Path) -> Result<Vec<String>> {
    let mut result = referenced_publications(target)?
        .into_iter()
        .filter_map(|publication| publication.job_id)
        .collect::<Vec<_>>();
    result.sort();
    result.dedup();
    Ok(result)
}
pub fn maintain_artifacts(root: &Path, target: &Path) -> Result<()> {
    maintain_artifacts_cancellable(root, target, &|| false).map(|_| ())
}

/// 清理未被引用的历史构建候选产物。若中途被取消则返回 `Ok(false)`，且不会更新维护时间戳缓存；
/// 仅当候选目录结构完整、哈希校验通过且未被任何当前发布或活跃任务引用时，才会被删除。
pub fn maintain_artifacts_cancellable(
    root: &Path,
    target: &Path,
    cancelled: &impl Fn() -> bool,
) -> Result<bool> {
    static LAST: LazyLock<std::sync::Mutex<Option<(String, Instant)>>> =
        LazyLock::new(|| std::sync::Mutex::new(None));
    if cancelled() {
        return Ok(false);
    }
    let mut retained = referenced_publications(target)?
        .into_iter()
        .map(|publication| publication.artifact.id)
        .collect::<std::collections::HashSet<_>>();
    let jobs = root.join("data/.plugin-jobs");
    if jobs.is_dir() {
        for entry in fs::read_dir(jobs)? {
            if cancelled() {
                return Ok(false);
            }
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let job: serde_json::Value = read_json(&path)?;
            if job["phase"].as_str().is_some_and(|phase| {
                matches!(
                    phase,
                    "queued" | "downloading" | "building" | "removing" | "awaiting_restart"
                )
            }) && let Some(id) = job["artifact"]["id"].as_str()
            {
                retained.insert(id.into());
            }
        }
    }
    let mut ids = retained.iter().cloned().collect::<Vec<_>>();
    ids.sort();
    let signature = format!("{root:?}:{target:?}:{ids:?}");
    let mut last = loop {
        if cancelled() {
            return Ok(false);
        }
        match LAST.try_lock() {
            Ok(last) => break last,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    if cancelled() {
        return Ok(false);
    }
    if last.as_ref().is_some_and(|(previous, time)| {
        *previous == signature && time.elapsed() < Duration::from_secs(600)
    }) {
        return Ok(true);
    }
    let directory = target.join("nuono-app/artifacts");
    if !directory.is_dir() {
        return Ok(true);
    }
    for entry in fs::read_dir(directory)? {
        if cancelled() {
            return Ok(false);
        }
        let entry = entry?;
        let path = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        if retained.contains(&id)
            || uuid::Uuid::parse_str(&id).is_err()
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let artifact: BuildArtifact = match read_json(&path.join("manifest.json")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if artifact.id != id {
            continue;
        }
        let binary = match verify_candidate_manifest(target, &artifact) {
            Ok(binary) => binary,
            Err(_) => continue,
        };
        match hash_file_cancellable(&binary, cancelled) {
            Ok(Some(hash)) if hash == artifact.sha256 => {}
            Ok(None) => return Ok(false),
            _ => continue,
        }
        let mut intact = true;
        for file in fs::read_dir(&path)? {
            if cancelled() {
                return Ok(false);
            }
            let file = file?;
            if !file.file_type()?.is_file()
                || !matches!(
                    file.file_name().to_str(),
                    Some(
                        "application"
                            | "manifest.json"
                            | "publication.json"
                            | "previous-application"
                            | "previous-receipt.json"
                    )
                )
            {
                intact = false;
                break;
            }
        }
        if path.join("previous-application").exists() {
            let publication: Publication = match read_json(&path.join("publication.json")) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if !same_artifact(&artifact, &publication.artifact) {
                intact = false;
            }
            match hash_file_cancellable(&path.join("previous-application"), cancelled) {
                Ok(Some(hash)) if Some(hash.as_str()) == publication.previous_sha256.as_deref() => {
                }
                Ok(None) => return Ok(false),
                _ => intact = false,
            }
            if publication.previous_receipt
                && read_json::<Publication>(&path.join("previous-receipt.json")).is_err()
            {
                intact = false;
            }
        }
        if cancelled() {
            return Ok(false);
        }
        if intact {
            fs::remove_dir_all(&path)?;
        }
    }
    if cancelled() {
        return Ok(false);
    }
    *last = Some((signature, Instant::now()));
    Ok(true)
}
