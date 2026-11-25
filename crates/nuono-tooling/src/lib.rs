//! 源码构建、脚手架与发行管理公共工具库（运行时生命周期与鉴权策略由 `nuono-server` 负责）。
#[cfg(test)]
#[path = "../build_profile.rs"]
mod build_profile;

pub mod adapter_dev;
pub mod build_config;
pub mod cargo_cache;
pub mod cli;
mod dev_commands;
pub mod git;
pub mod launcher;
pub mod plugin_dev;
pub mod plugin_scaffold;
pub mod recovery;
pub mod release;
mod repository;
pub mod source_build;
pub mod source_startup;
pub mod startup_progress;
pub use dev_commands::run_new_plugin_command;
pub use repository::find_nuono_repository_root;

mod offline;
pub use offline::run_offline_command;
