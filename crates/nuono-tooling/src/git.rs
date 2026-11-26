//! 受超时与输出大小限制的 Git 命令执行器及独立仓库校验工具（不依赖宿主运行时策略）。
use crate::source_build;
use std::{
    path::Path as FsPath,
    process::{Command, Stdio},
    time::Duration,
};
const MAX_OUTPUT: usize = 512 * 1024;
/// Git 命令执行失败错误；`Display` 仅输出脱敏摘要，绝不包含仓库凭据。
pub struct GitError {
    message: String,
    stderr: String,
}
impl GitError {
    /// 原始的子进程标准错误输出（不可信文本），调用方在写入日志前必须先行脱敏。
    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}
impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::fmt::Debug for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for GitError {}
pub fn run(target: &FsPath, args: &[String], timeout: Duration) -> Result<String, GitError> {
    let mut stderr = String::new();
    execute(target, args, timeout, &mut stderr).map_err(|message| GitError { message, stderr })
}
pub fn git_sync(target: &FsPath, args: &[String], timeout: Duration) -> Result<String, String> {
    run(target, args, timeout).map_err(|error| error.to_string())
}
fn execute(
    target: &FsPath,
    args: &[String],
    timeout: Duration,
    stderr_output: &mut String,
) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "-c",
            "color.ui=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-C",
        ])
        .arg(target)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_command(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    execute_command(command, timeout, stderr_output)
}

fn execute_command(
    command: Command,
    timeout: Duration,
    stderr_output: &mut String,
) -> Result<String, String> {
    // 启动专用线程运行单线程 Tokio 运行时，使该同步接口既可在普通同步上下文中调用，
    // 也可在 Tokio 工作线程内安全调用，并支持在超时时取消 stdout/stderr 管道读取。
    let output = std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("nuono-git".into())
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "无法初始化 Git 执行环境".to_string())?;
                runtime.block_on(read_command(command, timeout))
            })
            .map_err(|_| "无法启动 Git 执行线程".to_string())?;
        worker
            .join()
            .map_err(|_| "Git 执行线程未正常退出".to_string())?
    })?;
    if !output.status.success() {
        *stderr_output = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err("Git 命令未能完成".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn read_command(command: Command, timeout: Duration) -> Result<std::process::Output, String> {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or("Git 超时设置超出时钟范围")?;
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "无法启动 Git，请检查是否已安装".to_string())?;
    let id = child.id();
    // Windows 没有进程组；把子进程放进作业对象后，超时可由 Job 连带回收其孙进程。
    // 必须紧接 spawn 执行：作业内的进程再派生的子进程会自动继承成员身份，
    // 越早加入，能逃逸到作业之外的后代就越少。
    #[cfg(windows)]
    let job = id.and_then(|id| job::ProcessJob::containing(id).ok());
    let stdout = child.stdout.take().ok_or("无法读取 Git 输出")?;
    let stderr = child.stderr.take().ok_or("无法读取 Git 错误")?;
    let result = tokio::time::timeout_at(deadline, async {
        let (stdout, stderr) = tokio::try_join!(drain(stdout), drain(stderr))?;
        // 在读完子进程管道前不要提前 wait 回收子进程，确保在超时清理前其 PID 不会被系统分配给其他进程组。
        let status = child.wait().await.map_err(|error| error.to_string())?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    let error = match result {
        Ok(Ok(output)) => return Ok(output),
        Ok(Err(error)) => error,
        Err(_) => "Git 操作超时，请稍后重试".to_string(),
    };
    #[cfg(unix)]
    if let Some(group) = id
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    #[cfg(windows)]
    if let Some(job) = &job {
        // 先终止整个作业，孙进程随之退出并释放继承来的管道写端；
        // 否则运行时销毁时会等待这些残留的异步读操作完成。
        job.terminate();
    }
    #[cfg(not(any(unix, windows)))]
    let _ = id;
    let _ = child.start_kill();
    // 超时后上方读取管道的 Future 已被丢弃并关闭了读端句柄，
    // 即便有孙进程脱离了进程组，清理流程也绝不会阻塞等待其管道 EOF。
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    Err(error)
}

/// Windows 作业对象：Unix 侧用进程组保证「超时杀掉整棵子孙进程树」，Windows 没有
/// 等价物，只能靠 Job Object。不这样做时，孙进程会继续持有继承来的 stdout/stderr
/// 写端，导致超时后的运行时销毁阻塞到它自行退出为止。
#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct ProcessJob(HANDLE);

    impl ProcessJob {
        /// 创建一个配置为「句柄关闭即终止全部成员」的作业，并把 `pid` 加入其中。
        pub fn containing(pid: u32) -> Result<Self, ()> {
            // SAFETY: 传入空安全属性与匿名作业名，只要求返回一个新句柄；失败时返回空句柄。
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(());
            }
            let job = Self(handle);

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: 结构体是 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 的完整可读副本，
            // 传入的长度与其大小一致，且句柄由上方创建并在此调用期间有效。
            let configured = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast::<c_void>(),
                    u32::try_from(std::mem::size_of_val(&limits)).map_err(|_| ())?,
                )
            };
            if configured == 0 {
                return Err(());
            }

            // SAFETY: 以最小权限打开目标进程，句柄在加入作业后立即关闭。
            let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                return Err(());
            }
            // SAFETY: 两个句柄均有效：`job.0` 由 `job` 持有，`process` 在下方关闭。
            let assigned = unsafe { AssignProcessToJobObject(job.0, process) };
            // SAFETY: `process` 是本进程用 `OpenProcess` 获得的、尚未关闭的句柄。
            unsafe { CloseHandle(process) };
            if assigned == 0 {
                return Err(());
            }
            Ok(job)
        }

        /// 立即终止作业内全部进程（含递归成员）。
        pub fn terminate(&self) {
            // SAFETY: 句柄由 `self` 持有且有效；退出码只用于标记被终止的进程。
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            // SAFETY: `self.0` 是本类型独占的、由 `CreateJobObjectW` 返回的句柄，
            // 且 `Drop` 只会执行一次。
            unsafe { CloseHandle(self.0) };
        }
    }
}

async fn drain(mut stream: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let size = stream
            .read(&mut buffer)
            .await
            .map_err(|_| "Git 输出读取失败")?;
        if size == 0 {
            return Ok(output);
        }
        if output.len() + size > MAX_OUTPUT {
            return Err("Git 输出过多，已停止读取".into());
        }
        output.extend_from_slice(&buffer[..size]);
    }
}
pub fn configure_command(command: &mut Command) {
    source_build::configure_git_command(command);
}
pub fn independent_repository_sync(target: &FsPath) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(target).map_err(|_| "插件源码目录不存在".to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("该插件目录是符号链接，请在终端管理".into());
    }
    let git_dir = target.join(".git");
    if !git_dir.exists() {
        return Err("该插件没有独立 Git 版本记录".into());
    }
    let git_metadata = std::fs::symlink_metadata(&git_dir).map_err(|_| "无法确认 Git 数据目录")?;
    if git_metadata.file_type().is_symlink() || !git_metadata.is_dir() {
        return Err("Git 数据目录不是独立目录，请在终端管理链接或工作树".into());
    }
    let toplevel = git_sync(
        target,
        &["rev-parse".into(), "--show-toplevel".into()],
        Duration::from_secs(15),
    )?;
    let actual = std::path::PathBuf::from(toplevel.trim())
        .canonicalize()
        .map_err(|_| "无法确认 Git 仓库目录")?;
    let expected = target.canonicalize().map_err(|_| "无法确认插件目录")?;
    if actual != expected {
        return Err("插件目录不属于独立 Git 仓库，请在终端管理框架源码".into());
    }
    let actual_git = git_sync(
        target,
        &["rev-parse".into(), "--absolute-git-dir".into()],
        Duration::from_secs(15),
    )?;
    if std::path::Path::new(actual_git.trim())
        .canonicalize()
        .map_err(|_| "无法确认 Git 数据目录")?
        != git_dir
            .canonicalize()
            .map_err(|_| "无法确认 Git 数据目录")?
    {
        return Err("Git 数据目录位于插件目录之外，已停止操作".into());
    }
    let common = git_sync(
        target,
        &["rev-parse".into(), "--git-common-dir".into()],
        Duration::from_secs(15),
    )?;
    let common = std::path::PathBuf::from(common.trim());
    let common = if common.is_absolute() {
        common
    } else {
        target.join(common)
    };
    if common
        .canonicalize()
        .map_err(|_| "无法确认 Git 共享数据目录")?
        != git_dir
            .canonicalize()
            .map_err(|_| "无法确认 Git 数据目录")?
    {
        return Err("Git 数据与其他仓库共享，请在终端管理".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/git/tests.rs"]
mod tests;
