use super::{STOP_REQUESTED, restart_exit_code};
use anyhow::{Context, Result};
use rustix::process::{Pid, Signal, kill_process_group};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::{atomic::Ordering, mpsc},
    thread::JoinHandle,
};
use tokio::signal::unix::{SignalKind, signal};

enum RestartCommand {
    Program,
    Cargo {
        program: OsString,
        manifest: PathBuf,
        profile: String,
        target: Option<String>,
    },
}

struct Recipe {
    executable: PathBuf,
    directory: PathBuf,
    arguments: Vec<OsString>,
    restart: RestartCommand,
}

impl Recipe {
    fn current() -> Result<Self> {
        let executable = std::env::current_exe().context("读取启动程序路径失败")?;
        let directory = std::env::current_dir().context("读取启动目录失败")?;
        let mut restart = RestartCommand::Program;
        if let Some(root) = crate::find_nuono_repository_root(&directory) {
            let target_dir = crate::source_build::target_dir(&root)?;
            if super::managed_source_entry(&executable, &target_dir) {
                let canonical_target = target_dir.canonicalize()?;
                let layout = executable.canonicalize()?;
                let profile_parent = layout.parent().and_then(|path| path.parent());
                let target = Some(env!("NUONO_BUILD_TARGET"))
                    .filter(|triple| {
                        profile_parent == Some(canonical_target.join(triple).as_path())
                    })
                    .map(str::to_owned);
                restart = RestartCommand::Cargo {
                    program: std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
                    manifest: root.join("Cargo.toml"),
                    profile: match env!("NUONO_BUILD_PROFILE") {
                        "debug" => "dev".into(),
                        other => other.into(),
                    },
                    target,
                };
            }
        }
        Ok(Self {
            executable,
            directory,
            arguments: std::env::args_os().skip(1).collect(),
            restart,
        })
    }

    fn command(&self, first: bool) -> Result<Command> {
        let mut command = match (&self.restart, first) {
            (
                RestartCommand::Cargo {
                    program,
                    manifest,
                    profile,
                    target,
                },
                false,
            ) => {
                let mut command = restart_cargo(program, |key| std::env::var_os(key))?;
                command
                    .arg("run")
                    .arg("--manifest-path")
                    .arg(manifest)
                    .args(["--bin", "nuono", "--profile", profile]);
                if let Some(target) = target {
                    command.arg("--target").arg(target);
                }
                command.arg("--");
                crate::build_config::clear_parent_package_environment(
                    &mut command,
                    std::env::vars_os().map(|(key, _)| key),
                );
                // 新一轮 Cargo 进程会自行创建 jobserver；外层 Cargo 首次调用时留下的文件描述符编号此时已不再打开，必须移除。
                command.env_remove("CARGO_MAKEFLAGS");
                command
            }
            _ => Command::new(&self.executable),
        };
        command
            .current_dir(&self.directory)
            .args(&self.arguments)
            .env("NUONO_SUPERVISED", "1")
            .env_remove("NUONO_STARTUP_ELAPSED_MS");
        Ok(command)
    }
}

fn restart_cargo(
    program: &OsStr,
    environment: impl Fn(&str) -> Option<OsString>,
) -> Result<Command> {
    let home = environment("HOME").map(PathBuf::from);
    let cargo_home = environment("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
    let rustup_home = environment("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".rustup")));
    let proxy = cargo_home.map(|home| home.join("bin/cargo"));
    let toolchains = rustup_home.map(|home| home.join("toolchains"));
    let inherited_toolchain = environment("RUSTUP_TOOLCHAIN");
    let direct_bin = Path::new(program).parent().filter(|bin| {
        bin.file_name() == Some(OsStr::new("bin"))
            && toolchains
                .as_deref()
                .is_some_and(|toolchains| bin.parent().and_then(Path::parent) == Some(toolchains))
    });
    let uses_rustup = direct_bin.is_some()
        || proxy.as_deref() == Some(Path::new(program))
        || (program == "cargo" && inherited_toolchain.is_some());
    if !uses_rustup {
        return Ok(Command::new(program));
    }

    // Cargo 启动子进程时会把具体工具链下的 cargo 路径和 `RUSTUP_TOOLCHAIN` 导出到环境变量中。
    // 重启时清除这些旧的工具链锁定并重新走 rustup 代理入口，确保常驻守护进程在仓库 `rust-toolchain.toml` 更新后能切换到新工具链。
    let proxy = proxy.filter(|path| path.is_file());
    let mut command = Command::new(proxy.as_deref().unwrap_or_else(|| Path::new("cargo")));
    command.env_remove("CARGO").env_remove("RUSTUP_TOOLCHAIN");
    let old_bin = direct_bin
        .map(Path::to_path_buf)
        .or_else(|| Some(toolchains?.join(inherited_toolchain?).join("bin")));
    if let Some(old_bin) = old_bin {
        for key in ["RUSTC", "RUSTDOC"] {
            if environment(key).is_some_and(|value| Path::new(&value).parent() == Some(&old_bin)) {
                command.env_remove(key);
            }
        }
        if let Some(path) = environment("PATH") {
            let paths = std::env::split_paths(&path).filter(|entry| entry != &old_bin);
            command.env(
                "PATH",
                std::env::join_paths(paths).context("重建 Cargo 搜索路径失败")?,
            );
        }
    }
    Ok(command)
}

pub fn supervise_if_needed() -> Result<Option<i32>> {
    if std::env::var("NUONO_SUPERVISED").is_ok_and(|value| value == "1") {
        return Ok(None);
    }
    let recipe = Recipe::current()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(supervise(
            |first| recipe.command(first),
            restart_exit_code(),
        ))
        .map(Some)
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn forward(group: Pid, signal: Signal) -> Result<()> {
    match kill_process_group(group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn supervise(
    mut command: impl FnMut(bool) -> Result<Command>,
    restart_code: i32,
) -> Result<i32> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut first = true;
    loop {
        let mut command = tokio::process::Command::from(command(first)?);
        command.process_group(0).kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("无法启动 Nuono 或 Cargo，请检查启动程序和工具链")?;
        let group = Pid::from_raw(child.id().context("子进程没有 PID")? as i32)
            .context("子进程 PID 无效")?;
        let mut stopping = false;
        let status = loop {
            tokio::select! {
                biased;
                _ = interrupt.recv() => { stopping = true; forward(group, Signal::INT)?; }
                _ = terminate.recv() => { stopping = true; forward(group, Signal::TERM)?; }
                status = child.wait() => break status.context("等待 Nuono 退出失败")?,
            }
        };
        if stopping {
            return Ok(0);
        }
        let code = exit_code(status);
        if code != restart_code {
            return Ok(code);
        }
        eprintln!("[启动器] Nuono 已退出，正在重新执行启动命令…");
        first = false;
    }
}

/// 在业务服务运行时尚未启动的同步源码编译阶段捕获终止信号，
/// 通知当前构建流程终止并清理其派生的独立 Cargo/Git 子进程组。
pub struct StartupSignals {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl StartupSignals {
    pub fn install() -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("nuono-startup-signals".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async {
                        let mut interrupt = signal(SignalKind::interrupt())?;
                        let mut terminate = signal(SignalKind::terminate())?;
                        let _ = ready_tx.send(Ok(()));
                        loop {
                            tokio::select! {
                                biased;
                                _ = &mut stopped => return Ok(()),
                                _ = interrupt.recv() => {},
                                _ = terminate.recv() => {},
                            }
                            STOP_REQUESTED.store(true, Ordering::Release);
                            crate::source_build::shutdown_operations();
                        }
                    })
                })();
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error));
                }
            })?;
        // 确保在首次执行构建或恢复操作前，信号监听已注册就绪。
        ready_rx.recv().context("启动信号监听线程提前退出")??;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for StartupSignals {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/launcher/unix/tests.rs"]
mod tests;
