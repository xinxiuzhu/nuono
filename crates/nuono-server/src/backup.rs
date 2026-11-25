//! 数据库自动备份与恢复（PostgreSQL / SQLite 双后端）。
//!
//! - PostgreSQL：`pg_dump` custom-format 归档 + `pg_restore --clean`。
//! - SQLite：`VACUUM INTO ?1` 参数一致性快照（单读事务整库复制，不复制活主文件、
//!   无需 `-wal` 伴随）；恢复在下次启动、连接池建立前校验（header /
//!   `integrity_check` / `foreign_key_check`）后原子换入。
//!
//! 备份统一 `.dump` 后缀，恢复时按文件 magic 区分格式（`PGDMP` vs
//! `SQLite format 3\0`），跨后端备份一律拒绝。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, Statement,
    TryGetable,
};
use serde::{Deserialize, Serialize};
use url::Url;

pub mod transfer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreMarker {
    pub source: String,
    pub actor: String,
}

/// 自动备份目录名（相对 data 目录）。
pub const BACKUP_DIR: &str = "backup";
/// 距上一份备份多久后才允许再备一份。
pub const BACKUP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

pub const AUTO_BACKUP_PREFIX: &str = "nuono_auto_backup_";
pub const MANUAL_BACKUP_PREFIX: &str = "nuono_manual_backup_";
pub const PRE_RESTORE_BACKUP_PREFIX: &str = "nuono_backup_pre_restore_";
const BACKUP_SUFFIX: &str = ".dump";
const TIMESTAMP_FORMAT: &str = "%Y%m%d_%H%M%S";
/// 待恢复标记文件名（相对 data 目录）。内容 = 备份文件的绝对路径。
pub const RESTORE_MARKER: &str = "restore_pending";

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
const PG_DUMP_MAGIC: &[u8; 5] = b"PGDMP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupFormat {
    Sqlite,
    PostgresCustom,
}

/// 按 URL 分类的数据库目标。
enum DatabaseKind {
    /// 文件库（路径已由 core 的 SQLite URL helper 解析）。
    SqliteFile(PathBuf),
    /// 内存库：可连接但无法备份/恢复到文件。
    SqliteMemory,
    Postgres,
}

/// 复用 core 的 [`nuono_core::db::sqlite_database_path`]，不手写重复解析；
/// 非 SQLite 连接串按 PostgreSQL 处理（格式错误由 pg 工具/连接阶段报错）。
///
/// 先做 scheme 前缀判断，避免把其他后端的 URL 交给 SQLite 文件路径解析。
fn classify(database_url: &str) -> DatabaseKind {
    let lower = database_url.trim().to_ascii_lowercase();
    if !(lower.starts_with("sqlite:") || lower == ":memory:") {
        return DatabaseKind::Postgres;
    }
    match nuono_core::db::sqlite_database_path(database_url) {
        Ok(Some(path)) => DatabaseKind::SqliteFile(path),
        _ => DatabaseKind::SqliteMemory,
    }
}

pub(crate) fn detect_backup_format(path: &Path) -> Result<BackupFormat, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("读取备份文件失败 ({}): {e}", path.display()))?;
    let mut header = [0u8; 16];
    let read = file
        .read(&mut header)
        .map_err(|e| format!("读取备份文件头失败 ({}): {e}", path.display()))?;
    if read >= PG_DUMP_MAGIC.len() && &header[..PG_DUMP_MAGIC.len()] == PG_DUMP_MAGIC {
        return Ok(BackupFormat::PostgresCustom);
    }
    if read == SQLITE_MAGIC.len() && &header == SQLITE_MAGIC {
        return Ok(BackupFormat::Sqlite);
    }
    Err(format!(
        "备份文件格式无法识别（既不是 PostgreSQL custom dump 也不是 SQLite 数据库）: {}",
        path.display()
    ))
}

/// 恢复接口接受的备份文件名必须满足的形状：纯 basename、不能是路径、以 `.dump` 结尾。
pub fn validate_backup_filename(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.len() <= 255
        && !name.chars().any(char::is_control)
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && (name.starts_with(AUTO_BACKUP_PREFIX)
            || name.starts_with(MANUAL_BACKUP_PREFIX)
            || name.starts_with(PRE_RESTORE_BACKUP_PREFIX))
        && name.ends_with(BACKUP_SUFFIX)
}

/// 返回待恢复文件的真实路径；标记损坏时返回错误，清理操作必须保留现有备份。
pub(crate) fn pending_restore_source(data_dir: &Path) -> Result<Option<PathBuf>, String> {
    let marker = data_dir.join(RESTORE_MARKER);
    let metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取恢复标记失败: {error}")),
    };
    if !metadata.file_type().is_file() || metadata.len() > 16 * 1024 {
        return Err("恢复标记不是有效的普通文件，请先检查待恢复任务".to_string());
    }
    let content =
        std::fs::read_to_string(marker).map_err(|error| format!("读取恢复标记失败: {error}"))?;
    let marker: RestoreMarker = serde_json::from_str(&content)
        .map_err(|_| "恢复标记无法解析，请先检查待恢复任务".to_string())?;
    std::fs::canonicalize(marker.source)
        .map(Some)
        .map_err(|error| format!("待恢复备份无法读取，请先检查待恢复任务: {error}"))
}

/// 备份 dump 是整库明文（含 `runtime_secrets`）：目录 0700、文件 0600；非 Unix 空操作。
fn restrict_permissions(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            tracing::warn!(path = %path.display(), error = %e, "收紧备份权限失败");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

/// 在 `PATH` 与常见安装位置（macOS EDB `/Library/PostgreSQL/<v>/bin`、Homebrew、
/// Linux `/usr/lib/postgresql/<v>/bin`）定位 pg 工具。
fn resolve_pg_tool(name: &str) -> Option<PathBuf> {
    let works = |path: &Path| {
        Command::new(path)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    };
    // 显式指定客户端目录（版本对齐：PG 18 服务器需要 18 客户端，旧 pg_dump 拒绝 dump）。
    if let Some(dir) = std::env::var_os("NUONO_TEST_PG_CLIENT_BIN") {
        let candidate = PathBuf::from(dir).join(name);
        if works(&candidate) {
            return Some(candidate);
        }
    }
    if works(Path::new(name)) {
        return Some(PathBuf::from(name));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for ver in ["18", "17", "16", "15", "14"] {
            candidates.push(home.join(format!("Library/PostgreSQL/{ver}/bin/{name}")));
        }
    }
    for base in [
        "/Library/PostgreSQL",
        "/opt/homebrew/opt",
        "/usr/lib/postgresql",
        "/usr/local/opt",
    ] {
        for ver in ["18", "17", "16", "15", "14"] {
            candidates.push(PathBuf::from(format!("{base}/{ver}/bin/{name}")));
        }
        candidates.push(PathBuf::from(format!("{base}/bin/{name}")));
    }
    candidates.push(PathBuf::from(format!("/opt/homebrew/bin/{name}")));
    candidates.push(PathBuf::from(format!("/usr/local/bin/{name}")));
    candidates
        .into_iter()
        .find(|path| path.is_file() && works(path))
}

/// 给子进程 `PATH` 前置工具所在目录（仅影响该子进程，不改动本进程环境）。
fn prepend_child_path(command: &mut Command, tool: &Path) {
    if let Some(dir) = tool.parent()
        && !dir.as_os_str().is_empty()
    {
        let mut paths = vec![dir.to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        if let Ok(joined) = std::env::join_paths(&paths) {
            command.env("PATH", joined);
        }
    }
}

/// 创建一个数据库备份到 `destination`（按 URL 分派后端）。
pub async fn dump_database(database_url: &str, destination: &Path) -> Result<(), String> {
    publish_dump(destination, |staged| async move {
        match classify(database_url) {
            DatabaseKind::SqliteFile(_) => sqlite_dump_database(database_url, &staged).await,
            DatabaseKind::SqliteMemory => {
                Err("SQLite 内存数据库无法备份：内存库没有可持久化的文件".to_string())
            }
            DatabaseKind::Postgres => pg_dump_database(database_url, &staged).await,
        }
    })
    .await
}

/// 先写入同分区的私有暂存目录并 fsync，再通过硬链接原子发布为最终 `.dump` 文件名（绝不覆盖同名旧备份）。
/// 因此一旦最终 `.dump` 文件可见就必然是完整备份；中途任何失败都会由 RAII 守卫自动清理暂存目录。
async fn publish_dump<F, Fut>(destination: &Path, write: F) -> Result<(), String>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let parent = destination.parent().ok_or("备份目标没有父目录")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建备份目录失败: {e}"))?;
    restrict_permissions(parent, 0o700);
    let stage = parent.join(format!(".nuono-backup-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::create_dir(&stage).map_err(|e| format!("创建备份暂存目录失败: {e}"))?;
    let _cleanup = StagedDump(stage.clone());
    restrict_permissions(&stage, 0o700);
    let staged = stage.join("database.dump");
    write(staged.clone()).await?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&staged)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("同步备份文件失败: {e}"))?;
    restrict_permissions(&staged, 0o600);
    std::fs::hard_link(&staged, destination).map_err(|e| format!("发布完整备份失败: {e}"))?;
    fsync_path(parent);
    Ok(())
}

struct StagedDump(PathBuf);

impl Drop for StagedDump {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 把备份导入当前数据库（按 URL 与备份 magic 分派；跨后端拒绝）。
pub async fn restore_database(database_url: &str, source: &Path) -> Result<(), String> {
    let is_sqlite_backup = detect_backup_format(source)? == BackupFormat::Sqlite;
    let is_sqlite_target = !matches!(classify(database_url), DatabaseKind::Postgres);
    match (is_sqlite_backup, is_sqlite_target) {
        (true, true) => sqlite_restore_database(database_url, source).await,
        (false, false) => pg_restore_database(database_url, source).await,
        (false, true) => Err(
            "备份格式与当前数据库后端不匹配：备份是 PostgreSQL custom dump，当前后端是 SQLite；\
             跨后端恢复被拒绝"
                .to_string(),
        ),
        (true, false) => Err(
            "备份格式与当前数据库后端不匹配：备份是 SQLite 数据库文件，当前后端是 PostgreSQL；\
             跨后端恢复被拒绝（切换后端请用迁移工具而非备份恢复）"
                .to_string(),
        ),
    }
}

/// 独立的单连接校验池，不运行宿主迁移，也不改变现有 journal mode。
/// SeaORM 的连接选项钩子会作用于每条新连接，保证池重连后仍保留只读、外键和超时设置。
async fn open_sqlite_snapshot(
    path: &Path,
    read_only: bool,
    busy_timeout: Duration,
    acquire_timeout: Duration,
) -> Result<DatabaseConnection, DbErr> {
    let path = path.to_path_buf();
    // 只用 scheme 选择 SQLite 驱动；文件名通过选项传递，避免路径中的 ?/# 被当成 URL。
    let mut options = ConnectOptions::new("sqlite://nuono-backup");
    options
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(acquire_timeout)
        .map_sqlx_sqlite_opts(move |options| {
            options
                .filename(&path)
                .read_only(read_only)
                .create_if_missing(false)
                .foreign_keys(false)
                .busy_timeout(busy_timeout)
        });
    Database::connect(options).await
}

async fn query_scalar<T: TryGetable>(
    db: &DatabaseConnection,
    statement: Statement,
) -> Result<T, DbErr> {
    db.query_one_raw(statement)
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("数据库查询没有返回行".into()))?
        .try_get_by_index(0)
}

/// SQLite 备份：`VACUUM INTO` 参数一致性快照。
///
/// VACUUM INTO 在单个读事务里把整库写入目标文件（目标必须不存在），与宿主连接池的
/// 并发写互不干扰，产出自包含的完整 SQLite 文件（无需 `-wal` / `-shm` 伴随）。
async fn sqlite_dump_database(database_url: &str, destination: &Path) -> Result<(), String> {
    let source_path = match classify(database_url) {
        DatabaseKind::SqliteFile(path) => path,
        _ => return Err("SQLite 内存数据库无法备份：内存库没有可持久化的文件".to_string()),
    };
    let parent = destination
        .parent()
        .ok_or_else(|| "备份目标没有父目录".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建备份目录失败: {e}"))?;
    restrict_permissions(parent, 0o700);

    // 连接不开 read_only：sqlite 对 VACUUM INTO 的写权限检查针对目标文件而非源库。
    let db = open_sqlite_snapshot(
        &source_path,
        false,
        Duration::from_secs(30),
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| format!("连接 SQLite 源库失败 ({}): {e}", source_path.display()))?;
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "VACUUM INTO ?1",
            [destination.to_string_lossy().into_owned().into()],
        ))
        .await;
    let closed = db.close().await;
    result.map_err(|e| format!("VACUUM INTO 备份失败: {e}"))?;
    closed.map_err(|e| format!("关闭 SQLite 备份连接失败: {e}"))?;
    restrict_permissions(destination, 0o600);
    Ok(())
}

/// 校验 SQLite 备份文件：header + `integrity_check` + `foreign_key_check`。
async fn validate_sqlite_file(path: &Path) -> Result<(), String> {
    if detect_backup_format(path)? != BackupFormat::Sqlite {
        return Err("备份不是 SQLite 数据库文件".to_string());
    }
    let db = open_sqlite_snapshot(path, true, Duration::from_secs(30), Duration::from_secs(10))
        .await
        .map_err(|e| format!("打开备份文件失败: {e}"))?;
    let result = async {
        let integrity: String = query_scalar(
            &db,
            Statement::from_string(DbBackend::Sqlite, "PRAGMA integrity_check"),
        )
        .await
        .map_err(|e| format!("integrity_check 执行失败: {e}"))?;
        if integrity != "ok" {
            return Err(format!("integrity_check 未通过: {integrity}"));
        }
        let violations: i64 = query_scalar(
            &db,
            Statement::from_string(
                DbBackend::Sqlite,
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
            ),
        )
        .await
        .map_err(|e| format!("foreign_key_check 执行失败: {e}"))?;
        if violations > 0 {
            return Err(format!("foreign_key_check 发现 {violations} 处外键违约"));
        }
        Ok(())
    }
    .await;
    let closed = db.close().await;
    result?;
    closed.map_err(|e| format!("关闭备份校验连接失败: {e}"))
}

fn fsync_path(path: &Path) {
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(path).and_then(|f| f.sync_all());
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 额外防线（不作为唯一依据）：探测是否有**不遵循 Nuono sidecar 锁**的客户端
/// （外部 SQLite 程序）仍连着目标库——切换 journal mode 需要整库独占，
/// 任何其他连接都会让它 BUSY 失败。副作用是把目标切到 DELETE 模式，对即将被
/// 换掉/换入的文件无害，启动连接 helper 会重新设回 WAL。Nuono 实例之间的互斥
/// 由 sidecar 独占锁保证（见 [`nuono_core::db::SqliteDatabaseLock`] 的使用处），本探测只覆盖
/// 未协作客户端；文档要求外部 SQLite client 在恢复前关闭。
async fn assert_no_other_sqlite_connection(path: &Path) -> Result<(), String> {
    let db = open_sqlite_snapshot(path, false, Duration::ZERO, Duration::from_secs(5))
        .await
        .map_err(|e| format!("连接目标数据库失败 ({path:?}): {e}"))?;
    let result = query_scalar::<String>(
        &db,
        Statement::from_string(DbBackend::Sqlite, "PRAGMA journal_mode=DELETE"),
    )
    .await;
    let closed = db.close().await;
    match result {
        Ok(_) => closed.map_err(|e| format!("关闭目标数据库探测连接失败: {e}")),
        Err(e) => Err(format!(
            "目标数据库可能仍被其他进程连接（独占探测失败），已拒绝恢复: {e}"
        )),
    }
}

/// 在临时副本上运行框架自身的连接与迁移，验证备份与当前框架 schema 兼容。
///
/// 只做物理校验就换库，可能换入一个启动迁移必失败的旧库且旧库已被清理，无法
/// 回退；这里把「迁移能不能过」提前到换库前在副本上验证。校验（含迁移）全程只
/// 作用在副本上，不触碰 source 与旧 target；校验通过的副本随后**原样换入**目标
/// 路径。`Database::shutdown` 关池并释放其共享 sidecar 锁，不留写批线程。
async fn validate_restore_schema_compatibility(temp: &Path) -> Result<(), String> {
    let cfg = nuono_core::config::DatabaseConfig {
        url: format!("sqlite://{}?mode=rwc", temp.display()),
        max_connections: 1,
    };
    let db = nuono_core::Database::from_config(&cfg)
        .await
        .map_err(|e| format!("备份与当前框架 schema 不兼容（迁移未通过，目标库未改动）: {e}"))?;
    db.shutdown()
        .await
        .map_err(|e| format!("关闭 schema 校验连接失败: {e}"))?;
    Ok(())
}

/// 把校验后的副本收敛成自包含主文件：切回 DELETE journal 会 checkpoint 已有 WAL
/// 并让 SQLite 删除 `-wal` / `-shm`，随后重验一次物理完整性，确保 rename 换入的
/// 文件不依赖任何伴随文件（不能假定 `Database::shutdown` 的 close 一定 checkpoint）。
async fn finalize_temp_snapshot(temp: &Path) -> Result<(), String> {
    let db = open_sqlite_snapshot(
        temp,
        false,
        Duration::from_secs(30),
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| format!("重开 schema 校验副本失败: {e}"))?;
    let result = query_scalar::<String>(
        &db,
        Statement::from_string(DbBackend::Sqlite, "PRAGMA journal_mode=DELETE"),
    )
    .await;
    let closed = db.close().await;
    result.map_err(|e| format!("副本 checkpoint 失败: {e}"))?;
    closed.map_err(|e| format!("关闭副本 checkpoint 连接失败: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(companion_path(temp, suffix));
    }
    validate_sqlite_file(temp).await
}

/// 换库临时副本的 RAII 清理：drop 时移除副本及全部伴随文件（`-wal` / `-shm` /
/// `.nuono-lock`）。恢复成功时主文件已被 rename 走，drop 只清残留伴随文件。
struct TempCopyGuard {
    temp: PathBuf,
}

impl Drop for TempCopyGuard {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", ".nuono-lock"] {
            let _ = std::fs::remove_file(companion_path(&self.temp, suffix));
        }
    }
}

/// `base` 拼后缀的伴随文件路径（`-wal` / `-shm` / `.nuono-lock`）。
fn companion_path(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// SQLite 恢复：校验副本后原子换入目标路径。只在启动早期（连接池建立前）调用。
///
/// 1. 备份复制到同目录临时文件（0600），做 header / integrity / foreign key 校验，
///    再用框架自身迁移验证 schema 兼容、checkpoint 收敛成自包含文件；任何失败
///    直接中止并清理全部临时文件，目标库保持原样；
/// 2. 对目标库**无条件取 sidecar 独占锁并贯穿到换库结束**（Nuono 实例互斥，含
///    目标尚不存在时的建库窗口；见 [`nuono_core::db::SqliteDatabaseLock`]，正常
///    Nuono 生命周期持共享锁），另用 journal-mode 探测兜底未协作的裸客户端
///    （见 [`assert_no_other_sqlite_connection`]）；
/// 3. 旧库与 `-wal` / `-shm` 成组挪到保留文件（三者必须成组，避免旧 WAL 被新库
///    重放），临时文件 rename 换入；成功删保留文件，失败成组搬回，数据不丢失。
async fn sqlite_restore_database(database_url: &str, source: &Path) -> Result<(), String> {
    let target = match classify(database_url) {
        DatabaseKind::SqliteFile(path) => path,
        _ => return Err("SQLite 内存数据库无法从备份恢复".to_string()),
    };
    let raw_parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(raw_parent).map_err(|e| format!("创建数据库目录失败: {e}"))?;
    let target = if target.exists() {
        target
            .canonicalize()
            .map_err(|e| format!("解析数据库真实路径失败: {e}"))?
    } else {
        raw_parent
            .canonicalize()
            .map_err(|e| format!("解析数据库目录失败: {e}"))?
            .join(
                target
                    .file_name()
                    .ok_or_else(|| "SQLite 数据库路径没有文件名".to_string())?,
            )
    };
    let parent = target
        .parent()
        .ok_or_else(|| "SQLite 数据库路径没有父目录".to_string())?;

    let temp = parent.join(format!(
        "nuono_restore_{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::copy(source, &temp).map_err(|e| format!("复制备份到临时文件失败: {e}"))?;
    restrict_permissions(&temp, 0o600);
    let _temp_guard = TempCopyGuard { temp: temp.clone() };
    if let Err(e) = validate_sqlite_file(&temp).await {
        return Err(format!(
            "备份文件校验失败，已中止恢复（目标数据库未改动）: {e}"
        ));
    }
    fsync_path(&temp);
    validate_restore_schema_compatibility(&temp).await?;
    finalize_temp_snapshot(&temp).await?;

    // Sidecar 独占锁无条件获取（目标不存在时同样要锁，堵住建库窗口）并持有到
    // 换库结束（guard drop 释放）：rename 前后不会再有正常 Nuono 实例拿到共享锁
    // 打开/创建该库。
    let _exclusive = nuono_core::db::SqliteDatabaseLock::acquire_exclusive(&target)
        .map_err(|e| format!("{e}；恢复已中止（目标数据库未改动）"))?;
    if target.exists()
        && let Err(e) = assert_no_other_sqlite_connection(&target).await
    {
        return Err(e);
    }

    let keep = parent.join(format!(
        "nuono_restore_prev_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let had_target = target.exists();
    let mut swapped = false;
    let result = (|| -> std::io::Result<()> {
        if had_target {
            std::fs::rename(&target, &keep)?;
            for suffix in ["-wal", "-shm"] {
                let from = companion_path(&target, suffix);
                if from.exists() {
                    std::fs::rename(&from, companion_path(&keep, suffix))?;
                }
            }
        }
        std::fs::rename(&temp, &target)?;
        swapped = true;
        fsync_path(&target);
        Ok(())
    })();

    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(&keep);
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(companion_path(&keep, suffix));
            }
            Ok(())
        }
        Err(e) => {
            if swapped {
                return Err(format!("恢复已完成但收尾失败，请核对数据库状态: {e}"));
            }
            if had_target && keep.exists() {
                let _ = std::fs::rename(&keep, &target);
                for suffix in ["-wal", "-shm"] {
                    let kept = companion_path(&keep, suffix);
                    if kept.exists() {
                        let _ = std::fs::rename(&kept, companion_path(&target, suffix));
                    }
                }
            }
            let _ = std::fs::remove_file(&temp);
            Err(format!("恢复换入失败，原数据库已成组回退、保持原样: {e}"))
        }
    }
}

fn pg_command_env(database_url: &str) -> Result<(String, Vec<(String, String)>), String> {
    let mut url = Url::parse(database_url).map_err(|e| format!("数据库 URL 无效: {e}"))?;
    let mut env = Vec::new();
    if let Some(password) = url.password().map(str::to_string) {
        env.push(("PGPASSWORD".to_string(), password));
        url.set_password(None)
            .map_err(|_| "数据库 URL 密码无法清除".to_string())?;
    }
    Ok((url.to_string(), env))
}

async fn pg_dump_database(database_url: &str, destination: &Path) -> Result<(), String> {
    let (database_url, env) = pg_command_env(database_url)?;
    let tool = resolve_pg_tool("pg_dump")
        .ok_or_else(|| "未找到 pg_dump，请安装 postgresql-client 并确保其在 PATH 中".to_string())?;
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Some(parent) = destination.parent() else {
            return Err("备份目标没有父目录".to_string());
        };
        std::fs::create_dir_all(parent).map_err(|e| format!("创建备份目录失败: {e}"))?;
        restrict_permissions(parent, 0o700);
        let mut command = Command::new(&tool);
        command
            .args(["--format=custom", "--no-owner", "--file"])
            .arg(&destination)
            .arg(&database_url);
        for (key, value) in &env {
            command.env(key, value);
        }
        prepend_child_path(&mut command, &tool);
        let output = command
            .output()
            .map_err(|e| format!("执行 pg_dump 失败: {e}"))?;
        if output.status.success() {
            restrict_permissions(&destination, 0o600);
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&destination);
            Err(format!("pg_dump 失败: {}", stderr.trim()))
        }
    })
    .await
    .map_err(|e| format!("备份任务失败: {e}"))??;
    Ok(())
}

async fn pg_restore_database(database_url: &str, source: &Path) -> Result<(), String> {
    let (database_url, env) = pg_command_env(database_url)?;
    let tool = resolve_pg_tool("pg_restore").ok_or_else(|| {
        "未找到 pg_restore，请安装 postgresql-client 并确保其在 PATH 中".to_string()
    })?;
    let source = source.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(&tool);
        command
            .args([
                "--clean",
                "--if-exists",
                "--no-owner",
                "--exit-on-error",
                "--single-transaction",
                "--dbname",
            ])
            .arg(&database_url)
            .arg(&source);
        for (key, value) in &env {
            command.env(key, value);
        }
        prepend_child_path(&mut command, &tool);
        let output = command
            .output()
            .map_err(|e| format!("执行 pg_restore 失败: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("pg_restore 失败: {}", stderr.trim()))
        }
    })
    .await
    .map_err(|e| format!("恢复任务失败: {e}"))??;
    Ok(())
}

/// 在托管数据库恢复完成前，重放不落明文内容的隐私删除日志（确保已注销/已删除的伴侣隐私数据不会因恢复旧备份而死灰复燃）。
pub(crate) async fn replay_companion_deletions(
    database_url: &str,
    data_dir: &Path,
) -> Result<(), String> {
    let journal = nuono_core::service::privacy_journal::PrivacyJournal::new(data_dir);
    if journal
        .is_empty()
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let config = nuono_core::config::DatabaseConfig {
        url: database_url.to_string(),
        ..Default::default()
    };
    let db = nuono_core::db::Database::from_config(&config)
        .await
        .map_err(|error| error.to_string())?;
    let replay = journal.replay(&db).await;
    let closed = db.shutdown().await;
    replay.and(closed).map_err(|error| error.to_string())
}

pub(crate) async fn restore_managed_database(
    database_url: &str,
    source: &Path,
    data_dir: &Path,
) -> Result<(), String> {
    nuono_core::service::privacy_journal::PrivacyJournal::new(data_dir)
        .is_empty()
        .await
        .map_err(|error| error.to_string())?;
    restore_database(database_url, source).await?;
    replay_companion_deletions(database_url, data_dir).await
}

/// 启动时处理待恢复标记；删除记录重放成功后才删除标记。
///
/// 换库必须发生在宿主连接池建立之前；恢复或删除重放失败时停止启动。
pub async fn apply_pending_restore(
    database_url: &str,
    data_dir: &Path,
) -> Result<Option<String>, String> {
    let marker = data_dir.join(RESTORE_MARKER);
    if !marker.exists() {
        return Ok(None);
    }
    let marker_data: RestoreMarker = serde_json::from_str(
        &std::fs::read_to_string(&marker).map_err(|e| format!("读取恢复标记失败: {e}"))?,
    )
    .map_err(|e| format!("解析恢复标记失败: {e}"))?;
    let raw_source = PathBuf::from(marker_data.source);
    let metadata =
        std::fs::symlink_metadata(&raw_source).map_err(|e| format!("读取恢复源失败: {e}"))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "恢复标记指向的备份不是普通文件: {}",
            raw_source.display()
        ));
    }
    let source =
        std::fs::canonicalize(&raw_source).map_err(|e| format!("解析恢复路径失败: {e}"))?;
    let backup_dir = std::fs::canonicalize(data_dir.join(BACKUP_DIR))
        .map_err(|e| format!("解析备份目录失败: {e}"))?;
    if source.parent() != Some(backup_dir.as_path()) {
        return Err(format!(
            "恢复标记指向的备份不是普通文件: {}",
            source.display()
        ));
    }
    let file_name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    restore_managed_database(database_url, &source, data_dir).await?;
    std::fs::remove_file(&marker).map_err(|e| format!("删除恢复标记失败: {e}"))?;
    Ok(Some(file_name))
}

#[derive(Debug, PartialEq, Eq)]
pub enum BackupOutcome {
    NotDue,
    Created(PathBuf),
    Pruned(usize),
    CreatedAndPruned(PathBuf, usize),
}

pub fn parse_backup_timestamp(name: &str) -> Option<chrono::NaiveDateTime> {
    let name = name
        .strip_prefix(AUTO_BACKUP_PREFIX)?
        .strip_suffix(BACKUP_SUFFIX)?;
    let ts = name.get(..15)?;
    chrono::NaiveDateTime::parse_from_str(ts, TIMESTAMP_FORMAT).ok()
}

pub fn is_backup_due(
    latest: Option<chrono::NaiveDateTime>,
    now: chrono::NaiveDateTime,
    every: Duration,
) -> bool {
    match latest {
        None => true,
        Some(latest) => {
            now.signed_duration_since(latest)
                > chrono::Duration::from_std(every).unwrap_or_else(|_| chrono::Duration::days(1))
        }
    }
}

pub fn prune_candidates(
    names: &[String],
    now: chrono::NaiveDateTime,
    retention_days: u64,
) -> Vec<String> {
    let days = i64::try_from(retention_days).unwrap_or(i64::MAX);
    let cutoff = now - chrono::Duration::days(days);
    names
        .iter()
        .filter(|name| parse_backup_timestamp(name).is_some_and(|ts| ts < cutoff))
        .cloned()
        .collect()
}

pub fn list_auto_backups(data_dir: &Path) -> Vec<String> {
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data_dir.join(BACKUP_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(AUTO_BACKUP_PREFIX) && name.ends_with(BACKUP_SUFFIX) {
                backups.push(name);
            }
        }
    }
    backups.sort();
    backups
}

pub async fn run_backup_cycle(
    database_url: &str,
    data_dir: &Path,
    retention_days: u64,
) -> anyhow::Result<BackupOutcome> {
    let backup_dir = data_dir.join(BACKUP_DIR);
    tokio::fs::create_dir_all(&backup_dir).await?;
    restrict_permissions(&backup_dir, 0o700);
    let names = list_auto_backups(data_dir);
    let latest = names
        .iter()
        .filter_map(|name| parse_backup_timestamp(name))
        .max();
    let now = chrono::Local::now().naive_local();

    let mut created = None;
    if is_backup_due(latest, now, BACKUP_EVERY) {
        let ts = chrono::Local::now().format(TIMESTAMP_FORMAT).to_string();
        let unique_id = uuid::Uuid::new_v4().simple().to_string();
        let target = backup_dir.join(format!(
            "{AUTO_BACKUP_PREFIX}{ts}_{unique_id}{BACKUP_SUFFIX}"
        ));
        dump_database(database_url, &target)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(path = %target.display(), "🗄️ 自动备份完成");
        created = Some(target);
    }

    let stale = prune_candidates(&names, now, retention_days);
    let mut pruned = 0;
    for name in stale {
        let path = backup_dir.join(&name);
        match pending_restore_source(data_dir) {
            Ok(Some(source)) if std::fs::canonicalize(&path).ok().as_ref() == Some(&source) => {
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "待恢复任务状态不可读，保留全部剩余备份");
                break;
            }
            _ => {}
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                pruned += 1;
                tracing::info!(path = %path.display(), "🗑️ 清理过期自动备份");
            }
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "清理过期备份失败"),
        }
    }

    Ok(match (created, pruned > 0) {
        (Some(path), true) => BackupOutcome::CreatedAndPruned(path, pruned),
        (Some(path), false) => BackupOutcome::Created(path),
        (None, true) => BackupOutcome::Pruned(pruned),
        (None, false) => BackupOutcome::NotDue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .and_then(|date| date.and_hms_opt(h, min, s))
            .unwrap()
    }

    #[test]
    fn parses_backup_filenames() {
        assert_eq!(
            parse_backup_timestamp("nuono_auto_backup_20260812_030000_abc123.dump"),
            Some(dt(2026, 8, 12, 3, 0, 0))
        );
        assert!(parse_backup_timestamp("nuono_backup_20260812_030000_abc123.db").is_none());
        assert!(parse_backup_timestamp("other.dump").is_none());
        assert!(validate_backup_filename(
            "nuono_manual_backup_20260815_120000_abc.dump"
        ));
        assert!(!validate_backup_filename("../nuono.dump"));
        assert!(!validate_backup_filename("a/b.dump"));
        assert!(!validate_backup_filename("nuono_backup_1.db"));
    }

    #[test]
    fn due_check_and_pruning_are_time_based() {
        let now = dt(2026, 8, 13, 4, 0, 0);
        assert!(is_backup_due(None, now, BACKUP_EVERY));
        assert!(!is_backup_due(
            Some(dt(2026, 8, 13, 3, 0, 0)),
            now,
            BACKUP_EVERY
        ));
        let names = vec![
            "nuono_auto_backup_20260801_030000_aaa.dump".to_string(),
            "nuono_auto_backup_20260811_030000_bbb.dump".to_string(),
        ];
        assert_eq!(
            prune_candidates(&names, dt(2026, 8, 13, 0, 0, 0), 7),
            vec![names[0].clone()]
        );
    }

    #[tokio::test]
    async fn automatic_pruning_preserves_pending_restore_source() {
        let root =
            std::env::temp_dir().join(format!("nuono-backup-prune-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let now = chrono::Local::now().naive_local();
        // 写入一份新鲜的备份文件，使 `is_backup_due` 判定尚未到期，从而在不连真实数据库的情况下测试过期清理逻辑。
        let recent = root.join(BACKUP_DIR).join(format!(
            "{AUTO_BACKUP_PREFIX}{}_recent.dump",
            now.format(TIMESTAMP_FORMAT)
        ));
        let stale = root.join(BACKUP_DIR).join(format!(
            "{AUTO_BACKUP_PREFIX}{}_stale.dump",
            (now - chrono::Duration::days(30)).format(TIMESTAMP_FORMAT)
        ));
        std::fs::write(&recent, PG_DUMP_MAGIC).unwrap();
        std::fs::write(&stale, PG_DUMP_MAGIC).unwrap();
        write_marker(&root, &stale);
        assert_eq!(
            run_backup_cycle("postgres://unused", &root, 7)
                .await
                .unwrap(),
            BackupOutcome::NotDue
        );
        assert!(stale.is_file());
        std::fs::write(root.join(RESTORE_MARKER), "broken marker").unwrap();
        assert_eq!(
            run_backup_cycle("postgres://unused", &root, 7)
                .await
                .unwrap(),
            BackupOutcome::NotDue
        );
        assert!(stale.is_file());
        std::fs::remove_file(root.join(RESTORE_MARKER)).unwrap();
        assert_eq!(
            run_backup_cycle("postgres://unused", &root, 7)
                .await
                .unwrap(),
            BackupOutcome::Pruned(1)
        );
        assert!(!stale.exists());
        assert!(recent.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_matches_core_path_helper() {
        assert!(matches!(
            classify("sqlite::memory:"),
            DatabaseKind::SqliteMemory
        ));
        assert!(matches!(
            classify("sqlite://data/nuono.db?mode=rwc"),
            DatabaseKind::SqliteFile(_)
        ));
        assert!(matches!(
            classify("sqlite:///tmp/nuono.db?mode=rwc"),
            DatabaseKind::SqliteFile(_)
        ));
        assert!(matches!(
            classify("postgres://nuono:nuono@127.0.0.1/nuono"),
            DatabaseKind::Postgres
        ));
    }

    #[test]
    fn detects_backup_magic() {
        let root = std::env::temp_dir().join(format!("nuono-magic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pg = root.join("pg.dump");
        std::fs::write(&pg, b"PGDMP\x01\x02rest-of-archive").unwrap();
        assert_eq!(
            detect_backup_format(&pg).unwrap(),
            BackupFormat::PostgresCustom
        );
        let sqlite = root.join("s.dump");
        std::fs::write(&sqlite, SQLITE_MAGIC).unwrap();
        assert_eq!(detect_backup_format(&sqlite).unwrap(), BackupFormat::Sqlite);
        let junk = root.join("junk.dump");
        std::fs::write(&junk, b"not-a-database-at-all").unwrap();
        assert!(detect_backup_format(&junk).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// 建一个带一张表一行数据的临时 SQLite 库，返回其 `sqlite://` URL。
    async fn seeded_sqlite_db(dir: &Path, name: &str) -> String {
        let url = format!("sqlite://{}?mode=rwc", dir.join(name).to_string_lossy());
        let framework =
            nuono_core::db::Database::from_config(&nuono_core::config::DatabaseConfig {
                url: url.clone(),
                max_connections: 1,
            })
            .await
            .unwrap();
        framework.shutdown().await.unwrap();
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS probe (id INTEGER PRIMARY KEY, note TEXT)",
        )
        .await
        .unwrap();
        pool.execute_unprepared("INSERT INTO probe (note) VALUES ('before-backup')")
            .await
            .unwrap();
        pool.close().await.unwrap();
        url
    }

    async fn count_probe(url: &str, note: &str) -> i64 {
        let pool = Database::connect(url).await.unwrap();
        let n: i64 = query_scalar(
            &pool,
            Statement::from_sql_and_values(
                pool.get_database_backend(),
                "SELECT COUNT(*) FROM probe WHERE note = ?1",
                [note.into()],
            ),
        )
        .await
        .unwrap();
        pool.close().await.unwrap();
        n
    }

    #[tokio::test]
    async fn snapshot_connection_preserves_sqlite_options_and_literal_file_path() {
        let root = tempfile::tempdir().unwrap();
        seeded_sqlite_db(root.path(), "seed.db").await;
        #[cfg(unix)]
        let path = root.path().join("snapshot ?mode=memory#%.db");
        #[cfg(not(unix))]
        let path = root.path().join("snapshot.db");
        std::fs::rename(root.path().join("seed.db"), &path).unwrap();

        let db = open_sqlite_snapshot(
            &path,
            true,
            Duration::from_secs(30),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        for (sql, expected) in [
            ("SELECT COUNT(*) FROM probe", 1_i64),
            ("PRAGMA foreign_keys", 0),
            ("PRAGMA busy_timeout", 30_000),
        ] {
            assert_eq!(
                query_scalar::<i64>(&db, Statement::from_string(DbBackend::Sqlite, sql))
                    .await
                    .unwrap(),
                expected,
            );
        }
        assert!(db.execute_unprepared("DELETE FROM probe").await.is_err());
        db.close().await.unwrap();

        let db = open_sqlite_snapshot(&path, false, Duration::ZERO, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            query_scalar::<i64>(
                &db,
                Statement::from_string(DbBackend::Sqlite, "PRAGMA busy_timeout"),
            )
            .await
            .unwrap(),
            0,
        );
        db.close().await.unwrap();
        assert!(
            open_sqlite_snapshot(
                &root.path().join("missing.db"),
                false,
                Duration::ZERO,
                Duration::from_secs(5),
            )
            .await
            .is_err()
        );
        assert!(!root.path().join("missing.db").exists());
    }

    fn write_marker(data_dir: &Path, source: &Path) {
        std::fs::write(
            data_dir.join(RESTORE_MARKER),
            serde_json::json!({ "source": source, "actor": "test" }).to_string(),
        )
        .unwrap();
    }

    fn leftover_swap_files(data_dir: &Path) -> Vec<String> {
        std::fs::read_dir(data_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            // .nuono-lock 是 core 锁 helper 的 sidecar 文件，不属于换库残留。
            .filter(|n| n.starts_with("nuono_restore_") && !n.ends_with(".nuono-lock"))
            .collect()
    }

    #[tokio::test]
    async fn sqlite_dump_creates_consistent_snapshot_and_restore_roundtrips() {
        let root = std::env::temp_dir().join(format!("nuono-sqlite-rt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let url = seeded_sqlite_db(&root, "nuono.db").await;

        let dump = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000000_rt.dump");
        dump_database(&url, &dump).await.unwrap();
        assert_eq!(detect_backup_format(&dump).unwrap(), BackupFormat::Sqlite);
        validate_sqlite_file(&dump).await.unwrap();

        // 快照之后活库继续演进（模拟运行期漂移）。
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared("INSERT INTO probe (note) VALUES ('after-backup')")
            .await
            .unwrap();
        pool.close().await.unwrap();

        write_marker(&root, &dump);
        let restored = apply_pending_restore(&url, &root).await.unwrap();
        assert_eq!(
            restored.as_deref(),
            Some(dump.file_name().unwrap().to_str().unwrap())
        );
        assert!(!root.join(RESTORE_MARKER).exists());
        assert_eq!(count_probe(&url, "before-backup").await, 1);
        assert_eq!(
            count_probe(&url, "after-backup").await,
            0,
            "恢复后不应残留快照之后的写入"
        );
        assert!(
            leftover_swap_files(&root).is_empty(),
            "不应残留临时/保留文件"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_restore_preserves_symlink_and_replaces_real_target() {
        let root =
            std::env::temp_dir().join(format!("nuono-sqlite-symlink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let url = seeded_sqlite_db(&root, "real.db").await;
        let link = root.join("alias.db");
        std::os::unix::fs::symlink(root.join("real.db"), &link).unwrap();
        let alias_url = format!("sqlite://{}?mode=rwc", link.display());
        let dump = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000000_alias.dump");
        dump_database(&alias_url, &dump).await.unwrap();
        let pool = Database::connect(&url).await.unwrap();
        pool.execute_unprepared("INSERT INTO probe (note) VALUES ('after-backup')")
            .await
            .unwrap();
        pool.close().await.unwrap();
        restore_database(&alias_url, &dump).await.unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(count_probe(&url, "before-backup").await, 1);
        assert_eq!(count_probe(&url, "after-backup").await, 0);
        assert_eq!(count_probe(&alias_url, "after-backup").await, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn sqlite_restore_of_corrupt_backup_keeps_target_and_marker() {
        let root = std::env::temp_dir().join(format!("nuono-sqlite-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let url = seeded_sqlite_db(&root, "nuono.db").await;

        // 带合法 SQLite magic 但内容损坏的备份。
        let bad = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000000_bad.dump");
        let mut bytes = SQLITE_MAGIC.to_vec();
        bytes.extend_from_slice(b"garbage-not-a-real-database");
        std::fs::write(&bad, &bytes).unwrap();

        write_marker(&root, &bad);
        assert!(apply_pending_restore(&url, &root).await.is_err());
        assert!(
            root.join(RESTORE_MARKER).exists(),
            "失败时标记保留，下次启动重试"
        );
        assert_eq!(
            count_probe(&url, "before-backup").await,
            1,
            "目标数据库未受影响"
        );
        assert!(leftover_swap_files(&root).is_empty(), "不应残留临时副本");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restore_is_blocked_while_nuono_instance_holds_shared_lock() {
        let root = std::env::temp_dir().join(format!("nuono-sqlite-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let url = seeded_sqlite_db(&root, "nuono.db").await;
        let target_path = nuono_core::db::sqlite_database_path(&url).unwrap().unwrap();

        let dump = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000000_lock.dump");
        dump_database(&url, &dump).await.unwrap();

        // 模拟另一个 Nuono 实例打开库（共享 sidecar 锁在持有中）。
        let live = nuono_core::db::SqliteDatabaseLock::acquire_shared(&target_path).unwrap();
        write_marker(&root, &dump);
        let err = apply_pending_restore(&url, &root).await.unwrap_err();
        assert!(err.contains("仍被 Nuono 实例使用"), "拒绝信息: {err}");
        assert!(root.join(RESTORE_MARKER).exists(), "失败时标记保留");
        assert_eq!(
            count_probe(&url, "before-backup").await,
            1,
            "目标数据库未受影响"
        );

        drop(live);
        // 锁释放后同一标记可以完成恢复。
        apply_pending_restore(&url, &root).await.unwrap();
        assert_eq!(count_probe(&url, "before-backup").await, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cross_backend_restore_is_rejected() {
        let root = std::env::temp_dir().join(format!("nuono-cross-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        let url = seeded_sqlite_db(&root, "nuono.db").await;
        let sqlite_dump = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000000_sqlite.dump");
        dump_database(&url, &sqlite_dump).await.unwrap();

        let err = restore_database("postgres://nuono:nuono@127.0.0.1:5432/nuono", &sqlite_dump)
            .await
            .unwrap_err();
        assert!(err.contains("不匹配"), "跨后端拒绝信息: {err}");

        let pg_dump = root
            .join(BACKUP_DIR)
            .join("nuono_manual_backup_20260930_000001_pg.dump");
        std::fs::write(&pg_dump, b"PGDMPfake-archive").unwrap();
        let err = restore_database(&url, &pg_dump).await.unwrap_err();
        assert!(err.contains("不匹配"), "跨后端拒绝信息: {err}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// 真实 pg_dump / pg_restore 回路（隔离库）。需要可连接的 PostgreSQL
    /// （`NUONO_TEST_DATABASE_URL` 或默认本地）与 pg 工具。仅在显式选择 SQLite
    /// 后端（`NUONO_TEST_DATABASE_BACKEND=sqlite`）时跳过；PG 后端下工具或连接
    /// 缺失一律断言失败，不允许静默假绿。
    #[tokio::test]
    async fn pg_dump_and_restore_roundtrip_on_isolated_database() {
        if std::env::var("NUONO_TEST_DATABASE_BACKEND")
            .map(|v| v.eq_ignore_ascii_case("sqlite"))
            .unwrap_or(false)
        {
            eprintln!("SQLite 后端选定，跳过 PG 专属备份回路");
            return;
        }
        let admin_url = std::env::var("NUONO_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://nuono:nuono@127.0.0.1:5432/postgres".to_string());
        assert!(
            resolve_pg_tool("pg_dump").is_some() && resolve_pg_tool("pg_restore").is_some(),
            "PG 后端测试要求 pg_dump / pg_restore 可用（PATH 或常见安装位置）"
        );
        let admin = Database::connect(&admin_url).await.unwrap_or_else(|e| {
            panic!("PG 后端测试要求可连接的测试 PostgreSQL ({admin_url}): {e}")
        });
        let name = format!("nuono_pg_backup_test_{}", uuid::Uuid::new_v4().simple());
        // 库名由固定前缀和 UUID 十六进制组成，不含用户输入；建库不放入事务。
        admin
            .execute_unprepared(&format!("CREATE DATABASE \"{name}\""))
            .await
            .unwrap();
        admin.close().await.unwrap();

        let mut url = url::Url::parse(&admin_url).unwrap();
        url.set_path(&format!("/{name}"));
        let url = url.to_string();

        async move {
            let pool = Database::connect(&url).await.unwrap();
            pool.execute_unprepared("CREATE TABLE probe (id int PRIMARY KEY, note text)")
                .await
                .unwrap();
            pool.execute_unprepared("INSERT INTO probe VALUES (1, 'before-backup')")
                .await
                .unwrap();
            pool.close().await.unwrap();

            let root = std::env::temp_dir().join(format!("nuono-pg-rt-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
            let dump = root
                .join(BACKUP_DIR)
                .join("nuono_manual_backup_20260930_000000_pg.dump");
            dump_database(&url, &dump).await.unwrap();
            assert_eq!(
                detect_backup_format(&dump).unwrap(),
                BackupFormat::PostgresCustom
            );

            // 漂移：清空表。
            let pool = Database::connect(&url).await.unwrap();
            pool.execute_unprepared("TRUNCATE probe").await.unwrap();
            pool.close().await.unwrap();

            restore_database(&url, &dump).await.unwrap();
            let pool = Database::connect(&url).await.unwrap();
            let n: i64 = query_scalar(
                &pool,
                Statement::from_string(
                    pool.get_database_backend(),
                    "SELECT COUNT(*) FROM probe WHERE note = 'before-backup'",
                ),
            )
            .await
            .unwrap();
            pool.close().await.unwrap();
            assert_eq!(n, 1, "pg_restore 后数据应回到备份时点");

            // PGDMP 归档在 SQLite 后端被拒（跨后端）。
            let sqlite_root =
                std::env::temp_dir().join(format!("nuono-pg-x-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&sqlite_root).unwrap();
            let sqlite_url = seeded_sqlite_db(&sqlite_root, "s.db").await;
            assert!(
                restore_database(&sqlite_url, &dump)
                    .await
                    .unwrap_err()
                    .contains("不匹配")
            );
            let _ = std::fs::remove_dir_all(&sqlite_root);
            let _ = std::fs::remove_dir_all(&root);
        }
        .await;

        // 清理隔离库（断言失败 panic 时只能容忍泄漏，容器环境一次性）。
        if let Ok(admin) = Database::connect(&admin_url).await {
            let _ = admin
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid()",
                    [name.clone().into()],
                ))
                .await;
            let _ = admin
                .execute_unprepared(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                .await;
            admin.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_backup_waiter_keeps_partial_file_private_and_lock_owned() {
        let root = tempfile::tempdir().unwrap();
        let destination = root
            .path()
            .join("nuono_manual_backup_20261015_000000_cancel.dump");
        let final_path = destination.clone();
        let operations = crate::database_operations::DatabaseOperations::new();
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let task_lock = lock.clone();
        let task_operations = operations.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let request = tokio::spawn(async move {
            task_operations
                .run(task_lock, async move {
                    publish_dump(&final_path, |stage| async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::write(&stage, b"PGDMPpartial").unwrap();
                            started.send(()).unwrap();
                            blocked.recv().unwrap();
                            std::fs::write(stage, b"PGDMPcomplete").unwrap();
                            Ok(())
                        })
                        .await
                        .unwrap()
                    })
                    .await
                })
                .await
        });
        ready.await.unwrap();
        assert!(
            !destination.exists(),
            "partial files must not use the published name"
        );
        assert!(list_auto_backups(root.path()).is_empty());
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(lock.try_lock().is_err());
        operations.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), operations.wait())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        operations.wait().await;
        assert_eq!(std::fs::read(&destination).unwrap(), b"PGDMPcomplete");
        assert!(lock.try_lock().is_ok());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failed_backup_does_not_publish_and_cleans_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("failed.dump");
        let result = publish_dump(&destination, |stage| async move {
            std::fs::write(stage, b"PGDMPpartial").unwrap();
            Err("injected disk failure".into())
        })
        .await;
        assert!(result.unwrap_err().contains("injected disk failure"));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn backup_publication_preserves_an_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("existing.dump");
        std::fs::write(&destination, b"original backup").unwrap();
        assert!(
            publish_dump(&destination, |stage| async move {
                std::fs::write(stage, b"new backup").map_err(|error| error.to_string())
            })
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"original backup");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn pending_restore_keeps_marker_when_source_missing() {
        let root =
            std::env::temp_dir().join(format!("nuono-restore-miss-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        write_marker(&root, &root.join("missing.dump"));
        assert!(
            apply_pending_restore("postgres://invalid", &root)
                .await
                .is_err()
        );
        assert!(root.join(RESTORE_MARKER).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
#[path = "../tests/unit/backup/privacy.rs"]
mod privacy_tests;
