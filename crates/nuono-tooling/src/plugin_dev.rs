//! 离线插件开发辅助命令（无需连接数据库或启动宿主服务）。
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy)]
pub enum CargoAction {
    Check,
    Build,
    Test,
}

struct Project {
    manifest: PathBuf,
    directory: PathBuf,
    name: String,
    sdk: Option<PathBuf>,
}

fn project(path: &Path) -> Result<Project> {
    let manifest = if path.is_dir() {
        path.join("Cargo.toml")
    } else {
        path.to_path_buf()
    }
    .canonicalize()
    .with_context(|| format!("找不到插件 Cargo.toml: {}", path.display()))?;
    let directory = manifest
        .parent()
        .context("插件 manifest 没有父目录")?
        .to_path_buf();
    let content: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest)?).context("插件 Cargo.toml 语法错误")?;
    validate_manifest(&content)?;
    let name = content["package"]["name"]
        .as_str()
        .context("缺少 package.name")?
        .to_string();
    let sdk = content
        .get("dependencies")
        .and_then(|v| v.get("nuono-plugin"))
        .and_then(|v| v.get("path"))
        .and_then(toml::Value::as_str)
        .map(|path| directory.join(path).canonicalize())
        .transpose()
        .context("nuono-plugin SDK 路径不存在，请检查 dependencies.nuono-plugin.path")?;
    Ok(Project {
        manifest,
        directory,
        name,
        sdk,
    })
}

pub fn validate_manifest(content: &toml::Value) -> Result<()> {
    ensure!(
        content
            .get("package")
            .and_then(|v| v.get("name"))
            .and_then(toml::Value::as_str)
            .is_some(),
        "目标不是插件 crate：需要 [package] 和 name"
    );
    ensure!(
        content.get("workspace").is_some(),
        "插件需独立 workspace：在 Cargo.toml 添加空的 [workspace]"
    );
    if let Some(types) = content
        .get("lib")
        .and_then(|v| v.get("crate-type"))
        .and_then(toml::Value::as_array)
    {
        ensure!(
            types
                .iter()
                .any(|v| matches!(v.as_str(), Some("rlib" | "lib"))),
            "插件必须提供普通 Rust 库；移除 crate-type 或使用 [\"rlib\"]"
        );
        ensure!(
            !types
                .iter()
                .any(|v| matches!(v.as_str(), Some("cdylib" | "dylib"))),
            "插件使用统一编译，请移除动态库 crate-type"
        );
    }
    ensure!(
        content
            .get("dependencies")
            .and_then(|v| v.get("nuono-plugin"))
            .is_some(),
        "缺少 nuono-plugin SDK 依赖；请使用 plugin new 生成模板或添加当前 SDK 的 path 依赖"
    );
    Ok(())
}

fn check_toolchain(directory: &Path) -> Result<()> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("-vV")
        .current_dir(directory)
        .output()
        .context("无法运行 rustc；请安装 Rust 并确保 PATH 正确")?;
    ensure!(
        output.status.success(),
        "rustc 无法运行：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "{}",
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("rustc")
    );
    let cargo = Command::new("cargo")
        .arg("--version")
        .current_dir(directory)
        .output()
        .context("无法运行 cargo；请检查 PATH")?;
    ensure!(
        cargo.status.success(),
        "cargo 无法运行：{}",
        String::from_utf8_lossy(&cargo.stderr)
    );
    println!("{}", String::from_utf8_lossy(&cargo.stdout).trim());
    Ok(())
}

fn check_sdk(sdk: &Path) -> Result<()> {
    let manifest = sdk.join("Cargo.toml");
    let value: toml::Value = toml::from_str(
        &std::fs::read_to_string(&manifest)
            .with_context(|| format!("无法读取 SDK manifest: {}", manifest.display()))?,
    )?;
    ensure!(
        value
            .get("package")
            .and_then(|v| v.get("name"))
            .and_then(toml::Value::as_str)
            == Some("nuono-plugin"),
        "SDK path 指向的 crate 不是 nuono-plugin"
    );
    let version = value
        .get("package")
        .and_then(|v| v.get("version"))
        .context("SDK manifest 缺少 package.version")?;
    let version = if let Some(version) = version.as_str() {
        version.to_string()
    } else {
        ensure!(
            version.get("workspace").and_then(toml::Value::as_bool) == Some(true),
            "SDK package.version 格式不正确"
        );
        let root =
            super::find_nuono_repository_root(sdk).context("无法定位 SDK 的版本继承 workspace")?;
        let workspace: toml::Value =
            toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml"))?)?;
        workspace
            .get("workspace")
            .and_then(|v| v.get("package"))
            .and_then(|v| v.get("version"))
            .and_then(toml::Value::as_str)
            .context("workspace.package.version 缺失")?
            .to_string()
    };
    println!("SDK 包版本: {version}（与应用统一编译，兼容性由 Rust 类型检查）");
    println!("SDK: {}", sdk.display());
    Ok(())
}

fn target_dir(project: &Project) -> PathBuf {
    if let Some(path) =
        std::env::var_os("NUONO_PLUGIN_TARGET_DIR").or_else(|| std::env::var_os("CARGO_TARGET_DIR"))
    {
        let path = PathBuf::from(path);
        return if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| project.directory.clone())
                .join(path)
        };
    }
    super::find_nuono_repository_root(&project.directory)
        .or_else(|| {
            project
                .sdk
                .as_deref()
                .and_then(super::find_nuono_repository_root)
        })
        .map(|root| root.join("target"))
        .unwrap_or_else(|| project.directory.join("target"))
}

pub fn doctor(path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        let project = project(path)?;
        println!("插件: {}", project.name);
        println!("Manifest: {}", project.manifest.display());
        check_toolchain(&project.directory)?;
        if let Some(sdk) = &project.sdk {
            check_sdk(sdk)?;
        } else {
            println!("SDK 使用远程依赖；插件构建会解析 SDK 依赖");
        }
        println!("共享 target: {}", target_dir(&project).display());
    } else {
        let current = std::env::current_dir()?;
        let root = super::find_nuono_repository_root(&current)
            .context("未找到 Nuono 源码仓库；请在仓库内运行或指定插件目录")?;
        check_toolchain(&root)?;
        check_sdk(&root.join("crates/nuono-plugin"))?;
    }
    println!("诊断通过；插件作为 Rust 库与应用统一编译");
    Ok(())
}

pub fn run(action: CargoAction, path: &Path, config_path: Option<&Path>) -> Result<i32> {
    doctor(Some(path))?;
    let project = project(path)?;
    let target = target_dir(&project);
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
    let current = std::env::current_dir()?;
    let default_path = super::find_nuono_repository_root(&current)
        .or_else(|| super::find_nuono_repository_root(&project.directory))
        .or_else(|| {
            project
                .sdk
                .as_deref()
                .and_then(super::find_nuono_repository_root)
        })
        .unwrap_or(current)
        .join("nuono.toml");
    let config_path = config_path.unwrap_or(&default_path);
    let incremental = crate::build_config::apply(&mut command, config_path)?;
    println!("增量编译：{incremental}（配置：{}）", config_path.display());
    command.env("CARGO_PROFILE_DEV_DEBUG", "0");
    if std::env::var_os("NUONO_CARGO_OFFLINE").is_some() {
        command.arg("--offline");
    }
    // 插件开发命令不加 `--locked`，允许开发者在修改依赖后由 Cargo 自动更新插件的 Cargo.lock。
    let status = command
        .arg("--manifest-path")
        .arg(&project.manifest)
        .arg("--target-dir")
        .arg(&target)
        .current_dir(&project.directory)
        .status()
        .context("无法运行 Cargo 插件命令")?;
    if status.success() {
        println!("插件 {} 完成；产物目录: {}", project.name, target.display());
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> toml::Value {
        toml::from_str(
            r#"[package]
name = "demo"
[workspace]
[lib]
crate-type = ["rlib"]
[dependencies]
nuono-plugin = { path = "../../crates/nuono-plugin" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn rejects_framework_workspace_and_missing_sdk() {
        for key in ["package", "workspace", "dependencies"] {
            let mut manifest = valid_manifest();
            manifest.as_table_mut().unwrap().remove(key);
            assert!(validate_manifest(&manifest).is_err(), "missing {key}");
        }
        assert!(validate_manifest(&valid_manifest()).is_ok());
    }

    #[test]
    fn accepts_rust_library_and_rejects_dynamic_library() {
        let mut manifest = valid_manifest();
        manifest.as_table_mut().unwrap().remove("lib");
        assert!(validate_manifest(&manifest).is_ok());
        manifest.as_table_mut().unwrap().insert(
            "lib".into(),
            toml::from_str::<toml::Value>("crate-type = [\"cdylib\"]").unwrap(),
        );
        assert!(validate_manifest(&manifest).is_err());
    }
}
