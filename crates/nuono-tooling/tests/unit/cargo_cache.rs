use super::*;

fn marker(target: &Path, profile: &str, name: &str) -> PathBuf {
    let directory = target.join(profile);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, "").unwrap();
    path
}

#[test]
fn discovers_custom_profiles_targets_and_both_lock_types_without_visiting_build_units() {
    let root = tempfile::tempdir().unwrap();
    for (profile, lock) in [
        ("debug", ".cargo-lock"),
        ("production", ".cargo-build-lock"),
        ("custom-target/production", ".cargo-build-lock"),
    ] {
        marker(root.path(), profile, lock);
    }
    // 编译单元内部子目录不是顶层 Cargo profile 目录，扫描时必须忽略。
    marker(root.path(), "debug/build/package/hash/out", ".cargo-lock");
    fs::create_dir_all(root.path().join("nuono-app/artifacts")).unwrap();
    let locks = lock_build_directories(root.path()).unwrap();
    assert_eq!(
        locks.profiles(),
        ["custom-target/production", "debug", "production"].map(|p| root.path().join(p))
    );
    for profile in locks.profiles() {
        for name in LOCK_NAMES {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(profile.join(name))
                .unwrap();
            assert!(
                file.try_lock_exclusive().is_err(),
                "unlocked {}",
                profile.join(name).display()
            );
        }
    }
    drop(locks);
    assert!(lock_build_directories(root.path()).is_ok());
}

#[test]
fn startup_recovery_refuses_custom_profiles_and_shared_cache_locks() {
    for (profile, name) in [
        ("production", ".cargo-lock"),
        ("custom-target/debug", ".cargo-build-lock"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let path = marker(&target, profile, name);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.lock_exclusive().unwrap();
        let error = crate::recovery::recover_before_build(root.path()).unwrap_err();
        assert!(error.contains("构建正在运行"), "{error}");
        FileExt::unlock(&file).unwrap();
        crate::recovery::recover_before_build(root.path()).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn linked_profiles_and_lock_files_are_rejected_without_touching_the_destination() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("keep"), "keep").unwrap();
    symlink(external.path(), root.path().join("production")).unwrap();
    assert!(lock_build_directories(root.path()).is_err());
    fs::remove_file(root.path().join("production")).unwrap();
    fs::create_dir(root.path().join("production")).unwrap();
    symlink(
        external.path().join("keep"),
        root.path().join("production/.cargo-build-lock"),
    )
    .unwrap();
    assert!(lock_build_directories(root.path()).is_err());
    assert_eq!(
        fs::read_to_string(external.path().join("keep")).unwrap(),
        "keep"
    );
    assert!(!external.path().join(".cargo-lock").exists());
}
