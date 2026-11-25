//! 读取本地启动配置中的构建偏好，并应用到 Cargo 子进程环境变量。
use anyhow::Result;
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(default)]
struct BuildConfig {
    incremental: bool,
}
impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            incremental: nuono_domain::deployment::DEFAULT_BUILD_INCREMENTAL,
        }
    }
}
impl BuildConfig {
    fn load(path: &Path) -> Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => anyhow::bail!("读取 {} 失败: {error}", path.display()),
        };
        #[derive(Default, Deserialize)]
        struct BootstrapBuild {
            #[serde(default)]
            build: BuildConfig,
        }
        toml::from_str::<BootstrapBuild>(&content)
            .map(|config| config.build)
            .map_err(|error| anyhow::anyhow!("解析 {} 失败: {error}", path.display()))
    }
}
use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::Command,
};

pub fn apply(command: &mut Command, config_path: &Path) -> Result<&'static str> {
    let config = BuildConfig::load(config_path)?;
    clear_parent_package_environment(command, std::env::vars_os().map(|(key, _)| key));
    // 编译并行度交由 Cargo 根据可用 CPU 核心数或用户配置（含继承的 CARGO_BUILD_JOBS）自行决定。
    Ok(apply_overrides(
        command,
        config.incremental,
        std::env::var_os("CARGO_INCREMENTAL"),
        std::env::var_os("CARGO_BUILD_INCREMENTAL"),
    ))
}

fn is_package_environment(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    matches!(
        key,
        "OUT_DIR"
            | "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_MANIFEST_LINKS"
            | "CARGO_CRATE_NAME"
            | "CARGO_BIN_NAME"
            | "CARGO_PRIMARY_PACKAGE"
            | "CARGO_TARGET_TMPDIR"
    ) || [
        "CARGO_PKG_",
        "CARGO_FEATURE_",
        "CARGO_CFG_",
        "CARGO_BIN_EXE_",
        "DEP_",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
}

pub fn clear_parent_package_environment(
    command: &mut Command,
    inherited_keys: impl IntoIterator<Item = OsString>,
) {
    // 通过 `cargo run` 启动时，父进程环境变量中会带有启动器自身的包元数据和 `OUT_DIR`。
    // Cargo 计算构建指纹时会记录被 `build.rs` 读取的环境变量；如果不清理这些变量，
    // 直接运行二进制与通过 `cargo run` 启动就会互相使对方的增量构建缓存失效。
    // 此处保留全局构建输入（如 `RUSTFLAGS`、`CARGO_BUILD_*`、target 配置、工具链和 jobserver），
    // 仅剔除父级 Cargo 注入的单包级别环境变量。
    let keys: Vec<_> = inherited_keys
        .into_iter()
        .chain(command.get_envs().map(|(key, _)| key.to_owned()))
        .filter(|key| is_package_environment(key))
        .collect();
    for key in keys {
        command.env_remove(key);
    }
}

fn apply_overrides(
    command: &mut Command,
    configured: bool,
    incremental: Option<OsString>,
    build_incremental: Option<OsString>,
) -> &'static str {
    let (key, value, label) = if let Some(value) = incremental {
        let label = if value == "1" {
            "开启（环境变量）"
        } else if value == "0" {
            "关闭（环境变量）"
        } else {
            "由环境变量指定"
        };
        ("CARGO_INCREMENTAL", value, label)
    } else if let Some(value) = build_incremental {
        let label = if value == "true" {
            "开启（环境变量）"
        } else if value == "false" {
            "关闭（环境变量）"
        } else {
            "由环境变量指定"
        };
        ("CARGO_BUILD_INCREMENTAL", value, label)
    } else {
        (
            "CARGO_INCREMENTAL",
            OsString::from(if configured { "1" } else { "0" }),
            if configured { "开启" } else { "关闭" },
        )
    };
    command.env(key, value);
    label
}

#[cfg(test)]
#[path = "../tests/unit/build_config/tests.rs"]
mod tests;
