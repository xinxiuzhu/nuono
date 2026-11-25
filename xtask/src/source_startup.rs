use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::plugin_checks::{cargo_target, copy_tree};

fn command(program: impl AsRef<std::ffi::OsStr>, directory: &Path, target: &Path) -> Command {
    let mut command = Command::new(program);
    // 测试桩使用自带的配置文件、数据库和监听地址；必须清除开发者环境中的 NUONO_* 变量，防止测试连向真实环境。
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("NUONO_") && key != "NUONO_CARGO_OFFLINE" {
            command.env_remove(key);
        }
    }
    command
        .current_dir(directory)
        .env("CARGO_TARGET_DIR", target)
        // 独立验证启动器与 profile 的默认增量编译策略，不受外部环境变量干扰。
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("CARGO_BUILD_INCREMENTAL")
        // 测试断言需要检查 Cargo 的纯文本构建输出（即使在开启了彩色输出的 CI 环境中）。
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_QUIET", "false");
    command
}

fn checked(command: &mut Command) -> Result<()> {
    println!("+ {command:?}");
    let status = command
        .status()
        .with_context(|| format!("execute {command:?}"))?;
    ensure!(status.success(), "command failed ({status}): {command:?}");
    Ok(())
}

/// 确保断言失败、超时或 HTTP 检查失败时，始终回收正在运行的子进程。
struct RunningChild(Child);

impl RunningChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(Self(
            command
                .spawn()
                .with_context(|| format!("spawn {command:?}"))?,
        ))
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn terminate(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            // 该子进程由当前测试直接拉起，不会误杀无关 PID。
            let result = unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
            if result != 0 {
                return Err(std::io::Error::last_os_error()).context("send SIGTERM to Nuono");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            bail!("the graceful SIGTERM binary startup check requires a Unix host")
        }
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            // 本地启动器管理着独立的业务子进程组；先发送信号让启动器转发停机指令并回收业务进程，再强杀启动器。
            #[cfg(unix)]
            if self.terminate().is_ok()
                && self
                    .wait_for(Duration::from_secs(25))
                    .ok()
                    .flatten()
                    .is_some()
            {
                return;
            }
            #[cfg(unix)]
            {
                // 强杀整个子进程组，包括尚未脱离该进程组的后代进程。
                unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn start(
    binary: &Path,
    directory: &Path,
    target: &Path,
    config: &Path,
    expect_config_failure: bool,
) -> Result<String> {
    // 将输出重定向到临时文件而非管道，防止父进程在轮询等待（最长 15 分钟）期间因 Cargo 输出过多日志而塞满管道死锁。
    let output = tempfile::NamedTempFile::new()?;
    let mut invocation = command(binary, directory, target);
    invocation.arg("--config").arg(config);
    // stdout 与 stderr 必须共用同一个底层文件描述符偏移量，避免两个独立打开的句柄互相覆盖输出。
    let log = File::create(output.path())?;
    invocation
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let mut child = RunningChild::spawn(&mut invocation)?;
    let status = child.wait_for(Duration::from_secs(900))?;
    if status.is_none() {
        drop(child);
        bail!(
            "source startup exceeded 900 seconds:\n{}",
            fs::read_to_string(output.path()).unwrap_or_default()
        );
    }
    let output = fs::read_to_string(output.path())?;
    let status = status.context("missing startup exit status")?;
    ensure!(
        !status.success() && (!expect_config_failure || output.contains("配置")),
        "source startup did not fail as expected ({status}):\n{output}"
    );
    Ok(output)
}

fn sha(path: &Path) -> Result<Vec<u8>> {
    Ok(
        Sha256::digest(fs::read(path).with_context(|| format!("read {}", path.display()))?)
            .to_vec(),
    )
}

fn executable(directory: &Path, name: &str) -> PathBuf {
    directory.join(format!("{name}{}", env::consts::EXE_SUFFIX))
}

fn dashboard_assets(index: &str) -> Result<BTreeSet<String>> {
    // Trunk 生成的 HTML 同时包含标签链接与内联 JS 中的模块/WASM 路径；
    // 直接正则提取全部静态资源路径，不写死特定构建的哈希文件名。
    let references = regex::Regex::new(r#"["'](/[^"'?#\s]+\.(?:css|m?js|wasm))["']"#)?;
    Ok(references
        .captures_iter(index)
        .map(|capture| capture[1].to_owned())
        .collect())
}

fn check_dashboard(
    client: &reqwest::blocking::Client,
    base: &str,
    runtime_dir: &Path,
) -> Result<()> {
    let static_dir = runtime_dir.join("nuono-hub/dist");
    let expected_index = fs::read_to_string(static_dir.join("index.html"))?;
    let response = client.get(base).send()?;
    ensure!(
        response.status().is_success(),
        "Dashboard: HTTP {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html")),
        "Dashboard response must be HTML"
    );
    let actual_index = response.text()?;
    // HTTP 服务层会在每次请求时向各个 `<script>` 标签动态注入 CSP nonce 属性。
    let nonce = regex::Regex::new(r#"<script nonce="[0-9a-f]+""#)?;
    ensure!(
        nonce.replace_all(&actual_index, "<script") == expected_index,
        "runtime did not serve the copied Dashboard index"
    );
    let assets = dashboard_assets(&actual_index)?;
    for extension in [".css", ".js", ".wasm"] {
        ensure!(
            assets.iter().any(|path| path.ends_with(extension)),
            "Dashboard index has no {extension} asset reference"
        );
    }
    for asset in assets {
        let expected = fs::read(static_dir.join(asset.trim_start_matches('/')))
            .with_context(|| format!("read copied Dashboard asset {asset}"))?;
        let response = client.get(format!("{base}{asset}")).send()?;
        ensure!(
            response.status().is_success(),
            "Dashboard asset {asset}: HTTP {}",
            response.status()
        );
        if asset.ends_with(".wasm") {
            ensure!(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    == Some("application/wasm"),
                "Dashboard WASM must be served as application/wasm"
            );
            ensure!(
                expected.starts_with(b"\0asm"),
                "Dashboard WASM is not a WebAssembly binary"
            );
        }
        ensure!(
            response.bytes()?.as_ref() == expected,
            "runtime did not serve the copied Dashboard asset {asset}"
        );
    }
    Ok(())
}

fn check_binary_runtime(
    runtime_dir: &Path,
    target: &Path,
    binary: &Path,
    expected_plugin: Option<&str>,
    expected_adapter: Option<&str>,
) -> Result<()> {
    check_runtime(
        runtime_dir,
        target,
        binary,
        expected_plugin,
        expected_adapter,
        false,
    )
}

fn check_runtime(
    runtime_dir: &Path,
    target: &Path,
    binary: &Path,
    expected_plugin: Option<&str>,
    expected_adapter: Option<&str>,
    source: bool,
) -> Result<()> {
    // 导出后的独立可执行文件绝不能受目录中残留的源码标识或损坏插件影响（即使 PATH 中完全没有 Cargo）。
    if !source {
        fs::write(
            runtime_dir.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/nuono-server\"]\n",
        )?;
        fs::create_dir_all(runtime_dir.join("crates/nuono-server"))?;
        fs::write(
            runtime_dir.join("crates/nuono-server/Cargo.toml"),
            "[package]\nname=\"nuono-server\"\n",
        )?;
        fs::create_dir_all(runtime_dir.join("plugins/broken"))?;
        fs::write(
            runtime_dir.join("plugins/broken/Cargo.toml"),
            "must not be read",
        )?;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let configured_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let configured_port = configured_listener.local_addr()?.port();
    let config = runtime_dir.join("runtime config.toml");
    let database_url = toml::Value::String(format!(
        "sqlite://{}/runtime.db?mode=rwc",
        runtime_dir.to_string_lossy().replace('\\', "/")
    ));
    fs::write(
        &config,
        format!(
            "host=\"127.0.0.1\"\nport={configured_port}\nsuperusers=[]\nmasters=[]\n[log]\nlevel=\"INFO\"\noutput=[\"stdout\"]\nmodule=false\n[database]\nurl={database_url}\nmax_connections=4\n[upstream]\ndefault_url=\"ws://127.0.0.1:{port}/ws\"\nreconnect_interval_secs=5\nheartbeat_timeout_secs=30\n[backup]\nenabled=false\n[security]\nadmin_auth_enabled=false\n"
        ),
    )?;
    let log_path = runtime_dir.join("runtime.log");
    let log = File::create(&log_path)?;
    let mut invocation = command(binary, runtime_dir, target);
    invocation
        .arg("--config")
        .arg(&config)
        .arg("--port")
        .arg(port.to_string())
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if !source {
        invocation.env("PATH", runtime_dir);
    }
    let mut process = RunningChild::spawn(&mut invocation)?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut ready = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && process.0.try_wait()?.is_none() {
        if client
            .get(format!("http://127.0.0.1:{port}/readyz"))
            .timeout(Duration::from_secs(1))
            .send()
            .is_ok_and(|response| response.status() == reqwest::StatusCode::OK)
        {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(
        ready,
        "exported runtime did not become ready:\n{}",
        fs::read_to_string(&log_path).unwrap_or_default()
    );
    check_dashboard(&client, &format!("http://127.0.0.1:{port}"), runtime_dir)?;
    let base = format!("http://127.0.0.1:{port}");
    let types: serde_json::Value = serde_json::from_slice(
        &client
            .get(format!("{base}/api/bot-instance-types"))
            .send()?
            .bytes()?,
    )?;
    let types = types["data"]
        .as_array()
        .with_context(|| format!("missing adapter type list: {types}"))?;
    if let Some(adapter) = expected_adapter {
        ensure!(
            types.iter().any(|item| item["adapter_type"] == adapter),
            "selected adapter was not registered"
        );
    } else {
        ensure!(
            types.is_empty(),
            "zero-adapter program invented an instance type"
        );
    }
    ensure!(
        client.get(format!("{base}/ws")).send()?.status() == reqwest::StatusCode::NOT_FOUND,
        "OneBot endpoint is available without OneBot installed"
    );
    let session: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/sandbox/sessions"))
            .header("content-type", "application/json")
            .body(r#"{"message_type":"private"}"#)
            .send()?
            .bytes()?,
    )?;
    let id = session["data"]["id"]
        .as_str()
        .context("sandbox did not open without OneBot")?;
    let receipt: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/sandbox/sessions/{id}/messages"))
            .header("content-type", "application/json")
            .body(r#"{"text":"hello"}"#)
            .send()?
            .bytes()?,
    )?;
    ensure!(
        receipt["success"] == true,
        "sandbox rejected a canonical message: {receipt}"
    );
    ensure!(
        client
            .delete(format!("{base}/api/sandbox/sessions/{id}"))
            .send()?
            .status()
            .is_success(),
        "sandbox close failed"
    );
    if source {
        // 服务就绪（/readyz）开始接收流量时，后台的历史构建产物清理任务可能仍持有源码操作锁；
        // 触发重启测试前需先等待首次后台清理完成。
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if fs::read_to_string(&log_path)?.contains("历史构建产物后台维护完成") {
                break;
            }
            ensure!(
                Instant::now() < deadline && process.0.try_wait()?.is_none(),
                "source artifact maintenance did not finish before restart:\n{}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
    check_chat_restart(
        &client,
        &base,
        &mut process,
        &log_path,
        if source {
            Duration::from_secs(900)
        } else {
            Duration::from_secs(45)
        },
    )?;
    check_dashboard(&client, &base, runtime_dir)?;
    process.terminate()?;
    let status = process.wait_for(Duration::from_secs(20))?;
    ensure!(
        status.is_some_and(|status| status.success()),
        "exported runtime did not exit successfully after SIGTERM ({status:?}):\n{}",
        fs::read_to_string(&log_path).unwrap_or_default()
    );
    let output = fs::read_to_string(&log_path)?;
    if source {
        ensure!(
            output.lines().any(|line| line.contains("Running ")
                && line.contains("--config")
                && line.contains("runtime config.toml")),
            "source restart did not execute Cargo's run command:\n{output}"
        );
    }
    if let Some(plugin) = expected_plugin {
        ensure!(
            output
                .lines()
                .any(|line| line.contains(plugin) && line.contains("已加载")),
            "selected plugin did not finish loading:\n{output}"
        );
        if source {
            ensure!(
                output
                    .lines()
                    .any(|line| line.contains(plugin) && line.contains("运行已启动")),
                "selected plugin did not start business runtime after setup:\n{output}"
            );
        }
    }
    ensure!(
        !output.contains("startup-two"),
        "removed plugin was loaded:\n{output}"
    );
    ensure!(
        source || (!output.contains("Compiling ") && !output.contains("统一构建")),
        "exported runtime attempted source compilation:\n{output}"
    );
    Ok(())
}

/// 通过隔离的沙盒虚拟会话触发核心重启指令。
/// 临时管理员 ID 仅写入当前测试桩的临时 SQLite 数据库。
fn check_chat_restart(
    client: &reqwest::blocking::Client,
    base: &str,
    process: &mut RunningChild,
    log_path: &Path,
    timeout: Duration,
) -> Result<()> {
    let read_pid = || -> Option<u64> {
        let response = client
            .get(format!("{base}/api/system/info"))
            .timeout(Duration::from_secs(1))
            .send()
            .ok()?;
        let value: serde_json::Value = serde_json::from_slice(&response.bytes().ok()?).ok()?;
        value["data"]["pid"].as_u64()
    };
    let original_pid = read_pid().context("runtime did not report its PID")?;
    ensure!(
        original_pid != u64::from(process.0.id()),
        "local launcher did not separate service lifetime"
    );
    let session: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/sandbox/sessions"))
            .header("content-type", "application/json")
            .body(r#"{"message_type":"private"}"#)
            .send()?
            .bytes()?,
    )?;
    let id = session["data"]["id"]
        .as_str()
        .context("missing restart session")?;
    let user = session["data"]["user_id"]
        .as_str()
        .context("missing virtual user")?;
    let setup: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/setup"))
            .header("content-type", "application/json")
            .body(r#"{"username":"fixture_owner","password":"FixtureOwnerPassword123!"}"#)
            .send()?
            .bytes()?,
    )?;
    ensure!(
        setup["success"] == true,
        "fixture owner setup failed: {setup}"
    );
    let login_response = client
        .post(format!("{base}/api/auth/login"))
        .header("content-type", "application/json")
        .body(r#"{"username":"fixture_owner","password":"FixtureOwnerPassword123!"}"#)
        .send()?;
    let mut session_token = None;
    let mut csrf_token = None;
    for header in login_response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
    {
        let cookie = header.to_str().unwrap_or_default();
        if let Some(pair) = cookie.split(';').next() {
            if let Some(value) = pair.strip_prefix("session_token=") {
                session_token = Some(value.to_owned());
            } else if let Some(value) = pair.strip_prefix("csrf_token=") {
                csrf_token = Some(value.to_owned());
            }
        }
    }
    let login: serde_json::Value = serde_json::from_slice(&login_response.bytes()?)?;
    ensure!(
        login["success"] == true,
        "fixture owner login failed: {login}"
    );
    let session_token = session_token.context("missing session_token cookie after login")?;
    let csrf_token = csrf_token.context("missing csrf_token cookie after login")?;
    let cookie_header = format!("session_token={session_token}; csrf_token={csrf_token}");
    let bind: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/users/1/identities"))
            .header("content-type", "application/json")
            .header(reqwest::header::COOKIE, &cookie_header)
            .header("x-csrf-token", &csrf_token)
            .body(
                serde_json::json!({
                    "platform": "onebot",
                    "native_user_id": user,
                })
                .to_string(),
            )
            .send()?
            .bytes()?,
    )?;
    ensure!(
        bind["success"] == true,
        "fixture owner identity bind failed: {bind}"
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        &client
            .post(format!("{base}/api/sandbox/sessions/{id}/messages"))
            .header("content-type", "application/json")
            .body(r#"{"text":"nuono重启"}"#)
            .send()?
            .bytes()?,
    )?;
    ensure!(
        receipt["success"] == true,
        "restart message rejected: {receipt}"
    );
    println!("Checking full restart through nuono重启 (service PID {original_pid})...");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && process.0.try_wait()?.is_none() {
        if read_pid().is_some_and(|pid| pid != original_pid)
            && client
                .get(format!("{base}/readyz"))
                .timeout(Duration::from_secs(1))
                .send()
                .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "chat restart did not return a ready new process on the original CLI port:\n{}",
        fs::read_to_string(log_path).unwrap_or_default()
    )
}

pub fn run(root: &Path, profile: &str) -> Result<()> {
    ensure!(
        matches!(profile, "dev" | "release"),
        "unknown startup profile: {profile}"
    );
    let target = cargo_target(root)?;
    let mut build = command("cargo", root, &target);
    build.args([
        "build",
        "--locked",
        "-p",
        "nuono-tooling",
        "--profile",
        profile,
    ]);
    if env::var_os("NUONO_CARGO_OFFLINE").is_some_and(|value| !value.is_empty()) {
        build.arg("--offline");
    }
    checked(&mut build)?;
    let profile_directory = target.join(if profile == "dev" { "debug" } else { profile });
    let binary = executable(&profile_directory, "nuono");
    let application = executable(&profile_directory, "nuono-application");
    let temporary = tempfile::Builder::new()
        .prefix("nuono-source-startup-")
        .tempdir()?;
    let directory = temporary.path();
    for name in [
        "crates",
        ".cargo",
        "nuono-hub/dist",
        "xtask/fixtures/adapter-host",
    ] {
        copy_tree(&root.join(name), &directory.join(name), &["target", "logs"])?;
    }
    for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "LICENSE"] {
        fs::copy(root.join(name), directory.join(name))?;
    }
    for name in ["startup-one", "startup-two"] {
        checked(command(&binary, directory, &target).args(["plugin", "new", name]))?;
        fs::write(
            directory.join("plugins").join(name).join("src/lib.rs"),
            format!(
                "use nuono_plugin::prelude::*;\n#[derive(Default)]\n#[nuono_plugin(name=\"{name}\",version=\"0.0.1\")]\nstruct Probe;\nregister_plugin!(Probe);\n"
            ),
        )?;
    }
    let hidden = directory.join("plugins/.unselected");
    fs::create_dir_all(&hidden)?;
    fs::write(hidden.join("Cargo.toml"), "this must never compile")?;
    let config = directory.join("invalid.toml");
    // 源码构建配置会在宿主运行时校验启动参数之前被读取。
    fs::write(
        &config,
        "host=\"127.0.0.1\"\nport=0\nsuperusers=[]\nmasters=[]\n[log]\nlevel=\"INFO\"\noutput=[\"stdout\"]\nmodule=false\n[database]\nurl=\"sqlite::memory:\"\nmax_connections=1\n",
    )?;
    println!("Checking the first source startup with zero installed extensions...");
    let parked = directory.join(".parked-plugins");
    fs::rename(directory.join("plugins"), &parked)?;
    let empty = start(&binary, directory, &target, &config, true)?;
    fs::rename(&parked, directory.join("plugins"))?;
    ensure!(
        empty.contains("0 个已安装源码扩展"),
        "first empty workspace must build the full application: {empty}"
    );
    println!("Checking initial incremental build with two installed source plugins...");
    let first = start(&binary, directory, &target, &config, true)?;
    ensure!(
        first.contains("2 个已安装源码扩展"),
        "expected two installed plugins:\n{first}"
    );
    println!("Checking runtime-only declaration command forwarded by the source entry...");
    let description = command(&binary, directory, &target)
        .args(["release", "describe"])
        .output()?;
    ensure!(
        description.status.success(),
        "source declaration forwarding failed: {}",
        String::from_utf8_lossy(&description.stderr)
    );
    let description: serde_json::Value = serde_json::from_slice(&description.stdout)?;
    let expected_profile = if profile == "dev" { "debug" } else { profile };
    ensure!(
        description["build"]["build_profile"] == expected_profile,
        "application build metadata lost the selected Cargo profile: {description}"
    );
    ensure!(
        description["extensions"]
            .as_array()
            .is_some_and(|extensions| extensions.len() == 2),
        "declaration must come from the linked candidate: {description}"
    );
    let original = sha(&application)?;
    let publication_path = target.join(format!(
        "nuono-app/published-{}.json",
        if profile == "release" {
            "release"
        } else {
            "debug"
        }
    ));
    let receipt_before = fs::read(&publication_path)?;
    let artifact_count_before = fs::read_dir(target.join("nuono-app/artifacts"))?.count();
    let manifest_path = target.join("nuono-app/Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)?;
    ensure!(
        manifest.contains("incremental = true")
            && manifest.contains("startup-one")
            && manifest.contains("startup-two"),
        "generated manifest must select both plugins and default to incremental compilation:\n{manifest}"
    );
    println!("Checking cached startup without recompilation...");
    let warm = start(&binary, directory, &target, &config, true)?;
    ensure!(
        !warm.contains("Compiling "),
        "unchanged source rebuilt unexpectedly:\n{warm}"
    );
    ensure!(
        sha(&application)? == original,
        "unchanged application was relinked"
    );
    ensure!(
        warm.contains("应用产物未变化")
            && fs::read(&publication_path)? == receipt_before
            && fs::read_dir(target.join("nuono-app/artifacts"))?.count() == artifact_count_before,
        "unchanged application was republished or created another candidate:\n{warm}"
    );
    let source = directory.join("plugins/startup-one/src/lib.rs");
    fs::write(
        &source,
        fs::read_to_string(&source)?.replace("version=\"0.0.1\"", "version=\"0.0.2\""),
    )?;
    println!("Checking rebuild after a plugin source change...");
    let changed = start(&binary, directory, &target, &config, true)?;
    ensure!(
        changed.contains("Compiling startup-one ")
            && changed.contains("Compiling nuono-application "),
        "changed plugin and application were not rebuilt:\n{changed}"
    );
    ensure!(
        !changed.contains("Compiling nuono-core ") && !changed.contains("Compiling nuono-server "),
        "a plugin change rebuilt the host:\n{changed}"
    );
    ensure!(
        sha(&application)? != original,
        "plugin changes did not relink application"
    );
    fs::remove_dir_all(directory.join("plugins/startup-two"))?;
    println!("Checking rebuild after plugin removal...");
    let removed = start(&binary, directory, &target, &config, true)?;
    ensure!(
        removed.contains("1 个已安装源码扩展"),
        "expected one installed plugin after uninstall:\n{removed}"
    );
    ensure!(
        !fs::read_to_string(&manifest_path)?.contains("startup-two"),
        "uninstalled plugin remains in application manifest"
    );
    let good = fs::read_to_string(&source)?;
    fs::write(&source, "this is invalid Rust")?;
    println!("Checking failure on invalid plugin source...");
    let failure = start(&binary, directory, &target, &config, false)?;
    ensure!(
        failure.contains("应用构建失败"),
        "invalid source did not fail during application build:\n{failure}"
    );
    fs::write(&source, good)?;
    println!("Checking recovery after restoring valid source...");
    start(&binary, directory, &target, &config, true)?;
    println!("Checking source launcher restart through Cargo and the original CLI port...");
    check_runtime(directory, &target, &binary, Some("startup-one"), None, true)?;
    let runtime_dir = directory.join("binary-runtime");
    fs::create_dir(&runtime_dir)?;
    let exported = executable(&runtime_dir, "nuono");
    println!("Checking single executable export...");
    checked(
        command(&binary, directory, &target)
            .args(["build", "--profile", profile, "--output"])
            .arg(&exported),
    )?;
    ensure!(
        sha(&exported)? == sha(&application)?,
        "exported executable does not match the compiled application"
    );
    // 显式准备好独立二进制运行所需的静态前端资源与启动配置。
    copy_tree(
        &directory.join("nuono-hub/dist"),
        &runtime_dir.join("nuono-hub/dist"),
        &[],
    )?;
    println!("Checking direct binary and Dashboard startup without Cargo...");
    check_binary_runtime(&runtime_dir, &target, &exported, Some("startup-one"), None)?;
    println!("Checking mixed plugin/adapter composition...");
    checked(command(&binary, directory, &target).args(["adapter", "new", "startup-adapter"]))?;
    let mixed = start(&binary, directory, &target, &config, true)?;
    ensure!(
        mixed.contains("2 个已安装源码扩展"),
        "mixed composition was not built: {mixed}"
    );
    fs::remove_dir_all(directory.join("plugins/startup-one"))?;
    let adapter_only = start(&binary, directory, &target, &config, true)?;
    ensure!(
        adapter_only.contains("1 个已安装源码扩展"),
        "adapter-only composition was not built: {adapter_only}"
    );
    let adapter_runtime = directory.join("adapter-runtime");
    fs::create_dir(&adapter_runtime)?;
    let adapter_export = executable(&adapter_runtime, "nuono");
    checked(
        command(&binary, directory, &target)
            .args(["build", "--profile", profile, "--output"])
            .arg(&adapter_export),
    )?;
    copy_tree(
        &directory.join("nuono-hub/dist"),
        &adapter_runtime.join("nuono-hub/dist"),
        &[],
    )?;
    check_binary_runtime(
        &adapter_runtime,
        &target,
        &adapter_export,
        None,
        Some("startup-adapter"),
    )?;
    fs::remove_dir_all(directory.join("adapters/startup-adapter"))?;
    let empty_runtime = directory.join("empty-runtime");
    fs::create_dir(&empty_runtime)?;
    let empty_export = executable(&empty_runtime, "nuono");
    checked(
        command(&binary, directory, &target)
            .args(["build", "--profile", profile, "--output"])
            .arg(&empty_export),
    )?;
    copy_tree(
        &directory.join("nuono-hub/dist"),
        &empty_runtime.join("nuono-hub/dist"),
        &[],
    )?;
    check_binary_runtime(&empty_runtime, &target, &empty_export, None, None)?;
    println!("Checking source startup after removing the final extension...");
    let empty_source = start(&binary, directory, &target, &config, true)?;
    ensure!(
        empty_source.contains("0 个已安装源码扩展"),
        "empty source composition must use the generated full application: {empty_source}"
    );
    let manifest = fs::read_to_string(&manifest_path)?;
    ensure!(
        !manifest.contains("startup-adapter") && !manifest.contains("startup-one"),
        "removed source extensions remain in the empty application"
    );
    println!(
        "Selected source plugins, cached incremental rebuilds, unchanged publication reuse, uninstall, compilation failures, single executable export and compiler-free Dashboard startup passed."
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn dashboard_asset_references_include_inline_wasm_and_deduplicate_preloads() {
        let index = r#"<link href="/main-a.css"><script type="module">
import init from '/dashboard-a.js';
await init({ module_or_path: '/dashboard-a_bg.wasm' });
</script><link href="/dashboard-a.js"><link href="/snippets/module.js">"#;
        assert_eq!(
            dashboard_assets(index).unwrap(),
            BTreeSet::from([
                "/main-a.css".to_owned(),
                "/dashboard-a.js".to_owned(),
                "/dashboard-a_bg.wasm".to_owned(),
                "/snippets/module.js".to_owned(),
            ])
        );
    }

    #[test]
    fn runtime_subprocesses_ignore_ambient_nuono_variables() {
        const CHILD: &str = "NUONO_XTASK_ENV_TEST_CHILD";
        if env::var_os(CHILD).is_some() {
            let temporary = tempfile::tempdir().unwrap();
            let status = command("sh", temporary.path(), temporary.path())
                .args([
                    "-c",
                    "test -z \"${NUONO_SUPERVISED+x}\" && test -z \"${NUONO_RESTART_EXIT_CODE+x}\" && test \"$NUONO_CARGO_OFFLINE\" = 1 && test \"$XTASK_ENV_TEST_KEEP\" = keep && test -z \"${CARGO_INCREMENTAL+x}\" && test -z \"${CARGO_BUILD_INCREMENTAL+x}\" && test \"$CARGO_TERM_COLOR\" = never",
                ])
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // 启动独立子进程注入环境变量，避免在并行跑其他测试时污染当前测试进程的全局环境变量。
        let status = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "source_startup::tests::runtime_subprocesses_ignore_ambient_nuono_variables",
            ])
            .env(CHILD, "1")
            .env("NUONO_SUPERVISED", "1")
            .env("NUONO_RESTART_EXIT_CODE", "99")
            .env("NUONO_CARGO_OFFLINE", "1")
            .env("CARGO_TERM_COLOR", "always")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_BUILD_INCREMENTAL", "false")
            .env("XTASK_ENV_TEST_KEEP", "keep")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
