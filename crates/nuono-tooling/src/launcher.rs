//! 本地子进程守护与重启控制（Cargo 与源码构建策略仍保留在原有的启动路径中）。
//!
//! Unix 工作子进程运行在独立的进程组中。守护父进程本身不持有任何服务端口租约、
//! 数据库连接或插件实例，仅当子进程以约定的重启退出码退出时才重新拉起新一轮进程。
use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn shutdown_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Acquire)
}

fn parse_restart_code(value: Option<&str>) -> i32 {
    // 1/2 为常规启动或 CLI 参数错误，101 为 Cargo 编译失败或 panic，>=128 为信号终止退出码；
    // 排除这些常见错误码，防止异常崩溃时意外陷入无限重启循环。
    value
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|code| (3..=125).contains(code) && *code != 101)
        .unwrap_or(75)
}

pub fn restart_exit_code() -> i32 {
    parse_restart_code(std::env::var("NUONO_RESTART_EXIT_CODE").ok().as_deref())
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{StartupSignals, supervise_if_needed};

// 非 Unix 平台没有进程组与信号语义，这里只做最基础的守护：子进程以约定的重启退出码退出时
// 重新拉起自己（重新走一遍启动准备，从而完成构建发布与「重启并生效」），其余退出码原样返回。
#[cfg(not(unix))]
pub fn supervise_if_needed() -> anyhow::Result<Option<i32>> {
    use std::process::Command;
    if std::env::var("NUONO_SUPERVISED").is_ok_and(|value| value == "1") {
        return Ok(None);
    }
    loop {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(std::env::args_os().skip(1))
            .env("NUONO_SUPERVISED", "1")
            .env_remove("NUONO_STARTUP_ELAPSED_MS");
        let status = command.status()?;
        if status.code() != Some(restart_exit_code()) {
            return Ok(Some(status.code().unwrap_or(1)));
        }
        eprintln!("[启动器] Nuono 已退出，正在重新执行启动命令…");
    }
}

#[cfg(not(unix))]
pub struct StartupSignals;

#[cfg(not(unix))]
impl StartupSignals {
    pub fn install() -> anyhow::Result<Self> {
        Ok(Self)
    }
}

pub fn managed_source_entry(executable: &std::path::Path, target: &std::path::Path) -> bool {
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    let Ok(executable) = executable.canonicalize() else {
        return false;
    };
    executable.starts_with(target)
        && executable.file_name().is_some_and(|name| {
            ["nuono", "nuono-application"].into_iter().any(|expected| {
                name == format!("{expected}{}", std::env::consts::EXE_SUFFIX).as_str()
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_codes_cannot_alias_cargo_cli_or_signal_failure() {
        for invalid in [
            None,
            Some(""),
            Some("0"),
            Some("1"),
            Some("2"),
            Some("101"),
            Some("130"),
            Some("256"),
            Some("-1"),
            Some("oops"),
        ] {
            assert_eq!(parse_restart_code(invalid), 75);
        }
        assert_eq!(parse_restart_code(Some("76")), 76);
    }
}
