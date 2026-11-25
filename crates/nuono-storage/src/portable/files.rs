//! 与跨数据库可移植快照配套的私有资产文件打包、SHA-256 完整性校验与带崩溃恢复标记的导入暂存（单文件与总大小均设有上限保护）。

use super::{DatabaseImage, invalid};
use crate::error::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// 导入恢复未完成标记文件名：只要数据目录中存在该标记文件，宿主就会拒绝正常启动，直至数据库与全部资产文件校验通过后清除。
pub const PENDING: &str = ".portable-transfer-pending";
const MANIFEST: &str = "manifest.json";
const IMAGE: &str = "database.json";
const ASSETS: &str = "assets";
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_DATABASE_JSON: u64 = 160 * 1024 * 1024;

/// 单个资产文件条目（使用平台无关的 `/` 分隔相对路径，记录精确字节大小与 SHA-256 摘要）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// 跨数据库可移植迁移包的总清单（`manifest.json`）：必须在所有数据与资产文件写入落盘后作为最后一步发布，标志着逻辑迁移包完整生成。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub database_sha256: String,
    pub schema_fingerprint: String,
    pub assets: Vec<Asset>,
    pub external: Vec<super::external::ExternalAsset>,
}

/// 创建权限受限的私有迁移包目录，将 `data` 资产、外置资产与数据库镜像逐一复制并校验无误后，最后写入 `manifest.json`。
///
/// # Errors
/// 若目标目录已存在、包含符号链接或特殊文件、单文件/总体积超限，或复制期间源文件被并发修改，则报错拒绝。调用方在整个导出期间须持有部署维护锁。
pub fn write_package(
    path: &Path,
    data: &Path,
    sqlite_path: Option<&Path>,
    image: &DatabaseImage,
) -> Result<()> {
    if path.exists() {
        return Err(invalid("迁移包目录已存在，拒绝覆盖"));
    }
    let data = data.canonicalize().map_err(io)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize().map_err(io)?;
    if parent.starts_with(&data) {
        return Err(invalid("迁移包必须写在源数据目录之外"));
    }
    let excluded = excluded(sqlite_path, &data)?;
    let before = collect(&data, &excluded)?;
    private_dir(path)?;
    private_dir(&path.join(ASSETS))?;
    let mut assets = Vec::new();
    let mut total = 0;
    for relative in before {
        let source = data.join(&relative);
        let target = path.join(ASSETS).join(&relative);
        let parent = target.parent().ok_or_else(|| invalid("资产缺少父目录"))?;
        private_dir(parent)?;
        let size = fs::symlink_metadata(&source).map_err(io)?.len();
        total += size;
        if size > MAX_FILE || total > MAX_TOTAL {
            return Err(invalid("资产超过包大小限制"));
        }
        let original = hash_file(&source)?;
        let mut input = File::open(&source).map_err(io)?;
        let mut output = create_file(&target)?;
        std::io::copy(&mut input, &mut output).map_err(io)?;
        output.sync_all().map_err(io)?;
        if original != hash_file(&source)? || original != hash_file(&target)? {
            return Err(invalid("资产复制期间发生变化"));
        }
        assets.push(Asset {
            path: relative,
            size,
            sha256: original,
        });
    }
    let after = collect(&data, &excluded)?;
    if after != assets.iter().map(|a| a.path.clone()).collect::<Vec<_>>() {
        return Err(invalid("导出期间资产目录发生变化"));
    }
    let database = serde_json::to_vec(image).map_err(|e| invalid(e.to_string()))?;
    if database.len() as u64 > MAX_DATABASE_JSON {
        return Err(invalid("数据库逻辑包过大"));
    }
    write_new(&path.join(IMAGE), &database)?;
    let manifest = Manifest {
        format: 1,
        database_sha256: hex::encode(Sha256::digest(&database)),
        schema_fingerprint: image.schema_fingerprint.clone(),
        assets,
        external: super::external::write_package(&path.join("external"), image, &data)?,
    };
    check_combined_limits(&manifest)?;
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| invalid(e.to_string()))?;
    if manifest_bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid("资产清单超过 4 MiB 上限"));
    }
    write_new(&path.join(MANIFEST), &manifest_bytes)?;
    sync_directory(path)?;
    Ok(())
}

/// 离线读取并完整校验可移植迁移包的清单、数据库镜像与全部资产文件哈希（仅做静态解析与校验，绝不执行包内携带的任何 SQL）。
///
/// # Errors
/// 若存在文件缺失、哈希不匹配、体积超限、重复路径或清单外未登记的文件，则报错拒绝。
pub fn read_package(path: &Path) -> Result<(Manifest, DatabaseImage)> {
    let manifest: Manifest =
        super::parse_document(&read_bounded(&path.join(MANIFEST), 4 * 1024 * 1024)?)
            .map_err(|e| invalid(e.to_string()))?;
    if manifest.format != 1 {
        return Err(invalid("不支持的文件包格式"));
    }
    let database = read_bounded(&path.join(IMAGE), MAX_DATABASE_JSON)?;
    if hex::encode(Sha256::digest(&database)) != manifest.database_sha256 {
        return Err(invalid("数据库包摘要不匹配"));
    }
    let image: DatabaseImage =
        super::parse_document(&database).map_err(|e| invalid(e.to_string()))?;
    if image.schema_fingerprint != manifest.schema_fingerprint {
        return Err(invalid("文件包与数据库结构指纹不同"));
    }
    super::external::validate_declarations(&image, &manifest.external)?;
    verify_assets(&path.join(ASSETS), &manifest)?;
    super::external::verify_package(&path.join("external"), &manifest.external)?;
    check_combined_limits(&manifest)?;
    let expected: BTreeSet<_> = [MANIFEST, IMAGE, ASSETS, "external"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let actual = fs::read_dir(path)
        .map_err(io)?
        .map(|r| r.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<BTreeSet<_>>>()
        .map_err(io)?;
    if actual != expected {
        return Err(invalid("迁移包包含未登记的顶层文件"));
    }
    Ok((manifest, image))
}

/// 将已校验的资产文件暂存复制到新的目标部署 `data` 目录中，并在目标目录内保留崩溃恢复阻断标记（`.portable-transfer-pending`）。
///
/// # Errors
/// 若目标目录非空且包含已有业务文件，或恢复标记与当前迁移包不匹配，则报错拒绝。
/// 失败时保留阻断标记，防止普通宿主在半途失败的导入目录上直接启动。
pub fn stage_assets(
    package: &Path,
    data: &Path,
    manifest: &Manifest,
    sqlite_path: Option<&Path>,
) -> Result<()> {
    let excluded = excluded(sqlite_path, &data.canonicalize().map_err(io)?)?;
    let pending = data.join(PENDING);
    let resuming = pending.exists();
    if resuming && read_bounded(&pending, 128)? != manifest.database_sha256.as_bytes() {
        return Err(invalid("恢复标记属于不同迁移包"));
    }
    let present = collect(data, &excluded)?;
    if !resuming && !present.is_empty() {
        return Err(invalid("目标数据目录包含已有业务文件"));
    }
    if resuming
        && present
            .iter()
            .any(|p| !manifest.assets.iter().any(|a| a.path == *p))
    {
        return Err(invalid("暂存目标出现未登记业务文件"));
    }
    if !resuming {
        write_new(&pending, manifest.database_sha256.as_bytes())?;
    }
    sync_directory(data)?;
    for asset in &manifest.assets {
        validate_relative(&asset.path)?;
        let target = data.join(&asset.path);
        if target.exists() {
            if hash_file(&target)? == asset.sha256 {
                continue;
            }
            // 仅在首次确认目标目录为空之后才会创建 pending 恢复标记；断点续传时若发现上次半途写坏的残缺普通文件，可安全清理后重写。
            regular(&target)?;
            fs::remove_file(&target).map_err(io)?;
        }
        private_dir(target.parent().ok_or_else(|| invalid("缺少目标父目录"))?)?;
        let mut input = File::open(package.join(ASSETS).join(&asset.path)).map_err(io)?;
        let mut output = create_file(&target)?;
        std::io::copy(&mut input, &mut output).map_err(io)?;
        output.sync_all().map_err(io)?;
        if hash_file(&target)? != asset.sha256 {
            return Err(invalid("目标资产摘要不一致"));
        }
    }
    sync_directory(data)?;
    Ok(())
}

/// 在目标数据库导入并校验通过后，复核目标 `data` 目录下的全部资产文件并清除导入阻断标记（`.portable-transfer-pending`）。
///
/// # Errors
/// 若恢复标记与当前迁移包不符或任何资产文件发生变动则报错。仅应在配套的目标数据库也已通过完整校验后调用。
pub fn finish_assets(data: &Path, manifest: &Manifest, sqlite_path: Option<&Path>) -> Result<()> {
    verify_data(data, manifest, sqlite_path)?;
    let pending_path = data.join(PENDING);
    if pending_path.exists()
        && read_bounded(&pending_path, 128)? != manifest.database_sha256.as_bytes()
    {
        return Err(invalid("目标恢复标记属于另一份迁移包"));
    }
    if pending_path.exists() {
        fs::remove_file(pending_path).map_err(io)?;
    }
    sync_directory(data)?;
    Ok(())
}

/// 对照迁移包清单完整校验 `data` 目录下的所有业务资产文件列表与内容哈希（不清除任何恢复标记）。
///
/// # Errors
/// 若存在多余或缺失的文件路径，或文件字节哈希不一致，则返回错误。
pub fn verify_data(data: &Path, manifest: &Manifest, sqlite_path: Option<&Path>) -> Result<()> {
    let excluded = excluded(sqlite_path, &data.canonicalize().map_err(io)?)?;
    let actual = collect(data, &excluded)?;
    let expected: Vec<_> = manifest.assets.iter().map(|a| a.path.clone()).collect();
    if actual != expected {
        return Err(invalid("目标资产清单不一致"));
    }
    for asset in &manifest.assets {
        if hash_file(&data.join(&asset.path))? != asset.sha256 {
            return Err(invalid("目标资产内容不一致"));
        }
    }
    Ok(())
}

fn verify_assets(path: &Path, manifest: &Manifest) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut total = 0_u64;
    if manifest.assets.len() > MAX_FILES {
        return Err(invalid("资产文件数过多"));
    }
    for asset in &manifest.assets {
        validate_relative(&asset.path)?;
        if !names.insert(asset.path.clone()) {
            return Err(invalid("重复资产路径"));
        }
        total = total
            .checked_add(asset.size)
            .ok_or_else(|| invalid("资产大小溢出"))?;
        if asset.size > MAX_FILE || total > MAX_TOTAL {
            return Err(invalid("资产大小超限"));
        }
        let full = path.join(&asset.path);
        if regular(&full)?.len() != asset.size || hash_file(&full)? != asset.sha256 {
            return Err(invalid("资产内容或大小不一致"));
        }
    }
    if collect(path, &BTreeSet::new())? != names.into_iter().collect::<Vec<_>>() {
        return Err(invalid("资产包含未登记文件"));
    }
    Ok(())
}

fn check_combined_limits(manifest: &Manifest) -> Result<()> {
    let files = manifest
        .assets
        .iter()
        .chain(manifest.external.iter().flat_map(|root| root.files.iter()));
    let mut total = 0_u64;
    let mut count = 0_usize;
    for file in files {
        total = total
            .checked_add(file.size)
            .ok_or_else(|| invalid("资产大小溢出"))?;
        count += 1;
        if file.size > MAX_FILE || total > MAX_TOTAL || count > MAX_FILES {
            return Err(invalid("全部持久资产超过包限制"));
        }
    }
    Ok(())
}

fn excluded(sqlite_path: Option<&Path>, data: &Path) -> Result<BTreeSet<String>> {
    let mut result: BTreeSet<_> = [
        "backup",
        "plugin-jobs",
        "store-jobs",
        "runtime_tasks",
        ".nuono-service.lock",
        "nuono.lock",
        ".portable-external",
        ".portable-transfer-journal.json",
        super::TARGET_PLUGINS,
        PENDING,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(database) = sqlite_path {
        let full = database.canonicalize().map_err(io)?;
        if let Ok(relative) = full.strip_prefix(data) {
            let value = relative
                .to_str()
                .ok_or_else(|| invalid("数据库路径不是 UTF-8"))?;
            for suffix in ["", "-wal", "-shm", ".nuono-lock"] {
                result.insert(format!("{value}{suffix}"));
            }
        }
    }
    Ok(result)
}

pub(super) fn collect(root: &Path, excluded: &BTreeSet<String>) -> Result<Vec<String>> {
    if !fs::symlink_metadata(root).map_err(io)?.is_dir() {
        return Err(invalid("资产根不是实际目录"));
    }
    let mut queue = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = queue.pop() {
        for entry in fs::read_dir(dir).map_err(io)? {
            let path = entry.map_err(io)?.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| invalid(e.to_string()))?
                .to_str()
                .ok_or_else(|| invalid("资产路径不是 UTF-8"))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if excluded
                .iter()
                .any(|prefix| relative == *prefix || relative.starts_with(&format!("{prefix}/")))
            {
                continue;
            }
            validate_relative(&relative)?;
            let meta = fs::symlink_metadata(&path).map_err(io)?;
            if meta.is_dir() {
                queue.push(path);
            } else {
                regular(&path)?;
                files.push(relative);
            }
            if files.len() > MAX_FILES {
                return Err(invalid("资产文件数过多"));
            }
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn validate_relative(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 1024
        || value.contains(['\\', ':'])
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(invalid("非法相对资产路径"));
    }
    Ok(())
}

pub(super) fn regular(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(invalid("拒绝符号链接、目录和特殊文件"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(invalid("拒绝硬链接文件"));
        }
    }
    Ok(meta)
}

pub(super) fn hash_file(path: &Path) -> Result<String> {
    regular(path)?;
    let mut file = File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    let mut size = 0_u64;
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > MAX_FILE {
            return Err(invalid("文件超过大小上限"));
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

pub(super) fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    if regular(path)?.len() > maximum {
        return Err(invalid("文件超过读取上限"));
    }
    let mut result = Vec::new();
    File::open(path)
        .map_err(io)?
        .take(maximum + 1)
        .read_to_end(&mut result)
        .map_err(io)?;
    if result.len() as u64 > maximum {
        return Err(invalid("文件读取期间变大"));
    }
    Ok(result)
}

pub(super) fn create_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(io)
}

pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_file(path)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}

pub(super) fn private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        if !fs::symlink_metadata(path).map_err(io)?.is_dir() {
            return Err(invalid("目标父路径不是实际目录"));
        }
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io)
}

pub(super) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path).map_err(io)?.sync_all().map_err(io)
}
pub(super) fn io(error: std::io::Error) -> crate::error::NuonoError {
    invalid(error.to_string())
}

/// 当数据库 URL 指向本地磁盘上的 SQLite 数据库文件时，返回其解析后的文件路径（PostgreSQL 或内存 SQLite 返回 `Ok(None)`）。
pub fn sqlite_file(url: &str) -> Result<Option<PathBuf>> {
    if url.starts_with("sqlite:") {
        crate::db::sqlite_database_path(url)
    } else {
        Ok(None)
    }
}
