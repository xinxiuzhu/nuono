//! 源码模式下的应用构建准备与独立二进制导出（不涉及数据库连接或服务运行时初始化）。
use crate::source_build::*;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
/// 在扫描扩展目录前先执行中断恢复，随后编译并发布应用（即使当前未安装任何源码扩展也会生成完整宿主程序）。
pub fn prepare(config_path: &Path) -> Result<PathBuf> {
    let root = crate::find_nuono_repository_root(&std::env::current_dir()?)
        .context("请在 Nuono 源码仓库启动")?;
    crate::recovery::recover_before_build(&root).map_err(anyhow::Error::msg)?;
    let lock = application_lock(&root)?;
    build_locked(&root, env!("NUONO_BUILD_PROFILE"), None, &lock, config_path)
}
pub fn export(
    output: &Path,
    requested_profile: Option<&str>,
    config_path: Option<&Path>,
) -> Result<()> {
    let root = crate::find_nuono_repository_root(&std::env::current_dir()?)
        .context("请在 Nuono 源码仓库构建应用程序")?;
    let profile = if env!("NUONO_BUILD_PROFILE") == "debug" {
        "release"
    } else {
        env!("NUONO_BUILD_PROFILE")
    };
    let default_path = PathBuf::from("nuono.toml");
    crate::recovery::recover_before_build(&root).map_err(anyhow::Error::msg)?;
    // 在导出复制完成前持有应用构建锁，确保候选产物与插件源码清单保持稳定；
    // 导出可执行文件（可能为跨平台 target）绝不会覆盖本地运行时已发布的程序入口。
    let lock = application_lock(&root)?;
    let candidate = build_candidate_prepared_locked(
        &root,
        requested_profile.unwrap_or(profile),
        None,
        &lock,
        config_path.unwrap_or(&default_path),
        true,
    )?;
    ensure!(
        same_sources(&current_sources(&root)?, &candidate.artifact().sources),
        "构建后插件源码已变化，未导出程序"
    );
    export_validated_candidate(&lock.target, candidate, output)?;
    eprintln!(
        "应用程序已导出: {}（包含当前安装的插件，无需 Cargo 或插件源码）",
        output.display()
    );
    Ok(())
}
