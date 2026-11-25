use std::sync::OnceLock;

/// 编译最终应用程序时由 Cargo 固化的构建元信息（导出后的独立二进制无需依赖 Cargo 或 rustc 即可直接读取）。
#[derive(Debug, serde::Serialize)]
pub struct BuildMetadata {
    pub lock_sha256: Option<&'static str>,
    pub rust_version: Option<&'static str>,
    pub compiler: Option<&'static str>,
    pub compiler_commit: Option<&'static str>,
    pub llvm_version: Option<&'static str>,
    pub target_triple: Option<&'static str>,
    pub build_host: Option<&'static str>,
    pub build_profile: &'static str,
    pub dependencies: &'static [BuildDependencyMetadata],
}

#[derive(Debug, serde::Serialize)]
pub struct BuildDependencyMetadata {
    pub name: &'static str,
    pub versions: &'static [&'static str],
}

static FALLBACK: BuildMetadata = include!(concat!(env!("OUT_DIR"), "/build_metadata.rs"));
static APPLICATION: OnceLock<&'static BuildMetadata> = OnceLock::new();

/// 由生成的应用主入口在服务器启动前调用；
/// 注册后优先使用最终可执行文件自身的依赖锁与编译信息，而非 `nuono-server` 库编译时的回退值。
pub fn register(metadata: &'static BuildMetadata) {
    let _ = APPLICATION.set(metadata);
}

fn select_metadata(
    application: Option<&'static BuildMetadata>,
    fallback: &'static BuildMetadata,
) -> &'static BuildMetadata {
    application.unwrap_or(fallback)
}

pub(crate) fn current() -> &'static BuildMetadata {
    select_metadata(APPLICATION.get().copied(), &FALLBACK)
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../nuono-tooling/build_support.rs"]
mod build_support;

#[cfg(test)]
mod tests {
    use super::*;

    static APPLICATION_INFO: BuildMetadata = BuildMetadata {
        lock_sha256: Some("application-lock"),
        rust_version: Some("application-rust"),
        compiler: None,
        compiler_commit: None,
        llvm_version: None,
        target_triple: Some("application-target"),
        build_host: None,
        build_profile: "release",
        dependencies: &[BuildDependencyMetadata {
            name: "reqwest",
            versions: &["application-version"],
        }],
    };

    #[test]
    fn application_metadata_overrides_the_server_library_fallback() {
        assert!(std::ptr::eq(select_metadata(None, &FALLBACK), &FALLBACK));
        let selected = select_metadata(Some(&APPLICATION_INFO), &FALLBACK);
        assert!(std::ptr::eq(selected, &APPLICATION_INFO));
        assert_eq!(selected.build_profile, "release");
        assert_eq!(selected.dependencies[0].versions, &["application-version"]);
    }
}
