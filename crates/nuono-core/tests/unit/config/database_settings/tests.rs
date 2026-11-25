use std::sync::Arc;

use super::*;
use crate::config::{BOOTSTRAP_TEMPLATE, NuonoConfig};

const OLD_URL: &str = "postgres://operator:old-secret@127.0.0.1:5432/nuono";
const NEW_URL: &str = "postgresql://operator:new-secret@127.0.0.1:5433/next";

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
    manager: Arc<ConfigManager>,
}

impl Fixture {
    fn new(content: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("nuono-database-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("nuono.toml");
        std::fs::write(&path, content).unwrap();
        let mut config = NuonoConfig::default();
        config.database.url = OLD_URL.into();
        let manager = Arc::new(ConfigManager::for_test(config, path.clone()));
        Self {
            directory,
            path,
            manager,
        }
    }

    fn raw(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }

    fn assert_no_temporary_files(&self) {
        let entries: Vec<_> = std::fs::read_dir(&self.directory).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn postgres_save_changes_only_address_and_keeps_active_snapshot() {
    let original = format!(
        "# deployment comment\nhost = '127.0.0.1'\n[database] # database header\nurl = \"{OLD_URL}\" # keep this comment\nmax_connections = 9\nunknown = 'future'\n[custom]\nkeep = [1, 2]\n"
    );
    let fixture = Fixture::new(&original);
    let before = fixture.manager.database_settings().await.unwrap();
    assert_eq!(before.url, OLD_URL);
    assert_eq!(before.active_url, OLD_URL);
    assert!(!before.restart_required);

    let after = fixture
        .manager
        .save_database_settings(NEW_URL, &before.revision)
        .await
        .unwrap();
    assert_eq!(fixture.raw(), original.replace(OLD_URL, NEW_URL));
    assert_eq!(after.url, NEW_URL);
    assert_eq!(after.active_url, OLD_URL);
    assert!(after.restart_required);
    assert_ne!(after.revision, before.revision);
    assert_eq!(fixture.manager.snapshot().await.database.url, OLD_URL);
    assert_eq!(fixture.manager.startup.database.url, OLD_URL);
    let reread = fixture.manager.database_settings().await.unwrap();
    assert_eq!(reread.revision, after.revision);
    assert_eq!(reread.url, NEW_URL);
    fixture.assert_no_temporary_files();
}

#[tokio::test]
async fn sqlite_save_survives_bootstrap_reload() {
    let _environment_lock = crate::test_support::env_lock().await;
    let old = crate::config::DatabaseConfig::default().url;
    let original = BOOTSTRAP_TEMPLATE.replace(&old, OLD_URL);
    assert!(original.contains(OLD_URL));
    let fixture = Fixture::new(&original);
    let before = fixture.manager.database_settings().await.unwrap();
    let address = "sqlite://data/new.db?mode=rwc";
    fixture
        .manager
        .save_database_settings(address, &before.revision)
        .await
        .unwrap();
    let loaded = NuonoConfig::read_toml(Some(fixture.path.clone()), true)
        .await
        .unwrap();
    assert_eq!(loaded.database.url, address);
    assert!(loaded.validate_bootstrap().is_ok());
    assert_eq!(fixture.manager.snapshot().await.database.url, OLD_URL);
    fixture.assert_no_temporary_files();
}

#[tokio::test]
async fn inline_and_dotted_tables_keep_sibling_fields_and_comments() {
    for original in [
        format!(
            "# inline\ndatabase = {{ url = \"{OLD_URL}\", max_connections = 7, future = 'keep' }} # trailing\n"
        ),
        format!(
            "# spaced inline\ndatabase = {{ 'url'  =  \"{OLD_URL}\"  ,max_connections=7 }} # trailing\n"
        ),
        format!("# dotted\ndatabase.url = \"{OLD_URL}\" # trailing\ndatabase.future = 'keep'\n"),
        format!(
            "[database]\n# database address\n  'url'  = \"{OLD_URL}\" # trailing\nfuture = 'keep'\n"
        ),
    ] {
        let fixture = Fixture::new(&original);
        let before = fixture.manager.database_settings().await.unwrap();
        fixture
            .manager
            .save_database_settings(NEW_URL, &before.revision)
            .await
            .unwrap();
        assert_eq!(fixture.raw(), original.replace(OLD_URL, NEW_URL));
        fixture.assert_no_temporary_files();
    }
}

#[tokio::test]
async fn missing_database_url_can_be_added_without_rewriting_existing_values() {
    for original in [
        "# only custom\n[custom]\nkeep = 'untouched'\n",
        "# existing table\n[database]\nmax_connections = 11\n",
    ] {
        let fixture = Fixture::new(original);
        let before = fixture.manager.database_settings().await.unwrap();
        assert_eq!(before.url, DatabaseConfig::default().url);
        fixture
            .manager
            .save_database_settings(NEW_URL, &before.revision)
            .await
            .unwrap();
        let raw = fixture.raw();
        assert!(raw.contains(original));
        assert_eq!(
            document_url(&parse_document(&raw).unwrap()).unwrap(),
            NEW_URL
        );
        fixture.assert_no_temporary_files();
    }
}

#[tokio::test]
async fn revision_covers_external_edits_to_unrelated_fields() {
    let original = format!("[database]\nurl = \"{OLD_URL}\"\n");
    let fixture = Fixture::new(&original);
    let before = fixture.manager.database_settings().await.unwrap();
    let edited = format!("{original}# saved by external editor\n");
    std::fs::write(&fixture.path, &edited).unwrap();
    let result = fixture
        .manager
        .save_database_settings(NEW_URL, &before.revision)
        .await;
    assert!(matches!(result, Err(NuonoError::Conflict(_))));
    assert_eq!(fixture.raw(), edited);
    assert_eq!(fixture.manager.snapshot().await.database.url, OLD_URL);
    fixture.assert_no_temporary_files();
}

#[tokio::test]
async fn simultaneous_saves_with_one_revision_have_one_winner() {
    let fixture = Fixture::new(&format!("[database]\nurl = \"{OLD_URL}\"\n"));
    let before = fixture.manager.database_settings().await.unwrap();
    let (one, two) = tokio::join!(
        fixture
            .manager
            .save_database_settings(NEW_URL, &before.revision),
        fixture
            .manager
            .save_database_settings("sqlite://other.db?mode=rwc", &before.revision)
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    let failure = if let Err(error) = one {
        error
    } else {
        two.err().unwrap()
    };
    assert!(matches!(failure, NuonoError::Conflict(_)));
    assert_eq!(fixture.manager.snapshot().await.database.url, OLD_URL);
    fixture.assert_no_temporary_files();
}

#[tokio::test]
async fn same_value_is_a_noop_but_still_checks_revision() {
    let original = format!("database = {{ url = '{OLD_URL}' }} # literal quoted\n");
    let fixture = Fixture::new(&original);
    let before = fixture.manager.database_settings().await.unwrap();
    let after = fixture
        .manager
        .save_database_settings(OLD_URL, &before.revision)
        .await
        .unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(fixture.raw(), original);
    assert!(matches!(
        fixture
            .manager
            .save_database_settings(OLD_URL, "stale")
            .await,
        Err(NuonoError::Conflict(_))
    ));
}

#[tokio::test]
async fn invalid_addresses_leave_file_and_runtime_unchanged_without_leaking_credentials() {
    let original = format!("[database]\nurl = \"{OLD_URL}\"\n");
    let fixture = Fixture::new(&original);
    let before = fixture.manager.database_settings().await.unwrap();
    let oversized = format!("postgres://user:{}@localhost/db", "secret".repeat(1400));
    for invalid in [
        "",
        " ",
        "mysql://user:secret@localhost/db",
        "postgres://user:secret@host/db\n",
        "sqlite://file:secret.db",
        oversized.as_str(),
    ] {
        let result = fixture
            .manager
            .save_database_settings(invalid, &before.revision)
            .await;
        let error = match result {
            Ok(_) => panic!("invalid address accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("secret"));
        assert_eq!(fixture.raw(), original);
        assert_eq!(fixture.manager.snapshot().await.database.url, OLD_URL);
    }
    fixture.assert_no_temporary_files();
}

#[tokio::test]
async fn invalid_files_are_never_replaced_or_returned_as_parser_excerpts() {
    for original in [
        "[database]\nurl = \"secret\" invalid\n",
        "database = \"secret\"\n",
        "[database]\nurl = 123\n",
    ] {
        let fixture = Fixture::new(original);
        let error = match fixture.manager.database_settings().await {
            Ok(_) => panic!("invalid file accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("secret"));
        let result = fixture
            .manager
            .save_database_settings(NEW_URL, &revision(original.as_bytes()))
            .await;
        assert!(result.is_err());
        assert_eq!(fixture.raw(), original);
        fixture.assert_no_temporary_files();
    }
}

#[tokio::test]
async fn missing_file_is_reported_without_creating_a_replacement() {
    let fixture = Fixture::new("# will be removed\n");
    std::fs::remove_file(&fixture.path).unwrap();
    assert!(fixture.manager.database_settings().await.is_err());
    assert!(
        fixture
            .manager
            .save_database_settings(NEW_URL, "missing")
            .await
            .is_err()
    );
    assert!(!fixture.path.exists());
    assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn original_permissions_survive_and_readonly_or_symlink_files_are_preserved() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let original = format!("[database]\nurl = \"{OLD_URL}\"\n");
    let fixture = Fixture::new(&original);
    std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let before = fixture.manager.database_settings().await.unwrap();
    fixture
        .manager
        .save_database_settings(NEW_URL, &before.revision)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&fixture.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o440)).unwrap();
    let before = fixture.manager.database_settings().await.unwrap();
    assert!(
        fixture
            .manager
            .save_database_settings(OLD_URL, &before.revision)
            .await
            .is_err()
    );
    assert_eq!(
        fixture.manager.database_settings().await.unwrap().url,
        NEW_URL
    );
    fixture.assert_no_temporary_files();

    let target = fixture.directory.join("actual.toml");
    std::fs::rename(&fixture.path, &target).unwrap();
    symlink(&target, &fixture.path).unwrap();
    assert!(fixture.manager.database_settings().await.is_err());
    assert!(
        fixture
            .manager
            .save_database_settings(OLD_URL, &before.revision)
            .await
            .is_err()
    );
    assert!(
        std::fs::symlink_metadata(&fixture.path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        original.replace(OLD_URL, NEW_URL)
    );
}

#[tokio::test]
async fn external_changes_during_preparation_reject_publication_and_remove_temporary_file() {
    let original = format!("[database]\nurl = \"{OLD_URL}\"\n");
    let fixture = Fixture::new(&original);
    let snapshot = read_startup_file(&fixture.path).await.unwrap();
    let edited = format!("{original}# concurrent editor\n");
    std::fs::write(&fixture.path, &edited).unwrap();
    let candidate = original.replace(OLD_URL, NEW_URL);
    let result = replace_startup_file(&fixture.path, &snapshot, candidate.as_bytes()).await;
    assert!(matches!(result, Err(NuonoError::Conflict(_))));
    assert_eq!(fixture.raw(), edited);
    fixture.assert_no_temporary_files();
}
