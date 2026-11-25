use super::*;

#[test]
fn cargo_children_leave_parallelism_to_cargo_and_keep_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("nuono.toml");
    for jobs in [None, Some("1"), Some("8"), Some("-1")] {
        let mut command = Command::new("cargo");
        if let Some(value) = jobs {
            command.env("CARGO_BUILD_JOBS", value);
        }
        apply(&mut command, &config_path).unwrap();
        let actual = command
            .get_envs()
            .find(|(key, _)| *key == "CARGO_BUILD_JOBS");
        assert_eq!(
            actual,
            jobs.map(|value| (OsStr::new("CARGO_BUILD_JOBS"), Some(OsStr::new(value))))
        );
        assert_eq!(command.get_args().len(), 0);
    }
}

#[test]
fn child_cargo_does_not_inherit_the_launchers_build_fingerprint() {
    // 模拟 `cargo run` 注入的父级包环境变量，无需修改当前测试进程的真实全局环境，也不依赖测试的启动方式。
    let outer_package = [
        (
            "OUT_DIR",
            "/workspace/target/release/build/nuono-server/out",
        ),
        ("CARGO_MANIFEST_DIR", "/workspace/crates/nuono-server"),
        ("CARGO_PKG_NAME", "nuono-server"),
        ("CARGO_PKG_VERSION", "0.0.1"),
    ];
    let mut command = Command::new("cargo");
    clear_parent_package_environment(
        &mut command,
        outer_package.map(|(key, _)| OsString::from(key)),
    );
    let removed: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(removed.len(), outer_package.len());
    assert!(removed.values().all(Option::is_none));
    assert!(removed.contains_key(OsStr::new("OUT_DIR")));
    assert!(removed.contains_key(OsStr::new("CARGO_MANIFEST_DIR")));
    assert!(removed.contains_key(OsStr::new("CARGO_PKG_NAME")));
}

#[test]
fn child_cargo_preserves_explicit_build_settings_and_unrelated_environment() {
    let preserved = [
        ("CARGO_INCREMENTAL", "1"),
        ("CARGO_BUILD_INCREMENTAL", "true"),
        ("CARGO_BUILD_JOBS", "4"),
        ("CARGO_TARGET_DIR", "/build/cache"),
        ("CARGO_BUILD_TARGET", "aarch64-apple-darwin"),
        ("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER", "clang"),
        ("CARGO_PROFILE_RELEASE_OPT_LEVEL", "2"),
        ("RUSTFLAGS", "-C debuginfo=0"),
        ("CARGO_ENCODED_RUSTFLAGS", "-C\x1fdebuginfo=0"),
        ("CARGO_ENCODED_RUSTDOCFLAGS", "--cfg\x1fdocs"),
        ("CARGO", "/toolchain/bin/cargo"),
        ("CARGO_HOME", "/toolchain/cargo"),
        ("CARGO_MAKEFLAGS", "--jobserver-auth=3,4"),
        ("RUSTC", "/toolchain/bin/rustc"),
        ("RUSTDOC", "/toolchain/bin/rustdoc"),
        ("RUSTC_WRAPPER", "sccache"),
        ("RUSTC_LINKER", "clang"),
        ("PATH", "/toolchain/bin"),
        ("CUSTOM_APP_VAR", "/assets"),
    ];
    let mut command = Command::new("cargo");
    command.envs(preserved);
    command.env("OUT_DIR", "/outer/build/out");
    clear_parent_package_environment(&mut command, preserved.map(|(key, _)| OsString::from(key)));
    for (key, value) in preserved {
        assert!(
            command
                .get_envs()
                .any(|(actual, actual_value)| actual == key
                    && actual_value == Some(OsStr::new(value))),
            "child Cargo must preserve {key}"
        );
    }
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "OUT_DIR" && value.is_none())
    );
}

#[test]
fn nuono_config_controls_cargo_children_without_changing_process_environment() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nuono.toml");
    let mut command = Command::new("cargo");
    let config = BuildConfig::load(&path).unwrap();
    assert_eq!(
        apply_overrides(&mut command, config.incremental, None, None),
        "开启"
    );
    assert!(
        command
            .get_envs()
            .any(|(key, value)| { key == "CARGO_INCREMENTAL" && value == Some(OsStr::new("1")) })
    );
    assert!(!path.exists());
    std::fs::write(&path, "[build]\nincremental = true\n").unwrap();
    let mut command = Command::new("cargo");
    // 解析与生产环境相同格式的配置文件，通过参数注入环境变量覆盖项，避免修改当前测试进程的全局环境变量。
    let config = BuildConfig::load(&path).unwrap();
    assert_eq!(
        apply_overrides(&mut command, config.incremental, None, None),
        "开启"
    );
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "CARGO_INCREMENTAL" && value == Some("1".as_ref()))
    );
    std::fs::write(&path, "[build]\nincremental = false\n").unwrap();
    let config = BuildConfig::load(&path).unwrap();
    apply_overrides(&mut command, config.incremental, None, None);
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "CARGO_INCREMENTAL" && value == Some("0".as_ref()))
    );
}

#[test]
fn explicit_cargo_environment_takes_precedence_over_nuono_config() {
    for (configured, value) in [(false, "1"), (true, "0")] {
        let mut command = Command::new("cargo");
        apply_overrides(
            &mut command,
            configured,
            Some(value.into()),
            Some("true".into()),
        );
        assert!(
            command
                .get_envs()
                .any(|(key, actual)| key == "CARGO_INCREMENTAL" && actual == Some(value.as_ref()))
        );
        assert!(
            !command
                .get_envs()
                .any(|(key, _)| key == "CARGO_BUILD_INCREMENTAL")
        );
    }
    let mut command = Command::new("cargo");
    apply_overrides(&mut command, false, None, Some("true".into()));
    assert!(
        command
            .get_envs()
            .any(|(key, value)| key == "CARGO_BUILD_INCREMENTAL" && value == Some("true".as_ref()))
    );
    assert!(
        !command
            .get_envs()
            .any(|(key, _)| key == "CARGO_INCREMENTAL")
    );
}
