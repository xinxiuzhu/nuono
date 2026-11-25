//! 离线跨数据库迁移后的部署目录切换（Cutover）：通过持久化阶段日志将已验证的暂存部署原子发布为正式环境，并支持在正式启用前回滚。

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{external, files, invalid};
use crate::error::Result;

/// 部署切换涉及的各目录与配置文件具体路径（数据库连接串等敏感信息仍保留在配置文件内，不写入切换日志）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutoverPlan {
    pub package: PathBuf,
    pub active_data: PathBuf,
    pub staged_data: PathBuf,
    pub active_config: PathBuf,
    pub prepared_config: PathBuf,
}

/// 部署切换阶段状态机：每一步目录或文件重命名均具备幂等可恢复性，即使进程在写入下一阶段状态前崩溃也能安全续跑。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    SourceParked,
    DataPublished,
    ConfigPublished,
    Complete,
    RolledBack,
}

/// 部署切换的持久化恢复回执（恢复或续跑时必须校验迁移包哈希与预备配置文件哈希未发生变化）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutoverJournal {
    pub plan: CutoverPlan,
    pub phase: Phase,
    pub package_hash: String,
    pub config_hash: String,
    pub source_config_hash: String,
    pub backup_data: PathBuf,
    pub backup_config: PathBuf,
    pub next_config: PathBuf,
}

/// 位于待重命名的 `data` 目录父级之外的固定启动阻断守卫文件路径（宿主在创建目录和启动前会先检查该守卫文件）。
pub fn startup_guard(data: &Path) -> PathBuf {
    let parent = data
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = data
        .file_name()
        .map(|v| v.to_string_lossy())
        .unwrap_or_default();
    parent.join(format!(
        ".nuono-transfer-guard-{}",
        hex::encode(Sha256::digest(name.as_bytes()))
    ))
}

/// 在移动源数据目录或替换配置文件之前，预先校验所有路径并写入持久化的切换操作日志与启动阻断标记。
///
/// # Errors
/// 若切换日志已存在、路径包含符号链接、或活动目录与暂存目录存在重叠嵌套，则拒绝执行。
pub fn prepare(path: &Path, mut plan: CutoverPlan, package_hash: &str) -> Result<CutoverJournal> {
    if path.exists() {
        return Err(invalid("切换日志已存在，请显式 recover 或 rollback"));
    }
    for path in [
        &plan.package,
        &plan.active_data,
        &plan.staged_data,
        &plan.active_config,
        &plan.prepared_config,
    ] {
        if !path.is_absolute()
            || fs::symlink_metadata(path)
                .map_err(files::io)?
                .file_type()
                .is_symlink()
        {
            return Err(invalid("切换位置必须是已存在的绝对路径且不能是符号链接"));
        }
    }
    plan.package = plan.package.canonicalize().map_err(files::io)?;
    plan.active_config = plan.active_config.canonicalize().map_err(files::io)?;
    plan.prepared_config = plan.prepared_config.canonicalize().map_err(files::io)?;
    let source = plan.active_data.canonicalize().map_err(files::io)?;
    let staged = plan.staged_data.canonicalize().map_err(files::io)?;
    plan.active_data = source.clone();
    plan.staged_data = staged.clone();
    if source.starts_with(&staged) || staged.starts_with(&source) {
        return Err(invalid("活动与暂存 data 不能相同或嵌套"));
    }
    for config in [&plan.active_config, &plan.prepared_config, path] {
        if config.starts_with(&source) || config.starts_with(&staged) {
            return Err(invalid("配置和切换日志必须在两份 data 目录之外"));
        }
    }
    let operation = uuid::Uuid::new_v4().simple().to_string();
    let parent = plan
        .active_config
        .parent()
        .ok_or_else(|| invalid("活动配置缺少父目录"))?;
    let journal = CutoverJournal {
        config_hash: files::hash_file(&plan.prepared_config)?,
        source_config_hash: files::hash_file(&plan.active_config)?,
        backup_data: source.with_file_name(format!(".nuono-source-{operation}")),
        backup_config: parent.join(format!(".nuono-config-source-{operation}")),
        next_config: parent.join(format!(".nuono-config-target-{operation}")),
        plan,
        phase: Phase::Prepared,
        package_hash: package_hash.into(),
    };
    // 在目标配置文件所在的同一父目录下创建配置副本，确保后续切换时可以通过同文件系统 rename 原子替换。
    files::write_new(
        &journal.next_config,
        &files::read_bounded(&journal.plan.prepared_config, 4 * 1024 * 1024)?,
    )?;
    external::atomic_json(path, &journal)?;
    ensure_guard(&journal.plan.active_data, package_hash)?;
    ensure_marker(&journal.plan.active_data, package_hash)?;
    ensure_marker(&journal.plan.staged_data, package_hash)?;
    Ok(journal)
}

/// 读取有大小上限保护的切换恢复日志（纯离线解析，不执行任何部署初始化或插件生命周期代码）。
///
/// # Errors
/// 若日志文件缺失、超限或 JSON 格式损坏则返回错误。
pub fn read(path: &Path) -> Result<CutoverJournal> {
    super::parse_document(&files::read_bounded(path, 4 * 1024 * 1024)?)
}

/// 推进文件系统层面的目录与配置文件切换（在最终数据库与资产复核通过前，始终保留启动阻断标记）。
///
/// # Errors
/// 若遇到路径冲突或配置文件被意外修改，将保留当前已持久化的阶段状态并返回错误，以便排查后恢复。
pub fn advance(path: &Path, journal: &mut CutoverJournal) -> Result<()> {
    if matches!(journal.phase, Phase::Complete | Phase::RolledBack) {
        return Err(invalid("切换操作已经结束"));
    }
    if files::hash_file(&journal.plan.prepared_config)? != journal.config_hash {
        return Err(invalid("待发布配置被修改"));
    }
    ensure_guard(&journal.plan.active_data, &journal.package_hash)?;
    if journal.phase == Phase::Prepared {
        ensure_marker(&journal.plan.staged_data, &journal.package_hash)?;
        if journal.backup_data.exists() {
            if journal.plan.active_data.exists() {
                return Err(invalid("源目录和备份同时存在，拒绝猜测归属"));
            }
            check_marker(&journal.backup_data, &journal.package_hash)?;
        } else {
            ensure_marker(&journal.plan.active_data, &journal.package_hash)?;
            fs::rename(&journal.plan.active_data, &journal.backup_data).map_err(files::io)?;
            sync_parent(&journal.plan.active_data)?;
        }
        journal.phase = Phase::SourceParked;
        external::atomic_json(path, journal)?;
    }
    if journal.phase == Phase::SourceParked {
        if journal.plan.staged_data.exists() {
            if journal.plan.active_data.exists() {
                return Err(invalid("活动目标路径已被占用"));
            }
            fs::rename(&journal.plan.staged_data, &journal.plan.active_data).map_err(files::io)?;
            sync_parent(&journal.plan.active_data)?;
            sync_parent(&journal.plan.staged_data)?;
        }
        check_marker(&journal.plan.active_data, &journal.package_hash)?;
        journal.phase = Phase::DataPublished;
        external::atomic_json(path, journal)?;
    }
    if journal.phase == Phase::DataPublished {
        if !journal.backup_config.exists() {
            if files::hash_file(&journal.plan.active_config)? != journal.source_config_hash {
                return Err(invalid("源配置被修改"));
            }
            fs::rename(&journal.plan.active_config, &journal.backup_config).map_err(files::io)?;
            sync_parent(&journal.plan.active_config)?;
        }
        if journal.next_config.exists() {
            if journal.plan.active_config.exists() {
                return Err(invalid("活动配置出现未登记内容"));
            }
            if files::hash_file(&journal.next_config)? != journal.config_hash {
                return Err(invalid("准备的配置副本被修改"));
            }
            fs::rename(&journal.next_config, &journal.plan.active_config).map_err(files::io)?;
            sync_parent(&journal.plan.active_config)?;
        }
        if files::hash_file(&journal.plan.active_config)? != journal.config_hash {
            return Err(invalid("发布后的配置不一致"));
        }
        journal.phase = Phase::ConfigPublished;
        external::atomic_json(path, journal)?;
    }
    Ok(())
}

/// 在调用方完成发布后的目标数据库与全部资产校验后，将切换标记为完成并解除宿主启动阻断。
///
/// # Errors
/// 若配置文件尚未发布完成，或仍有未完成的外置文件发布任务，则拒绝解除启动阻断。
pub fn complete(path: &Path, journal: &mut CutoverJournal) -> Result<()> {
    if journal.phase != Phase::ConfigPublished {
        return Err(invalid("切换尚未发布配置，不能解除启动阻断"));
    }
    check_marker(&journal.plan.active_data, &journal.package_hash)?;
    if journal.plan.active_data.join(external::PENDING).exists() {
        return Err(invalid("外置文件发布尚未完成"));
    }
    // 先将 Complete 状态持久化写入切换日志，从这一刻起严禁再回滚回旧源目录；随后清理启动阻断标记。
    journal.phase = Phase::Complete;
    external::atomic_json(path, journal)?;
    fs::remove_file(journal.plan.active_data.join(files::PENDING)).map_err(files::io)?;
    files::sync_directory(&journal.plan.active_data)?;
    clear_guard(&journal.plan.active_data, &journal.package_hash)
}

/// 仅用于恢复在持久化 `Phase::Complete` 回执之后、删除启动阻断标记之前意外中断的收尾清理。
///
/// # Errors
/// 调用方在调用本方法前必须先重新验证目标数据库与资产完整性；若阶段不是 `Complete` 或标记不匹配则返回错误。
pub fn finish_completed(journal: &CutoverJournal) -> Result<()> {
    if journal.phase != Phase::Complete {
        return Err(invalid("切换未完成"));
    }
    let marker = journal.plan.active_data.join(files::PENDING);
    if marker.exists() {
        check_marker(&journal.plan.active_data, &journal.package_hash)?;
        fs::remove_file(marker).map_err(files::io)?;
        files::sync_directory(&journal.plan.active_data)?;
    }
    clear_guard(&journal.plan.active_data, &journal.package_hash)?;
    Ok(())
}

/// 将尚未解除启动阻断的切换操作回退到原源目录与原配置文件（一旦切换已标记为 `Complete` 则严禁静默回退）。
///
/// # Errors
/// 若切换已完成，或源/目标目录及配置文件在切换期间被外部篡改，则拒绝回退。
pub fn rollback(path: &Path, journal: &mut CutoverJournal) -> Result<()> {
    if journal.phase == Phase::Complete {
        return Err(invalid("目标可能已接收写入，必须从当前权威库反向迁移"));
    }
    if journal.phase == Phase::RolledBack {
        return finish_rollback(journal);
    }
    if journal.backup_config.exists() {
        if journal.plan.active_config.exists() {
            if files::hash_file(&journal.plan.active_config)? != journal.config_hash {
                return Err(invalid("目标配置已改变，拒绝回退"));
            }
            if !journal.next_config.exists() {
                fs::rename(&journal.plan.active_config, &journal.next_config).map_err(files::io)?;
            } else {
                return Err(invalid("目标配置副本冲突"));
            }
        }
        if files::hash_file(&journal.backup_config)? != journal.source_config_hash {
            return Err(invalid("源配置备份已改变"));
        }
        fs::rename(&journal.backup_config, &journal.plan.active_config).map_err(files::io)?;
        sync_parent(&journal.plan.active_config)?;
    }
    if journal.backup_data.exists() {
        if journal.plan.active_data.exists() {
            check_marker(&journal.plan.active_data, &journal.package_hash)?;
            if journal.plan.staged_data.exists() {
                return Err(invalid("暂存目录已被占用"));
            }
            fs::rename(&journal.plan.active_data, &journal.plan.staged_data).map_err(files::io)?;
        }
        check_marker(&journal.backup_data, &journal.package_hash)?;
        fs::rename(&journal.backup_data, &journal.plan.active_data).map_err(files::io)?;
        sync_parent(&journal.plan.active_data)?;
    }
    journal.phase = Phase::RolledBack;
    external::atomic_json(path, journal)?;
    finish_rollback(journal)
}

fn finish_rollback(journal: &CutoverJournal) -> Result<()> {
    let marker = journal.plan.active_data.join(files::PENDING);
    if marker.exists() {
        check_marker(&journal.plan.active_data, &journal.package_hash)?;
        fs::remove_file(marker).map_err(files::io)?;
        files::sync_directory(&journal.plan.active_data)?;
    }
    clear_guard(&journal.plan.active_data, &journal.package_hash)?;
    Ok(())
}

fn check_marker(data: &Path, hash: &str) -> Result<()> {
    if files::read_bounded(&data.join(files::PENDING), 128)? != hash.as_bytes() {
        return Err(invalid("切换目录不属于当前恢复操作"));
    }
    Ok(())
}
fn ensure_marker(data: &Path, hash: &str) -> Result<()> {
    if data.join(files::PENDING).exists() {
        check_marker(data, hash)
    } else {
        files::write_new(&data.join(files::PENDING), hash.as_bytes())?;
        files::sync_directory(data)
    }
}
fn sync_parent(path: &Path) -> Result<()> {
    files::sync_directory(path.parent().ok_or_else(|| invalid("路径缺少父目录"))?)
}
fn ensure_guard(data: &Path, hash: &str) -> Result<()> {
    let guard = startup_guard(data);
    if guard.exists() {
        if files::read_bounded(&guard, 128)? != hash.as_bytes() {
            return Err(invalid("数据目录属于其他切换操作"));
        }
    } else {
        files::write_new(&guard, hash.as_bytes())?;
        sync_parent(&guard)?;
    }
    Ok(())
}
fn clear_guard(data: &Path, hash: &str) -> Result<()> {
    let guard = startup_guard(data);
    if guard.exists() {
        if files::read_bounded(&guard, 128)? != hash.as_bytes() {
            return Err(invalid("切换保护标记不匹配"));
        }
        fs::remove_file(&guard).map_err(files::io)?;
        sync_parent(&guard)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, CutoverPlan) {
        let root = std::env::temp_dir().join(format!("nuono-cutover-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let plan = CutoverPlan {
            package: root.join("package"),
            active_data: root.join("active"),
            staged_data: root.join("staged"),
            active_config: root.join("active.toml"),
            prepared_config: root.join("prepared.toml"),
        };
        for dir in [&plan.package, &plan.active_data, &plan.staged_data] {
            fs::create_dir(dir).unwrap();
        }
        fs::write(plan.active_data.join("value"), b"source").unwrap();
        fs::write(plan.staged_data.join("value"), b"target").unwrap();
        fs::write(&plan.active_config, b"source config").unwrap();
        fs::write(&plan.prepared_config, b"target config").unwrap();
        (root, plan)
    }
    #[test]
    fn resumes_every_rename_before_its_phase_receipt_and_blocks_post_publish_rollback() {
        for crash in 0..5 {
            let (root, plan) = fixture();
            let path = root.join("cutover.json");
            let mut journal = prepare(&path, plan, "hash").unwrap();
            if crash >= 1 {
                fs::rename(&journal.plan.active_data, &journal.backup_data).unwrap();
            }
            if crash >= 2 {
                journal.phase = Phase::SourceParked;
                external::atomic_json(&path, &journal).unwrap();
                fs::rename(&journal.plan.staged_data, &journal.plan.active_data).unwrap();
            }
            if crash >= 3 {
                journal.phase = Phase::DataPublished;
                external::atomic_json(&path, &journal).unwrap();
                fs::rename(&journal.plan.active_config, &journal.backup_config).unwrap();
            }
            if crash >= 4 {
                fs::rename(&journal.next_config, &journal.plan.active_config).unwrap();
            }
            let mut resumed = read(&path).unwrap();
            advance(&path, &mut resumed).unwrap();
            assert!(startup_guard(&resumed.plan.active_data).exists());
            assert!(resumed.plan.active_data.join(files::PENDING).exists());
            assert_eq!(
                fs::read(resumed.plan.active_data.join("value")).unwrap(),
                b"target"
            );
            assert_eq!(
                fs::read(resumed.backup_data.join("value")).unwrap(),
                b"source"
            );
            complete(&path, &mut resumed).unwrap();
            assert!(rollback(&path, &mut resumed).is_err());
            assert!(!startup_guard(&resumed.plan.active_data).exists());
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn rollback_receipt_survives_marker_cleanup_interruption() {
        let (root, plan) = fixture();
        let path = root.join("cutover.json");
        let mut journal = prepare(&path, plan, "hash").unwrap();
        advance(&path, &mut journal).unwrap();
        rollback(&path, &mut journal).unwrap();
        ensure_guard(&journal.plan.active_data, "hash").unwrap();
        ensure_marker(&journal.plan.active_data, "hash").unwrap();
        let mut recovered = read(&path).unwrap();
        rollback(&path, &mut recovered).unwrap();
        assert_eq!(
            fs::read(recovered.plan.active_data.join("value")).unwrap(),
            b"source"
        );
        assert_eq!(
            fs::read(&recovered.plan.active_config).unwrap(),
            b"source config"
        );
        assert!(!startup_guard(&recovered.plan.active_data).exists());
        assert!(!recovered.plan.active_data.join(files::PENDING).exists());
        assert!(recovered.plan.staged_data.join(files::PENDING).exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn completed_receipt_finishes_interrupted_guard_removal_without_reverting_source() {
        let (root, plan) = fixture();
        let path = root.join("cutover.json");
        let mut journal = prepare(&path, plan, "hash").unwrap();
        advance(&path, &mut journal).unwrap();
        journal.phase = Phase::Complete;
        external::atomic_json(&path, &journal).unwrap();
        assert!(journal.plan.active_data.join(files::PENDING).exists());
        finish_completed(&journal).unwrap();
        assert!(!startup_guard(&journal.plan.active_data).exists());
        assert!(rollback(&path, &mut journal).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
