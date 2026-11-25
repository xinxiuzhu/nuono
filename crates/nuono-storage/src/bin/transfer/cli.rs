//! 离线跨数据库可移植备份与部署迁移 CLI 实现；执行过程中不初始化宿主运行时，也不触发插件回调。

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use nuono_storage::{
    db::SqliteDatabaseLock,
    portable::{self, Schema, cutover, external, files, purge},
};
use sea_orm::{ConnectOptions, ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAINTENANCE_LOCK: i64 = 0x4e55_4f4e_4f4d_4149;
const JOURNAL: &str = ".portable-transfer-journal.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportJournal {
    package_hash: String,
    target: String,
    phase: String,
}

struct OfflineDatabase {
    connection: DatabaseConnection,
    _sqlite_lock: Option<SqliteDatabaseLock>,
}

impl OfflineDatabase {
    async fn open(url: &str) -> Result<Self> {
        if !url.starts_with("sqlite:")
            && !url.starts_with("postgres:")
            && !url.starts_with("postgresql:")
        {
            return Err("only SQLite/PostgreSQL URLs are accepted".into());
        }
        let sqlite = files::sqlite_file(url)?;
        if url.starts_with("sqlite:") && sqlite.is_none() {
            return Err("maintenance requires an on-disk SQLite database".into());
        }
        let lock = sqlite
            .as_ref()
            .map(|p| SqliteDatabaseLock::acquire_exclusive(p))
            .transpose()?;
        let mut options = ConnectOptions::new(url);
        options
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .sqlx_logging(false);
        let connection = sea_orm::Database::connect(options).await?;
        if connection.get_database_backend() == DbBackend::Sqlite {
            connection
                .execute_unprepared("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
                .await?;
            let enabled = connection
                .query_one_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    "PRAGMA foreign_keys",
                ))
                .await?
                .ok_or("missing foreign key setting")?
                .try_get::<i64>("", "foreign_keys")?;
            if enabled != 1 {
                return Err("SQLite foreign keys are disabled".into());
            }
        } else {
            connection
                .execute_unprepared(
                    "SET TIME ZONE 'UTC'; SET search_path TO public; SET lock_timeout='5s';",
                )
                .await?;
            let encoding = connection
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SHOW server_encoding",
                ))
                .await?
                .ok_or("missing database encoding")?
                .try_get::<String>("", "server_encoding")?;
            if encoding != "UTF8" {
                return Err("PostgreSQL must use UTF8".into());
            }
            let acquired = connection
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT pg_try_advisory_lock($1) AS acquired",
                    [MAINTENANCE_LOCK.into()],
                ))
                .await?
                .ok_or("missing maintenance lease")?
                .try_get::<bool>("", "acquired")?;
            if !acquired {
                return Err(
                    "a Nuono host or maintenance operation still owns this PostgreSQL database"
                        .into(),
                );
            }
        }
        Ok(Self {
            connection,
            _sqlite_lock: lock,
        })
    }
    async fn close(self) -> Result<()> {
        self.connection.close().await?;
        Ok(())
    }
}

fn lease(data: &Path) -> Result<File> {
    fs::create_dir_all(data)?;
    if fs::symlink_metadata(data)?.file_type().is_symlink() {
        return Err("data directory must not be a symlink".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join(".nuono-service.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .map_err(|_| "stop the Nuono host before maintenance")?;
    Ok(file)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    if fs::metadata(path)?.len() > 4 * 1024 * 1024 {
        return Err("operation document exceeds 4 MiB".into());
    }
    Ok(portable::parse_document(&fs::read(path)?)?)
}

fn persist<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("operation document exceeds 4 MiB".into());
    }
    let next = path.with_extension("next");
    if next.exists() {
        fs::remove_file(&next)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&next)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(next, path)?;
    File::open(path.parent().ok_or("journal has no parent")?)?.sync_all()?;
    Ok(())
}

fn database_identity(url: &str) -> Result<String> {
    let value = if let Some(path) = files::sqlite_file(url)? {
        format!("sqlite:{}", path.display())
    } else {
        let parsed = url::Url::parse(url)?;
        format!(
            "postgres:{}:{}:{}",
            parsed.host_str().unwrap_or(""),
            parsed.port().unwrap_or(5432),
            parsed.path()
        )
    };
    Ok(hex::encode(Sha256::digest(value.as_bytes())))
}

pub async fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(mode) = args.first().map(String::as_str) else {
        help();
        return Ok(());
    };
    if matches!(mode, "--help" | "-h") {
        help();
        return Ok(());
    }
    if mode == "plugin-versions" {
        if args.len() != 3 {
            return Err("plugin-versions <data> <output.json>".into());
        }
        let _lease = lease(Path::new(&args[1]))?;
        let database = OfflineDatabase::open(&std::env::var("NUONO_TRANSFER_SOURCE_URL")?).await?;
        let schema = portable::registered_schema(&database.connection).await?;
        let image = portable::export(&database.connection, &schema).await?;
        let output = Path::new(&args[2]);
        if output.exists() {
            return Err("inventory output already exists".into());
        }
        persist(output, &portable::installed_plugin_versions(&image)?)?;
        database.close().await?;
        println!(
            "Exported source deployment declaration; this does not attest a target executable."
        );
        return Ok(());
    }
    if matches!(mode, "purge-plan" | "purge" | "purge-recover") {
        return purge_command(mode, &args[1..]).await;
    }
    if matches!(mode, "publish" | "recover" | "rollback") {
        if args.len() != 2 {
            return Err("publish takes a plan JSON; recover/rollback take its journal JSON".into());
        }
        return publication(mode, Path::new(&args[1])).await;
    }
    if !(3..=4).contains(&args.len())
        || !matches!(mode, "export" | "import" | "resume-import" | "verify")
    {
        return Err("invalid arguments; use --help".into());
    }
    let package = PathBuf::from(&args[1]);
    let data = PathBuf::from(&args[2]);
    if mode == "export" && !data.is_dir() {
        return Err("source data directory does not exist".into());
    }
    if cutover::startup_guard(&data).exists() {
        return Err("deployment cutover is pending; recover its journal first".into());
    }
    let _lease = lease(&data)?;
    let environment = if mode == "export" {
        "NUONO_TRANSFER_SOURCE_URL"
    } else {
        "NUONO_TRANSFER_TARGET_URL"
    };
    let url = std::env::var(environment).map_err(|_| format!("missing {environment}"))?;
    let sqlite = files::sqlite_file(&url)?;
    if mode == "export" && sqlite.as_ref().is_some_and(|p| !p.is_file()) {
        return Err("source database does not exist".into());
    }
    let database = OfflineDatabase::open(&url).await?;
    if mode == "export" {
        if current_fence(&database).await?.is_some() {
            return Err("database has incomplete maintenance; recover before exporting".into());
        }
        if data.join(files::PENDING).exists() || data.join(external::PENDING).exists() {
            return Err("source has incomplete maintenance; recover before exporting".into());
        }
        let schema = portable::registered_schema(&database.connection).await?;
        let image = portable::export(&database.connection, &schema).await?;
        files::write_package(&package, &data, sqlite.as_deref(), &image)?;
        println!(
            "Export verified: {} tables, retained plugin manifests and persistent files. Keep the source stopped.",
            image.tables.len()
        );
    } else {
        let (manifest, image) = files::read_package(&package)?;
        let schema = Schema::from_image(&image)?;
        let targets = match args.get(3) {
            Some(path) => read_json::<external::ExternalTargets>(Path::new(path))?,
            None => Default::default(),
        };
        if mode == "verify" {
            portable::verify(&database.connection, &schema, &image).await?;
            files::verify_data(&data, &manifest, sqlite.as_deref())?;
            external::verify_published(&data, &manifest.database_sha256, &manifest.external)?;
            files::finish_assets(&data, &manifest, sqlite.as_deref())?;
            println!(
                "Database, identities, schema constraints, sequences and every persistent file verified."
            );
        } else {
            let declared = match std::env::var("NUONO_TRANSFER_TARGET_PLUGINS") {
                Ok(path) => read_json::<portable::PluginVersions>(Path::new(&path))?,
                Err(_) => Default::default(),
            };
            if portable::installed_plugin_versions(&image)? != declared {
                return Err("declare matching target code versions in NUONO_TRANSFER_TARGET_PLUGINS before import".into());
            }
            let journal_path = data.join(JOURNAL);
            let identity = database_identity(&url)?;
            let resuming = mode == "resume-import";
            if resuming {
                let journal: ImportJournal = read_json(&journal_path)?;
                if journal.package_hash != manifest.database_sha256 || journal.target != identity {
                    return Err(
                        "resume package or target database does not match the durable journal"
                            .into(),
                    );
                }
                if journal.phase == "complete" {
                    portable::verify(&database.connection, &schema, &image).await?;
                    files::verify_data(&data, &manifest, sqlite.as_deref())?;
                    external::verify_completed_targets(
                        &data,
                        &manifest.database_sha256,
                        &manifest.external,
                        &targets,
                    )?;
                    files::finish_assets(&data, &manifest, sqlite.as_deref())?;
                    database.close().await?;
                    println!(
                        "Import already completed and verified; its durable receipt was retained."
                    );
                    return Ok(());
                }
            } else {
                if journal_path.exists() || data.join(files::PENDING).exists() {
                    return Err(
                        "an import already exists; use resume-import with its original package"
                            .into(),
                    );
                }
                portable::preflight_import(&database.connection, &schema, &image).await?;
            }
            external::preflight(&data, &manifest.external, &targets)?;
            let required = manifest
                .assets
                .iter()
                .chain(manifest.external.iter().flat_map(|a| a.files.iter()))
                .try_fold(0_u64, |sum, f| sum.checked_add(f.size))
                .ok_or("asset size overflow")?;
            if fs2::available_space(&data)? < required.saturating_add(16 * 1024 * 1024) {
                return Err("insufficient target filesystem space for staged assets".into());
            }
            persist(
                &journal_path,
                &ImportJournal {
                    package_hash: manifest.database_sha256.clone(),
                    target: identity.clone(),
                    phase: "staging".into(),
                },
            )?;
            files::stage_assets(&package, &data, &manifest, sqlite.as_deref())?;
            portable::stage_target_plugin_versions(&data, &image, &declared)?;
            external::stage(
                &package.join("external"),
                &data,
                &manifest.database_sha256,
                &manifest.external,
                &targets,
            )?;
            if portable::preflight_import(&database.connection, &schema, &image)
                .await
                .is_ok()
            {
                portable::import(&database.connection, &schema, &image).await?;
            } else {
                portable::verify(&database.connection, &schema, &image).await?;
            }
            persist(
                &journal_path,
                &ImportJournal {
                    package_hash: manifest.database_sha256.clone(),
                    target: identity.clone(),
                    phase: "database_committed".into(),
                },
            )?;
            files::verify_data(&data, &manifest, sqlite.as_deref())?;
            external::publish(&data, &manifest.database_sha256)?;
            external::verify_published(&data, &manifest.database_sha256, &manifest.external)?;
            persist(
                &journal_path,
                &ImportJournal {
                    package_hash: manifest.database_sha256.clone(),
                    target: identity,
                    phase: "complete".into(),
                },
            )?;
            files::finish_assets(&data, &manifest, sqlite.as_deref())?;
            println!(
                "Import verified. Source is retained. Use publish with an explicit cutover plan to change the active deployment."
            );
        }
    }
    database.close().await
}

fn help() {
    println!(
        "nuono-database-transfer <export|import|resume-import|verify> <package> <data> [external-targets.json]\nnuono-database-transfer publish <cutover-plan.json>\nnuono-database-transfer <recover|rollback> <cutover-journal.json>\nnuono-database-transfer plugin-versions <data> <output.json>\nnuono-database-transfer purge-plan <plugin-id> <data> <plan.json>\nnuono-database-transfer purge <plan.json> <data>\nnuono-database-transfer purge-recover <data>\nTarget import inventory: NUONO_TRANSFER_TARGET_PLUGINS.\nStop all source/target hosts and external writers. Connections use NUONO_TRANSFER_SOURCE_URL and NUONO_TRANSFER_TARGET_URL. Retained plugin declarations are loaded automatically; no plugin lifecycle is executed."
    );
}

async fn purge_command(mode: &str, args: &[String]) -> Result<()> {
    let (data,plan_path,plugin)=match (mode,args) {
        ("purge-plan",[plugin,data,plan])=>(PathBuf::from(data),Some(PathBuf::from(plan)),Some(plugin.as_str())),
        ("purge",[plan,data])=>(PathBuf::from(data),Some(PathBuf::from(plan)),None),
        ("purge-recover",[data])=>(PathBuf::from(data),None,None),
        _=>return Err("purge-plan <plugin-id> <data> <plan.json>; purge <plan.json> <data>; purge-recover <data>".into()),
    };
    if cutover::startup_guard(&data).exists() || data.join(files::PENDING).exists() {
        return Err("deployment transfer must be recovered before plugin clearing".into());
    }
    let _lease = lease(&data)?;
    let database = OfflineDatabase::open(&std::env::var("NUONO_TRANSFER_SOURCE_URL")?).await?;
    if mode == "purge-plan" {
        let plan = purge::plan(
            &database.connection,
            plugin.ok_or("missing plugin identity")?,
            &data,
        )
        .await?;
        let path = plan_path.ok_or("missing plan path")?;
        if path.exists() {
            return Err("plan path already exists".into());
        }
        persist(&path, &plan)?;
        println!(
            "Review {} owned tables, {} asset roots and {} affected authorizations; {} foreign-key dependencies must be resolved before purge. Plan: {}",
            plan.tables.len(),
            plan.files.len(),
            plan.affected_authorizations,
            plan.dependencies.len(),
            path.display()
        );
    } else if mode == "purge" {
        let plan: purge::PurgePlan = read_json(&plan_path.ok_or("missing plan path")?)?;
        purge::apply(&database.connection, &data, &plan).await?;
        println!(
            "Plugin business data, files and applied migration records cleared; plugin identity and revoked authorization history retained."
        );
    } else {
        purge::recover(&database.connection, &data).await?;
        println!("Plugin data clearing recovery completed.");
    }
    database.close().await
}

fn prepared_url(path: &Path) -> Result<String> {
    if fs::metadata(path)?.len() > 4 * 1024 * 1024 {
        return Err("prepared configuration is too large".into());
    }
    let config: toml::Value = toml::from_str(&fs::read_to_string(path)?)
        .map_err(|_| "prepared configuration is invalid TOML")?;
    Ok(config
        .get("database")
        .and_then(|v| v.get("url"))
        .and_then(toml::Value::as_str)
        .ok_or("prepared configuration must explicitly specify database.url")?
        .into())
}

fn remap_sqlite(url: &str, from: &Path, to: &Path) -> Result<String> {
    let Some(path) = files::sqlite_file(url)? else {
        return Ok(url.into());
    };
    let relative = path
        .strip_prefix(from)
        .map_err(|_| "SQLite database must be inside its declared data root")?;
    Ok(format!("sqlite://{}?mode=rw", to.join(relative).display()))
}

async fn current_fence(database: &OfflineDatabase) -> Result<Option<String>> {
    Ok(database
        .connection
        .query_one_raw(Statement::from_string(
            database.connection.get_database_backend(),
            "SELECT maintenance_operation FROM nuono_deployment WHERE id=1",
        ))
        .await?
        .ok_or("missing deployment identity")?
        .try_get("", "maintenance_operation")?)
}

async fn fence(database: &OfflineDatabase, operation: &str) -> Result<()> {
    match current_fence(database).await? {
        None => portable::set_maintenance(&database.connection, None, Some(operation)).await?,
        Some(existing) if existing == operation => {}
        Some(_) => return Err("database belongs to another maintenance operation".into()),
    }
    Ok(())
}

async fn clear_fence(database: &OfflineDatabase, operation: &str) -> Result<()> {
    match current_fence(database).await? {
        None => {}
        Some(existing) if existing == operation => {
            portable::set_maintenance(&database.connection, Some(operation), None).await?
        }
        Some(_) => return Err("database belongs to another maintenance operation".into()),
    }
    Ok(())
}

async fn publication(mode: &str, path: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    let (journal_path, mut journal) = if mode == "publish" {
        let plan: cutover::CutoverPlan = read_json(&path)?;
        let _source = lease(&plan.active_data)?;
        let _target = lease(&plan.staged_data)?;
        let (manifest, image) = files::read_package(&plan.package)?;
        let schema = Schema::from_image(&image)?;
        let source_url = std::env::var("NUONO_TRANSFER_SOURCE_URL")?;
        let target_url = std::env::var("NUONO_TRANSFER_TARGET_URL")?;
        let final_url = prepared_url(&plan.prepared_config)?;
        if database_identity(&source_url)?
            != database_identity(&prepared_url(&plan.active_config)?)?
        {
            return Err(
                "active configuration must explicitly identify the verified source database".into(),
            );
        }
        let staged_url = if files::sqlite_file(&target_url)?.is_some() {
            remap_sqlite(&final_url, &plan.active_data, &plan.staged_data)?
        } else {
            final_url.clone()
        };
        if database_identity(&target_url)? != database_identity(&staged_url)? {
            return Err("prepared database URL differs from the verified target".into());
        }
        let source = OfflineDatabase::open(&source_url).await?;
        portable::verify(&source.connection, &schema, &image).await?;
        files::verify_data(
            &plan.active_data,
            &manifest,
            files::sqlite_file(&source_url)?.as_deref(),
        )?;
        let target = OfflineDatabase::open(&target_url).await?;
        portable::verify(&target.connection, &schema, &image).await?;
        files::verify_data(
            &plan.staged_data,
            &manifest,
            files::sqlite_file(&target_url)?.as_deref(),
        )?;
        external::verify_published(
            &plan.staged_data,
            &manifest.database_sha256,
            &manifest.external,
        )?;
        let journal_path = path.with_extension("journal.json");
        let journal = cutover::prepare(&journal_path, plan, &manifest.database_sha256)?;
        fence(&source, &journal.package_hash).await?;
        fence(&target, &journal.package_hash).await?;
        source.close().await?;
        target.close().await?;
        (journal_path, journal)
    } else {
        (path.clone(), cutover::read(&path)?)
    };
    if mode == "rollback" && journal.phase == cutover::Phase::Complete {
        return Err("published target may have accepted new writes; reverse-transfer the authoritative database instead".into());
    }
    let (manifest, image) = files::read_package(&journal.plan.package)?;
    if manifest.database_sha256 != journal.package_hash {
        return Err("cutover package changed".into());
    }
    let schema = Schema::from_image(&image)?;
    let final_url = prepared_url(&journal.plan.prepared_config)?;
    if journal.phase == cutover::Phase::Complete {
        let _target = lease(&journal.plan.active_data)?;
        let target = OfflineDatabase::open(&final_url).await?;
        if current_fence(&target).await?.is_some() {
            portable::verify_maintenance(
                &target.connection,
                &schema,
                &image,
                &journal.package_hash,
            )
            .await?;
            files::verify_data(
                &journal.plan.active_data,
                &manifest,
                files::sqlite_file(&final_url)?.as_deref(),
            )?;
            external::verify_published(
                &journal.plan.active_data,
                &manifest.database_sha256,
                &manifest.external,
            )?;
            cutover::finish_completed(&journal)?;
            clear_fence(&target, &journal.package_hash).await?;
        } else if journal.plan.active_data.join(files::PENDING).exists()
            || cutover::startup_guard(&journal.plan.active_data).exists()
        {
            return Err("completed receipt has inconsistent startup markers".into());
        }
        target.close().await?;
        println!(
            "Cutover already completed; source rollback is permanently disabled for this receipt."
        );
        return Ok(());
    }
    if journal.phase == cutover::Phase::RolledBack {
        let _source = lease(&journal.plan.active_data)?;
        let _target = lease(&journal.plan.staged_data)?;
        let source_url = std::env::var("NUONO_TRANSFER_SOURCE_URL")?;
        if database_identity(&source_url)?
            != database_identity(&prepared_url(&journal.plan.active_config)?)?
        {
            return Err("source connection does not match restored configuration".into());
        }
        let source = OfflineDatabase::open(&source_url).await?;
        if current_fence(&source).await?.is_some() {
            portable::verify_maintenance(
                &source.connection,
                &schema,
                &image,
                &journal.package_hash,
            )
            .await?;
            files::verify_data(
                &journal.plan.active_data,
                &manifest,
                files::sqlite_file(&source_url)?.as_deref(),
            )?;
            cutover::rollback(&journal_path, &mut journal)?;
            clear_fence(&source, &journal.package_hash).await?;
        } else if journal.plan.active_data.join(files::PENDING).exists()
            || cutover::startup_guard(&journal.plan.active_data).exists()
        {
            return Err("completed rollback has inconsistent startup markers".into());
        }
        source.close().await?;
        let staged_url = remap_sqlite(
            &final_url,
            &journal.plan.active_data,
            &journal.plan.staged_data,
        )?;
        let target = OfflineDatabase::open(&staged_url).await?;
        if current_fence(&target).await?.is_some() {
            portable::verify_maintenance(
                &target.connection,
                &schema,
                &image,
                &journal.package_hash,
            )
            .await?;
            clear_fence(&target, &journal.package_hash).await?;
        }
        target.close().await?;
        println!(
            "Rollback already completed; restored source is authoritative and was not rewound."
        );
        return Ok(());
    }
    let source_location = if journal.backup_data.exists() {
        &journal.backup_data
    } else {
        &journal.plan.active_data
    };
    let target_location = if journal.plan.staged_data.exists() {
        &journal.plan.staged_data
    } else {
        &journal.plan.active_data
    };
    let _source = lease(source_location)?;
    let _target = if source_location != target_location {
        Some(lease(target_location)?)
    } else {
        None
    };
    let source_config = if journal.backup_config.exists() {
        &journal.backup_config
    } else {
        &journal.plan.active_config
    };
    let original_source_url = std::env::var("NUONO_TRANSFER_SOURCE_URL")?;
    if database_identity(&original_source_url)? != database_identity(&prepared_url(source_config)?)?
    {
        return Err("source connection does not match the retained source configuration".into());
    }
    let current_source_url = remap_sqlite(
        &original_source_url,
        &journal.plan.active_data,
        source_location,
    )?;
    let current_target_url = remap_sqlite(&final_url, &journal.plan.active_data, target_location)?;
    let source = OfflineDatabase::open(&current_source_url).await?;
    let target = OfflineDatabase::open(&current_target_url).await?;
    fence(&source, &journal.package_hash).await?;
    fence(&target, &journal.package_hash).await?;
    portable::verify_maintenance(&source.connection, &schema, &image, &journal.package_hash)
        .await?;
    portable::verify_maintenance(&target.connection, &schema, &image, &journal.package_hash)
        .await?;
    source.close().await?;
    target.close().await?;
    if mode == "rollback" {
        cutover::rollback(&journal_path, &mut journal)?;
        let source = OfflineDatabase::open(&original_source_url).await?;
        clear_fence(&source, &journal.package_hash).await?;
        source.close().await?;
        let staged_url = remap_sqlite(
            &final_url,
            &journal.plan.active_data,
            &journal.plan.staged_data,
        )?;
        let target = OfflineDatabase::open(&staged_url).await?;
        clear_fence(&target, &journal.package_hash).await?;
        target.close().await?;
        println!(
            "Source restored before target activation; staged target and recovery receipt retained."
        );
        return Ok(());
    }
    cutover::advance(&journal_path, &mut journal)?;
    let target = OfflineDatabase::open(&final_url).await?;
    portable::verify_maintenance(&target.connection, &schema, &image, &journal.package_hash)
        .await?;
    files::verify_data(
        &journal.plan.active_data,
        &manifest,
        files::sqlite_file(&final_url)?.as_deref(),
    )?;
    external::verify_published(
        &journal.plan.active_data,
        &manifest.database_sha256,
        &manifest.external,
    )?;
    cutover::complete(&journal_path, &mut journal)?;
    clear_fence(&target, &journal.package_hash).await?;
    target.close().await?;
    println!(
        "Published and verified. Source database, files and previous configuration remain preserved and fenced; journal: {}",
        journal_path.display()
    );
    Ok(())
}
