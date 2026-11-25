use super::{manifest::*, *};
use std::{collections::BTreeMap, fs, path::Path};

pub(super) fn fixture(root: &Path) -> ReleaseManifest {
    fs::create_dir_all(root.join("nuono-hub/dist")).unwrap();
    fs::create_dir(root.join("build-info")).unwrap();
    for (name, body) in [
        (
            "index.html",
            "<link href='/main.css'><script src='/app.js'></script><link href='/app.wasm'>",
        ),
        ("main.css", "body{}"),
        ("app.js", "export default 1"),
        ("app.wasm", "wasm fixture"),
    ] {
        fs::write(root.join("nuono-hub/dist").join(name), body).unwrap();
    }
    let executable = format!("nuono{}", std::env::consts::EXE_SUFFIX);
    fs::write(
        root.join(&executable),
        "a verification command must never execute this file",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join(&executable), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let lock = b"version = 4\n";
    fs::write(root.join("build-info/Cargo.lock"), lock).unwrap();
    fs::write(
        root.join("build-info/application.Cargo.toml"),
        "[package]\nname='fixture'",
    )
    .unwrap();
    let sources = vec![SourceRecord {
        kind: "framework".into(),
        id: "nuono".into(),
        repository: None,
        commit: "a".repeat(40),
        tree: "b".repeat(40),
    }];
    fs::write(
        root.join("build-info/sources.json"),
        serde_json::to_vec(&sources).unwrap(),
    )
    .unwrap();
    let mut manifest = ReleaseManifest {
        schema_version: 1,
        release_id: String::new(),
        created_at: "2026-10-22T00:00:00Z".into(),
        executable,
        application: ApplicationDescription {
            schema_version: 1,
            framework_version: "0.0.1".into(),
            extensions: vec![],
            build: BuildFacts {
                lock_sha256: Some(digest(lock)),
                rust_version: Some("1.99.0".into()),
                compiler: Some("rustc".into()),
                compiler_commit: Some("c".repeat(40)),
                llvm_version: None,
                target_triple: Some("fixture-target".into()),
                build_host: Some("fixture-target".into()),
                build_profile: "debug".into(),
                dependencies: vec![],
            },
            migrations: vec![MigrationSet {
                namespace: "framework".into(),
                migrations: vec![Migration {
                    version: "m0001_baseline".into(),
                    description: "initial".into(),
                    postgres_sha256: digest(b"pg"),
                    sqlite_sha256: digest(b"sqlite"),
                }],
            }],
        },
        sources,
        readiness: Readiness {
            checks: vec![
                "database".into(),
                "framework_migrations".into(),
                "required_extension_initialization".into(),
            ],
            required_plugins: vec![],
            required_adapters: vec![],
            external_connections: "reported_separately".into(),
        },
        build_parameters: BTreeMap::new(),
        files: inventory(root).unwrap(),
    };
    save(root, &mut manifest);
    manifest
}
pub(super) fn save(root: &Path, manifest: &mut ReleaseManifest) {
    manifest.release_id = manifest.identity().unwrap();
    fs::write(
        root.join("release.json"),
        serde_json::to_vec_pretty(manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn complete_package_verifies_without_executing_its_program() {
    let root = tempfile::tempdir().unwrap();
    let expected = fixture(root.path());
    assert_eq!(verify(root.path()).unwrap().release_id, expected.release_id);
}
#[test]
fn damaged_missing_or_extra_payloads_fail_closed() {
    for case in 0..4 {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        match case {
            0 => fs::write(root.path().join("nuono-hub/dist/app.js"), "tampered").unwrap(),
            1 => fs::remove_file(root.path().join("nuono-hub/dist/app.wasm")).unwrap(),
            2 => fs::write(
                root.path().join("nuono.toml"),
                "secret must never be included",
            )
            .unwrap(),
            _ => fs::write(root.path().join("build-info/Cargo.lock"), "wrong lock").unwrap(),
        }
        assert!(verify(root.path()).is_err(), "case {case}");
    }
}
#[test]
fn matching_checksums_cannot_hide_inconsistent_inventory_or_source_identity() {
    for case in 0..6 {
        let root = tempfile::tempdir().unwrap();
        let mut manifest = fixture(root.path());
        match case {
            0 => manifest.schema_version = 99,
            1 => {
                manifest.files.insert(
                    "../outside".into(),
                    manifest.files.values().next().unwrap().clone(),
                );
            }
            2 => manifest.application.build.lock_sha256 = Some("0".repeat(64)),
            3 => manifest
                .readiness
                .required_plugins
                .push("not-installed".into()),
            4 => manifest.application.migrations.clear(),
            _ => {
                manifest.sources[0].commit = "bad-revision".into();
                fs::write(
                    root.path().join("build-info/sources.json"),
                    serde_json::to_vec(&manifest.sources).unwrap(),
                )
                .unwrap();
            }
        }
        save(root.path(), &mut manifest);
        assert!(verify(root.path()).is_err(), "case {case}");
    }
}
#[test]
fn dashboard_references_are_validated_even_if_checksums_match() {
    let root = tempfile::tempdir().unwrap();
    let mut manifest = fixture(root.path());
    fs::write(
        root.path().join("nuono-hub/dist/index.html"),
        "<script src='/missing.js'></script>",
    )
    .unwrap();
    manifest.files = inventory(root.path()).unwrap();
    save(root.path(), &mut manifest);
    assert!(verify(root.path()).is_err());
}
#[test]
fn paths_cannot_escape_the_distribution() {
    for path in [
        "",
        "/abs",
        "a/../b",
        "a/./b",
        "a//b",
        "a\\b",
        "C:/program",
        "a/",
        "a\0b",
    ] {
        assert!(safe_path(path).is_err(), "{path:?}");
    }
    safe_path("nuono-hub/dist/snippets/a b.mjs").unwrap();
}
#[cfg(unix)]
#[test]
fn symlinks_are_rejected_including_a_symlinked_manifest() {
    use std::os::unix::fs::symlink;
    for manifest_link in [true, false] {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let name = if manifest_link {
            "release.json"
        } else {
            "nuono-hub/dist/app.js"
        };
        let bytes = fs::read(root.path().join(name)).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), bytes).unwrap();
        fs::remove_file(root.path().join(name)).unwrap();
        symlink(outside.path(), root.path().join(name)).unwrap();
        assert!(verify(root.path()).is_err());
    }
}
#[test]
fn formal_source_inventory_rejects_unrecorded_work_and_removes_url_credentials() {
    let root = tempfile::tempdir().unwrap();
    fn git(root: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        source_build::configure_git_command(&mut command);
        assert!(
            command
                .arg("-C")
                .arg(root)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
    git(root.path(), &["init", "-q"]);
    fs::write(root.path().join("source.rs"), "fn main(){}").unwrap();
    git(root.path(), &["add", "source.rs"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    git(
        root.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://user:fixture-key@example.invalid/project?token=private",
        ],
    );
    let record = build::source(root.path(), "framework", "nuono").unwrap();
    assert_eq!(
        record.repository.as_deref(),
        Some("https://example.invalid/project")
    );
    fs::write(root.path().join("new.rs"), "unrecorded source").unwrap();
    assert!(build::source(root.path(), "framework", "nuono").is_err());
    fs::remove_file(root.path().join("new.rs")).unwrap();
    fs::write(root.path().join("source.rs"), "modified source").unwrap();
    assert!(build::source(root.path(), "framework", "nuono").is_err());
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn publishing_is_atomic_and_never_overwrites_an_existing_directory() {
    let root = tempfile::tempdir().unwrap();
    let staged = root.path().join("staging");
    let output = root.path().join("output");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("complete"), "yes").unwrap();
    fs::create_dir(&output).unwrap();
    assert!(build::publish(&staged, &output).is_err());
    assert!(staged.join("complete").is_file());
    assert!(!output.join("complete").exists());
    fs::remove_dir(&output).unwrap();
    build::publish(&staged, &output).unwrap();
    assert!(!staged.exists());
    assert_eq!(fs::read_to_string(output.join("complete")).unwrap(), "yes");
}

#[test]
fn destination_is_persistent_and_does_not_follow_aliases_into_build_cache() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("target");
    fs::create_dir(&cache).unwrap();
    let cache = cache.canonicalize().unwrap();
    assert!(build::destination(&cache.join("releases/first"), &cache).is_err());
    let destination = build::destination(&root.path().join("releases/first"), &cache).unwrap();
    assert!(!destination.exists());
    fs::create_dir(&destination).unwrap();
    assert!(build::destination(&destination, &cache).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&cache, root.path().join("alias")).unwrap();
        assert!(build::destination(&root.path().join("alias/release"), &cache).is_err());
        std::os::unix::fs::symlink(root.path().join("missing"), root.path().join("dangling"))
            .unwrap();
        assert!(build::destination(&root.path().join("dangling"), &cache).is_err());
    }
}
