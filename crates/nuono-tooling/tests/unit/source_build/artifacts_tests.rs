use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("nuono-artifact-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn target(&self) -> PathBuf {
        self.0.join("target")
    }
    fn candidate(&self, content: &str, sources: Vec<SourcePlugin>) -> BuildArtifact {
        let binary = self.0.join("cargo-candidate");
        fs::write(&binary, content).unwrap();
        freeze_candidate(&self.target(), "debug", &binary, sources).unwrap()
    }
    fn validated_candidate(
        &self,
        content: &str,
        sources: Vec<SourcePlugin>,
        reuse: bool,
    ) -> ValidatedCandidate {
        let binary = self.0.join("cargo-candidate");
        fs::write(&binary, content).unwrap();
        freeze_candidate_validated(&self.target(), "debug", &binary, sources, reuse).unwrap()
    }
    fn public_contents(&self) -> String {
        fs::read_to_string(public_binary(&self.target(), "debug").unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn atomic_export_preserves_unrelated_tmp_files_and_replaces_tmp_destinations() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::write(&source, b"new executable").unwrap();
    let unrelated = directory.path().join("nuono.tmp");
    fs::write(&unrelated, b"unrelated existing file").unwrap();
    copy_atomic_hashed(&source, &directory.path().join("nuono"), None).unwrap();
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated existing file");
    let destination = directory.path().join("application.tmp");
    fs::write(&destination, b"old executable").unwrap();
    #[cfg(unix)]
    let mut old_file = fs::File::open(&destination).unwrap();
    copy_atomic_hashed(&source, &destination, None).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"new executable");
    #[cfg(unix)]
    {
        let mut old_bytes = Vec::new();
        old_file.read_to_end(&mut old_bytes).unwrap();
        assert_eq!(
            old_bytes, b"old executable",
            "replacement must never truncate the original inode"
        );
    }
    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".application-")
    }));
}

#[test]
fn independent_exports_with_the_same_basename_do_not_share_a_staging_file() {
    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers = ["bin", "exe"].map(|extension| {
        let root = directory.path().to_path_buf();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let contents = extension.repeat(512 * 1024);
            let source = root.join(format!("source-{extension}"));
            let destination = root.join(format!("nuono.{extension}"));
            fs::write(&source, &contents).unwrap();
            barrier.wait();
            copy_atomic_hashed(&source, &destination, None).unwrap();
            assert_eq!(fs::read_to_string(destination).unwrap(), contents);
        })
    });
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn exporting_a_new_target_keeps_the_local_program_and_publication_receipt_unchanged() {
    let fixture = Fixture::new();
    let original = fixture.candidate("local host program", vec![source("first", "old")]);
    publish_candidate(&fixture.target(), &original, None).unwrap();
    let receipt = receipt_path(&fixture.target(), "debug").unwrap();
    let before = fs::read(&receipt).unwrap();
    let candidate =
        fixture.validated_candidate("cross target program", vec![source("first", "new")], true);
    let output = fixture.0.join("dist/application.tmp");
    export_validated_candidate(&fixture.target(), candidate, &output).unwrap();
    assert_eq!(fs::read_to_string(&output).unwrap(), "cross target program");
    assert_eq!(fixture.public_contents(), "local host program");
    assert_eq!(fs::read(receipt).unwrap(), before);
    assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
}

#[test]
fn export_revalidates_copied_bytes_and_preserves_existing_destination_on_failure() {
    let fixture = Fixture::new();
    let candidate = fixture.validated_candidate("verified program", vec![], false);
    let path = candidate_path(&fixture.target(), candidate.artifact()).unwrap();
    fs::write(path, "changed since freezing").unwrap();
    let output = fixture.0.join("exported");
    fs::write(&output, "previous export").unwrap();
    assert!(export_validated_candidate(&fixture.target(), candidate, &output).is_err());
    assert_eq!(fs::read_to_string(output).unwrap(), "previous export");
}

#[test]
fn maintenance_hash_stops_at_a_chunk_boundary_without_accepting_a_partial_digest() {
    let bytes = vec![0x5au8; 3 * 64 * 1024];
    let mut reader = std::io::Cursor::new(&bytes);
    let checks = std::cell::Cell::new(0);
    let interrupted = hash_reader_cancellable(&mut reader, &|| {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    })
    .unwrap();
    assert!(interrupted.is_none());
    assert_eq!(reader.position(), 64 * 1024);
    let digest = hash_reader_cancellable(&mut std::io::Cursor::new(&bytes), &|| false)
        .unwrap()
        .unwrap();
    assert_eq!(digest, hex::encode(Sha256::digest(&bytes)));
}

#[test]
fn interrupted_maintenance_preserves_the_candidate_and_does_not_cache_an_incomplete_scan() {
    let fixture = Fixture::new();
    let bytes = "unreferenced program".repeat(16 * 1024);
    let artifact = fixture.candidate(&bytes, vec![]);
    let candidate = candidate_path(&fixture.target(), &artifact).unwrap();
    let checks = std::cell::Cell::new(0);
    let completed = maintain_artifacts_cancellable(&fixture.0, &fixture.target(), &|| {
        checks.set(checks.get() + 1);
        checks.get() >= 7
    })
    .unwrap();
    assert!(!completed);
    assert_eq!(fs::read_to_string(&candidate).unwrap(), bytes);
    verify_candidate(&fixture.target(), &artifact).unwrap();
    assert!(maintain_artifacts_cancellable(&fixture.0, &fixture.target(), &|| false).unwrap());
    assert!(!candidate.exists());
}
fn source(id: &str, code: &str) -> SourcePlugin {
    SourcePlugin {
        id: id.into(),
        package: id.into(),
        version: "0.1.0".into(),
        source_hash: code.into(),
        ..Default::default()
    }
}

#[test]
fn freezing_a_candidate_never_changes_the_public_program_and_detaches_cargo_output() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![source("first", "old")]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let new = fixture.candidate("new program", vec![source("first", "new")]);
    assert_eq!(fixture.public_contents(), "old program");
    fs::write(
        fixture.0.join("cargo-candidate"),
        "another concurrent cargo build",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(candidate_path(&fixture.target(), &new).unwrap()).unwrap(),
        "new program"
    );
    verify_candidate(&fixture.target(), &new).unwrap();
}

#[test]
fn awaiting_persistence_failure_keeps_the_previous_program_and_recovers_the_prepared_journal() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old", vec![]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let new = fixture.candidate("new", vec![]);
    let _prepared = prepare_publication(&fixture.target(), &new, Some("pending-job")).unwrap();
    let impossible_record = fixture.0.join("blocked-job.json");
    fs::create_dir(&impossible_record).unwrap();
    assert!(
        durable_json(
            &impossible_record,
            &serde_json::json!({"phase":"awaiting_restart", "artifact":new})
        )
        .is_err()
    );
    assert_eq!(fixture.public_contents(), "old");
    recover_publications(&fixture.target(), |_| false).unwrap();
    assert_eq!(fixture.public_contents(), "old");
    assert_eq!(
        read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact
            .id,
        old.id
    );
    assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
}

#[test]
fn committed_awaiting_job_recovers_crashes_before_and_after_the_binary_swap() {
    for already_swapped in [false, true] {
        let fixture = Fixture::new();
        let old = fixture.candidate("old", vec![]);
        publish_candidate(&fixture.target(), &old, None).unwrap();
        let new = fixture.candidate("new", vec![]);
        let publication =
            prepare_publication(&fixture.target(), &new, Some("committed-job")).unwrap();
        if already_swapped {
            copy_atomic(
                &candidate_path(&fixture.target(), &new).unwrap(),
                &public_binary(&fixture.target(), "debug").unwrap(),
            )
            .unwrap();
        }
        recover_publications(&fixture.target(), |record| {
            record.job_id.as_deref() == Some("committed-job")
                && same_artifact(&record.artifact, &publication.artifact)
        })
        .unwrap();
        assert_eq!(fixture.public_contents(), "new");
        assert_eq!(
            read_publication(&fixture.target(), "debug")
                .unwrap()
                .unwrap()
                .artifact
                .id,
            new.id
        );
        assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    }
}

#[test]
fn incomplete_or_failed_publish_restores_the_prior_waiting_artifact_and_its_receipt() {
    let fixture = Fixture::new();
    let old = fixture.candidate("prior awaiting program", vec![source("first", "waiting")]);
    publish_candidate(&fixture.target(), &old, Some("older-committed-job")).unwrap();
    let new = fixture.candidate("failed program", vec![source("second", "new")]);
    let publication = prepare_publication(&fixture.target(), &new, Some("failed-job")).unwrap();
    copy_atomic(
        &candidate_path(&fixture.target(), &new).unwrap(),
        &public_binary(&fixture.target(), "debug").unwrap(),
    )
    .unwrap();
    recover_publications(&fixture.target(), |_| false).unwrap();
    assert_eq!(fixture.public_contents(), "prior awaiting program");
    let restored = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert_eq!(restored.job_id.as_deref(), Some("older-committed-job"));
    assert!(same_artifact(&restored.artifact, &old));
    // 即使在产物已完全落盘发布后才丢失任务确认导致触发回滚，也必须能安全恢复上一版的发布回执。
    let committed = prepare_publication(&fixture.target(), &new, Some("failed-job")).unwrap();
    finish_publication(&fixture.target(), &committed).unwrap();
    rollback_candidate(&fixture.target(), &publication.artifact).unwrap();
    assert_eq!(fixture.public_contents(), "prior awaiting program");
    assert!(same_artifact(
        &read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact,
        &old
    ));
}

#[test]
fn verification_binds_the_whole_plugin_composition_and_detects_binary_tampering() {
    let fixture = Fixture::new();
    let first = source("first", "selected");
    let second = source("second", "old");
    let artifact = fixture.candidate("two plugins", vec![first.clone(), second.clone()]);
    publish_candidate(&fixture.target(), &artifact, None).unwrap();
    let publication = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    verify_publication(&fixture.target(), &publication, &[second, first.clone()]).unwrap();
    assert!(
        verify_publication(
            &fixture.target(),
            &publication,
            &[first.clone(), source("second", "external change")]
        )
        .is_err()
    );
    assert!(verify_publication(&fixture.target(), &publication, &[first]).is_err());
    fs::write(
        public_binary(&fixture.target(), "debug").unwrap(),
        "manually replaced program",
    )
    .unwrap();
    assert!(verify_publication(&fixture.target(), &publication, &artifact.sources).is_err());
    assert!(rollback_candidate(&fixture.target(), &artifact).is_err());
    assert_eq!(fixture.public_contents(), "manually replaced program");
}

#[test]
fn corrupted_candidate_is_never_published_and_last_plugin_removal_is_a_valid_manifest() {
    let fixture = Fixture::new();
    let installed = fixture.candidate("installed", vec![source("last", "old")]);
    publish_candidate(&fixture.target(), &installed, None).unwrap();
    let corrupt = fixture.candidate("candidate", vec![]);
    fs::write(
        candidate_path(&fixture.target(), &corrupt).unwrap(),
        "corrupt",
    )
    .unwrap();
    assert!(publish_candidate(&fixture.target(), &corrupt, Some("job")).is_err());
    assert_eq!(fixture.public_contents(), "installed");
    let empty = fixture.candidate("no compiled business plugins", vec![]);
    publish_candidate(&fixture.target(), &empty, Some("uninstall-last")).unwrap();
    let publication = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    verify_publication(&fixture.target(), &publication, &[]).unwrap();
    assert!(verify_publication(&fixture.target(), &publication, &installed.sources).is_err());
}

#[test]
fn artifact_collection_retains_receipt_backups_pending_jobs_and_modified_files() {
    let fixture = Fixture::new();
    let first = fixture.candidate("first", vec![]);
    publish_candidate(&fixture.target(), &first, Some("first-job")).unwrap();
    let second = fixture.candidate("second", vec![]);
    publish_candidate(&fixture.target(), &second, Some("second-job")).unwrap();
    let current = fixture.candidate("current", vec![]);
    publish_candidate(&fixture.target(), &current, Some("current-job")).unwrap();
    let awaiting = fixture.candidate("awaiting", vec![]);
    let job = fixture.0.join("data/.plugin-jobs/pending.json");
    durable_json(
        &job,
        &serde_json::json!({"phase":"awaiting_restart", "artifact":awaiting}),
    )
    .unwrap();
    let modified = fixture.candidate("user modified", vec![]);
    fs::write(
        artifact_directory(&fixture.target(), &modified)
            .unwrap()
            .join("notes.txt"),
        "keep user data",
    )
    .unwrap();
    maintain_artifacts(&fixture.0, &fixture.target()).unwrap();
    assert!(
        !artifact_directory(&fixture.target(), &first)
            .unwrap()
            .exists()
    );
    for artifact in [&second, &current, &awaiting, &modified] {
        assert!(
            artifact_directory(&fixture.target(), artifact)
                .unwrap()
                .exists()
        );
    }
    let protected = protected_job_ids(&fixture.target()).unwrap();
    assert!(protected.contains(&"current-job".into()) && protected.contains(&"second-job".into()));
    rollback_candidate(&fixture.target(), &current).unwrap();
    let restored = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(same_artifact(&restored.artifact, &second));
    verify_publication(&fixture.target(), &restored, &second.sources).unwrap();
}

#[test]
fn unchanged_launcher_build_reuses_the_receipt_without_copying_or_replacing_backups() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![source("first", "old")]);
    publish_candidate(&fixture.target(), &old, Some("older-job")).unwrap();
    let current = fixture.candidate("current program", vec![source("first", "current")]);
    publish_candidate(&fixture.target(), &current, Some("current-job")).unwrap();
    let directory = artifact_directory(&fixture.target(), &current).unwrap();
    let receipt = receipt_path(&fixture.target(), "debug").unwrap();
    let public = public_binary(&fixture.target(), "debug").unwrap();
    let receipt_before = fs::read(&receipt).unwrap();
    let public_modified = fs::metadata(&public).unwrap().modified().unwrap();
    let previous_modified = fs::metadata(directory.join("previous-application"))
        .unwrap()
        .modified()
        .unwrap();
    let artifact_count = fs::read_dir(fixture.target().join("nuono-app/artifacts"))
        .unwrap()
        .count();

    let candidate = fixture.validated_candidate("current program", current.sources.clone(), true);
    assert!(candidate.is_reused());
    assert!(same_artifact(candidate.artifact(), &current));
    publish_validated_candidate(&fixture.target(), candidate, None).unwrap();
    // 兼容入口在重复发布未变化的普通产物时同样视为空操作（no-op）。
    publish_candidate(&fixture.target(), &current, None).unwrap();
    assert_eq!(fs::read(&receipt).unwrap(), receipt_before);
    assert_eq!(
        fs::metadata(&public).unwrap().modified().unwrap(),
        public_modified
    );
    assert_eq!(
        fs::metadata(directory.join("previous-application"))
            .unwrap()
            .modified()
            .unwrap(),
        previous_modified
    );
    assert_eq!(
        fs::read_dir(fixture.target().join("nuono-app/artifacts"))
            .unwrap()
            .count(),
        artifact_count
    );
    assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    rollback_candidate(&fixture.target(), &current).unwrap();
    assert_eq!(fixture.public_contents(), "old program");
    assert!(same_artifact(
        &read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact,
        &old
    ));
}

#[test]
fn reuse_requires_binary_profile_and_complete_source_identity() {
    let fixture = Fixture::new();
    let old = fixture.candidate("unchanged bytes", vec![source("first", "old source")]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let changed_source = fixture.validated_candidate(
        "unchanged bytes",
        vec![source("first", "changed source")],
        true,
    );
    assert!(!changed_source.is_reused());
    let changed_artifact = changed_source.artifact().clone();
    assert_ne!(changed_artifact.id, old.id);
    publish_validated_candidate(&fixture.target(), changed_source, None).unwrap();
    let changed_binary =
        fixture.validated_candidate("different bytes", changed_artifact.sources.clone(), true);
    assert!(!changed_binary.is_reused());
    let release = freeze_candidate_validated(
        &fixture.target(),
        "release",
        &fixture.0.join("cargo-candidate"),
        changed_artifact.sources.clone(),
        true,
    )
    .unwrap();
    assert!(!release.is_reused());
    assert_eq!(release.artifact().profile, "release");
    rollback_candidate(&fixture.target(), &changed_artifact).unwrap();
    assert!(same_artifact(
        &read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact,
        &old
    ));
}

#[test]
fn reuse_rejects_stored_candidate_and_public_entry_tampering() {
    for corrupt_candidate in [false, true] {
        let fixture = Fixture::new();
        let current = fixture.candidate("verified program", vec![]);
        publish_candidate(&fixture.target(), &current, None).unwrap();
        let path = if corrupt_candidate {
            candidate_path(&fixture.target(), &current).unwrap()
        } else {
            public_binary(&fixture.target(), "debug").unwrap()
        };
        fs::write(&path, "tampered program").unwrap();
        let binary = fixture.0.join("cargo-candidate");
        fs::write(&binary, "verified program").unwrap();
        let attempt = freeze_candidate_validated(&fixture.target(), "debug", &binary, vec![], true);
        if corrupt_candidate {
            assert!(attempt.is_err());
        } else {
            assert!(
                publish_validated_candidate(&fixture.target(), attempt.unwrap(), None).is_err()
            );
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "tampered program");
        assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    }
}

#[test]
fn fresh_validation_proof_cannot_publish_changed_bytes_or_a_changed_manifest() {
    for corrupt_manifest in [false, true] {
        let fixture = Fixture::new();
        let old = fixture.candidate("old program", vec![]);
        publish_candidate(&fixture.target(), &old, None).unwrap();
        let candidate = fixture.validated_candidate("new program", vec![], false);
        if corrupt_manifest {
            let mut manifest = candidate.artifact().clone();
            manifest.sources.push(source("injected", "changed"));
            durable_json(
                &artifact_directory(&fixture.target(), candidate.artifact())
                    .unwrap()
                    .join("manifest.json"),
                &manifest,
            )
            .unwrap();
        } else {
            fs::write(
                candidate_path(&fixture.target(), candidate.artifact()).unwrap(),
                "new program modified after freezing",
            )
            .unwrap();
        }
        assert!(publish_validated_candidate(&fixture.target(), candidate, None).is_err());
        assert_eq!(fixture.public_contents(), "old program");
        assert!(same_artifact(
            &read_publication(&fixture.target(), "debug")
                .unwrap()
                .unwrap()
                .artifact,
            &old
        ));
        assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    }
}

#[test]
fn reused_validation_proof_cannot_authorize_a_changed_receipt_or_a_store_job() {
    for start_store_job in [false, true] {
        let fixture = Fixture::new();
        let current = fixture.candidate("current program", vec![]);
        publish_candidate(&fixture.target(), &current, None).unwrap();
        let candidate = fixture.validated_candidate("current program", vec![], true);
        let job = if start_store_job {
            Some("new-store-job")
        } else {
            let mut publication = read_publication(&fixture.target(), "debug")
                .unwrap()
                .unwrap();
            publication.job_id = Some("changed-authorization".into());
            durable_json(
                &receipt_path(&fixture.target(), "debug").unwrap(),
                &publication,
            )
            .unwrap();
            None
        };
        assert!(publish_validated_candidate(&fixture.target(), candidate, job).is_err());
        assert!(prepare_publication(&fixture.target(), &current, Some("new-job")).is_err());
        assert_eq!(fixture.public_contents(), "current program");
        assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    }
}

#[test]
fn checked_copy_rejects_unexpected_bytes_before_replacing_the_destination() {
    let fixture = Fixture::new();
    let input = fixture.0.join("input");
    let output = fixture.0.join("output");
    fs::write(&input, "original input").unwrap();
    let expected = hash_file(&input).unwrap();
    fs::write(&input, "changed input").unwrap();
    fs::write(&output, "keep destination").unwrap();
    assert!(copy_atomic_hashed(&input, &output, Some(&expected)).is_err());
    assert_eq!(fs::read_to_string(&output).unwrap(), "keep destination");
    assert!(fs::read_dir(&fixture.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".application-")
    }));
}

#[test]
fn rollback_rejects_a_tampered_backup_without_overwriting_the_current_program() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let current = fixture.candidate("current program", vec![]);
    publish_candidate(&fixture.target(), &current, None).unwrap();
    fs::write(
        artifact_directory(&fixture.target(), &current)
            .unwrap()
            .join("previous-application"),
        "tampered backup",
    )
    .unwrap();
    assert!(rollback_candidate(&fixture.target(), &current).is_err());
    assert_eq!(fixture.public_contents(), "current program");
    assert!(same_artifact(
        &read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact,
        &current
    ));
}

#[test]
fn locked_binary_defers_publication_supports_batch_updates_and_materializes_on_restart() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![source("base", "v0")]);
    publish_candidate(&fixture.target(), &old, None).unwrap();

    // 第一次运行中安装：公开程序被占用，转为延迟发布，不覆盖旧入口且不残留事务日志。
    let first = fixture.candidate("first program", vec![source("plugin-a", "v1")]);
    let first_prepared = prepare_publication(&fixture.target(), &first, Some("job-1")).unwrap();
    finish_publication_with_policy(
        &fixture.target(),
        &first_prepared,
        PublishPolicy::SimulateLocked,
    )
    .unwrap();
    assert_eq!(fixture.public_contents(), "old program");
    assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    assert!(
        fs::read_dir(fixture.target().join("debug"))
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".application-")
            })
    );
    let first_receipt = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(first_receipt.deferred_binary);
    assert!(same_artifact(&first_receipt.artifact, &first));
    verify_publication(&fixture.target(), &first_receipt, &first.sources).unwrap();
    maintain_artifacts(&fixture.0, &fixture.target()).unwrap();
    assert!(
        artifact_directory(&fixture.target(), &first)
            .unwrap()
            .exists()
    );
    assert!(
        artifact_directory(&fixture.target(), &old)
            .unwrap()
            .exists()
    );

    // 未重启前继续安装第二个插件：应顺利完成第二次延迟发布，支持批量安装后统一重启。
    let second = fixture.candidate(
        "second program",
        vec![source("plugin-a", "v1"), source("plugin-b", "v2")],
    );
    let second_prepared = prepare_publication(&fixture.target(), &second, Some("job-2")).unwrap();
    finish_publication_with_policy(
        &fixture.target(),
        &second_prepared,
        PublishPolicy::SimulateLocked,
    )
    .unwrap();
    assert_eq!(fixture.public_contents(), "old program");
    let second_receipt = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(second_receipt.deferred_binary);
    assert!(same_artifact(&second_receipt.artifact, &second));
    verify_publication(&fixture.target(), &second_receipt, &second.sources).unwrap();

    // 回滚第二次延迟发布应直接恢复第一次延迟回执，无需覆写被占用的旧程序入口。
    rollback_candidate(&fixture.target(), &second).unwrap();
    assert_eq!(fixture.public_contents(), "old program");
    let restored_first = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(restored_first.deferred_binary);
    assert!(same_artifact(&restored_first.artifact, &first));

    // 重新提交第二次延迟发布，再模拟旧进程退出后的源码启动器接管落盘。
    let second_retry = fixture.candidate(
        "second program",
        vec![source("plugin-a", "v1"), source("plugin-b", "v2")],
    );
    let second_retry_prepared =
        prepare_publication(&fixture.target(), &second_retry, Some("job-2-retry")).unwrap();
    finish_publication_with_policy(
        &fixture.target(),
        &second_retry_prepared,
        PublishPolicy::SimulateLocked,
    )
    .unwrap();

    let restarted =
        fixture.validated_candidate("second program", second_retry.sources.clone(), true);
    assert!(restarted.is_reused());
    assert!(restarted.is_deferred_reuse());
    publish_validated_candidate(&fixture.target(), restarted, None).unwrap();
    assert_eq!(fixture.public_contents(), "second program");
    let materialized = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(!materialized.deferred_binary);
    assert!(same_artifact(&materialized.artifact, &second_retry));
    verify_publication(&fixture.target(), &materialized, &second_retry.sources).unwrap();
}

#[test]
fn deferred_publication_rejects_tampering_and_recovers_interrupted_materialization() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![source("base", "v0")]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let next = fixture.candidate("next program", vec![source("plugin", "v1")]);
    let prepared = prepare_publication(&fixture.target(), &next, Some("job-1")).unwrap();
    finish_publication_with_policy(&fixture.target(), &prepared, PublishPolicy::SimulateLocked)
        .unwrap();

    // 若公开程序被外部篡改，延迟校验与启动器落盘都必须拒绝，且不得覆盖外部修改。
    let public = public_binary(&fixture.target(), "debug").unwrap();
    fs::write(&public, "tampered program").unwrap();
    let receipt = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(verify_publication(&fixture.target(), &receipt, &next.sources).is_err());
    let candidate = fixture.validated_candidate("next program", next.sources.clone(), true);
    assert!(publish_validated_candidate(&fixture.target(), candidate, None).is_err());
    assert_eq!(fixture.public_contents(), "tampered program");

    // 若上次启动器在写入新程序后、更新回执前中断，下次启动应幂等地清除延迟标记。
    fs::write(&public, "next program").unwrap();
    let candidate = fixture.validated_candidate("next program", next.sources, true);
    publish_validated_candidate(&fixture.target(), candidate, None).unwrap();
    let finalized = read_publication(&fixture.target(), "debug")
        .unwrap()
        .unwrap();
    assert!(!finalized.deferred_binary);
    assert_eq!(fixture.public_contents(), "next program");
}

#[test]
fn restore_skips_redundant_overwrite_when_unchanged_and_preserves_both_errors_on_failure() {
    let fixture = Fixture::new();
    let old = fixture.candidate("old program", vec![]);
    publish_candidate(&fixture.target(), &old, None).unwrap();
    let next = fixture.candidate("next program", vec![]);
    let prepared = prepare_publication(&fixture.target(), &next, Some("job-1")).unwrap();

    // 公开程序尚未被覆盖时，即使备份不可写或已变化，回滚也无需触碰公开入口，并应清理事务日志。
    fs::remove_file(
        artifact_directory(&fixture.target(), &next)
            .unwrap()
            .join("previous-application"),
    )
    .unwrap();
    recover_publications(&fixture.target(), |_| false).unwrap();
    assert_eq!(fixture.public_contents(), "old program");
    assert!(!journal_path(&fixture.target(), "debug").unwrap().exists());
    assert!(same_artifact(
        &read_publication(&fixture.target(), "debug")
            .unwrap()
            .unwrap()
            .artifact,
        &old
    ));

    // 当发布与恢复双双失败时，返回错误必须同时保留原始发布原因和恢复失败原因。
    let broken = fixture.candidate("broken program", vec![]);
    let broken_prepared =
        prepare_publication(&fixture.target(), &broken, Some("job-broken")).unwrap();
    fs::write(
        candidate_path(&fixture.target(), &broken).unwrap(),
        "corrupted candidate",
    )
    .unwrap();
    fs::write(
        public_binary(&fixture.target(), "debug").unwrap(),
        "externally modified public binary",
    )
    .unwrap();
    let error = finish_publication(&fixture.target(), &broken_prepared)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("候选构建产物校验失败"),
        "缺少原始发布失败原因: {error}"
    );
    assert!(
        error.contains("公开程序已被其他操作修改"),
        "缺少恢复失败原因: {error}"
    );
    let _ = prepared;
}

#[cfg(windows)]
#[test]
fn windows_locked_binary_errors_match_access_denied_and_sharing_violation() {
    assert!(is_locked_binary_error_parts(
        std::io::ErrorKind::PermissionDenied,
        Some(5)
    ));
    assert!(is_locked_binary_error_parts(
        std::io::ErrorKind::Other,
        Some(32)
    ));
    assert!(!is_locked_binary_error_parts(
        std::io::ErrorKind::NotFound,
        Some(2)
    ));
}
