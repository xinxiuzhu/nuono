//! 供源码启动器与导出的独立应用共用的离线 CLI 子命令入口。
use crate::{
    adapter_dev,
    cli::{Command, PluginCommand, ReleaseCommand},
    plugin_dev, release, run_new_plugin_command, source_startup,
};

pub fn run_offline_command(command: Command, config_path: Option<&std::path::Path>) -> i32 {
    match command {
        Command::Release { command } => {
            let result = match command {
                ReleaseCommand::Build {
                    output,
                    profile,
                    required_plugins,
                    required_adapters,
                } => release::build(
                    &output,
                    &profile,
                    config_path,
                    required_plugins,
                    required_adapters,
                ),
                ReleaseCommand::Verify { path } => release::verify(&path).map(|manifest| {
                    println!("发行校验通过: {}", manifest.release_id);
                }),
                ReleaseCommand::Preflight { current, candidate } => {
                    release::preflight(&current, &candidate).and_then(|report| {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                        anyhow::ensure!(
                            report.blockers.is_empty(),
                            "发行预检未通过，请检查 blockers"
                        );
                        Ok(())
                    })
                }
                ReleaseCommand::Describe => unreachable!("runtime command forwarded"),
            };
            match result {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("发行操作失败: {error:#}");
                    1
                }
            }
        }
        Command::Admin { .. } => {
            unreachable!("administrator recovery handled before offline commands")
        }
        Command::Adapter { command } => {
            let result = match command {
                PluginCommand::New { name } => adapter_dev::new(&name),
                PluginCommand::Doctor { path } => adapter_dev::doctor(path.as_deref()).map(|()| 0),
                PluginCommand::Check { path } => {
                    adapter_dev::run(plugin_dev::CargoAction::Check, &path, config_path)
                }
                PluginCommand::Build { path } => {
                    adapter_dev::run(plugin_dev::CargoAction::Build, &path, config_path)
                }
                PluginCommand::Test { path } => {
                    adapter_dev::run(plugin_dev::CargoAction::Test, &path, config_path)
                }
            };
            match result {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("适配器开发命令失败：{error:#}");
                    1
                }
            }
        }
        Command::Plugin {
            command: PluginCommand::New { name },
        } => run_new_plugin_command(&name),
        Command::Plugin { command } => {
            let result = match command {
                PluginCommand::Doctor { path } => plugin_dev::doctor(path.as_deref()).map(|()| 0),
                PluginCommand::Check { path } => {
                    plugin_dev::run(plugin_dev::CargoAction::Check, &path, config_path)
                }
                PluginCommand::Build { path } => {
                    plugin_dev::run(plugin_dev::CargoAction::Build, &path, config_path)
                }
                PluginCommand::Test { path } => {
                    plugin_dev::run(plugin_dev::CargoAction::Test, &path, config_path)
                }
                PluginCommand::New { .. } => unreachable!(),
            };
            match result {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("插件开发命令失败: {error:#}");
                    1
                }
            }
        }
        Command::Build { output, profile } => {
            let output = output.unwrap_or_else(|| std::path::PathBuf::from("dist/nuono"));
            match source_startup::export(&output, profile.as_deref(), config_path) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("应用构建失败: {error:#}");
                    1
                }
            }
        }
    }
}
