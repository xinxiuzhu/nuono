use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Nuono development and CI tools")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// 检查 crate 依赖方向与源码模块边界。
    Architecture,
    /// 编译并测试独立的源码插件脚手架模板。
    PluginTemplates,
    /// 验证源码增量重编译及脱离 Cargo 的独立二进制直接启动。
    SourceStartup {
        #[arg(long, value_parser = ["dev", "release"], default_value = "dev")]
        profile: String,
    },
    /// 校验、导入或构建仓库内置的 GitHub 扩展注册表。
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
}

#[derive(Subcommand)]
enum RegistryAction {
    /// 校验 registry/entries/ 下的所有扩展条目。
    Validate,
    /// 生成 extensions.json 到 --out 指定目录。
    Build {
        #[arg(long)]
        out: PathBuf,
    },
    /// 解析已通过审核的 GitHub Issue 表单内容并导入到 registry/entries/。
    ImportIssue {
        #[arg(long)]
        issue_body_file: PathBuf,
    },
}

fn main() -> Result<()> {
    let root = xtask::repository_root();
    match Cli::parse().command {
        Task::Architecture => xtask::architecture::run(root),
        Task::PluginTemplates => xtask::plugin_checks::templates(root),
        Task::SourceStartup { profile } => xtask::source_startup::run(root, &profile),
        Task::Registry { action } => match action {
            RegistryAction::Validate => xtask::registry::validate(root).map(|_| ()),
            RegistryAction::Build { out } => xtask::registry::build(root, &out).map(|_| ()),
            RegistryAction::ImportIssue { issue_body_file } => {
                xtask::registry::import_issue(root, &issue_body_file).map(|_| ())
            }
        },
    }
}
