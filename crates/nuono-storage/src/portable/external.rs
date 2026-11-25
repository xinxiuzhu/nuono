//! 跨数据库可移植迁移包中的外置文件资产（位于 `data` 目录之外的持久化文件）清单收集、隔离暂存与可断点续传发布。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use nuono_app::plugin_manifest::PersistentAsset;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    DatabaseImage,
    files::{self, Asset},
    invalid,
};
use crate::error::Result;

pub const STAGING: &str = ".portable-external";
pub const PENDING: &str = ".portable-external/pending.json";

/// 单个逻辑外置资产根目录及其在可移植迁移包中实际打包保留的文件清单。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAsset {
    pub identity: String,
    pub source_path: String,
    pub declared_path: String,
    pub restore: bool,
    pub required: bool,
    pub present: bool,
    pub directory: bool,
    pub files: Vec<Asset>,
}

/// 按迁移包中逻辑资产标识（`<owner>/<key>`）索引的显式恢复目标路径映射。
pub type ExternalTargets = BTreeMap<String, PathBuf>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    operation_id: String,
    package_hash: String,
    targets: ExternalTargets,
    assets: Vec<ExternalAsset>,
    published: BTreeSet<String>,
}

fn slot(identity: &str) -> String {
    hex::encode(Sha256::digest(identity.as_bytes()))
}

pub(super) fn write_package(
    package: &Path,
    image: &DatabaseImage,
    data: &Path,
) -> Result<Vec<ExternalAsset>> {
    files::private_dir(package)?;
    let mut declarations = Vec::new();
    if let Some(deployment) = &image.deployment {
        declarations.extend(deployment.assets.iter().map(|asset| ("nuono", asset)));
        for plugin in &deployment.plugins {
            declarations.extend(
                plugin
                    .manifest
                    .assets
                    .iter()
                    .map(|asset| (plugin.plugin_id.as_str(), asset)),
            );
        }
    }
    let mut seen = BTreeSet::new();
    let mut roots = Vec::new();
    for (owner, declaration) in declarations {
        let (key, path, required, restore) = match declaration {
            PersistentAsset::External {
                key,
                path,
                required,
            } => (key, path, required, true),
            PersistentAsset::Reference {
                key,
                path,
                required,
            } => (key, path, required, false),
            PersistentAsset::DataDirectory { .. } => continue,
        };
        let identity = format!("{owner}/{key}");
        if !seen.insert(identity.clone()) {
            return Err(invalid("外置资产标识重复"));
        }
        let actual = if restore {
            resolved_path(data, &identity, Path::new(path))?
        } else {
            PathBuf::from(path)
        };
        let source = actual.as_path();
        if !source.is_absolute() {
            return Err(invalid(format!("外置资产 {identity} 必须声明实际绝对路径")));
        }
        let mut asset = ExternalAsset {
            identity,
            source_path: source.to_string_lossy().into_owned(),
            declared_path: path.clone(),
            restore,
            required: *required,
            present: source.exists(),
            directory: false,
            files: Vec::new(),
        };
        if !asset.present {
            if *required {
                return Err(invalid(format!("必需外置资产缺失：{}", asset.identity)));
            }
            roots.push(asset);
            continue;
        }
        let meta = fs::symlink_metadata(source).map_err(files::io)?;
        if meta.file_type().is_symlink() {
            return Err(invalid("外置根不得是符号链接"));
        }
        asset.directory = meta.is_dir();
        let relative = if asset.directory {
            files::collect(source, &BTreeSet::new())?
        } else {
            files::regular(source)?;
            vec!["value".into()]
        };
        let destination = package.join(slot(&asset.identity));
        files::private_dir(&destination)?;
        for relative in relative {
            let path = if asset.directory {
                source.join(&relative)
            } else {
                source.into()
            };
            let output = destination.join(&relative);
            files::private_dir(
                output
                    .parent()
                    .ok_or_else(|| invalid("外置资产缺少父路径"))?,
            )?;
            let before = files::hash_file(&path)?;
            let size = files::regular(&path)?.len();
            let mut input = fs::File::open(&path).map_err(files::io)?;
            let mut output_file = files::create_file(&output)?;
            std::io::copy(&mut input, &mut output_file).map_err(files::io)?;
            output_file.sync_all().map_err(files::io)?;
            if before != files::hash_file(&path)? || before != files::hash_file(&output)? {
                return Err(invalid("外置资产复制时发生变化"));
            }
            asset.files.push(Asset {
                path: relative,
                size,
                sha256: before,
            });
        }
        if asset.directory
            && files::collect(source, &BTreeSet::new())?
                != asset
                    .files
                    .iter()
                    .map(|f| f.path.clone())
                    .collect::<Vec<_>>()
        {
            return Err(invalid("外置资产目录复制时发生变化"));
        }
        roots.push(asset);
    }
    roots.sort_by(|a, b| a.identity.cmp(&b.identity));
    files::sync_directory(package)?;
    Ok(roots)
}

pub(super) fn verify_package(package: &Path, assets: &[ExternalAsset]) -> Result<()> {
    let mut identities = BTreeSet::new();
    let mut expected = Vec::new();
    for asset in assets {
        if !identities.insert(&asset.identity) || asset.required && !asset.present {
            return Err(invalid("外置资产清单重复或缺失"));
        }
        if !asset.present && !asset.files.is_empty() {
            return Err(invalid("缺失外置资产不能有文件"));
        }
        let mut names = BTreeSet::new();
        for file in &asset.files {
            files::validate_relative(&file.path)?;
            if !names.insert(&file.path) || !asset.directory && file.path != "value" {
                return Err(invalid("外置文件清单无效"));
            }
            let relative = format!("{}/{}", slot(&asset.identity), file.path);
            let path = package.join(&relative);
            if files::regular(&path)?.len() != file.size || files::hash_file(&path)? != file.sha256
            {
                return Err(invalid("外置资产摘要不一致"));
            }
            expected.push(relative);
        }
    }
    expected.sort();
    if files::collect(package, &BTreeSet::new())? != expected {
        return Err(invalid("外置包包含未登记文件"));
    }
    Ok(())
}

/// 在暂存任何外置资产文件之前，预先校验调用方显式提供的所有外置目标路径映射。
///
/// # Errors
/// 若存在遗漏或多余的映射项、目标路径相互重叠或落入 `data` 目录内、祖先路径含符号链接，或目标路径下已有内容与迁移包冲突，则返回错误。
pub fn preflight(data: &Path, assets: &[ExternalAsset], targets: &ExternalTargets) -> Result<()> {
    let expected = assets
        .iter()
        .filter(|a| a.present && a.restore)
        .map(|a| a.identity.as_str())
        .collect::<BTreeSet<_>>();
    if targets.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(invalid(
            "必须为每个实际外置资产提供唯一目标路径；不得漏项或额外添加",
        ));
    }
    let data = data.canonicalize().map_err(files::io)?;
    let mut paths = Vec::<PathBuf>::new();
    for asset in assets.iter().filter(|a| a.present && a.restore) {
        let path = &targets[&asset.identity];
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(invalid("外置目标必须是无 .. 的绝对路径"));
        }
        for ancestor in path.ancestors().filter(|p| p.exists()) {
            if fs::symlink_metadata(ancestor)
                .map_err(files::io)?
                .file_type()
                .is_symlink()
            {
                return Err(invalid("外置目标祖先不得包含符号链接"));
            }
        }
        if path.starts_with(&data)
            || paths
                .iter()
                .any(|p| path.starts_with(p) || p.starts_with(path))
        {
            return Err(invalid("外置目标与 data 或另一资产根重叠"));
        }
        if path.exists() {
            verify_root(path, asset)?;
        }
        paths.push(path.clone());
    }
    Ok(())
}

/// 校验所有已发布的外置资产根目录与迁移包清单完全一致（或确认该迁移包无需恢复任何外置资产）。
///
/// # Errors
/// 若仍存在未完成的外置发布恢复日志（`pending.json`），或已发布的外置目标文件内容被篡改，则报错并阻断部署启动。
pub fn verify_published(data: &Path, hash: &str, assets: &[ExternalAsset]) -> Result<()> {
    if assets.iter().all(|a| !a.present || !a.restore) {
        return Ok(());
    }
    if data.join(PENDING).exists() {
        return Err(invalid("外置资产仍等待发布恢复"));
    }
    let journal: Journal = super::parse_document(&files::read_bounded(
        &data.join(STAGING).join("completed.json"),
        4 * 1024 * 1024,
    )?)?;
    if journal.package_hash != hash
        || serde_json::to_value(&journal.assets)? != serde_json::to_value(assets)?
    {
        return Err(invalid("外置发布回执与迁移包不符"));
    }
    for asset in assets.iter().filter(|a| a.present && a.restore) {
        verify_root(
            journal
                .targets
                .get(&asset.identity)
                .ok_or_else(|| invalid("外置映射不完整"))?,
            asset,
        )?;
    }
    Ok(())
}

/// 校验已完成导入的部署环境中的外置资产目标路径与文件内容（不会修改任何外置目标路径）。
///
/// # Errors
/// 若传入的恢复路径映射与完成回执记录不一致、回执缺失，或已发布的外置文件被修改，则返回错误。
pub fn verify_completed_targets(
    data: &Path,
    hash: &str,
    assets: &[ExternalAsset],
    targets: &ExternalTargets,
) -> Result<()> {
    preflight(data, assets, targets)?;
    verify_published(data, hash, assets)?;
    if assets.iter().all(|asset| !asset.present || !asset.restore) {
        return Ok(());
    }
    let journal: Journal = super::parse_document(&files::read_bounded(
        &data.join(STAGING).join("completed.json"),
        4 * 1024 * 1024,
    )?)?;
    if journal.targets != *targets {
        return Err(invalid("已完成导入的外置恢复路径不能改变"));
    }
    Ok(())
}

pub(super) fn validate_declarations(image: &DatabaseImage, assets: &[ExternalAsset]) -> Result<()> {
    let mut expected = BTreeMap::new();
    if let Some(deployment) = &image.deployment {
        for (owner, declarations) in std::iter::once(("nuono", &deployment.assets)).chain(
            deployment
                .plugins
                .iter()
                .map(|p| (p.plugin_id.as_str(), &p.manifest.assets)),
        ) {
            for declaration in declarations {
                let (key, path, required, restore) = match declaration {
                    PersistentAsset::External {
                        key,
                        path,
                        required,
                    } => (key, path, required, true),
                    PersistentAsset::Reference {
                        key,
                        path,
                        required,
                    } => (key, path, required, false),
                    PersistentAsset::DataDirectory { .. } => continue,
                };
                if expected
                    .insert(format!("{owner}/{key}"), (path, *required, restore))
                    .is_some()
                {
                    return Err(invalid("外置声明标识重复"));
                }
            }
        }
    }
    if expected.len() != assets.len() {
        return Err(invalid("迁移包没有包含完整外置声明"));
    }
    for asset in assets {
        let Some((path, required, restore)) = expected.get(&asset.identity) else {
            return Err(invalid("迁移包额外外置根未登记"));
        };
        if path.as_str() != asset.declared_path
            || *required != asset.required
            || *restore != asset.restore
        {
            return Err(invalid("外置根与持久声明不一致"));
        }
    }
    Ok(())
}

/// 将迁移包中的外置资产复制到目标 `data` 目录下的隔离暂存区（`.portable-external`），并写入持久化恢复日志。
///
/// # Errors
/// 若中途失败或中断，会保留阻断宿主启动的 `pending.json` 日志；本阶段绝不会直接覆盖任何外置目标路径。
pub fn stage(
    package: &Path,
    data: &Path,
    package_hash: &str,
    assets: &[ExternalAsset],
    targets: &ExternalTargets,
) -> Result<()> {
    preflight(data, assets, targets)?;
    if assets.iter().all(|a| !a.present || !a.restore) {
        return Ok(());
    }
    let root = data.join(STAGING);
    files::private_dir(&root)?;
    let pending = root.join("pending.json");
    if pending.exists() {
        let existing: Journal =
            super::parse_document(&files::read_bounded(&pending, 4 * 1024 * 1024)?)?;
        if existing.package_hash != package_hash
            || existing.targets != *targets
            || serde_json::to_value(&existing.assets)? != serde_json::to_value(assets)?
        {
            return Err(invalid("外置恢复参数与已登记操作不同"));
        }
    } else {
        let journal = Journal {
            operation_id: uuid::Uuid::new_v4().simple().to_string(),
            package_hash: package_hash.into(),
            targets: targets.clone(),
            assets: assets.into(),
            published: BTreeSet::new(),
        };
        files::write_new(&pending, &serde_json::to_vec(&journal)?)?;
    }
    files::sync_directory(&root)?;
    for asset in assets.iter().filter(|a| a.present && a.restore) {
        let stage = root.join(slot(&asset.identity));
        files::private_dir(&stage)?;
        for file in &asset.files {
            let target = stage.join(&file.path);
            if target.exists() {
                if files::hash_file(&target)? == file.sha256 {
                    continue;
                }
                files::regular(&target)?;
                fs::remove_file(&target).map_err(files::io)?;
            }
            files::private_dir(
                target
                    .parent()
                    .ok_or_else(|| invalid("暂存文件缺少父路径"))?,
            )?;
            let mut input = fs::File::open(package.join(slot(&asset.identity)).join(&file.path))
                .map_err(files::io)?;
            let mut output = files::create_file(&target)?;
            std::io::copy(&mut input, &mut output).map_err(files::io)?;
            output.sync_all().map_err(files::io)?;
            if files::hash_file(&target)? != file.sha256 {
                return Err(invalid("外置暂存内容不一致"));
            }
        }
        files::sync_directory(&stage)?;
    }
    files::sync_directory(&root)
}

/// 将暂存区中已校验的外置资产幂等发布到最终目标路径（若目标路径已存在且内容哈希完全一致则直接复用）。
///
/// # Errors
/// 若遇到任何路径或内容冲突，将保留恢复标记且不改动现有的外置目标文件。
pub fn publish(data: &Path, package_hash: &str) -> Result<()> {
    let path = data.join(PENDING);
    if !path.exists() {
        return Ok(());
    }
    let mut journal: Journal =
        super::parse_document(&files::read_bounded(&path, 4 * 1024 * 1024)?)?;
    if journal.package_hash != package_hash {
        return Err(invalid("外置恢复日志属于其他迁移包"));
    }
    for asset in &journal.assets {
        if !asset.present || !asset.restore {
            continue;
        }
        let target = &journal.targets[&asset.identity];
        if target.exists() {
            verify_root(target, asset)?;
        } else {
            let stage = data.join(STAGING).join(slot(&asset.identity));
            let staged_root = if asset.directory {
                stage
            } else {
                stage.join("value")
            };
            verify_root(&staged_root, asset)?;
            let parent = target
                .parent()
                .ok_or_else(|| invalid("外置目标缺少父目录"))?;
            files::private_dir(parent)?;
            // 始终保留暂存区内的原始副本，先在目标父目录下复制到临时路径并校验完整后，再原子重命名到最终目标位置。
            let temporary = parent.join(format!(
                ".nuono-transfer-{}-{}",
                journal.operation_id,
                slot(&asset.identity)
            ));
            if temporary.exists() && verify_root(&temporary, asset).is_err() {
                let metadata = fs::symlink_metadata(&temporary).map_err(files::io)?;
                if metadata.file_type().is_symlink() {
                    return Err(invalid("外置恢复暂存路径被替换"));
                }
                if metadata.is_dir() {
                    fs::remove_dir_all(&temporary).map_err(files::io)?;
                } else {
                    files::regular(&temporary)?;
                    fs::remove_file(&temporary).map_err(files::io)?;
                }
            }
            if !temporary.exists() {
                copy_root(&staged_root, &temporary, asset)?;
            }
            fs::rename(&temporary, target).map_err(files::io)?;
            files::sync_directory(parent)?;
        }
        journal.published.insert(asset.identity.clone());
        atomic_json(&path, &journal)?;
    }
    fs::rename(&path, data.join(STAGING).join("completed.json")).map_err(files::io)?;
    files::sync_directory(&data.join(STAGING))
}

fn copy_root(source: &Path, target: &Path, asset: &ExternalAsset) -> Result<()> {
    if asset.directory {
        files::private_dir(target)?;
    }
    for file in &asset.files {
        let input = if asset.directory {
            source.join(&file.path)
        } else {
            source.into()
        };
        let output = if asset.directory {
            target.join(&file.path)
        } else {
            target.into()
        };
        files::private_dir(output.parent().ok_or_else(|| invalid("外置父路径缺失"))?)?;
        let mut input = fs::File::open(input).map_err(files::io)?;
        let mut output = files::create_file(&output)?;
        std::io::copy(&mut input, &mut output).map_err(files::io)?;
        output.sync_all().map_err(files::io)?;
    }
    verify_root(target, asset)
}

fn verify_root(path: &Path, asset: &ExternalAsset) -> Result<()> {
    let names = if asset.directory {
        files::collect(path, &BTreeSet::new())?
    } else {
        files::regular(path)?;
        vec!["value".into()]
    };
    if names
        != asset
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>()
    {
        return Err(invalid("外置目标文件清单不匹配"));
    }
    for file in &asset.files {
        let path = if asset.directory {
            path.join(&file.path)
        } else {
            path.into()
        };
        if files::regular(&path)?.len() != file.size || files::hash_file(&path)? != file.sha256 {
            return Err(invalid("外置目标内容冲突"));
        }
    }
    Ok(())
}

pub(super) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let next = path.with_extension("next");
    if next.exists() {
        fs::remove_file(&next).map_err(files::io)?;
    }
    files::write_new(&next, &serde_json::to_vec(value)?)?;
    fs::rename(next, path).map_err(files::io)?;
    files::sync_directory(path.parent().ok_or_else(|| invalid("日志缺少父目录"))?)
}

/// 根据最近一次经过校验的外置资产发布回执，解析声明的外置资产在当前部署环境下实际映射的绝对路径。
///
/// # Errors
/// 若发布回执（`completed.json`）格式损坏或找不到对应映射，直接返回错误，绝不静默回退到未验证的路径。
pub fn resolved_path(data: &Path, identity: &str, declared: &Path) -> Result<PathBuf> {
    let receipt = data.join(STAGING).join("completed.json");
    if !receipt.exists() {
        return Ok(declared.into());
    }
    let journal: Journal = super::parse_document(&files::read_bounded(&receipt, 4 * 1024 * 1024)?)?;
    if journal
        .assets
        .iter()
        .any(|a| a.identity == identity && a.restore && Path::new(&a.declared_path) == declared)
    {
        return journal
            .targets
            .get(identity)
            .cloned()
            .ok_or_else(|| invalid("外置资产发布回执缺少路径"));
    }
    Ok(declared.into())
}
