//! 供所有 Nuono 构建脚本（build.rs）共用的 Cargo 编译配置（profile）名称提取工具。
use std::path::Path;

/// 保留自定义 profile 的真实目录名（Cargo 的 `PROFILE` 环境变量只会简化为 debug 或 release）。
/// 同时兼容 Rust 1.100 之前和当前的 `OUT_DIR` 目录层级，对未知目录结构返回 `None`。
pub fn from_out_dir(out: &Path) -> Option<&str> {
    if out.file_name()? != "out" {
        return None;
    }
    // 旧版布局: <profile>/build/<package>-<hash>/out
    // 新版布局: <profile>/build/<package>/<hash>/out
    let parent = out.parent()?.parent()?;
    let build = if parent.file_name()? == "build" {
        parent
    } else {
        parent.parent()?
    };
    if build.file_name()? != "build" {
        return None;
    }
    build.parent()?.file_name()?.to_str()
}

#[cfg(test)]
#[path = "tests/unit/build_profile.rs"]
mod tests;
