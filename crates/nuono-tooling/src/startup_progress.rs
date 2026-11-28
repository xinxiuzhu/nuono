//! 日志系统（tracing）初始化前后的启动阶段耗时统计与进度输出（支持跨越启动器 `exec` 边界累计耗时）。
//!
//! 从环境变量继承的耗时仅用于展示日志提示，绝不作为产物授权依据，也不影响二进制选择或任何启动校验逻辑。
use std::{
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const ELAPSED_ENV: &str = "NUONO_STARTUP_ELAPSED_MS";
const MAX_INHERITED_MS: u64 = 7 * 24 * 60 * 60 * 1000;

struct StartupClock {
    started: Instant,
    inherited: Duration,
}

static CLOCK: OnceLock<StartupClock> = OnceLock::new();
static TELEMETRY_READY: AtomicBool = AtomicBool::new(false);

fn inherited_duration(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds <= MAX_INHERITED_MS)
        .map(Duration::from_millis)
        .unwrap_or_default()
}

fn clock() -> &'static StartupClock {
    CLOCK.get_or_init(|| StartupClock {
        started: Instant::now(),
        inherited: inherited_duration(std::env::var(ELAPSED_ENV).ok().as_deref()),
    })
}

pub fn start() {
    let _ = clock();
}

pub fn telemetry_ready() {
    TELEMETRY_READY.store(true, Ordering::Release);
}

pub fn elapsed() -> Duration {
    let clock = clock();
    clock.inherited.saturating_add(clock.started.elapsed())
}

pub fn forward_to(command: &mut Command) {
    command.env(ELAPSED_ENV, elapsed().as_millis().to_string());
}

fn report(level: tracing::Level, message: &str) {
    if TELEMETRY_READY.load(Ordering::Acquire) {
        match level {
            tracing::Level::WARN => tracing::warn!(target: "nuono::startup", "{message}"),
            tracing::Level::INFO => tracing::info!(target: "nuono::startup", "{message}"),
            _ => tracing::debug!(target: "nuono::startup", "{message}"),
        }
    } else {
        let detailed = std::env::var("RUST_LOG").is_ok_and(|filter| {
            filter.split(',').any(|part| {
                matches!(
                    part.rsplit('=').next().unwrap_or("").trim(),
                    "debug" | "trace"
                )
            })
        });
        if level != tracing::Level::DEBUG || detailed {
            let timestamp = chrono::Local::now().format("%m-%d %H:%M:%S");
            let padding = if level == tracing::Level::DEBUG {
                " "
            } else {
                "  "
            };
            eprintln!("{timestamp} [{level}]{padding}[核心/启动] {message}");
        }
    }
}

/// 常规启动检查留在 DEBUG，超过一秒的阶段及未完成阶段保持可见。
pub struct Stage {
    name: &'static str,
    started: Instant,
    finished: bool,
    visible: bool,
}

impl Stage {
    pub fn new(name: &'static str) -> Self {
        start();
        report(tracing::Level::DEBUG, &format!("{name}…"));
        Self {
            name,
            started: Instant::now(),
            finished: false,
            visible: false,
        }
    }

    /// 为编译、浏览器预热等耗时操作立即显示进度。
    pub fn visible(name: &'static str) -> Self {
        Self::visible_with(name, name)
    }

    /// 开始提示与完成提示分开：开始提示用于告诉用户「正在做什么、请等待」，完成提示仍用阶段名。
    pub fn visible_with(name: &'static str, start_message: &'static str) -> Self {
        start();
        report(tracing::Level::INFO, &format!("{start_message}…"));
        Self {
            name,
            started: Instant::now(),
            finished: false,
            visible: true,
        }
    }

    pub fn finish(mut self) {
        self.finished = true;
        let level = if self.visible || self.started.elapsed() >= Duration::from_secs(1) {
            tracing::Level::INFO
        } else {
            tracing::Level::DEBUG
        };
        report(
            level,
            &format!(
                "{}完成（耗时 {:.2} 秒）",
                self.name,
                self.started.elapsed().as_secs_f64()
            ),
        );
    }

    pub fn cancel(mut self) {
        self.finished = true;
        report(
            tracing::Level::DEBUG,
            &format!(
                "{}已停止（耗时 {:.2} 秒）",
                self.name,
                self.started.elapsed().as_secs_f64()
            ),
        );
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.finished {
            report(
                tracing::Level::WARN,
                &format!(
                    "{}未完成（耗时 {:.2} 秒）",
                    self.name,
                    self.started.elapsed().as_secs_f64()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_elapsed_is_bounded_diagnostic_input() {
        assert_eq!(
            inherited_duration(Some("82230")),
            Duration::from_millis(82_230)
        );
        for value in [
            None,
            Some(""),
            Some("-1"),
            Some("invalid"),
            Some("18446744073709551615"),
        ] {
            assert_eq!(inherited_duration(value), Duration::ZERO);
        }
        assert_eq!(
            inherited_duration(Some(&MAX_INHERITED_MS.to_string())),
            Duration::from_millis(MAX_INHERITED_MS)
        );
        assert_eq!(
            inherited_duration(Some(&(MAX_INHERITED_MS + 1).to_string())),
            Duration::ZERO
        );
    }

    #[test]
    fn exec_duration_is_forwarded_only_to_the_selected_child() {
        let mut command = Command::new("nuono-application");
        command.env("UNCHANGED", "value");
        forward_to(&mut command);
        let elapsed_value = command
            .get_envs()
            .find(|(key, _)| *key == ELAPSED_ENV)
            .unwrap()
            .1
            .unwrap()
            .to_str()
            .unwrap();
        assert!(elapsed_value.parse::<u64>().is_ok());
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "UNCHANGED"
                    && value == Some(std::ffi::OsStr::new("value")))
        );
    }
}
