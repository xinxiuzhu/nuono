//! 源码模式启动与构建编排：在扫描和编译插件前先恢复未完成的操作任务账本。
//! 本模块单向调用 `source_operations` 与 `source_build`，下层模块不会反向回调此处。
use crate::source_build::{
    application_lock, build_locked, cached_running_path, current_sources, extensions, hash_file,
    launch, public_binary, read_publication, remember_running_executable,
    remember_running_executable_path, running_binary_sha256, running_executable, running_manifest,
    same_sources, target_dir, verify_publication,
};
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

/// 启动前源码工作区准备结果：未受管二进制（`Unmanaged`）不能直接视为空插件清单，
/// 只有实际扫描源码工作区确认无任何扩展且无历史构建发布记录时，才返回 `EmptySourceWorkspace`。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartupPreparation {
    Unmanaged,
    EmptySourceWorkspace,
    Launch(PathBuf),
}

pub(crate) fn prepare_startup(config_path: &Path) -> Result<StartupPreparation> {
    prepare_startup_at(&std::env::current_dir()?, config_path)
}

fn prepare_startup_at(current: &Path, config_path: &Path) -> Result<StartupPreparation> {
    let Some(root) = crate::find_nuono_repository_root(current) else {
        return Ok(StartupPreparation::Unmanaged);
    };
    let preparation = crate::startup_progress::Stage::new("检查源码工作区及未完成发布");
    crate::source_operations::recover_before_build(&root).map_err(anyhow::Error::msg)?;
    let lock = application_lock(&root)?;
    let requires_build = !extensions(&root)?.is_empty()
        || read_publication(&target_dir(&root)?, env!("NUONO_BUILD_PROFILE"))?.is_some();
    preparation.finish();
    prepare_known_source_startup(requires_build, || {
        build_locked(&root, env!("NUONO_BUILD_PROFILE"), None, &lock, config_path)
    })
}

fn prepare_known_source_startup(
    requires_build: bool,
    build_application: impl FnOnce() -> Result<PathBuf>,
) -> Result<StartupPreparation> {
    if requires_build {
        Ok(StartupPreparation::Launch(build_application()?))
    } else {
        Ok(StartupPreparation::EmptySourceWorkspace)
    }
}

fn source_application_entry(executable: &Path, target: &Path) -> bool {
    let executable = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.to_path_buf());
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    executable.file_name().is_some_and(|name| {
        name == format!("nuono-application{}", std::env::consts::EXE_SUFFIX).as_str()
    }) && executable.starts_with(&target)
}

pub(crate) fn remember_runtime_executable() {
    remember_running_executable_path();
    if crate::source_operations::jobs::source_root().is_ok() {
        remember_running_executable();
    }
}

pub(crate) fn validate_source_startup() -> Result<()> {
    let executable = running_executable()
        .map(Ok)
        .unwrap_or_else(|| std::env::current_exe().map(cached_running_path))?;
    let Some(root) = crate::find_nuono_repository_root(&std::env::current_dir()?) else {
        return Ok(());
    };
    let target = target_dir(&root)?;
    if !source_application_entry(&executable, &target) {
        return Ok(());
    }
    ensure!(
        running_binary_sha256().is_some(),
        "无法读取正在运行的源码应用映像，已停止启动，请重新运行源码启动器"
    );
    let target = target.canonicalize().unwrap_or(target);
    crate::source_operations::recover_before_build(&root).map_err(anyhow::Error::msg)?;
    if let Some(publication) = read_publication(&target, env!("NUONO_BUILD_PROFILE"))? {
        ensure!(
            !publication.deferred_binary,
            "新版本构建尚未写入程序入口，请通过源码启动器重新启动"
        );
        if running_binary_sha256() != Some(publication.artifact.sha256.as_str()) {
            verify_publication(&target, &publication, &current_sources(&root)?)?;
            return launch(&public_binary(&target, &publication.artifact.profile)?);
        }
        ensure!(
            same_sources(&running_manifest(), &publication.artifact.sources),
            "运行程序的完整插件清单与发布凭据不一致"
        );
        verify_publication(&target, &publication, &current_sources(&root)?)?;
    } else {
        ensure!(
            running_binary_sha256()
                == Some(hash_file(&public_binary(&target, env!("NUONO_BUILD_PROFILE"))?)?.as_str()),
            "当前程序没有有效发布凭据，且公开程序已变化，请重新运行源码启动器"
        );
        ensure!(
            same_sources(&running_manifest(), &current_sources(&root)?),
            "旧程序插件清单与当前源码不一致，请重新运行源码启动器"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_recovers_the_operation_ledger_before_scanning_source_plugins() {
        const CHILD: &str = "NUONO_SOURCE_STARTUP_RECOVERY_TEST_CHILD";
        if std::env::var(CHILD).ok().as_deref() != Some("1") {
            // 生产代码会读取 `CARGO_TARGET_DIR` 环境变量；因此在独立子进程中运行该恢复测试并指向临时 target 目录，
            // 既不会污染当前并行测试进程的环境，也不会误碰宿主仓库真实的共享 `target/` 目录。
            let isolated = tempfile::tempdir().unwrap();
            let test_name = concat!(
                "source_startup::tests::",
                "startup_recovers_the_operation_ledger_before_scanning_source_plugins"
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, "1")
                .env("CARGO_TARGET_DIR", isolated.path().join("target"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("running 1 test"),
                "isolated startup recovery test failed or was not selected:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("crates/nuono-server")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['crates/nuono-server']\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/nuono-server/Cargo.toml"),
            "[package]\nname = 'nuono-server'\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("plugins/demo")).unwrap();
        std::fs::write(root.join("plugins/demo/Cargo.toml"), "interrupted download").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let staged_name = format!(".nuono-install-{id}");
        let ledger = root.join("data/.plugin-jobs");
        std::fs::create_dir_all(&ledger).unwrap();
        let job_path = ledger.join(format!("{id}.json"));
        source_build_job(&job_path, &id, &staged_name);

        assert_eq!(
            prepare_startup_at(root, &root.join("nuono.toml")).unwrap(),
            StartupPreparation::EmptySourceWorkspace,
            "recovery must precede manifest parsing and must preserve the zero-plugin path"
        );
        assert!(!root.join("plugins/demo").exists());
        assert!(
            root.join("plugins")
                .join(staged_name)
                .join("Cargo.toml")
                .exists()
        );
        let recovered: serde_json::Value =
            serde_json::from_slice(&std::fs::read(job_path).unwrap()).unwrap();
        assert_eq!(recovered["phase"], "failed");
        assert!(!root.join("nuono.toml").exists());
    }

    fn source_build_job(path: &Path, id: &str, staged_name: &str) {
        crate::source_build::durable_json(
            path,
            &serde_json::json!({
                "id": id, "plugin_id": "demo", "operation": "install", "phase": "building",
                "message": "interrupted", "error": null, "created_at": "2026-01-01",
                "updated_at": "2026-01-01", "repo_url": null, "previous_commit": null,
                "staged_name": staged_name, "runtime_name": null, "previously_enabled": false,
                "expected": null, "owner_pid": 0,
            }),
        )
        .unwrap();
    }

    #[test]
    fn unknown_startup_is_not_an_authoritative_empty_plugin_catalog() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare_startup_at(directory.path(), &directory.path().join("nuono.toml")).unwrap(),
            StartupPreparation::Unmanaged
        );
        assert!(!directory.path().join("target").exists());
        assert!(!directory.path().join("nuono.toml").exists());
    }

    #[test]
    fn confirmed_source_startup_keeps_empty_build_and_build_failure_distinct() {
        assert_eq!(
            prepare_known_source_startup(false, || panic!("空目录不需要构建")).unwrap(),
            StartupPreparation::EmptySourceWorkspace
        );
        let binary = PathBuf::from("target/release/nuono-application");
        assert_eq!(
            prepare_known_source_startup(true, || Ok(binary.clone())).unwrap(),
            StartupPreparation::Launch(binary)
        );
        assert!(
            prepare_known_source_startup(true, || anyhow::bail!("candidate build failed")).is_err(),
            "构建失败不能降级成权威空清单"
        );
    }

    #[test]
    fn exported_entries_do_not_require_a_target_directory_and_cached_entries_survive_replacement() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let target = root.join("target");
        let name = format!("nuono-application{}", std::env::consts::EXE_SUFFIX);
        assert!(!source_application_entry(&root.join("dist/nuono"), &target));
        assert!(!source_application_entry(
            &root.join("dist").join(&name),
            &target
        ));
        assert!(source_application_entry(
            &target.join("debug").join(&name),
            &target
        ));
        assert!(!source_application_entry(
            &target.join("debug/nuono"),
            &target
        ));
        assert!(!target.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_cached_source_entry_preserves_the_real_running_image_after_unlink() {
        let path = PathBuf::from("/tmp/nuono/target/release/nuono-application (deleted)");
        let cached = cached_running_path(path);
        assert_eq!(
            cached,
            PathBuf::from("/tmp/nuono/target/release/nuono-application")
        );
        assert!(source_application_entry(
            &cached,
            Path::new("/tmp/nuono/target")
        ));
        assert!(hash_file(Path::new("/proc/self/exe")).is_ok());
        let unrelated = PathBuf::from("/tmp/export (deleted)");
        assert_eq!(cached_running_path(unrelated.clone()), unrelated);
    }
}
