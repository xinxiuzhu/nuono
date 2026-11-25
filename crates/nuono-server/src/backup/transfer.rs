//! 整站可移植数据迁移包（含数据库快照与 `data/` 业务目录），在启动早期打开数据库或加载插件前执行恢复。
//! 由于文件系统与 SQL 数据库无法共享同一个原子事务，这里通过持久化恢复日志（Journal）保留原数据库与目录备份；
//! 若恢复中途崩溃或中断，下次启动时会自动回滚到恢复前状态。
use super::{BACKUP_DIR, RESTORE_MARKER};
use crate::operation_error::{OperationError as OpError, Result as OpResult};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

pub const MAX_UPLOAD: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;
const FORMAT: u32 = 1;
const PENDING: &str = "pending.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub schema_fingerprint: String,
    pub framework_fingerprint: String,
    pub framework: String,
    pub created_at: String,
    pub backend: String,
    pub plugin_fingerprint: String,
    pub sources: Vec<crate::source_build::SourcePlugin>,
    pub plugins: BTreeMap<String, String>,
    pub roots: Vec<String>,
    pub files: Vec<Entry>,
    pub notes: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preview {
    pub id: String,
    pub manifest: Manifest,
    pub total_bytes: u64,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferResult {
    pub id: String,
    pub status: String,
    pub message: String,
    pub completed_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    database_target: String,
    id: String,
    phase: String,
    roots: Vec<String>,
    had_roots: Vec<String>,
}

pub fn directory(data: &Path) -> PathBuf {
    data.join(BACKUP_DIR).join("transfers")
}
pub fn is_pending(data: &Path) -> bool {
    directory(data).join(PENDING).exists()
}
pub fn read_status(data: &Path) -> Result<Option<TransferResult>, String> {
    let p = directory(data).join("result.json");
    if !p.exists() {
        return Ok(None);
    }
    Ok(Some(read_json(&p)?))
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn ioerr(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn backend(url: &str) -> &'static str {
    if url.trim().starts_with("sqlite:") {
        "sqlite"
    } else {
        "postgres"
    }
}
fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':'])
        && value.len() <= 1024
        && !value.chars().any(char::is_control)
        && value.split('/').count() <= 32
        && value
            .split('/')
            .all(|x| !x.is_empty() && x != "." && x != "..")
        && Path::new(value)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
}
fn safe_root(value: &str) -> bool {
    safe_relative(value)
        && !value.contains('/')
        && !value.starts_with('.')
        && !matches!(
            value,
            "backup"
                | "restore_pending"
                | "nuono.lock"
                | "plugin-jobs"
                | "store-jobs"
                | "runtime_tasks"
        )
}
fn plain_dir(path: &Path) -> Result<(), String> {
    let created = fs::symlink_metadata(path).is_err();
    if created {
        fs::create_dir_all(path).map_err(ioerr)?;
    }
    if !fs::symlink_metadata(path).map_err(ioerr)?.is_dir() {
        return Err(format!("目录不是普通目录：{}", path.display()));
    }
    if created {
        super::restrict_permissions(path, 0o700);
    }
    Ok(())
}
fn requested_io(error: std::io::Error) -> OpError {
    if error.kind() == std::io::ErrorKind::NotFound {
        OpError::missing("迁移包不存在")
    } else {
        error.into()
    }
}
pub fn workspace(data: &Path, id: &str) -> OpResult<PathBuf> {
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err(OpError::invalid("迁移包编号不合法"));
    }
    plain_dir(data)?;
    plain_dir(&data.join(BACKUP_DIR))?;
    plain_dir(&directory(data))?;
    let path = directory(data).join(id);
    if path.exists() && !fs::symlink_metadata(&path).map_err(ioerr)?.is_dir() {
        return Err(OpError::invalid("迁移包目录不合法"));
    }
    Ok(path)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let meta = fs::symlink_metadata(path).map_err(ioerr)?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        return Err("迁移元数据不是有效普通文件或体积过大".into());
    }
    serde_json::from_reader(File::open(path).map_err(ioerr)?).map_err(ioerr)
}
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f = File::create(&tmp).map_err(ioerr)?;
    super::restrict_permissions(&tmp, 0o600);
    serde_json::to_writer(&mut f, value).map_err(ioerr)?;
    f.sync_all().map_err(ioerr)?;
    fs::rename(&tmp, path).map_err(ioerr)?;
    sync_dir(path.parent().unwrap());
    Ok(())
}
fn sync_dir(p: &Path) {
    if let Ok(f) = File::open(p) {
        let _ = f.sync_all();
    }
}
fn hash_file(path: &Path) -> Result<(u64, String), String> {
    if !fs::symlink_metadata(path).map_err(ioerr)?.is_file() {
        return Err(format!("拒绝链接或特殊文件：{}", path.display()));
    }
    let mut f = File::open(path).map_err(ioerr)?;
    let mut h = Sha256::new();
    let mut size = 0u64;
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b).map_err(ioerr)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > MAX_UPLOAD {
            return Err("单个文件超过 512 MiB".into());
        }
        h.update(&b[..n]);
    }
    Ok((size, hex::encode(h.finalize())))
}
fn collect(root: &Path, path: &Path, files: &mut Vec<Entry>) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(ioerr)?;
    if m.is_dir() {
        for e in fs::read_dir(path).map_err(ioerr)? {
            collect(root, &e.map_err(ioerr)?.path(), files)?;
        }
    } else if m.is_file() {
        if files.len() >= MAX_FILES {
            return Err("文件数超过 20000".into());
        }
        let rel = path
            .strip_prefix(root)
            .map_err(ioerr)?
            .to_str()
            .ok_or("文件名不是 UTF-8")?
            .replace('\\', "/");
        if !safe_relative(&rel) {
            return Err(format!("不支持的文件路径：{rel}"));
        }
        let (size, sha256) = hash_file(path)?;
        files.push(Entry {
            path: rel,
            size,
            sha256,
        });
    } else {
        return Err(format!("迁移包拒绝符号链接和特殊文件：{}", path.display()));
    }
    Ok(())
}
fn sqlite_relative(url: &str, data: &Path) -> Result<Option<PathBuf>, String> {
    if backend(url) != "sqlite" {
        return Ok(None);
    }
    let p = nuono_core::db::sqlite_database_path(url)
        .map_err(ioerr)?
        .ok_or("内存数据库不能迁移")?;
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map_err(ioerr)?.join(p)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    Ok(abs
        .strip_prefix(data.canonicalize().map_err(ioerr)?)
        .ok()
        .map(Path::to_path_buf))
}
fn protect_database_roots(url: &str, data: &Path, roots: &[String]) -> Result<(), String> {
    if let Some(rel) = sqlite_relative(url, data)? {
        let first = rel
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy();
        if roots.iter().any(|r| {
            r == first.as_ref()
                || r == &format!("{first}-wal")
                || r == &format!("{first}-shm")
                || r == &format!("{first}.nuono-lock")
        }) {
            return Err(format!(
                "恢复目录 {first} 包含当前 SQLite 数据库，请先把数据库放在独立文件位置"
            ));
        }
    }
    Ok(())
}
fn target_fingerprint(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}
fn protected_machine_paths(config_path: &Path) -> Vec<PathBuf> {
    config_path.canonicalize().into_iter().collect()
}
pub fn machine_file_conflicts(
    data: &Path,
    roots: &[String],
    config_path: &Path,
) -> Result<(), String> {
    let base = data.canonicalize().map_err(ioerr)?;
    for file in protected_machine_paths(config_path) {
        if let Ok(relative) = file.strip_prefix(&base)
            && let Some(root) = relative.components().next()
        {
            let root = root.as_os_str().to_string_lossy();
            if roots.iter().any(|item| item == root.as_ref()) {
                return Err(format!(
                    "恢复目录 {root} 包含本机启动配置文件。请先将机器配置移出业务数据目录后重试。"
                ));
            }
        }
    }
    Ok(())
}

/// 在线打包生成数据迁移归档。数据库部分为一致性快照，业务文件在复制前后均会校验哈希；
/// 但在线打包不保证数据库与文件系统之间严格处于同一时间点。
pub async fn create(
    data: &Path,
    url: &str,
    config_path: &Path,
    plugins: BTreeMap<String, String>,
) -> OpResult<Preview> {
    if is_pending(data) || data.join(RESTORE_MARKER).exists() {
        return Err(OpError::conflict("等待恢复期间不能创建迁移包"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let work = workspace(data, &id)?;
    plain_dir(&work)?;
    let stage = work.join("stage");
    plain_dir(&stage)?;
    plain_dir(&stage.join("data"))?;
    if let Err(error) = super::dump_database(url, &stage.join("database.dump")).await {
        let _ = fs::remove_dir_all(&work);
        return Err(error.into());
    }
    let cleanup = work.clone();
    let data = data.to_path_buf();
    let url = url.to_string();
    let config_path = config_path.to_path_buf();
    let task_id = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        build_archive(&data, &url, &config_path, &task_id, &work, &stage, plugins)
    })
    .await
    .map_err(ioerr)?;

    if result.is_err() {
        let _ = fs::remove_dir_all(cleanup);
    }
    result.map_err(Into::into)
}
fn build_archive(
    data: &Path,
    url: &str,
    config_path: &Path,
    task_id: &str,
    work: &Path,
    stage: &Path,
    plugins: BTreeMap<String, String>,
) -> Result<Preview, String> {
    if !fs::symlink_metadata(config_path).map_err(ioerr)?.is_file() {
        return Err("启动配置必须是普通文件".into());
    }
    let protected = protected_machine_paths(config_path);
    fs::copy(config_path, stage.join("startup-reference.toml")).map_err(ioerr)?;
    let db_rel = sqlite_relative(url, data)?;
    let mut roots = Vec::new();
    let mut budget = (
        2usize,
        fs::metadata(stage.join("database.dump"))
            .map_err(ioerr)?
            .len()
            + fs::metadata(stage.join("startup-reference.toml"))
                .map_err(ioerr)?
                .len(),
    );
    for item in fs::read_dir(data).map_err(ioerr)? {
        let p = item.map_err(ioerr)?.path();
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("文件名不是 UTF-8")?
            .to_string();
        if matches!(
            name.as_str(),
            BACKUP_DIR
                | RESTORE_MARKER
                | "nuono.lock"
                | "plugin-jobs"
                | "store-jobs"
                | "runtime_tasks"
        ) || name.starts_with('.')
        {
            continue;
        }
        if let Some(db) = &db_rel {
            let db_name = db.to_string_lossy();
            if name == db_name
                || name == format!("{db_name}-wal")
                || name == format!("{db_name}-shm")
                || name == format!("{db_name}.nuono-lock")
            {
                continue;
            }
            if db.components().count() > 1 && db.starts_with(&name) {
                return Err(format!(
                    "{name} 同时存放数据库和素材，请先把 SQLite 数据库改到独立文件位置"
                ));
            }
        }
        let canonical = p.canonicalize().map_err(ioerr)?;
        if protected.iter().any(|file| file == &canonical) {
            continue;
        }
        if protected.iter().any(|file| file.starts_with(&canonical)) {
            return Err(format!(
                "{name} 包含启动配置文件，请将机器配置移出业务数据目录后重试"
            ));
        }
        if !safe_root(&name) {
            return Err(format!("不支持的数据目录名：{name}"));
        }
        copy_tree(&p, &stage.join("data").join(&name), &mut budget)?;
        roots.push(name);
    }
    roots.sort();
    let mut files = Vec::new();
    collect(stage, stage, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files.iter().map(|f| f.size).sum::<u64>() > MAX_EXPANDED {
        return Err("未压缩数据超过 2 GiB".into());
    }
    let manifest=Manifest{format:FORMAT,schema_fingerprint:env!("NUONO_STORAGE_FINGERPRINT").into(),framework_fingerprint:env!("NUONO_FRAMEWORK_FINGERPRINT").into(),framework:env!("CARGO_PKG_VERSION").into(),created_at:now(),backend:backend(url).into(),plugin_fingerprint:crate::source_build::running_fingerprint(),sources:crate::source_build::running_manifest(),plugins,roots,files,notes:vec!["数据库为一致性快照；文件在复制时逐项校验，运行中的数据库与文件不构成同一时点快照。关键业务建议停止外部操作后备份。".into(),"含数据库密钥和启动配置原文。启动配置仅供参考，恢复不覆盖本机监听地址或数据库连接。".into(),"不包含程序、插件源码、构建缓存、日志、历史备份及 data 目录之外的业务文件；目标机器需准备相同版本的框架和插件。".into()]};
    write_json(&work.join("manifest.json"), &manifest)?;
    let archive = File::create(work.join("archive.nuono.tar.gz")).map_err(ioerr)?;
    let encoder = GzEncoder::new(archive, Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    tar.append_path_with_name(work.join("manifest.json"), "manifest.json")
        .map_err(ioerr)?;
    for f in &manifest.files {
        tar.append_path_with_name(stage.join(&f.path), &f.path)
            .map_err(ioerr)?;
    }
    let out = tar.into_inner().map_err(ioerr)?.finish().map_err(ioerr)?;
    out.sync_all().map_err(ioerr)?;
    super::restrict_permissions(&work.join("archive.nuono.tar.gz"), 0o600);
    if out.metadata().map_err(ioerr)?.len() > MAX_UPLOAD {
        return Err("压缩包超过浏览器迁移限制 512 MiB，请使用命令行备份".into());
    }
    preview(data, url, task_id, &manifest.plugins).map_err(Into::into)
}
fn copy_tree(source: &Path, target: &Path, budget: &mut (usize, u64)) -> Result<(), String> {
    let meta = fs::symlink_metadata(source).map_err(ioerr)?;
    if meta.is_dir() {
        plain_dir(target)?;
        let before = fs::read_dir(source)
            .map_err(ioerr)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(ioerr)?;
        for name in &before {
            copy_tree(&source.join(name), &target.join(name), budget)?;
        }
        let after = fs::read_dir(source)
            .map_err(ioerr)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(ioerr)?;
        if before != after {
            return Err("备份期间文件列表发生变化，请暂停相关插件后重试".into());
        }
    } else if meta.is_file() {
        budget.0 += 1;
        budget.1 = budget.1.checked_add(meta.len()).ok_or("文件大小溢出")?;
        if budget.0 > MAX_FILES || budget.1 > MAX_EXPANDED {
            return Err("文件数超过 20000 或总大小超过 2 GiB".into());
        }
        let before = hash_file(source)?;
        fs::copy(source, target).map_err(ioerr)?;
        super::restrict_permissions(target, 0o600);
        if hash_file(source)? != before || hash_file(target)? != before {
            return Err(format!(
                "备份期间文件发生变化：{}，请暂停相关插件后重试",
                source.display()
            ));
        }
        File::open(target)
            .map_err(ioerr)?
            .sync_all()
            .map_err(ioerr)?;
    } else {
        return Err(format!("不能打包符号链接或特殊文件：{}", source.display()));
    }
    Ok(())
}
fn validate_manifest(m: &Manifest) -> Result<(), String> {
    if m.format != FORMAT {
        return Err("不支持的迁移包格式版本".into());
    }
    if m.files.len() > MAX_FILES || m.roots.len() > MAX_FILES {
        return Err("迁移包文件过多".into());
    }
    let roots = m.roots.iter().collect::<BTreeSet<_>>();
    if roots.len() != m.roots.len() || m.roots.iter().any(|r| !safe_root(r)) {
        return Err("迁移包包含重复或不合法的数据根路径".into());
    }
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for f in &m.files {
        if !safe_relative(&f.path)
            || !names.insert(f.path.clone())
            || f.sha256.len() != 64
            || hex::decode(&f.sha256).is_err()
        {
            return Err("迁移包包含重复/不安全路径或校验和无效".into());
        }
        if !matches!(f.path.as_str(), "database.dump" | "startup-reference.toml") {
            let Some(rest) = f.path.strip_prefix("data/") else {
                return Err("迁移包包含范围之外的文件".into());
            };
            let root = rest.split('/').next().unwrap();
            if !m.roots.iter().any(|r| r == root) {
                return Err("文件不在已声明的恢复范围内".into());
            }
        }
        if f.size > MAX_UPLOAD {
            return Err("单文件超过限制".into());
        }
        total = total.checked_add(f.size).ok_or("文件大小溢出")?;
    }
    if total > MAX_EXPANDED
        || !names.contains("database.dump")
        || !names.contains("startup-reference.toml")
    {
        return Err("数据超过 2 GiB 或缺少数据库/启动配置".into());
    }
    Ok(())
}
/// 严格按清单（manifest）白名单逐个提取普通文件并校验哈希，绝不直接调用 `tar::unpack`。
pub fn inspect_upload(
    data: &Path,
    url: &str,
    id: &str,
    plugins: &BTreeMap<String, String>,
) -> OpResult<Preview> {
    let work = workspace(data, id)?;
    let stage = work.join("stage");
    if stage.exists() {
        return Err(OpError::conflict("此上传已处理，请重新上传"));
    }
    plain_dir(&stage)?;
    let result = (|| {
        let input = File::open(work.join("archive.nuono.tar.gz")).map_err(ioerr)?;
        if input.metadata().map_err(ioerr)?.len() > MAX_UPLOAD {
            return Err(OpError::invalid("上传超过 512 MiB"));
        }
        let decoded = GzDecoder::new(input)
            .take(MAX_EXPANDED + MAX_MANIFEST + (MAX_FILES as u64) * 2048 + 65536);
        let mut archive = tar::Archive::new(decoded);
        // 使用 raw(true) 原始条目遍历，防止恶意 tar 包通过超大 PAX 扩展头或长文件名耗尽内存。
        let mut items = archive.entries().map_err(OpError::invalid)?.raw(true);
        let mut first = items
            .next()
            .ok_or_else(|| OpError::invalid("迁移包为空"))?
            .map_err(OpError::invalid)?;
        if first.path().map_err(OpError::invalid)?.as_ref() != Path::new("manifest.json")
            || !first.header().entry_type().is_file()
            || first.size() > MAX_MANIFEST
        {
            return Err(OpError::invalid("迁移包必须以有效 manifest.json 开始"));
        }
        let mut bytes = Vec::new();
        first
            .by_ref()
            .take(MAX_MANIFEST + 1)
            .read_to_end(&mut bytes)
            .map_err(OpError::invalid)?;
        if bytes.len() as u64 > MAX_MANIFEST {
            return Err(OpError::invalid("元数据过大"));
        }
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(OpError::invalid)?;
        validate_manifest(&manifest).map_err(OpError::invalid)?;
        let required = manifest.files.iter().map(|f| f.size).sum::<u64>();
        if fs2::available_space(&stage).map_err(ioerr)? < required.saturating_add(64 * 1024 * 1024)
        {
            return Err(OpError::conflict(
                "磁盘空间不足以展开迁移包，请清理构建缓存或使用更大磁盘后重试",
            ));
        }
        let expected = manifest
            .files
            .iter()
            .map(|f| (f.path.clone(), f))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        let mut long_name = None::<String>;
        for item in items {
            let mut item = item.map_err(OpError::invalid)?;
            if item.header().entry_type().is_gnu_longname() {
                if long_name.is_some() || item.size() > 1025 {
                    return Err(OpError::invalid("迁移包路径扩展过大或重复"));
                }
                let mut bytes = Vec::new();
                item.read_to_end(&mut bytes).map_err(OpError::invalid)?;
                if bytes.last() == Some(&0) {
                    bytes.pop();
                }
                let name = String::from_utf8(bytes).map_err(OpError::invalid)?;
                if !safe_relative(&name) {
                    return Err(OpError::invalid("迁移包长路径不合法"));
                }
                long_name = Some(name);
                continue;
            }
            if !item.header().entry_type().is_file() {
                return Err(OpError::invalid(
                    "迁移包拒绝符号链接、硬链接、目录及特殊文件",
                ));
            }
            let path = match long_name.take() {
                Some(name) => name,
                None => item
                    .path()
                    .map_err(OpError::invalid)?
                    .to_str()
                    .ok_or_else(|| OpError::invalid("路径非 UTF-8"))?
                    .to_string(),
            };
            if !safe_relative(&path) || !seen.insert(path.clone()) {
                return Err(OpError::invalid("迁移包含重复或越界路径"));
            }
            let f = expected
                .get(&path)
                .ok_or_else(|| OpError::invalid("迁移包包含未登记文件"))?;
            if item.size() != f.size {
                return Err(OpError::invalid("文件大小与清单不一致"));
            }
            let dest = stage.join(&path);
            fs::create_dir_all(dest.parent().unwrap()).map_err(ioerr)?;
            let mut out = File::create(&dest).map_err(ioerr)?;
            super::restrict_permissions(&dest, 0o600);
            // 将 tar 流解压读取与本地磁盘写入分步处理，以便将归档损坏归类为输入错误（400）而非服务器 I/O 错误（500）。
            let mut remaining = f.size;
            let mut buffer = [0u8; 64 * 1024];
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = item.read(&mut buffer[..limit]).map_err(OpError::invalid)?;
                if count == 0 {
                    return Err(OpError::invalid("迁移包文件被截断"));
                }
                out.write_all(&buffer[..count])?;
                remaining -= count as u64;
            }
            out.sync_all().map_err(ioerr)?;
            if hash_file(&dest)? != (f.size, f.sha256.clone()) {
                return Err(OpError::invalid(format!("文件校验失败：{path}")));
            }
        }
        if long_name.is_some() || seen.len() != expected.len() {
            return Err(OpError::invalid("迁移包文件缺失"));
        }
        for root in &manifest.roots {
            let p = stage.join("data").join(root);
            if !p.exists() {
                plain_dir(&p)?;
            }
        }
        write_json(&work.join("manifest.json"), &manifest)?;
        preview(data, url, id, plugins)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}
pub fn preview(
    data: &Path,
    url: &str,
    id: &str,
    plugins: &BTreeMap<String, String>,
) -> OpResult<Preview> {
    let work = workspace(data, id)?;
    let manifest_path = work.join("manifest.json");
    fs::symlink_metadata(&manifest_path).map_err(requested_io)?;
    let manifest: Manifest = read_json(&manifest_path)?;
    validate_manifest(&manifest)?;
    let mut blockers = Vec::new();
    if work.join("rollback").exists() {
        blockers.push("此迁移包已用于恢复，原数据正在留存。若要再次恢复，请重新上传下载的归档，创建新的恢复记录。".into());
    }
    if manifest.schema_fingerprint != env!("NUONO_STORAGE_FINGERPRINT") {
        blockers.push("数据库迁移结构不同，请先运行与迁移包相同的框架提交版本。".into());
    }
    if manifest.plugin_fingerprint != crate::source_build::running_fingerprint() {
        blockers.push("插件源码构建指纹不同，请使用源机的插件提交版本重新构建后导入。".into());
    }
    if manifest.backend != backend(url) {
        blockers.push(format!(
            "数据库后端不一致：包内 {}，本机 {}。请在目标机器使用相同数据库后端。",
            manifest.backend,
            backend(url)
        ));
    }
    if manifest.framework != env!("CARGO_PKG_VERSION") {
        blockers.push(format!(
            "框架版本不一致：包内 {}，本机 {}。请先使用相同版本完成恢复。",
            manifest.framework,
            env!("CARGO_PKG_VERSION")
        ));
    }
    for (name, version) in &manifest.plugins {
        if plugins.get(name) != Some(version) {
            blockers.push(format!(
                "请先安装并运行 {name} {version}，本机为 {}",
                plugins.get(name).map(String::as_str).unwrap_or("未安装")
            ));
        }
    }
    if let Err(e) = protect_database_roots(url, data, &manifest.roots) {
        blockers.push(e);
    }
    let format =
        super::detect_backup_format(&work.join("stage/database.dump")).map_err(OpError::invalid)?;
    let actual = if format == super::BackupFormat::Sqlite {
        "sqlite"
    } else {
        "postgres"
    };
    if actual != manifest.backend {
        return Err(OpError::invalid("数据库文件格式与清单不一致"));
    }
    let total_bytes = manifest.files.iter().map(|f| f.size).sum();
    let mut warnings = manifest.notes.clone();
    if manifest.framework_fingerprint != env!("NUONO_FRAMEWORK_FINGERPRINT") {
        warnings.push("框架源码构建指纹不同，但数据库迁移结构一致。无法代替业务兼容性保证；建议先使用源机提交版本恢复，再升级。".into());
    }
    warnings.push("将替换清单中的整个数据目录，目录内现有文件也会被替换；未列出的本机目录保留。恢复会回退账号和登录会话，请准备备份时使用的密码。".into());
    warnings.push(
        "源机启动配置随包保留为参考文件，不会覆盖本机；data 之外的业务文件需在目标机另行准备。"
            .into(),
    );
    Ok(Preview {
        id: id.into(),
        manifest,
        total_bytes,
        blockers,
        warnings,
    })
}
fn verify_stage(work: &Path, m: &Manifest) -> Result<(), String> {
    validate_manifest(m)?;
    let mut actual = Vec::new();
    collect(&work.join("stage"), &work.join("stage"), &mut actual)?;
    if actual.len() != m.files.len() {
        return Err("待恢复目录包含未登记文件或文件缺失".into());
    }
    for f in &m.files {
        let path = work.join("stage").join(&f.path);
        reject_symlink_ancestors(&path, &work.join("stage"))?;
        if hash_file(&path)? != (f.size, f.sha256.clone()) {
            return Err(format!("待恢复文件已改变：{}", f.path));
        }
    }
    Ok(())
}
fn reject_symlink_ancestors(path: &Path, root: &Path) -> Result<(), String> {
    let mut p = Some(path);
    while let Some(x) = p {
        let m = fs::symlink_metadata(x).map_err(ioerr)?;
        if m.file_type().is_symlink() {
            return Err("恢复路径包含符号链接".into());
        }
        if x == root {
            break;
        }
        p = x.parent();
    }
    Ok(())
}
pub fn schedule(
    data: &Path,
    url: &str,
    id: &str,
    plugins: &BTreeMap<String, String>,
) -> OpResult<()> {
    if is_pending(data) || data.join(RESTORE_MARKER).exists() {
        return Err(OpError::conflict("已有恢复任务，请完成后再提交"));
    }
    let p = preview(data, url, id, plugins)?;
    if !p.blockers.is_empty() {
        return Err(OpError::conflict(p.blockers.join("；")));
    }
    let work = workspace(data, id)?;
    verify_stage(&work, &p.manifest).map_err(OpError::conflict)?;
    write_json(
        &directory(data).join(PENDING),
        &Journal {
            database_target: target_fingerprint(url),
            id: id.into(),
            phase: "scheduled".into(),
            roots: p.manifest.roots,
            had_roots: Vec::new(),
        },
    )
    .map_err(Into::into)
}
pub fn remove(data: &Path, id: &str) -> OpResult<()> {
    if is_pending(data) {
        return Err(OpError::conflict("恢复期间不能删除迁移资料"));
    }
    let work = workspace(data, id)?;
    if work.join("rollback").exists() {
        return Err(OpError::conflict(
            "恢复回滚副本保留在服务器，请核对恢复结果后由运维归档，不通过此入口删除",
        ));
    }
    fs::remove_dir_all(work).map_err(requested_io)
}
pub fn list(data: &Path) -> Result<Vec<(String, Manifest)>, String> {
    let dir = directory(data);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e in fs::read_dir(dir).map_err(ioerr)? {
        let e = e.map_err(ioerr)?;
        let id = e.file_name().to_string_lossy().to_string();
        if uuid::Uuid::parse_str(&id).is_ok()
            && e.file_type().map_err(ioerr)?.is_dir()
            && let Ok(m) = read_json(&e.path().join("manifest.json"))
        {
            out.push((id, m));
        }
    }
    Ok(out)
}
async fn finish(data: &Path, j: &Journal, status: &str, message: String) -> Result<(), String> {
    write_json(
        &directory(data).join("result.json"),
        &TransferResult {
            id: j.id.clone(),
            status: status.into(),
            message,
            completed_at: now(),
        },
    )?;
    fs::remove_file(directory(data).join(PENDING)).map_err(ioerr)?;
    sync_dir(&directory(data));
    Ok(())
}
fn rollback_files(data: &Path, work: &Path, j: &Journal) -> Result<(), String> {
    for root in &j.roots {
        let old = work.join("rollback/files").join(root);
        let current = data.join(root);
        let staged = work.join("stage/data").join(root);
        if old.exists() {
            if current.exists() {
                let failed = work.join("failed-files");
                plain_dir(&failed)?;
                fs::rename(&current, failed.join(root)).map_err(ioerr)?;
            }
            fs::rename(old, current).map_err(ioerr)?;
        } else if !j.had_roots.contains(root) && !staged.exists() && current.exists() {
            let failed = work.join("failed-files");
            plain_dir(&failed)?;
            fs::rename(current, failed.join(root)).map_err(ioerr)?;
        }
    }
    sync_dir(data);
    Ok(())
}
/// 在建立任何数据库连接池或加载插件之前调用。
/// 返回 `Err` 表示恢复或回滚本身未能完成（或检测到配置冲突），调用方必须立即终止启动流程。
pub async fn apply_pending(
    url: &str,
    data: &Path,
    config_path: &Path,
) -> Result<Option<TransferResult>, String> {
    if !is_pending(data) {
        return Ok(None);
    }
    let mut j: Journal = read_json(&directory(data).join(PENDING))?;
    let work = workspace(data, &j.id)?;
    if j.database_target != target_fingerprint(url) {
        return Err(
            "恢复安排后数据库连接配置已改变，已停止启动；请恢复原目标连接配置后处理此任务".into(),
        );
    }
    machine_file_conflicts(data, &j.roots, config_path)?;
    if j.roots.iter().any(|r| !safe_root(r)) {
        return Err("恢复日志含不安全路径，已停止启动".into());
    }
    if j.phase == "committed" {
        super::replay_companion_deletions(url, data).await?;
        finish(
            data,
            &j,
            "completed",
            "迁移恢复成功。启动配置沿用本机，原数据已留存。".into(),
        )
        .await?;
        return read_status(data);
    }
    if j.phase == "restoring" {
        rollback_files(data, &work, &j)?;
        super::restore_managed_database(url, &work.join("rollback/database.dump"), data)
            .await
            .map_err(|e| format!("恢复被中断且数据库回滚失败，已停止启动：{e}"))?;
        finish(
            data,
            &j,
            "recovered",
            "检测到上次恢复中断，已回退原数据库和文件；请检查后重新导入。".into(),
        )
        .await?;
        return read_status(data);
    }
    if j.phase != "scheduled" {
        return Err("未知恢复日志阶段，已停止启动".into());
    }
    let m: Manifest = read_json(&work.join("manifest.json"))?;
    if j.roots != m.roots {
        return Err("恢复日志与归档范围不一致，已停止启动".into());
    }
    let preparation = (|| {
        verify_stage(&work, &m)?;
        protect_database_roots(url, data, &j.roots)?;
        if m.schema_fingerprint != env!("NUONO_STORAGE_FINGERPRINT")
            || m.framework != env!("CARGO_PKG_VERSION")
            || m.backend != backend(url)
            || m.plugin_fingerprint != crate::source_build::running_fingerprint()
        {
            return Err("确认后本机版本或数据库后端已变化，请重新预检查".into());
        }
        plain_dir(&work.join("rollback"))?;
        plain_dir(&work.join("rollback/files"))?;
        for root in &j.roots {
            let p = data.join(root);
            if p.exists() {
                reject_symlink_ancestors(&p, data)?;
                j.had_roots.push(root.clone());
            }
        }
        Ok::<_, String>(())
    })();
    if let Err(e) = preparation {
        finish(data, &j, "failed", format!("预检查失败，原数据未改动：{e}")).await?;
        return read_status(data);
    }
    if let Err(e) = super::dump_database(url, &work.join("rollback/database.dump")).await {
        finish(
            data,
            &j,
            "failed",
            format!("保存原数据库失败，原数据未改动：{e}"),
        )
        .await?;
        return read_status(data);
    }
    j.phase = "restoring".into();
    write_json(&directory(data).join(PENDING), &j)?;
    let swap = (|| {
        for root in &j.roots {
            let p = data.join(root);
            if p.exists() {
                fs::rename(&p, work.join("rollback/files").join(root)).map_err(ioerr)?;
                sync_dir(data);
            }
            fs::rename(work.join("stage/data").join(root), p).map_err(ioerr)?;
            sync_dir(data);
        }
        Ok::<_, String>(())
    })();
    let result = match swap {
        Ok(()) => {
            super::restore_managed_database(url, &work.join("stage/database.dump"), data).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        rollback_files(data, &work, &j)
            .map_err(|r| format!("恢复失败（{e}），文件回滚失败，停止启动：{r}"))?;
        super::restore_managed_database(url, &work.join("rollback/database.dump"), data)
            .await
            .map_err(|r| format!("恢复失败（{e}），数据库回滚失败，停止启动：{r}"))?;
        finish(
            data,
            &j,
            "failed",
            format!("恢复失败，已回退原数据库和文件：{e}"),
        )
        .await?;
    } else {
        j.phase = "committed".into();
        write_json(&directory(data).join(PENDING), &j)?;
        finish(
            data,
            &j,
            "completed",
            "迁移恢复成功。启动配置沿用本机，原数据保留在迁移包的 rollback 目录。".into(),
        )
        .await?;
    }
    read_status(data)
}

#[cfg(test)]
mod tests {
    use super::super::query_scalar;
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Statement};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("nuono-transfer-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    async fn db(path: &Path, value: &str) -> String {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let framework =
            nuono_core::db::Database::from_config(&nuono_core::config::DatabaseConfig {
                url: url.clone(),
                max_connections: 1,
            })
            .await
            .unwrap();
        framework.shutdown().await.unwrap();
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared("CREATE TABLE IF NOT EXISTS probe (value TEXT)")
            .await
            .unwrap();
        pool.execute_unprepared("DELETE FROM probe").await.unwrap();
        pool.execute_raw(Statement::from_sql_and_values(
            pool.get_database_backend(),
            "INSERT INTO probe VALUES (?)",
            [value.into()],
        ))
        .await
        .unwrap();
        pool.close().await.unwrap();
        url
    }
    async fn value(url: &str) -> String {
        let p = Database::connect(url).await.unwrap();
        let v = query_scalar(
            &p,
            Statement::from_string(p.get_database_backend(), "SELECT value FROM probe"),
        )
        .await
        .unwrap();
        p.close().await.unwrap();
        v
    }
    async fn archive(root: &Path) -> (PathBuf, String, Preview) {
        let data = root.join("data");
        fs::create_dir_all(data.join("Plugin/assets")).unwrap();
        fs::create_dir_all(data.join("hub/profile")).unwrap();
        fs::write(data.join("Plugin/config.toml"), "new plugin settings").unwrap();
        fs::write(data.join("Plugin/assets/picture.png"), b"image bytes").unwrap();
        fs::write(
            data.join("Plugin/assets")
                .join(format!("{}.png", "long-name-".repeat(16))),
            b"long name image",
        )
        .unwrap();
        fs::write(data.join("hub/profile/avatar.png"), b"avatar").unwrap();
        let cfg = root.join("nuono.toml");
        fs::write(&cfg, "host='127.0.0.1'").unwrap();
        let url = db(&data.join("runtime.db"), "source").await;
        let result = create(&data, &url, &cfg, BTreeMap::new()).await.unwrap();
        (data, url, result)
    }
    #[tokio::test]
    async fn package_roundtrip_restores_database_files_but_preserves_machine_config() {
        let source = Temp::new();
        let (data, _, created) = archive(&source.0).await;
        let target = Temp::new();
        let td = target.0.join("data");
        fs::create_dir_all(td.join("Plugin")).unwrap();
        fs::write(td.join("Plugin/config.toml"), "old").unwrap();
        fs::write(target.0.join("nuono.toml"), "host='0.0.0.0'").unwrap();
        let url = db(&td.join("target.db"), "target").await;
        let id = uuid::Uuid::new_v4().to_string();
        let w = workspace(&td, &id).unwrap();
        plain_dir(&w).unwrap();
        fs::copy(
            workspace(&data, &created.id)
                .unwrap()
                .join("archive.nuono.tar.gz"),
            w.join("archive.nuono.tar.gz"),
        )
        .unwrap();
        let p = inspect_upload(&td, &url, &id, &BTreeMap::new()).unwrap();
        assert!(p.blockers.is_empty());
        assert!(!p.manifest.files.iter().any(|f| f.path == "data/runtime.db"));
        schedule(&td, &url, &id, &BTreeMap::new()).unwrap();
        assert!(remove(&td, &id).is_err());
        let r = apply_pending(&url, &td, &target.0.join("nuono.toml"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, "completed");
        assert_eq!(value(&url).await, "source");
        assert_eq!(
            fs::read_to_string(td.join("Plugin/config.toml")).unwrap(),
            "new plugin settings"
        );
        assert_eq!(
            fs::read_to_string(w.join("rollback/files/Plugin/config.toml")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(target.0.join("nuono.toml")).unwrap(),
            "host='0.0.0.0'"
        );
        assert!(!is_pending(&td));
    }
    #[tokio::test]
    async fn interrupted_restore_rolls_back_before_startup() {
        let t = Temp::new();
        let (data, url, p) = archive(&t.0).await;
        let work = workspace(&data, &p.id).unwrap();
        db(&data.join("runtime.db"), "local-before-restore").await;
        plain_dir(&work.join("rollback/files")).unwrap();
        super::super::dump_database(&url, &work.join("rollback/database.dump"))
            .await
            .unwrap();
        fs::write(data.join("Plugin/config.toml"), "local config").unwrap();
        fs::rename(data.join("Plugin"), work.join("rollback/files/Plugin")).unwrap();
        fs::rename(work.join("stage/data/Plugin"), data.join("Plugin")).unwrap();
        db(&data.join("runtime.db"), "partially-restored").await;
        write_json(
            &directory(&data).join(PENDING),
            &Journal {
                database_target: target_fingerprint(&url),
                id: p.id,
                phase: "restoring".into(),
                roots: vec!["Plugin".into()],
                had_roots: vec!["Plugin".into()],
            },
        )
        .unwrap();
        assert_eq!(
            apply_pending(&url, &data, &t.0.join("nuono.toml"))
                .await
                .unwrap()
                .unwrap()
                .status,
            "recovered"
        );
        assert_eq!(value(&url).await, "local-before-restore");
        assert_eq!(
            fs::read_to_string(data.join("Plugin/config.toml")).unwrap(),
            "local config"
        );
    }
    #[tokio::test]
    async fn changed_files_and_incompatible_schema_cannot_be_scheduled() {
        let t = Temp::new();
        let (data, url, p) = archive(&t.0).await;
        let w = workspace(&data, &p.id).unwrap();
        let mut m: Manifest = read_json(&w.join("manifest.json")).unwrap();
        m.schema_fingerprint = "future-schema".into();
        write_json(&w.join("manifest.json"), &m).unwrap();
        assert!(
            !preview(&data, &url, &p.id, &BTreeMap::new())
                .unwrap()
                .blockers
                .is_empty()
        );
        assert!(schedule(&data, &url, &p.id, &BTreeMap::new()).is_err());
        m.schema_fingerprint = env!("NUONO_STORAGE_FINGERPRINT").into();
        write_json(&w.join("manifest.json"), &m).unwrap();
        fs::write(w.join("stage/data/Plugin/config.toml"), "changed").unwrap();
        assert!(
            schedule(&data, &url, &p.id, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("已改变")
        );
        assert!(!is_pending(&data));
    }
    #[test]
    fn rejects_traversal_roots_and_archive_limits() {
        for p in [
            "../outside",
            "/absolute",
            "a/../b",
            "a\\b",
            "C:/windows",
            "a//b",
            "a/./b",
        ] {
            assert!(!safe_relative(p), "{p}");
        }
        for p in [
            "backup",
            ".private",
            "restore_pending",
            "../a",
            "plugin-jobs",
            "runtime_tasks",
        ] {
            assert!(!safe_root(p));
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_never_exported_or_extracted() {
        let t = Temp::new();
        let (data, url, p) = archive(&t.0).await;
        std::os::unix::fs::symlink(t.0.join("nuono.toml"), data.join("Plugin/stolen")).unwrap();
        assert!(
            create(&data, &url, &t.0.join("nuono.toml"), BTreeMap::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("符号链接")
        );
        let id = uuid::Uuid::new_v4().to_string();
        let work = workspace(&data, &id).unwrap();
        plain_dir(&work).unwrap();
        let output = File::create(work.join("archive.nuono.tar.gz")).unwrap();
        let mut tar = tar::Builder::new(GzEncoder::new(output, Compression::fast()));
        let bytes = serde_json::to_vec(&p.manifest).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_cksum();
        tar.append_data(&mut h, "manifest.json", bytes.as_slice())
            .unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_link_name("/tmp/outside").unwrap();
        h.set_cksum();
        tar.append_data(&mut h, "data/Plugin/stolen", std::io::empty())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        assert!(
            inspect_upload(&data, &url, &id, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("拒绝")
        );
        assert!(!work.join("stage").exists());
    }
    #[tokio::test]
    async fn failed_database_restore_rolls_files_and_database_back() {
        let t = Temp::new();
        let (data, url, p) = archive(&t.0).await;
        let w = workspace(&data, &p.id).unwrap();
        // 构造一个文件头魔数与清单哈希都合法、但后续 SQLite 完整性校验必失败的伪造库文件，测试文件交换后的回滚逻辑。
        let bytes = b"SQLite format 3\0not a real sqlite database";
        fs::write(w.join("stage/database.dump"), bytes).unwrap();
        let mut m = p.manifest;
        let entry = m
            .files
            .iter_mut()
            .find(|f| f.path == "database.dump")
            .unwrap();
        let (size, hash) = hash_file(&w.join("stage/database.dump")).unwrap();
        entry.size = size;
        entry.sha256 = hash;
        write_json(&w.join("manifest.json"), &m).unwrap();
        db(&data.join("runtime.db"), "keep me").await;
        fs::write(data.join("Plugin/config.toml"), "keep settings").unwrap();
        schedule(&data, &url, &p.id, &BTreeMap::new()).unwrap();
        assert_eq!(
            apply_pending(&url, &data, &t.0.join("nuono.toml"))
                .await
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );
        assert_eq!(value(&url).await, "keep me");
        assert_eq!(
            fs::read_to_string(data.join("Plugin/config.toml")).unwrap(),
            "keep settings"
        );
    }
    #[tokio::test]
    async fn postgres_package_restore_and_failed_restore_preserve_files_and_data() {
        if std::env::var("NUONO_TEST_DATABASE_BACKEND").as_deref() == Ok("sqlite") {
            return;
        }
        let admin_url = std::env::var("NUONO_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://nuono:nuono@127.0.0.1:5432/postgres".into());
        let admin = Database::connect(&admin_url)
            .await
            .expect("PostgreSQL test management database");
        let name = format!("nuono_transfer_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute_unprepared(&format!("CREATE DATABASE \"{name}\""))
            .await
            .unwrap();
        let mut parsed = url::Url::parse(&admin_url).unwrap();
        parsed.set_path(&format!("/{name}"));
        let url = parsed.to_string();
        let t = Temp::new();
        let data = t.0.join("data");
        fs::create_dir_all(data.join("Plugin")).unwrap();
        fs::write(data.join("Plugin/config.toml"), "snapshot settings").unwrap();
        let config = t.0.join("nuono.toml");
        fs::write(&config, "host='127.0.0.1'").unwrap();
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared("CREATE TABLE probe(value TEXT)")
            .await
            .unwrap();
        pool.execute_unprepared("INSERT INTO probe VALUES ('snapshot')")
            .await
            .unwrap();
        pool.close().await.unwrap();
        let p = create(&data, &url, &config, BTreeMap::new()).await.unwrap();
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared("UPDATE probe SET value='newer'")
            .await
            .unwrap();
        pool.close().await.unwrap();
        fs::write(data.join("Plugin/config.toml"), "newer settings").unwrap();
        schedule(&data, &url, &p.id, &BTreeMap::new()).unwrap();
        assert_eq!(
            apply_pending(&url, &data, &t.0.join("nuono.toml"))
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            fs::read_to_string(data.join("Plugin/config.toml")).unwrap(),
            "snapshot settings"
        );
        let pool = Database::connect(&url).await.unwrap();
        let value: String = query_scalar(
            &pool,
            Statement::from_string(pool.get_database_backend(), "SELECT value FROM probe"),
        )
        .await
        .unwrap();
        assert_eq!(value, "snapshot");
        pool.close().await.unwrap();
        let broken = create(&data, &url, &config, BTreeMap::new()).await.unwrap();
        let work = workspace(&data, &broken.id).unwrap();
        fs::write(work.join("stage/database.dump"), b"PGDMP broken archive").unwrap();
        let mut manifest = broken.manifest;
        let record = manifest
            .files
            .iter_mut()
            .find(|f| f.path == "database.dump")
            .unwrap();
        let (size, sha) = hash_file(&work.join("stage/database.dump")).unwrap();
        record.size = size;
        record.sha256 = sha;
        write_json(&work.join("manifest.json"), &manifest).unwrap();
        fs::write(data.join("Plugin/config.toml"), "keep this").unwrap();
        schedule(&data, &url, &broken.id, &BTreeMap::new()).unwrap();
        assert_eq!(
            apply_pending(&url, &data, &t.0.join("nuono.toml"))
                .await
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );
        assert_eq!(
            fs::read_to_string(data.join("Plugin/config.toml")).unwrap(),
            "keep this"
        );
        let pool = Database::connect(&url).await.unwrap();
        let value: String = query_scalar(
            &pool,
            Statement::from_string(pool.get_database_backend(), "SELECT value FROM probe"),
        )
        .await
        .unwrap();
        assert_eq!(value, "snapshot");
        pool.close().await.unwrap();
        admin
            .execute_unprepared(&format!("DROP DATABASE \"{name}\""))
            .await
            .unwrap();
        admin.close().await.unwrap();
    }
    #[tokio::test]
    async fn restore_rejects_changed_database_target_and_machine_file_overlap() {
        let t = Temp::new();
        let (data, url, p) = archive(&t.0).await;
        assert!(
            machine_file_conflicts(&data, &p.manifest.roots, &data.join("Plugin/config.toml"))
                .unwrap_err()
                .contains("启动配置")
        );
        fs::create_dir_all(data.join(".plugin-jobs")).unwrap();
        fs::write(data.join(".plugin-jobs/pending.json"), "must not transfer").unwrap();
        let exported = create(&data, &url, &t.0.join("nuono.toml"), BTreeMap::new())
            .await
            .unwrap();
        assert!(
            !exported
                .manifest
                .roots
                .iter()
                .any(|r| r.contains("plugin-jobs"))
        );
        schedule(&data, &url, &p.id, &BTreeMap::new()).unwrap();
        let changed = format!("sqlite://{}?mode=rwc", t.0.join("different.db").display());
        assert!(
            apply_pending(&changed, &data, &t.0.join("nuono.toml"))
                .await
                .unwrap_err()
                .contains("连接配置已改变")
        );
        assert_eq!(value(&url).await, "source");
        assert!(is_pending(&data));
    }
}
