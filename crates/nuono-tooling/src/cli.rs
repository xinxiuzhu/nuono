use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "nuono", version)]
#[command(about = "Nuono Framework", long_about = None)]
pub struct Cli {
    /// 配置文件路径
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<String>,

    /// 监听地址
    #[arg(short = 'H', long)]
    pub host: Option<String>,

    /// 监听端口
    #[arg(short = 'P', long)]
    pub port: Option<u16>,

    /// 启用 tokio-console 调试
    #[arg(long)]
    pub console: bool,

    /// 启用 OpenTelemetry 链路追踪
    #[arg(long)]
    pub otel: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 在本机恢复管理员登录，无需邮件服务
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// 构建包含当前安装插件的应用程序，无需启动服务
    Build {
        /// 输出文件（默认 dist/nuono）
        #[arg(long)]
        output: Option<std::path::PathBuf>,
        #[arg(long, value_parser = ["dev", "release"])]
        profile: Option<String>,
    },
    /// 构建、检查完整应用发行包
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// 管理独立 Rust 源码适配器
    Adapter {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum ReleaseCommand {
    /// 生成包含程序、Dashboard 和可追溯清单的独立目录
    Build {
        #[arg(long)]
        output: std::path::PathBuf,
        #[arg(long, default_value = "release", value_parser = ["dev", "release"])]
        profile: String,
        #[arg(long = "require-plugin")]
        required_plugins: Vec<String>,
        #[arg(long = "require-adapter")]
        required_adapters: Vec<String>,
    },
    /// 仅校验文件和清单，不执行包内程序或访问数据库
    Verify { path: std::path::PathBuf },
    /// 离线比较新旧发行，输出迁移与扩展变化；不访问运行数据或切换程序
    Preflight {
        #[arg(long)]
        current: std::path::PathBuf,
        #[arg(long)]
        candidate: std::path::PathBuf,
    },
    /// 输出已编入程序的构建与迁移声明，不执行生命周期
    #[command(hide = true)]
    Describe,
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// 设置新密码并撤销该管理员的所有会话和账号令牌
    ResetPassword {
        #[arg(long)]
        username: String,
        /// 丢失验证器时同时关闭两步验证
        #[arg(long)]
        disable_two_factor: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum PluginCommand {
    /// 创建带配置、命令和测试的 Rust 插件
    New { name: String },
    /// 检查工具链可用性、SDK 路径及源码插件工程，无需数据库
    Doctor { path: Option<std::path::PathBuf> },
    /// 编译检查插件源码和单元测试
    Check { path: std::path::PathBuf },
    /// 构建插件 Rust 库，复用共享 target，不启动宿主
    Build { path: std::path::PathBuf },
    /// 运行插件单元测试，无需启动宿主
    Test { path: std::path::PathBuf },
}

impl Command {
    /// 判断该命令是否需要转发给已静态链接的完整应用运行时执行。
    pub fn requires_runtime(&self) -> bool {
        matches!(
            self,
            Self::Admin { .. }
                | Self::Release {
                    command: ReleaseCommand::Describe
                }
        )
    }
}

#[cfg(test)]
mod cli_parser_tests {
    use super::{AdminCommand, Cli, Command, PluginCommand, ReleaseCommand};
    use clap::Parser;

    #[test]
    fn release_commands_are_explicit_and_require_paths() {
        assert!(Cli::try_parse_from(["nuono", "release", "build"]).is_err());
        assert!(Cli::try_parse_from(["nuono", "release", "verify"]).is_err());
        let cli = Cli::try_parse_from([
            "nuono",
            "release",
            "build",
            "--output",
            "dist/my release",
            "--require-plugin",
            "notes",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Some(Command::Release { command: ReleaseCommand::Build { profile, required_plugins, .. } }) if profile == "release" && required_plugins == ["notes"])
        );
        assert!(matches!(
            Cli::try_parse_from(["nuono", "release", "verify", "dist/app"])
                .unwrap()
                .command,
            Some(Command::Release {
                command: ReleaseCommand::Verify { .. }
            })
        ));
    }

    #[test]
    fn cli_parser_preserves_single_program_export() {
        let cli = Cli::try_parse_from(["nuono", "build"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Build { output: None, .. })
        ));
        let cli = Cli::try_parse_from([
            "nuono",
            "build",
            "--output",
            "dist/custom program",
            "--profile",
            "release",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Some(Command::Build { output: Some(path), profile: Some(profile) }) if path == std::path::Path::new("dist/custom program") && profile == "release")
        );
    }

    #[test]
    fn cli_parser_accepts_local_admin_recovery_and_requires_username() {
        let cli = Cli::try_parse_from([
            "nuono",
            "--config",
            "nuono.toml",
            "admin",
            "reset-password",
            "--username",
            "admin",
            "--disable-two-factor",
        ])
        .unwrap();
        assert_eq!(cli.config.as_deref(), Some("nuono.toml"));
        assert!(
            matches!(cli.command, Some(Command::Admin { command: AdminCommand::ResetPassword { username, disable_two_factor: true } }) if username == "admin")
        );
        assert!(Cli::try_parse_from(["nuono", "admin", "reset-password"]).is_err());
    }

    #[test]
    fn cli_parser_accepts_nested_plugin_new_command() {
        let cli = Cli::try_parse_from(["nuono", "plugin", "new", "my-plugin"])
            .expect("parse nested plugin new command");

        assert!(matches!(
            cli.command,
            Some(Command::Plugin {
                command: PluginCommand::New { ref name }
            }) if name == "my-plugin"
        ));
    }

    #[test]
    fn cli_parser_accepts_plugin_development_commands_and_paths_with_spaces() {
        for command in ["doctor", "check", "build", "test"] {
            assert!(Cli::try_parse_from(["nuono", "plugin", command, "plugins/my plugin"]).is_ok());
        }
        assert!(Cli::try_parse_from(["nuono", "plugin", "doctor"]).is_ok());
        assert!(Cli::try_parse_from(["nuono", "plugin", "build"]).is_err());
    }
}
