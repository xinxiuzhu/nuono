use std::{fs, path::Path, process::Command};

use nuono_app::plugin_manifest::PersistentAsset;
use nuono_storage::{Database, config::DatabaseConfig, portable};
use sea_orm::ConnectionTrait;

fn command(
    mode: &str,
    package: &Path,
    data: &Path,
    url: &str,
    mapping: Option<&Path>,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nuono-database-transfer"));
    command
        .args([mode])
        .arg(package)
        .arg(data)
        .env_remove("NUONO_TRANSFER_TARGET_PLUGINS")
        .env("NUONO_TRANSFER_SOURCE_URL", url)
        .env("NUONO_TRANSFER_TARGET_URL", url);
    if let Some(mapping) = mapping {
        command.arg(mapping);
    }
    command.output().unwrap()
}

fn succeeded(output: std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn completed_import_resume_preserves_receipt_paths_and_later_writes() {
    let root = std::env::temp_dir().join(format!("nuono-cli-resume-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let source = root.join("source");
    let target = root.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("persistent.bin"), [0, 255, 1]).unwrap();
    let external_source = root.join("external-source");
    let external_target = root.join("external-target");
    fs::write(&external_source, [42, 0, 255]).unwrap();
    let source_url = format!("sqlite://{}?mode=rwc", source.join("database.db").display());
    let target_url = format!("sqlite://{}?mode=rwc", target.join("database.db").display());
    let db = Database::from_config(&DatabaseConfig {
        url: source_url.clone(),
        max_connections: 1,
    })
    .await
    .unwrap();
    db.register_deployment_assets(&[PersistentAsset::External {
        key: "fixture-key".into(),
        path: external_source.to_string_lossy().into_owned(),
        required: true,
    }])
    .await
    .unwrap();
    db.shutdown().await.unwrap();
    let package = root.join("package");
    let mapping = root.join("mapping.json");
    fs::write(
        &mapping,
        serde_json::to_vec(&serde_json::json!({"nuono/fixture-key":external_target})).unwrap(),
    )
    .unwrap();
    succeeded(command("export", &package, &source, &source_url, None));
    succeeded(command(
        "import",
        &package,
        &target,
        &target_url,
        Some(&mapping),
    ));
    let receipt = target.join(".portable-transfer-journal.json");
    let completed_receipt = fs::read(&receipt).unwrap();
    succeeded(command(
        "resume-import",
        &package,
        &target,
        &target_url,
        Some(&mapping),
    ));
    assert_eq!(fs::read(&receipt).unwrap(), completed_receipt);
    assert!(!portable::pending_operation(&target).unwrap());
    let (manifest, _) = portable::files::read_package(&package).unwrap();
    fs::write(
        target.join(portable::files::PENDING),
        manifest.database_sha256,
    )
    .unwrap();
    succeeded(command(
        "resume-import",
        &package,
        &target,
        &target_url,
        Some(&mapping),
    ));
    assert!(!target.join(portable::files::PENDING).exists());

    let alternative = root.join("other-identical-target");
    fs::copy(&external_target, &alternative).unwrap();
    fs::write(
        &mapping,
        serde_json::to_vec(&serde_json::json!({"nuono/fixture-key":alternative})).unwrap(),
    )
    .unwrap();
    assert!(
        !command(
            "resume-import",
            &package,
            &target,
            &target_url,
            Some(&mapping)
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&receipt).unwrap(), completed_receipt);

    fs::write(
        &mapping,
        serde_json::to_vec(&serde_json::json!({"nuono/fixture-key":external_target})).unwrap(),
    )
    .unwrap();
    let db = Database::from_config(&DatabaseConfig {
        url: target_url.clone(),
        max_connections: 1,
    })
    .await
    .unwrap();
    db.orm()
        .execute_unprepared("UPDATE nuono_deployment SET created_at_ms=created_at_ms+1")
        .await
        .unwrap();
    db.shutdown().await.unwrap();
    assert!(
        !command(
            "resume-import",
            &package,
            &target,
            &target_url,
            Some(&mapping)
        )
        .status
        .success()
    );
    assert_eq!(fs::read(&receipt).unwrap(), completed_receipt);
    assert!(!portable::pending_operation(&target).unwrap());
    assert!(!target.join(portable::files::PENDING).exists());
    assert_eq!(
        fs::read(target.join("persistent.bin")).unwrap(),
        [0, 255, 1]
    );
    fs::remove_dir_all(root).unwrap();
}
