//! 离线数据库与部署包迁移工具入口；执行过程中不启动主应用或任何插件生命周期。
#[path = "transfer/cli.rs"]
mod transfer_cli;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = transfer_cli::run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
