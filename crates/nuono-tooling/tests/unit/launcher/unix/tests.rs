use super::*;
use std::{
    fs,
    io::Write,
    process::{Child, Stdio},
    time::{Duration, Instant},
};

const FIXTURE: &str = "launcher::unix::tests::fixture_entry";
const MODE: &str = "NUONO_TEST_LAUNCHER_MODE";
const ROOT: &str = "NUONO_TEST_LAUNCHER_ROOT";

struct Running(Child);

impl Running {
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "fixture did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let pid = Pid::from_raw(self.0.id() as i32).unwrap();
            let _ = rustix::process::kill_process(pid, Signal::TERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn fixture_command(root: &std::path::Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", FIXTURE, "--nocapture"])
        .env(MODE, mode)
        .env(ROOT, root)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn spawn(root: &std::path::Path, mode: &str) -> Running {
    use std::os::unix::process::CommandExt;
    let mut command = fixture_command(root, mode);
    command.process_group(0);
    Running(command.spawn().unwrap())
}

fn wait_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fixture_entry() {
    let Some(mode) = std::env::var_os(MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let mode = mode.to_str().unwrap();
    if mode == "toolchain-upgrade" {
        let recipe = Recipe {
            executable: std::env::current_exe().unwrap(),
            directory: root.clone(),
            arguments: vec!["--exact".into(), FIXTURE.into(), "--nocapture".into()],
            restart: RestartCommand::Cargo {
                program: std::env::var_os("CARGO").unwrap(),
                manifest: root.join("Cargo.toml"),
                profile: "dev".into(),
                target: None,
            },
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime
            .block_on(supervise(
                |first| {
                    let mut command = recipe.command(first)?;
                    command.env(MODE, "upgrade-worker");
                    Ok(command)
                },
                75,
            ))
            .unwrap();
        std::process::exit(code);
    }
    if mode == "upgrade-worker" {
        assert!(std::env::var_os("RUSTUP_TOOLCHAIN").is_some());
        fs::rename(
            root.join("next-toolchain.toml"),
            root.join("rust-toolchain.toml"),
        )
        .unwrap();
        std::process::exit(75);
    }
    if mode.starts_with("supervisor-") {
        let child_mode = mode.trim_start_matches("supervisor-").to_owned();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime
            .block_on(supervise(
                |first| {
                    let mut command = fixture_command(&root, &child_mode);
                    command.env(
                        "NUONO_TEST_LAUNCHER_FIRST",
                        if first { "yes" } else { "no" },
                    );
                    Ok(command)
                },
                75,
            ))
            .unwrap();
        std::process::exit(code);
    }
    let mut runs = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("runs"))
        .unwrap();
    writeln!(
        runs,
        "{}",
        std::env::var("NUONO_TEST_LAUNCHER_FIRST").unwrap()
    )
    .unwrap();
    drop(runs);
    match mode {
        "restart" => {
            // 模拟业务服务持有的数据文件锁和监听端口：新一代子进程必须等旧工作进程彻底退出释放资源后才能成功获取。
            use fs2::FileExt;
            let lease = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(root.join("lease"))
                .unwrap();
            lease.try_lock_exclusive().unwrap();
            if std::env::var("NUONO_TEST_LAUNCHER_FIRST").unwrap() == "yes" {
                let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                fs::write(root.join("port"), socket.local_addr().unwrap().to_string()).unwrap();
                std::process::exit(75);
            }
            let _socket =
                std::net::TcpListener::bind(fs::read_to_string(root.join("port")).unwrap())
                    .unwrap();
            fs::write(root.join("restarted"), b"ok").unwrap();
        }
        "failure" => std::process::exit(101),
        "restart-failure" => std::process::exit(
            if std::env::var("NUONO_TEST_LAUNCHER_FIRST").unwrap() == "yes" {
                75
            } else {
                101
            },
        ),
        "repeat" => {
            if fs::read_to_string(root.join("runs"))
                .unwrap()
                .lines()
                .count()
                < 4
            {
                std::process::exit(75);
            }
        }
        "normal" => {}
        "interrupt" => {
            let _signals = StartupSignals::install().unwrap();
            fs::write(root.join("ready"), b"yes").unwrap();
            while !super::super::shutdown_requested() {
                std::thread::sleep(Duration::from_millis(5));
            }
            // 即便子进程在收到停机信号的同时恰好以重启码退出，守护父进程也绝不能再次拉起新进程。
            std::process::exit(75);
        }
        "build" => {
            let _signals = StartupSignals::install().unwrap();
            let mut command = Command::new("sh");
            command
                .args([
                    "-c",
                    "echo $$ > compiler; sleep 120 & echo $! > rustc; touch ready; wait",
                ])
                .current_dir(&root);
            let output: crate::source_build::BuildOutput = std::sync::Arc::new(|_| {});
            assert!(crate::source_build::run_logged(&mut command, output).is_err());
        }
        other => panic!("unknown fixture {other}"),
    }
}

#[test]
fn restart_waits_for_old_process_socket_and_lease_and_selects_restart_invocation() {
    let root = tempfile::tempdir().unwrap();
    let mut process = spawn(root.path(), "supervisor-restart");
    assert!(process.wait().success());
    assert!(root.path().join("restarted").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("runs")).unwrap(),
        "yes\nno\n"
    );
}

#[test]
fn normal_exit_and_compiler_failure_are_not_restarted() {
    for (mode, code) in [("normal", 0), ("failure", 101)] {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            spawn(root.path(), &format!("supervisor-{mode}"))
                .wait()
                .code(),
            Some(code)
        );
        assert_eq!(
            fs::read_to_string(root.path().join("runs")).unwrap(),
            "yes\n"
        );
    }
}

#[test]
fn successive_restarts_and_failure_during_relaunch_have_bounded_invocations() {
    for (mode, code, expected) in [
        ("repeat", 0, "yes\nno\nno\nno\n"),
        ("restart-failure", 101, "yes\nno\n"),
    ] {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            spawn(root.path(), &format!("supervisor-{mode}"))
                .wait()
                .code(),
            Some(code)
        );
        assert_eq!(
            fs::read_to_string(root.path().join("runs")).unwrap(),
            expected
        );
    }
}

#[test]
fn stopping_parent_takes_priority_over_child_restart_code() {
    let root = tempfile::tempdir().unwrap();
    let mut process = spawn(root.path(), "supervisor-interrupt");
    wait_file(&root.path().join("ready"));
    rustix::process::kill_process(Pid::from_raw(process.0.id() as i32).unwrap(), Signal::INT)
        .unwrap();
    assert!(process.wait().success());
    assert_eq!(
        fs::read_to_string(root.path().join("runs")).unwrap(),
        "yes\n"
    );
}

#[test]
fn stop_during_source_build_reaps_separate_compiler_process_group() {
    let root = tempfile::tempdir().unwrap();
    let mut process = spawn(root.path(), "supervisor-build");
    wait_file(&root.path().join("ready"));
    let pid = |name| {
        Pid::from_raw(
            fs::read_to_string(root.path().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        )
        .unwrap()
    };
    let compiler = pid("compiler");
    let rustc = pid("rustc");
    rustix::process::kill_process(Pid::from_raw(process.0.id() as i32).unwrap(), Signal::TERM)
        .unwrap();
    assert!(process.wait().success());
    assert!(rustix::process::test_kill_process(compiler).is_err());
    // 在 Linux 容器中，被强杀的孙进程可能会由 1 号进程（init）接管并暂时处于僵尸进程（Z）状态；
    // 僵尸进程已不再执行代码，其最终 wait 清理由 init 负责，而非 Nuono。
    let still_running = |pid: Pid| {
        #[cfg(target_os = "linux")]
        {
            if rustix::process::test_kill_process(pid).is_err() {
                return false;
            }
            let stat =
                fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_pid())).unwrap_or_default();
            !stat.is_empty()
                && !stat
                    .rsplit_once(')')
                    .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
        }
        #[cfg(not(target_os = "linux"))]
        {
            rustix::process::test_kill_process(pid).is_ok()
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while still_running(rustc) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!still_running(rustc));
}

#[test]
fn cargo_recipe_keeps_paths_arguments_profile_and_target_without_shell_parsing() {
    let recipe = Recipe {
        executable: "/tmp/root with space/nuono".into(),
        directory: "/tmp/root with space".into(),
        arguments: vec![
            "--config".into(),
            "file with spaces.toml".into(),
            "--port".into(),
            "9934".into(),
        ],
        restart: RestartCommand::Cargo {
            program: "/toolchain/cargo".into(),
            manifest: "/repo/Cargo.toml".into(),
            profile: "release".into(),
            target: Some("x86_64-unknown-linux-gnu".into()),
        },
    };
    let initial = recipe.command(true).unwrap();
    assert_eq!(initial.get_program(), recipe.executable.as_os_str());
    let restart = recipe.command(false).unwrap();
    assert_eq!(restart.get_program(), "/toolchain/cargo");
    let args: Vec<_> = restart
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "run",
            "--manifest-path",
            "/repo/Cargo.toml",
            "--bin",
            "nuono",
            "--profile",
            "release",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--",
            "--config",
            "file with spaces.toml",
            "--port",
            "9934"
        ]
    );
    assert_eq!(restart.get_current_dir(), Some(recipe.directory.as_path()));
}

#[test]
fn restart_cargo_refreshes_rustup_selection_and_keeps_custom_build_settings() {
    let root = tempfile::tempdir().unwrap();
    let cargo_home = root.path().join("cargo home");
    let rustup_home = root.path().join("rustup home");
    let old_bin = rustup_home.join("toolchains/old/bin");
    let proxy = cargo_home.join("bin/cargo");
    fs::create_dir_all(proxy.parent().unwrap()).unwrap();
    fs::write(&proxy, "proxy").unwrap();
    let mut environment = std::collections::BTreeMap::from([
        ("CARGO_HOME", cargo_home.into_os_string()),
        ("RUSTUP_HOME", rustup_home.into_os_string()),
        ("RUSTUP_TOOLCHAIN", "old".into()),
        ("CARGO", old_bin.join("cargo").into_os_string()),
        ("RUSTC", old_bin.join("rustc").into_os_string()),
        ("RUSTDOC", old_bin.join("rustdoc").into_os_string()),
        ("RUSTC_WRAPPER", "/custom/sccache".into()),
        ("CARGO_BUILD_JOBS", "3".into()),
        (
            "PATH",
            std::env::join_paths([old_bin.as_path(), Path::new("/usr/bin")]).unwrap(),
        ),
    ]);
    // 无论是从 `cargo run` 继承的具体工具链 cargo 路径，还是 rustup 代理入口，
    // 在仓库的 `rust-toolchain.toml` 更新后都必须重新选择新工具链。
    for program in [old_bin.join("cargo"), proxy.clone(), "cargo".into()] {
        let command =
            restart_cargo(program.as_os_str(), |key| environment.get(key).cloned()).unwrap();
        assert_eq!(command.get_program(), proxy.as_os_str());
        let changes: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in ["CARGO", "RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC"] {
            assert_eq!(changes.get(OsStr::new(key)), Some(&None), "{key}");
        }
        assert_eq!(
            changes.get(OsStr::new("PATH")),
            Some(&Some(OsStr::new("/usr/bin")))
        );
        for key in ["RUSTC_WRAPPER", "CARGO_BUILD_JOBS"] {
            assert!(!changes.contains_key(OsStr::new(key)), "{key}");
        }
    }
    environment.insert("RUSTC", "/custom/rustc".into());
    environment.insert("RUSTDOC", "/custom/rustdoc".into());
    let command = restart_cargo(old_bin.join("cargo").as_os_str(), |key| {
        environment.get(key).cloned()
    })
    .unwrap();
    assert!(
        command
            .get_envs()
            .all(|(key, _)| key != "RUSTC" && key != "RUSTDOC")
    );
    let custom = restart_cargo(OsStr::new("/custom/cargo"), |key| {
        environment.get(key).cloned()
    })
    .unwrap();
    assert_eq!(custom.get_program(), "/custom/cargo");
    assert_eq!(custom.get_envs().count(), 0);
}

#[test]
fn source_restart_uses_updated_toolchain_after_old_worker_exits() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};

    let root = tempfile::Builder::new()
        .prefix("nuono toolchain upgrade ")
        .tempdir()
        .unwrap();
    let cargo_home = root.path().join("cargo home");
    let rustup_home = root.path().join("rustup home");
    let old_bin = rustup_home.join("toolchains/old/bin");
    let proxy = cargo_home.join("bin/cargo");
    fs::create_dir_all(proxy.parent().unwrap()).unwrap();
    fs::create_dir_all(&old_bin).unwrap();
    let old_cargo = old_bin.join("cargo");
    fs::write(&old_cargo, "#!/bin/sh\nexit 101\n").unwrap();
    fs::write(
        &proxy,
        r#"#!/bin/sh
set -eu
test -z "${CARGO+x}"
test -z "${RUSTUP_TOOLCHAIN+x}"
test -z "${RUSTC+x}"
test -z "${RUSTDOC+x}"
test "$CARGO_BUILD_JOBS" = 3
test "$RUSTC_WRAPPER" = /custom/sccache
test "$1" = run
cat rust-toolchain.toml > selected-toolchain
"#,
    )
    .unwrap();
    for program in [&old_cargo, &proxy] {
        fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = 'old'\n",
    )
    .unwrap();
    let updated = "[toolchain]\nchannel = 'new'\n";
    fs::write(root.path().join("next-toolchain.toml"), updated).unwrap();
    let mut command = fixture_command(root.path(), "toolchain-upgrade");
    command
        .env("CARGO", old_cargo)
        .env("CARGO_HOME", cargo_home)
        .env("RUSTUP_HOME", rustup_home)
        .env("RUSTUP_TOOLCHAIN", "old")
        .env("RUSTC", old_bin.join("rustc"))
        .env("RUSTDOC", old_bin.join("rustdoc"))
        .env("CARGO_BUILD_JOBS", "3")
        .env("RUSTC_WRAPPER", "/custom/sccache")
        .env(
            "PATH",
            std::env::join_paths([old_bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
        )
        .process_group(0);
    assert!(Running(command.spawn().unwrap()).wait().success());
    assert_eq!(
        fs::read_to_string(root.path().join("selected-toolchain")).unwrap(),
        updated
    );
}
