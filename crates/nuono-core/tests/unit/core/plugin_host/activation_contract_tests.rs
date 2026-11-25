use super::*;
use nuono_app::plugin_data::PluginDataStore;
use nuono_plugin::PluginMigrationForBackend;
use std::sync::Mutex;

#[derive(Clone)]
struct ContractPlugin {
    name: String,
    steps: Arc<Mutex<Vec<(&'static str, PluginActivationReason)>>>,
    declarations: Arc<AtomicUsize>,
    reason: Option<PluginActivationReason>,
    migrations: Vec<PluginMigrationForBackend>,
    fail_enable: bool,
}

impl ContractPlugin {
    fn new(name: String) -> Self {
        let sql = "CREATE TABLE activation_contract (value TEXT PRIMARY KEY); INSERT INTO activation_contract VALUES ('1')";
        Self {
            name,
            steps: Default::default(),
            declarations: Default::default(),
            reason: None,
            migrations: vec![PluginMigrationForBackend {
                version: 1,
                description: "activation contract".into(),
                pg_sql: sql.into(),
                sqlite_sql: "CREATE TABLE activation_contract (value TEXT PRIMARY KEY) STRICT; INSERT INTO activation_contract VALUES ('1')".into(),
            }],
            fail_enable: false,
        }
    }

    fn loaded(&self) -> LoadedPlugin {
        LoadedPlugin {
            plugin: Box::new(self.clone()),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for ContractPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "activation contract"
    }
    fn migrations(&self) -> Vec<PluginMigrationForBackend> {
        self.declarations.fetch_add(1, Ordering::SeqCst);
        self.migrations.clone()
    }
    fn storage_declaration(
        &self,
    ) -> nuono_error::Result<nuono_plugin::storage_manifest::PluginStorageDeclaration> {
        use nuono_plugin::storage_manifest::{
            PluginStorageDeclaration, PluginStorageVersion, PortableManifest,
        };
        let migrations = self.migrations();
        PluginMigrationForBackend::validate_all(&migrations)?;
        let mut manifest = PortableManifest::empty(&self.name);
        manifest.table_prefix = "activation_".into();
        manifest.postgres_schema =
            "CREATE TABLE activation_contract (value TEXT PRIMARY KEY)".into();
        manifest.sqlite_schema =
            "CREATE TABLE activation_contract (value TEXT PRIMARY KEY) STRICT".into();
        Ok(PluginStorageDeclaration {
            manifest,
            versions: migrations
                .into_iter()
                .map(|migration| PluginStorageVersion {
                    version: migration.version,
                    description: migration.description,
                    postgres_sql: migration.pg_sql,
                    sqlite_sql: migration.sqlite_sql,
                })
                .collect(),
        })
    }
    async fn on_load(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        // 建表语句故意不加 IF NOT EXISTS 或冲突忽略：一旦迁移被重复执行就会直接报错。
        assert_eq!(
            ctx.db()
                .query_json("SELECT value AS data FROM activation_contract", vec![])
                .await?
                .len(),
            1
        );
        self.reason = Some(ctx.activation_reason());
        self.steps
            .lock()
            .unwrap()
            .push(("load", ctx.activation_reason()));
        Ok(())
    }
    async fn on_enable(&mut self) -> anyhow::Result<()> {
        self.steps
            .lock()
            .unwrap()
            .push(("enable", self.reason.unwrap()));
        anyhow::ensure!(!self.fail_enable, "enable rejected");
        Ok(())
    }
}

#[tokio::test]
async fn every_activation_runs_migrations_load_enable_with_an_explicit_reason() {
    let (host, db, name) = host_for_activation_test().await;
    let plugin = ContractPlugin::new(name.clone());
    host.init_with_plugins(vec![plugin.loaded()]).await.unwrap();
    host.toggle(&name, false).await.unwrap();
    host.toggle(&name, true).await.unwrap();
    host.reload_with_candidate(&name, plugin.loaded(), true)
        .await
        .unwrap();
    assert_eq!(
        *plugin.steps.lock().unwrap(),
        vec![
            ("load", PluginActivationReason::Startup),
            ("enable", PluginActivationReason::Startup),
            ("load", PluginActivationReason::Enable),
            ("enable", PluginActivationReason::Enable),
            ("load", PluginActivationReason::Rebuild),
            ("enable", PluginActivationReason::Rebuild),
        ]
    );
    assert_eq!(plugin.declarations.load(Ordering::SeqCst), 3);
    host.toggle(&name, false).await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn disabled_plugins_register_storage_without_running_lifecycle() {
    let (host, db, name) = host_for_activation_test().await;
    db.sync_plugin(&name, "1.0.0", &serde_json::json!({}))
        .await
        .unwrap();
    db.set_plugin_enabled(&name, false).await.unwrap();
    let plugin = ContractPlugin::new(name.clone());
    host.init_with_plugins(vec![plugin.loaded()]).await.unwrap();
    assert!(plugin.steps.lock().unwrap().is_empty());
    assert_eq!(
        db.query_json("SELECT value AS data FROM activation_contract", vec![])
            .await
            .unwrap()
            .len(),
        1
    );
    host.toggle(&name, true).await.unwrap();
    assert_eq!(plugin.steps.lock().unwrap().len(), 2);
    host.toggle(&name, false).await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_migration_declaration_preserves_a_running_instance_on_rebuild() {
    let (host, db, name) = host_for_activation_test().await;
    let plugin = ContractPlugin::new(name.clone());
    host.init_with_plugins(vec![plugin.loaded()]).await.unwrap();
    let mut invalid = plugin.clone();
    invalid.migrations[0].sqlite_sql.clear();
    assert!(
        host.reload_with_candidate(&name, invalid.loaded(), true)
            .await
            .is_err()
    );
    assert!(db.is_plugin_enabled(&name).await.unwrap());
    assert_eq!(plugin.steps.lock().unwrap().len(), 2);
    host.toggle(&name, false).await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn migration_failure_rolls_back_schema_and_never_calls_lifecycle() {
    let (host, db, name) = host_for_activation_test().await;
    let mut plugin = ContractPlugin::new(name.clone());
    let sql = "INSERT INTO missing_activation_table VALUES ('fail')";
    plugin.migrations.push(PluginMigrationForBackend {
        version: 2,
        description: "failing migration".into(),
        pg_sql: sql.into(),
        sqlite_sql: sql.into(),
    });
    assert!(host.init_with_plugins(vec![plugin.loaded()]).await.is_err());
    assert!(plugin.steps.lock().unwrap().is_empty());
    assert!(
        db.query_json("SELECT value AS data FROM activation_contract", vec![])
            .await
            .is_err()
    );
    let valid = ContractPlugin::new(name.clone());
    host.init_with_plugins(vec![valid.loaded()]).await.unwrap();
    assert_eq!(
        *valid.steps.lock().unwrap(),
        vec![
            ("load", PluginActivationReason::Startup),
            ("enable", PluginActivationReason::Startup)
        ]
    );
    host.toggle(&name, false).await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_enable_failure_is_fail_closed_but_does_not_undo_committed_migrations() {
    let (host, db, name) = host_for_activation_test().await;
    let mut plugin = ContractPlugin::new(name.clone());
    plugin.fail_enable = true;
    host.init_with_plugins(vec![plugin.loaded()]).await.unwrap();
    assert_eq!(plugin.steps.lock().unwrap().len(), 2);
    assert!(!db.is_plugin_enabled(&name).await.unwrap());
    assert!(!host.all_plugins().await.contains(&name));
    assert_eq!(
        db.query_json("SELECT value AS data FROM activation_contract", vec![])
            .await
            .unwrap()
            .len(),
        1
    );
    drop(host);
    db.shutdown().await.unwrap();
}
