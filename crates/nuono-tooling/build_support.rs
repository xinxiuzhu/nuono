//! 供 `nuono-server` 与生成的应用 `build.rs` 共用的编译期构建信息采集辅助模块。
//! 本模块中的所有逻辑均在编译期执行，部署后的二进制程序运行时无需安装 Rust 工具链。
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, env, fs, path::Path, process::Command};

#[path = "build_profile.rs"]
pub mod build_profile;

#[derive(Debug, Default)]
pub struct CompilerInfo {
    pub rust_version: Option<String>,
    pub compiler: Option<String>,
    pub compiler_commit: Option<String>,
    pub llvm_version: Option<String>,
    pub build_host: Option<String>,
}

pub fn parse_compiler_info(output: &str) -> CompilerInfo {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            line.strip_prefix(name)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
    };
    CompilerInfo {
        rust_version: field("release:"),
        compiler: output
            .lines()
            .next()
            .filter(|line| line.starts_with("rustc "))
            .map(str::to_owned),
        compiler_commit: field("commit-hash:"),
        llvm_version: field("LLVM version:"),
        build_host: field("host:"),
    }
}

#[derive(Debug)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    dependencies: Vec<String>,
}

fn resolve_dependency<'a>(
    packages: &'a [LockedPackage],
    reference: &str,
) -> Option<&'a LockedPackage> {
    // Cargo 仅在包名或“包名+版本”在 Cargo.lock 中唯一时，才会省略依赖项的版本号或来源。
    let (identity, source) = match reference.split_once(" (") {
        Some((identity, source)) => (identity, Some(source.strip_suffix(')')?)),
        None => (reference, None),
    };
    let mut parts = identity.split_whitespace();
    let name = parts.next()?;
    let version = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let mut matches = packages.iter().filter(|package| {
        package.name == name
            && version.is_none_or(|version| package.version == version)
            && source.is_none_or(|source| package.source.as_deref() == Some(source))
    });
    let package = matches.next()?;
    matches.next().is_none().then_some(package)
}

fn direct_dependencies<'a>(
    packages: &'a [LockedPackage],
    owner: &LockedPackage,
    name: &str,
) -> Option<Vec<&'a LockedPackage>> {
    let references = owner
        .dependencies
        .iter()
        .filter(|reference| reference.split_whitespace().next() == Some(name));
    references
        .map(|reference| resolve_dependency(packages, reference))
        .collect()
}

pub fn core_dependencies(
    lock: &str,
    package_name: &str,
    package_version: &str,
) -> Vec<(String, Vec<String>)> {
    let Some(document) = toml::from_str::<toml::Value>(lock).ok() else {
        return Vec::new();
    };
    let Some(values) = document.get("package").and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    let packages = values
        .iter()
        .filter_map(|value| {
            Some(LockedPackage {
                name: value.get("name")?.as_str()?.to_owned(),
                version: value.get("version")?.as_str()?.to_owned(),
                source: value
                    .get("source")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned),
                dependencies: value
                    .get("dependencies")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    let mut roots = packages.iter().filter(|package| {
        package.name == package_name
            && package.version == package_version
            && package.source.is_none()
    });
    let Some(root) = roots.next() else {
        return Vec::new();
    };
    if roots.next().is_some() {
        return Vec::new();
    }
    let servers = if root.name == "nuono-server" {
        vec![root]
    } else {
        direct_dependencies(&packages, root, "nuono-server").unwrap_or_default()
    };
    if servers.len() != 1 {
        return Vec::new();
    }
    let server = servers[0];
    let mut result = Vec::new();
    for name in [
        "axum",
        "tokio",
        "tower",
        "tower-http",
        "reqwest",
        "serde",
        "tracing",
    ] {
        if let Some(dependencies) = direct_dependencies(&packages, server, name) {
            let versions = dependencies
                .iter()
                .map(|package| package.version.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if !versions.is_empty() {
                result.push((name.to_owned(), versions));
            }
        }
    }
    // SeaORM 属于宿主 `nuono-core` / `nuono-storage` 的依赖图，不能取自插件的依赖版本。
    if let Some(cores) = direct_dependencies(&packages, server, "nuono-core") {
        let versions = cores
            .iter()
            .map(|core| direct_dependencies(&packages, core, "sea-orm"))
            .collect::<Option<Vec<_>>>()
            .map(|dependencies| {
                dependencies
                    .into_iter()
                    .flatten()
                    .map(|package| package.version.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            });
        if let Some(versions) = versions.filter(|versions| !versions.is_empty()) {
            result.push(("sea-orm".into(), versions));
        }
    }
    result
}

fn rust_option(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("Some({value:?})"),
        None => "None".into(),
    }
}

pub fn emit_build_metadata(type_path: &str, dependency_type_path: &str) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
    let compiler = env::var_os("RUSTC")
        .and_then(|rustc| Command::new(rustc).arg("-vV").output().ok())
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| parse_compiler_info(&output))
        .unwrap_or_default();
    let manifest = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let helper_directory = if package_is_server(Path::new(&manifest)) {
        Path::new(&manifest).join("../nuono-tooling")
    } else {
        Path::new(&manifest).to_path_buf()
    };
    for helper in ["build_support.rs", "build_profile.rs"] {
        println!(
            "cargo:rerun-if-changed={}",
            helper_directory.join(helper).display()
        );
    }
    let package_name = env::var("CARGO_PKG_NAME").expect("Cargo package name");
    let package_version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    // 统一生成的应用拥有独立的 Cargo.lock；单独编译 nuono-server 时则回退使用 workspace 的 Cargo.lock。
    let lock_path = Path::new(&manifest)
        .ancestors()
        .map(|directory| directory.join("Cargo.lock"))
        .find(|path| path.is_file());
    let mut lock_sha256: Option<String> = None;
    let dependencies = lock_path
        .and_then(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            fs::read_to_string(path).ok()
        })
        .map(|lock| {
            lock_sha256 = Some(
                Sha256::digest(lock.as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            );
            core_dependencies(&lock, &package_name, &package_version)
        })
        .unwrap_or_default();
    let target = env::var("TARGET").ok();
    let out = std::path::PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let profile = build_profile::from_out_dir(&out).expect("recognized Cargo OUT_DIR layout");
    let mut generated = format!(
        "{type_path} {{\n lock_sha256: {},\n rust_version: {},\n compiler: {},\n compiler_commit: {},\n llvm_version: {},\n target_triple: {},\n build_host: {},\n build_profile: {profile:?},\n dependencies: &[\n",
        rust_option(lock_sha256.as_deref()),
        rust_option(compiler.rust_version.as_deref()),
        rust_option(compiler.compiler.as_deref()),
        rust_option(compiler.compiler_commit.as_deref()),
        rust_option(compiler.llvm_version.as_deref()),
        rust_option(target.as_deref()),
        rust_option(compiler.build_host.as_deref()),
    );
    for (name, versions) in dependencies {
        generated.push_str(&format!(
            "{dependency_type_path} {{ name: {name:?}, versions: &{versions:?} }},\n"
        ));
    }
    generated.push_str("]\n}\n");
    fs::write(out.join("build_metadata.rs"), generated).expect("write build metadata");
}

fn package_is_server(manifest: &Path) -> bool {
    Path::new(manifest)
        .file_name()
        .is_some_and(|name| name == "nuono-server")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_versions_follow_framework_edges_in_a_multiversion_lock() {
        let lock = r#"
version = 4
[[package]]
name = "nuono-application"
version = "0.0.1"
dependencies = ["nuono-server", "plugin"]
[[package]]
name = "nuono-server"
version = "0.0.1"
dependencies = ["nuono-core", "reqwest 0.13.5", "tokio 1.50.0 (registry+https://example.test/core)"]
[[package]]
name = "nuono-core"
version = "0.0.1"
dependencies = ["sea-orm"]
[[package]]
name = "plugin"
version = "0.1.0"
dependencies = ["reqwest 0.12.28", "tokio 1.50.0 (git+https://example.test/fork)"]
[[package]]
name = "reqwest"
version = "0.12.28"
[[package]]
name = "reqwest"
version = "0.13.5"
[[package]]
name = "tokio"
version = "1.50.0"
source = "registry+https://example.test/core"
[[package]]
name = "tokio"
version = "1.50.0"
source = "git+https://example.test/fork"
[[package]]
name = "sea-orm"
version = "2.0.4"
"#;
        assert_eq!(
            core_dependencies(lock, "nuono-application", "0.0.1"),
            vec![
                ("tokio".into(), vec!["1.50.0".into()]),
                ("reqwest".into(), vec!["0.13.5".into()]),
                ("sea-orm".into(), vec!["2.0.4".into()]),
            ]
        );
        // 存在歧义的依赖项引用绝不能静默任选其中一个分支或版本。
        let ambiguous = lock.replace(
            "tokio 1.50.0 (registry+https://example.test/core)",
            "tokio 1.50.0",
        );
        assert!(
            core_dependencies(&ambiguous, "nuono-application", "0.0.1")
                .iter()
                .all(|(name, _)| name != "tokio")
        );
    }

    #[test]
    fn unavailable_metadata_does_not_invent_versions() {
        assert!(core_dependencies("invalid TOML", "nuono-server", "0.0.1").is_empty());
        assert!(core_dependencies("version = 4", "nuono-server", "0.0.1").is_empty());
        let missing = parse_compiler_info("");
        assert!(missing.rust_version.is_none());
        assert!(missing.compiler.is_none());
        let compiler = parse_compiler_info(
            "rustc 1.99.0 (abcdef 2026-09-01)\ncommit-hash: abcdef\nhost: aarch64-apple-darwin\nrelease: 1.99.0\nLLVM version: 22.1.0\n",
        );
        assert_eq!(compiler.rust_version.as_deref(), Some("1.99.0"));
        assert_eq!(compiler.compiler_commit.as_deref(), Some("abcdef"));
        assert_eq!(compiler.llvm_version.as_deref(), Some("22.1.0"));
        assert_eq!(compiler.build_host.as_deref(), Some("aarch64-apple-darwin"));
    }
}
