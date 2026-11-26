//! 源码适配器开发辅助命令（不会连接数据库或启动宿主服务）。
use crate::plugin_dev::CargoAction;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn project(path: &Path) -> Result<PathBuf> {
    let manifest = if path.is_dir() {
        path.join("Cargo.toml")
    } else {
        path.to_path_buf()
    }
    .canonicalize()?;
    let value: toml::Value = toml::from_str(&fs::read_to_string(&manifest)?)?;
    crate::source_build::validate_adapter_manifest(&value)?;
    Ok(manifest)
}
pub fn doctor(path: Option<&Path>) -> Result<()> {
    let current = std::env::current_dir()?;
    if let Some(path) = path {
        println!("适配器清单：{}", project(path)?.display());
    } else {
        crate::find_nuono_repository_root(&current)
            .context("请在框架仓库内运行，或指定适配器目录")?;
    }
    for tool in ["cargo", "rustc"] {
        ensure!(
            Command::new(tool).arg("--version").status()?.success(),
            "{tool} 不可用"
        );
    }
    println!("适配器作为普通 Rust 库，由生成的应用统一注册、链接");
    Ok(())
}
pub fn run(action: CargoAction, path: &Path, config: Option<&Path>) -> Result<i32> {
    doctor(Some(path))?;
    let manifest = project(path)?;
    let current = std::env::current_dir()?;
    let root = crate::find_nuono_repository_root(&current)
        .or_else(|| crate::find_nuono_repository_root(&manifest));
    let target = root
        .as_ref()
        .map(|root| root.join("target"))
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    let mut command = Command::new("cargo");
    match action {
        CargoAction::Check => {
            command.args(["check", "--tests"]);
        }
        CargoAction::Build => {
            command.args(["build", "--release"]);
        }
        CargoAction::Test => {
            command.arg("test");
        }
    }
    let config_path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.unwrap_or(current).join("nuono.toml"));
    crate::build_config::apply(&mut command, &config_path)?;
    if std::env::var_os("NUONO_CARGO_OFFLINE").is_some() {
        command.arg("--offline");
    }
    let status = command
        .arg("--manifest-path")
        .arg(crate::source_build::external_path(&manifest))
        .arg("--target-dir")
        .arg(crate::source_build::external_path(&target))
        .status()?;
    Ok(status.code().unwrap_or(1))
}
pub fn new(name: &str) -> Result<i32> {
    ensure!(
        name.len() <= 80
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !name.ends_with('-'),
        "适配器名称需使用小写字母、数字和连字符"
    );
    ensure!(
        !["hub", "api", "plugins", "adapters"].contains(&name),
        "适配器名称为保留名称"
    );
    let current = std::env::current_dir()?;
    let root = crate::find_nuono_repository_root(&current).context("请在 Nuono 源码仓库内运行")?;
    let adapters = root.join("adapters");
    fs::create_dir_all(&adapters)?;
    ensure!(
        !fs::symlink_metadata(&adapters)?.file_type().is_symlink(),
        "adapters 目录不能是符号链接"
    );
    let path = adapters.join(name);
    fs::create_dir(&path).context("适配器目录已存在，未覆盖")?;
    fs::create_dir(path.join("src"))?;
    fs::write(
        path.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
rust-version = "1.100.0"
license = "GPL-3.0-only"
publish = false
[workspace]
resolver = "3"
[package.metadata.nuono]
kind = "adapter"
[dependencies]
nuono-adapter = {{ path = "../../crates/nuono-adapter" }}
async-trait = "0.1"
[profile.dev]
debug = 0
incremental = true
[profile.release]
incremental = true
"#
        ),
    )?;
    fs::write(
        path.join("src/lib.rs"),
        include_str!("../templates/adapter/lib.rs.template").replace("__NAME__", name),
    )?;
    fs::write(path.join(".gitignore"), "/target/\n.nuono-pin\n")?;
    fs::copy(
        root.join("rust-toolchain.toml"),
        path.join("rust-toolchain.toml"),
    )?;
    fs::copy(root.join("LICENSE"), path.join("LICENSE"))?;
    println!(
        "已创建适配器：{}；先实现连接和收发，再运行 cargo adapter check {}",
        path.display(),
        path.display()
    );
    Ok(0)
}
