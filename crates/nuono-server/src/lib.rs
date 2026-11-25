//! Nuono 服务端主装配件（Composition Root）。
//!
//! 本模块负责按最终生效配置组装数据库、插件宿主、事件内核、平台适配器、HTTP 路由
//! 及优雅停机顺序。底层库组件不应自行解析 CLI 参数或绑定监听端口。生产环境部署至少应启用管理员认证，
//! 通过受信任的 TLS 反向代理限制管理面和 WebSocket 暴露范围，并由启动器或外部进程守护服务处理重启退出码。

mod adapter_host;
mod admin;
mod api;
mod asset_uploads;
mod background_tasks;
mod backup;
mod bootstrap;
mod bot_instances;
mod bot_qr_login;
mod build_metadata;
mod business_http;
mod business_startup;
mod database_operations;
mod disabled_adapter;
mod host_maintenance;
mod http_locale;
mod http_server;
mod interaction_ingress;
mod launcher;
mod maintenance;
mod middleware;
mod operation_error;
pub mod operations;
mod plugin_catalog;
mod plugin_dev;
mod plugin_icons;
mod release;
mod restart_receipt;
mod sandbox;
mod source_adapters;
mod source_build;
mod source_ingress;
mod source_operations;
mod source_schemas;
mod source_startup;
mod startup;
mod startup_progress;
mod ws;

pub use build_metadata::{BuildDependencyMetadata, BuildMetadata};
pub use nuono_adapter::AdapterFactory;

/// 在启动 server 前登记生成应用的构建元信息。
pub fn register_build_metadata(metadata: &'static BuildMetadata) {
    build_metadata::register(metadata);
}

use crate::middleware::RateLimiter;
use clap::Parser;
use metrics_exporter_prometheus::PrometheusHandle;
use nuono_core::{config::ConfigManager, core::ConnectionManager, db::Database};
use nuono_tooling::find_nuono_repository_root;
use std::{
    sync::{Arc, OnceLock, atomic::AtomicBool},
    time::Instant,
};
use tokio::sync::Mutex;

use nuono_tooling::cli::{AdminCommand, Cli, Command, ReleaseCommand, SignCommand};

/// HTTP handler 共享的已装配应用依赖。
///
/// 该结构不负责创建资源；所有字段都在 `async_main` 中完成校验和接线后注入，便于路由
/// 保持无全局单例，并让优雅关闭按显式所有权顺序释放适配器、任务、插件和数据库。
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub operations: Arc<operations::OperationsMonitor>,
    pub paths: nuono_core::core::Paths,
    pub identity_assets_lock: Arc<Mutex<()>>,
    pub(crate) asset_uploads: Arc<asset_uploads::AssetUploads>,
    pub(crate) plugin_page_sessions: Arc<api::plugin_pages::PageSessions>,
    pub restore_lock: Arc<Mutex<()>>,
    pub(crate) database_operations: Arc<database_operations::DatabaseOperations>,
    pub manager: Arc<ConnectionManager>,
    pub config: Arc<ConfigManager>,
    /// 在完成 bind 后发布，使运行时信息反映 CLI 覆盖与实际分配端口。
    pub(crate) listen_port: Arc<OnceLock<u16>>,
    pub rate_limiter: Arc<RateLimiter>,
    pub plugin_config: Arc<nuono_core::config::PluginConfigManager>,
    pub adapter_manager: Arc<nuono_core::adapter::AdapterManager>,
    pub bot_instances: Arc<bot_instances::BotInstanceRuntimeManager>,
    pub(crate) sandbox: Arc<sandbox::SandboxManager>,
    pub readiness: Arc<AtomicBool>,
    pub(crate) business_startup: Arc<business_startup::BusinessStartup>,
    pub started_at: Instant,
    pub prometheus: PrometheusHandle,
}

impl axum::extract::FromRef<AppState> for Arc<ConnectionManager> {
    fn from_ref(state: &AppState) -> Self {
        state.manager.clone()
    }
}

/// `nuono sign` 子命令：插件商店清单的 ed25519 签名工具。
///
/// 规范消息是 `"{repo}\n{commit}"`，与验证端共用 `service::plugin_store`。
/// 返回进程退出码：0 成功，1 失败。整个子命令不触碰配置与数据库。
fn run_sign_command(command: SignCommand) -> i32 {
    match command {
        SignCommand::Keygen => match nuono_core::service::plugin_store::generate_keypair() {
            Ok((secret, public)) => {
                println!("私钥（妥善保管，勿入库）: {secret}");
                println!("公钥（写入 nuono.toml [plugin_store] signing_public_key）: {public}");
                0
            }
            Err(error) => {
                eprintln!("生成密钥失败: {error}");
                1
            }
        },
        SignCommand::Manifest { path, secret } => {
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    eprintln!("读取清单失败: {error}");
                    return 1;
                }
            };
            let value: serde_json::Value = match serde_json::from_str(&content) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!(
                        "清单不是合法 JSON: {error}（请先将 plugin.toml 转换为 plugin.json 后再签名）"
                    );
                    return 1;
                }
            };
            let value = value.get("plugin").unwrap_or(&value);
            let (Some(repo), Some(commit)) = (
                value.get("repo").and_then(|value| value.as_str()),
                value.get("commit").and_then(|value| value.as_str()),
            ) else {
                eprintln!("清单缺少 repo 或 commit 字段，无法计算签名");
                return 1;
            };
            match nuono_core::service::plugin_store::sign_commit(repo, commit, &secret) {
                Ok(signature) => {
                    println!("请将以下内容写入清单 JSON（与 repo/commit 同级）:");
                    println!("\"signature\": \"{signature}\"");
                    0
                }
                Err(error) => {
                    eprintln!("签名失败: {error}");
                    1
                }
            }
        }
    }
}

fn run_offline_command(
    command: Command,
    config_path: Option<&std::path::Path>,
    factories: Option<&[nuono_core::plugin::source_loader::PluginFactory]>,
) -> i32 {
    match command {
        Command::Sign { command } => run_sign_command(command),
        Command::Release {
            command: ReleaseCommand::Describe,
        } => match release::describe(factories) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("发行操作失败: {error:#}");
                1
            }
        },
        command => nuono_tooling::run_offline_command(command, config_path),
    }
}

/// 记录当前程序实际链接的插件版本，供安装与备份检查使用。
pub fn register_source_manifest(manifest: &'static str) {
    source_build::register_running_manifest(manifest);
}

pub fn register_source_runtime_name(id: &str, name: &str) {
    source_build::register_runtime_name(id, name);
}

/// 生成的源码应用程序在编译期嵌入可选的插件图标，并将其绑定到运行时插件名，
/// 使导出后的独立可执行程序无需依赖插件源码目录即可展示图标。
pub fn register_plugin_icon(name: &str, content_type: &'static str, bytes: &'static [u8]) {
    plugin_icons::register(name, content_type, bytes);
}

pub fn run() -> anyhow::Result<()> {
    run_inner(None)
}

/// 生成应用的插件入口（插件工厂与宿主共享同一个 Cargo 依赖图）。
pub fn run_with_plugins(
    factories: Vec<nuono_core::plugin::source_loader::PluginFactory>,
) -> anyhow::Result<()> {
    run_inner(Some(factories))
}

/// 生成应用的完整扩展入口（仅静态链接并注册当前已安装的源码插件与平台适配器）。
pub fn run_with_extensions(
    factories: Vec<nuono_core::plugin::source_loader::PluginFactory>,
    adapters: Vec<AdapterFactory>,
) -> anyhow::Result<()> {
    nuono_adapter::register_running(adapters)?;
    let sources = source_build::running_manifest();
    if !sources.is_empty() {
        let expected: std::collections::BTreeSet<_> = sources
            .into_iter()
            .filter(|source| source.kind == source_build::SourceKind::Adapter)
            .map(|source| source.id)
            .collect();
        let actual: std::collections::BTreeSet<_> = nuono_adapter::running()
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id)
            .collect();
        anyhow::ensure!(
            expected == actual,
            "适配器工厂声明的 ID 与应用安装清单不一致"
        );
    }
    run_inner(Some(factories))
}

fn run_inner(
    factories: Option<Vec<nuono_core::plugin::source_loader::PluginFactory>>,
) -> anyhow::Result<()> {
    // 1. 解析一次 CLI。
    let mut cli = Cli::parse();

    // 2. 离线构建只读取本机构建选项，不连接数据库，也不构造异步运行时。
    if let Some(command) = cli.command.take() {
        if let Command::Admin {
            command:
                AdminCommand::ResetPassword {
                    username,
                    disable_two_factor,
                },
        } = command
        {
            let path = cli
                .config
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(nuono_core::config::default_config_path);
            return admin::reset_password(&path, &username, disable_two_factor);
        }
        let code = run_offline_command(
            command,
            cli.config.as_deref().map(std::path::Path::new),
            factories.as_deref(),
        );
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }

    // 只有实际运行服务的进程校验发行包并初始化运行时。
    if let Some(code) = launcher::supervise_if_needed()? {
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    release::prepare_runtime()?;
    let _startup_signals = launcher::StartupSignals::install()?;

    startup_progress::start();
    let config_stage = startup_progress::Stage::new("启动配置文件检查");

    let config_path = cli
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(nuono_core::config::default_config_path);
    if nuono_core::config::ensure_bootstrap_config(&config_path)? {
        eprintln!(
            "已生成配置文件：{}（构建选项在 [build]，修改后下次启动生效）",
            config_path.display()
        );
    }
    config_stage.finish();
    if launcher::shutdown_requested() {
        return Ok(());
    }
    if let Some(factories) = factories {
        let validation_stage = startup_progress::Stage::new("运行程序与插件来源校验");
        source_startup::remember_runtime_executable();
        source_startup::validate_source_startup()?;
        nuono_core::plugin::source_loader::register_factories(factories)?;
        validation_stage.finish();
    } else {
        match source_startup::prepare_startup(&config_path)? {
            source_startup::StartupPreparation::Launch(binary) => {
                return source_build::launch(&binary);
            }
            source_startup::StartupPreparation::EmptySourceWorkspace => {
                // 经源码目录检查确认零插件，区别于未提供完整工厂清单的嵌入入口。
                nuono_core::plugin::source_loader::register_factories(Vec::new())?;
            }
            source_startup::StartupPreparation::Unmanaged => {}
        }
        let validation_stage = startup_progress::Stage::new("运行程序标识校验");
        source_startup::remember_runtime_executable();
        validation_stage.finish();
    }

    if launcher::shutdown_requested() {
        return Ok(());
    }

    // 在执行任何数据恢复、数据库迁移或遗留任务清理之前先获取数据目录独占锁。
    // 将 `_service_lease` 放在 `async_main` 之外、且先于 Tokio Runtime 声明，
    // 确保即便异常或超时退出，也会先销毁 Runtime（及其残留异步任务），最后才释放数据目录锁。
    let _service_lease =
        bootstrap::service_lease::ServiceLease::acquire(&nuono_core::core::Paths::root().data)?;

    // Obscura 浏览器引擎要求其首个 V8 isolate 必须在主线程初始化；
    // 当链接的源码插件启用了浏览器特性时，通过 Cargo feature 统一在此处完成主线程预热。
    let browser_stage = startup_progress::Stage::visible("浏览器功能检查与预热");
    nuono_plugin::browser::initialize()?;
    browser_stage.finish();
    if launcher::shutdown_requested() {
        return Ok(());
    }
    let runtime_stage = startup_progress::Stage::new("异步运行时创建");
    let rt = tokio::runtime::Runtime::new()?;
    runtime_stage.finish();
    let exit_code = rt.block_on(bootstrap::async_main(cli))?;
    // 先显式销毁 Tokio Runtime 以确保所有后台任务彻底退出，再释放数据目录独占锁。
    drop(rt);
    drop(_service_lease);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
