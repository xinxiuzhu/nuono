use super::*;
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("crates/nuono-server")).unwrap();
    fs::write(dir.path().join("crates/nuono-server/Cargo.toml"), "").unwrap();
    fs::create_dir_all(dir.path().join("target/debug/deps")).unwrap();
    fs::write(dir.path().join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(dir.path().join("target/debug/deps/lib.rlib"), "artifact").unwrap();
    dir
}
#[test]
fn clean_keeps_program_and_data_and_refuses_build_lock() {
    let dir = fixture();
    let root = dir.path();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join("data/important"), "keep").unwrap();
    fs::write(root.join("target/debug/nuono"), "program").unwrap();
    fs::create_dir_all(root.join("target/nuono-app")).unwrap();
    let lock = acquire(&root.join("target/nuono-app/build.lock")).unwrap();
    assert!(clean(root).is_err());
    assert!(root.join("target/debug/deps/lib.rlib").exists());
    // 同进程内并行运行的测试可能会 spawn 子进程并继承已打开的文件描述符；因此在 drop 前必须显式调用 `unlock` 释放文件锁。
    FileExt::unlock(&lock).unwrap();
    drop(lock);
    let cargo_lock = acquire(&root.join("target/debug/.cargo-lock")).unwrap();
    assert!(clean(root).is_err());
    FileExt::unlock(&cargo_lock).unwrap();
    drop(cargo_lock);
    assert_eq!(clean(root).unwrap().removed_directories, 1);
    assert!(root.join("data/important").exists());
    assert!(root.join("target/debug/nuono").exists());
    assert!(root.join("target/nuono-app/build.lock").exists());
}
#[test]
fn custom_profiles_and_shared_build_cache_locks_block_all_cleanup() {
    for relative in [
        "target/production/.cargo-lock",
        "target/debug/.cargo-build-lock",
        "target/custom-target/production/.cargo-build-lock",
    ] {
        let dir = fixture();
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = acquire(&path).unwrap();
        assert!(clean(dir.path()).is_err(), "ignored active lock {relative}");
        assert!(dir.path().join("target/debug/deps/lib.rlib").exists());
        FileExt::unlock(&lock).unwrap();
    }
}
#[test]
fn an_external_cargo_build_blocks_cleanup() {
    for (profile, shared_cache) in [
        ("dev", false),
        ("production", false),
        ("dev", true),
        ("production", true),
    ] {
        assert_external_cargo_blocks_cleanup(profile, shared_cache);
    }
}

fn assert_external_cargo_blocks_cleanup(profile: &str, shared_cache: bool) {
    struct RunningCargo(std::process::Child);
    impl Drop for RunningCargo {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = fixture();
    let root = dir.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn example() {}\n").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'cache-clean-lock-fixture'\nversion = '0.1.0'\nedition = '2021'\n[profile.production]\ninherits='dev'\n",
    )
    .unwrap();
    fs::write(
        root.join("build.rs"),
        r#"fn main() {
            let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
            std::fs::write(root.join(".building"), "ready").unwrap();
            let started = std::time::Instant::now();
            while !root.join(".finish").exists() && started.elapsed().as_secs() < 20 {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }"#,
    )
    .unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = RunningCargo(
        std::process::Command::new(cargo)
            .args(["build", "--offline", "--target-dir"])
            .arg(root.join(if shared_cache {
                "other-target"
            } else {
                "target"
            }))
            .args(["--profile", profile])
            .env("CARGO_BUILD_BUILD_DIR", root.join("target"))
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !root.join(".building").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "fixture build exited"
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "fixture build timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let result = clean(root);
    let recovery = nuono_tooling::recovery::recover_before_build(root);
    fs::write(root.join(".finish"), "done").unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(result.unwrap_err().contains("构建正在运行"));
    assert!(recovery.unwrap_err().contains("构建正在运行"));
    assert!(root.join("target/debug/deps/lib.rlib").exists());
}

#[test]
fn clean_handles_new_layout_custom_profiles_and_explicit_targets() {
    let dir = fixture();
    for profile in ["production", "custom-target/production"] {
        let directory = dir.path().join("target").join(profile);
        fs::create_dir_all(directory.join("build/package/hash/out")).unwrap();
        fs::write(directory.join(".cargo-build-lock"), "").unwrap();
        fs::write(directory.join("nuono"), "program").unwrap();
        fs::write(
            directory.join("build/package/hash/out/library.rlib"),
            "cache",
        )
        .unwrap();
    }
    fs::create_dir_all(dir.path().join("target/nuono-app/artifacts/frozen")).unwrap();
    fs::write(
        dir.path().join("target/nuono-app/artifacts/frozen/nuono"),
        "frozen program",
    )
    .unwrap();
    assert_eq!(clean(dir.path()).unwrap().removed_directories, 3);
    for profile in ["production", "custom-target/production"] {
        let directory = dir.path().join("target").join(profile);
        assert!(!directory.join("build").exists());
        assert!(directory.join("nuono").exists());
    }
    assert!(
        dir.path()
            .join("target/nuono-app/artifacts/frozen/nuono")
            .exists()
    );
}
#[cfg(unix)]
#[test]
fn scan_and_cleanup_never_follow_symlinked_targets() {
    let dir = fixture();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("secret"), "preserve").unwrap();
    std::os::unix::fs::symlink(external.path(), dir.path().join("target/release")).unwrap();
    let value = category("x", "x", vec![dir.path().join("target")]);
    assert!(value.partial);
    assert_eq!(value.bytes, 8);
    assert!(clean(dir.path()).is_err());
    assert!(external.path().join("secret").exists());
    assert!(dir.path().join("target/debug/deps/lib.rlib").exists());
}
#[test]
fn scan_depth_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut deep = dir.path().to_path_buf();
    for _ in 0..30 {
        deep.push("child");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("hidden"), "xx").unwrap();
    let value = category("x", "x", vec![dir.path().into()]);
    assert!(value.partial);
    assert_eq!(value.files, 0);
}
