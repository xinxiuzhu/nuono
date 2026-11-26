//! 由当前这次 Cargo 构建进程直接确定最终可执行产物路径；生成工程保留的 `Cargo.lock` 则通过记录根 `Cargo.lock` 的种子哈希来增量复用。
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const LOCKFILE_SEED: &str = "lockfile-seed.json";

/// 仅当仓库根目录的 `Cargo.lock` 基线发生变化时才重新同步覆盖。
/// Cargo 在编译生成工程时可能会往 `target/nuono-app/Cargo.lock` 中追加已安装插件的依赖项；
/// 保留该锁文件可避免每次启动时重新解析插件依赖。
pub fn sync_lockfile(root: &Path, project: &Path) -> Result<()> {
    let root_lock = root.join("Cargo.lock");
    let bytes = match fs::read(&root_lock) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("无法读取根 Cargo.lock"),
    };
    let digest = hex::encode(Sha256::digest(&bytes));
    let seed_path = project.join(LOCKFILE_SEED);
    let previous = fs::read(&seed_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<String>(&bytes).ok());
    let destination = project.join("Cargo.lock");
    if previous.as_deref() == Some(&digest) && destination.is_file() {
        return Ok(());
    }
    // 校验复制出的锁文件与上方读取的内存快照哈希完全一致，防止复制期间根 Cargo.lock 被并发修改；
    // 若在写入种子文件前发生崩溃，下次构建时会自动重新同步。
    artifacts::copy_atomic_hashed(&root_lock, &destination, Some(&digest))?;
    durable_json(&seed_path, &digest)
}

#[derive(Default)]
struct ArtifactOutput {
    executable: Option<PathBuf>,
    error: Option<String>,
}

impl ArtifactOutput {
    fn observe(&mut self, message: &Value, manifest: &Path) {
        if message["reason"] != "compiler-artifact"
            || message["target"]["name"] != "nuono-application-candidate"
            || !message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        {
            return;
        }
        let matches_manifest = message["manifest_path"]
            .as_str()
            .and_then(|path| Path::new(path).canonicalize().ok())
            .is_some_and(|path| path == manifest);
        if !matches_manifest {
            return;
        }
        let Some(executable) = message["executable"].as_str().map(PathBuf::from) else {
            self.error = Some("Cargo 没有报告应用可执行文件路径".into());
            return;
        };
        if !executable.is_absolute() {
            self.error = Some("Cargo 报告了非绝对应用路径".into());
        } else if self
            .executable
            .as_ref()
            .is_some_and(|previous| previous != &executable)
        {
            self.error = Some("Cargo 生成了多个目标应用，请每次只构建一个 target".into());
        } else {
            self.executable = Some(executable);
        }
    }

    fn finish(&self) -> Result<PathBuf> {
        if let Some(error) = &self.error {
            anyhow::bail!("{error}");
        }
        let executable = self
            .executable
            .clone()
            .context("Cargo 未报告本次应用产物，未使用目录中的旧程序")?;
        ensure!(
            executable.is_file(),
            "Cargo 报告的应用文件不存在: {}",
            executable.display()
        );
        Ok(executable)
    }
}

fn forward_readable(output: &BuildOutput, text: &str) {
    for line in text.lines() {
        if line.len() > 4096 {
            output("[输出行超过 4096 字节，已隐藏]");
        } else {
            output(line);
        }
    }
}

fn handle_line(line: &str, manifest: &Path, state: &Mutex<ArtifactOutput>, output: &BuildOutput) {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        forward_readable(output, line);
        return;
    };
    match message["reason"].as_str() {
        Some("compiler-artifact") => state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .observe(&message, manifest),
        Some("compiler-message") => {
            if let Some(rendered) = message["message"]["rendered"].as_str() {
                forward_readable(output, rendered);
            }
        }
        Some("build-script-executed" | "build-finished") => {}
        _ => forward_readable(output, line),
    }
}

pub fn run(command: &mut Command, manifest: &Path, output: Option<BuildOutput>) -> Result<PathBuf> {
    let manifest = manifest.canonicalize().context("无法定位生成的应用清单")?;
    // 前台 CLI 构建保持在当前终端进程组中，以便用户按 Ctrl-C 能直接终止 Cargo；
    // 后台服务触发的构建任务则使用独立进程组，并启用超时与停机取消控制。
    let manage_lifecycle = output.is_some();
    let output = output.unwrap_or_else(|| Arc::new(|line| eprintln!("{line}")));
    let state = Arc::new(Mutex::new(ArtifactOutput::default()));
    let collect = state.clone();
    let readable = output.clone();
    let stdout: BuildOutput =
        Arc::new(move |line| handle_line(line, &manifest, &collect, &readable));
    command.arg("--message-format=json-render-diagnostics");
    // Cargo 输出的单行 JSON 构建事件可能远超普通日志行长度，因此 stdout 单行上限设为 1 MiB；
    // 渲染后的可读文本与 stderr 则继续保持 4 KiB 的单行截断限制。
    run_logged_with_outputs(command, stdout, output, 1024 * 1024, manage_lifecycle)?;
    state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_lock_changes_reseed_once_while_cargo_plugin_additions_remain_cached() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let project = root.join("target/nuono-app");
        fs::create_dir_all(&project).unwrap();
        let application_lock = project.join("Cargo.lock");
        fs::write(root.join("Cargo.lock"), "baseline-one").unwrap();
        // 没有 `lockfile-seed.json` 记录的旧目录也必须同步为当前的根 Cargo.lock。
        fs::write(&application_lock, "untracked-old-lock").unwrap();
        sync_lockfile(root, &project).unwrap();
        assert_eq!(
            fs::read_to_string(&application_lock).unwrap(),
            "baseline-one"
        );
        fs::write(&application_lock, "baseline-one + plugin-dependencies").unwrap();
        sync_lockfile(root, &project).unwrap();
        assert_eq!(
            fs::read_to_string(&application_lock).unwrap(),
            "baseline-one + plugin-dependencies"
        );
        fs::write(root.join("Cargo.lock"), "baseline-two").unwrap();
        sync_lockfile(root, &project).unwrap();
        assert_eq!(
            fs::read_to_string(&application_lock).unwrap(),
            "baseline-two"
        );
        fs::remove_file(&application_lock).unwrap();
        sync_lockfile(root, &project).unwrap();
        assert_eq!(
            fs::read_to_string(&application_lock).unwrap(),
            "baseline-two"
        );
    }

    #[test]
    fn missing_or_failed_seed_never_claims_that_the_root_lock_was_applied() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let project = root.join("target/nuono-app");
        fs::create_dir_all(&project).unwrap();
        let receipt = project.join(LOCKFILE_SEED);
        sync_lockfile(root, &project).unwrap();
        assert!(!receipt.exists());
        fs::write(root.join("Cargo.lock"), "baseline").unwrap();
        let destination = project.join("Cargo.lock");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), "not a lockfile").unwrap();
        assert!(sync_lockfile(root, &project).is_err());
        assert!(!receipt.exists());
        assert_eq!(
            fs::read_to_string(destination.join("keep")).unwrap(),
            "not a lockfile"
        );
    }

    fn artifact(manifest: &Path, executable: &Path) -> Value {
        json!({"reason":"compiler-artifact", "manifest_path":manifest,
            "target":{"name":"nuono-application-candidate", "kind":["bin"]},
            "executable":executable, "fresh":true})
    }

    #[test]
    fn only_this_invocations_reported_executable_is_selected_including_explicit_target() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manifest = root.join("Cargo.toml");
        fs::write(&manifest, "fixture").unwrap();
        let old = root.join("target/debug/nuono-application-candidate");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "old program").unwrap();
        let current = root.join("target/explicit-host-triple/debug/nuono-application-candidate");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "new program").unwrap();
        let mut output = ArtifactOutput::default();
        assert!(
            output.finish().is_err(),
            "old output cannot substitute for a Cargo report"
        );
        output.observe(&artifact(&root.join("other/Cargo.toml"), &old), &manifest);
        assert!(output.finish().is_err());
        output.observe(&artifact(&manifest, &current), &manifest);
        assert_eq!(output.finish().unwrap(), current);
        output.observe(&artifact(&manifest, &old), &manifest);
        assert!(
            output
                .finish()
                .unwrap_err()
                .to_string()
                .contains("多个目标")
        );
    }

    #[test]
    fn cargo_protocol_keeps_diagnostics_and_progress_readable_without_dumping_artifacts() {
        let logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = logs.clone();
        let output: BuildOutput = Arc::new(move |line| captured.lock().unwrap().push(line.into()));
        let state = Mutex::new(ArtifactOutput::default());
        let manifest = Path::new("/fixture/Cargo.toml");
        handle_line("   Compiling fixture", manifest, &state, &output);
        handle_line(&json!({"reason":"compiler-message","message":{"rendered":"error: failed\n --> source.rs:1\n"}}).to_string(), manifest, &state, &output);
        handle_line(
            &artifact(manifest, Path::new("/fixture/program")).to_string(),
            manifest,
            &state,
            &output,
        );
        handle_line(
            "{\"reason\":\"build-finished\",\"success\":true}",
            manifest,
            &state,
            &output,
        );
        assert_eq!(
            *logs.lock().unwrap(),
            ["   Compiling fixture", "error: failed", " --> source.rs:1"]
        );
    }

    #[test]
    fn real_cargo_explicit_host_target_cannot_select_a_preexisting_native_candidate() {
        let version = Command::new("rustc").arg("-vV").output().unwrap();
        assert!(version.status.success());
        let version = String::from_utf8(version.stdout).unwrap();
        let host = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manifest = root.join("Cargo.toml");
        fs::write(&manifest, "[package]\nname=\"nuono-application\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n[[bin]]\nname=\"nuono-application-candidate\"\npath=\"main.rs\"\n").unwrap();
        fs::write(
            root.join("main.rs"),
            "fn main() { println!(\"isolated fixture\"); }\n",
        )
        .unwrap();
        let target = root.join("target");
        let name = format!(
            "nuono-application-candidate{}",
            std::env::consts::EXE_SUFFIX
        );
        let stale = target.join("debug").join(&name);
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"stale executable must not be selected").unwrap();
        let mut command = Command::new("cargo");
        command
            .args(["build", "--offline", "--manifest-path"])
            .arg(crate::source_build::external_path(&manifest))
            .args(["--bin", "nuono-application-candidate", "--target-dir"])
            .arg(crate::source_build::external_path(&target))
            .env("CARGO_BUILD_TARGET", host)
            .current_dir(crate::source_build::external_path(&root));
        let executable = run(&mut command, &manifest, None).unwrap();
        let expected = crate::source_build::external_path(&target)
            .join(host)
            .join("debug")
            .join(&name);
        assert_eq!(executable, expected);
        assert_eq!(
            fs::read(&stale).unwrap(),
            b"stale executable must not be selected"
        );
        assert!(fs::metadata(executable).unwrap().len() > 1024);
    }
}
