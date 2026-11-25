//! 轻量级源码启动器命令行入口。需要完整运行时环境的子命令将原样转发给编译后的应用进程。
use anyhow::Result;
use clap::Parser;
use nuono_tooling::{
    cli::{Cli, Command},
    launcher, source_build, source_startup, startup_progress,
};
fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = cli.config.as_deref().map(std::path::Path::new);
    let runtime_command = cli.command.as_ref().is_some_and(Command::requires_runtime);
    if !runtime_command {
        if let Some(command) = cli.command {
            let code = nuono_tooling::run_offline_command(command, config);
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        if let Some(code) = launcher::supervise_if_needed()? {
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
    }
    let _signals = launcher::StartupSignals::install()?;
    startup_progress::start();
    let binary = source_startup::prepare(config.unwrap_or(std::path::Path::new("nuono.toml")))
        .map_err(|error| anyhow::anyhow!("应用构建失败: {error:#}"))?;
    if launcher::shutdown_requested() {
        return Ok(());
    }
    source_build::launch(&binary)
}
