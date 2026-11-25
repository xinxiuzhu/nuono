use super::*;
use crate::release::{
    Migration,
    tests::{fixture, save},
};
#[test]
fn verified_packages_report_appended_migrations_without_executing_programs() {
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    fixture(old.path());
    let mut candidate = fixture(new.path());
    candidate.application.migrations[0]
        .migrations
        .push(Migration {
            version: "m0002_fixture".into(),
            description: "append only".into(),
            postgres_sha256: super::super::digest(b"new pg"),
            sqlite_sha256: super::super::digest(b"new sqlite"),
        });
    save(new.path(), &mut candidate);
    let report = preflight(old.path(), new.path()).unwrap();
    assert!(report.blockers.is_empty());
    assert_eq!(report.added_migrations["framework"], ["m0002_fixture"]);
    assert!(!report.required_next_steps.is_empty());
    // 即便清单文件本身合法，一旦可执行文件字节被篡改，预检校验也必须报错拦截。
    std::fs::write(new.path().join(&candidate.executable), "tampered").unwrap();
    assert!(preflight(old.path(), new.path()).is_err());
}
#[test]
fn changed_removed_reordered_migrations_and_target_changes_block_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let current = fixture(root.path());
    for case in 0..4 {
        let mut candidate = current.clone();
        match case {
            0 => {
                candidate.application.migrations[0].migrations[0].sqlite_sha256 =
                    super::super::digest(b"changed")
            }
            1 => candidate.application.migrations[0].migrations.clear(),
            2 => candidate.application.migrations[0].migrations[0].version = "renamed".into(),
            _ => candidate.application.build.target_triple = Some("another-target".into()),
        }
        assert!(!compare(&current, &candidate).unwrap().blockers.is_empty());
    }
    let mut duplicate = current.clone();
    duplicate
        .application
        .migrations
        .push(duplicate.application.migrations[0].clone());
    assert!(compare(&current, &duplicate).is_err());
}
#[test]
fn command_requires_both_packages_and_is_offline() {
    use crate::cli::{Cli, Command, ReleaseCommand};
    use clap::Parser;
    assert!(Cli::try_parse_from(["nuono", "release", "preflight", "--candidate", "new"]).is_err());
    let cli = Cli::try_parse_from([
        "nuono",
        "release",
        "preflight",
        "--current",
        "old",
        "--candidate",
        "new",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Release {
            command: ReleaseCommand::Preflight { .. }
        })
    ));
}
