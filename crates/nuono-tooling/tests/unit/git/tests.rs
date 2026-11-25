use super::*;
use std::time::Instant;

const FIXTURE_MODE: &str = "NUONO_GIT_PIPE_FIXTURE";

// 在独立子进程中运行测试桩，模拟孙进程继承真实 Git stdout/stderr 管道的场景。
#[test]
fn process_fixture() {
    match std::env::var(FIXTURE_MODE).as_deref() {
        Ok("hold") => std::thread::sleep(Duration::from_secs(2)),
        Ok("success") => {
            println!("stdout-marker");
            eprintln!("stderr-marker");
        }
        Ok("failure") => {
            eprintln!("stderr-marker");
            std::process::exit(1);
        }
        Ok("overflow") => {
            use std::io::Write;
            let _ = std::io::stdout()
                .lock()
                .write_all(&vec![b'x'; MAX_OUTPUT + 1]);
            std::thread::sleep(Duration::from_secs(2));
        }
        Ok(mode @ ("parent" | "detached")) => {
            let mut child = fixture("hold");
            #[cfg(unix)]
            if mode == "detached" {
                use std::os::unix::process::CommandExt;
                child.process_group(0);
            }
            #[cfg(not(unix))]
            let _ = mode;
            let _child = child.spawn().unwrap();
            std::process::exit(0);
        }
        _ => {}
    }
}

fn fixture(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "git::tests::process_fixture", "--nocapture"])
        .env(FIXTURE_MODE, mode);
    command
}

#[test]
fn timeout_includes_descendant_pipes_after_the_parent_exits() {
    for mode in ["parent", "detached"] {
        let mut command = fixture(mode);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let started = Instant::now();
        let result = execute_command(command, Duration::from_millis(150), &mut String::new());
        assert!(result.is_err(), "{mode} must time out: {result:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{mode} waited for the pipe holder"
        );
    }
}

#[test]
fn bounded_reads_preserve_success_failure_details_and_output_limits() {
    for mode in ["success", "failure", "overflow"] {
        let mut command = fixture(mode);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut stderr = String::new();
        let result = execute_command(command, Duration::from_secs(1), &mut stderr);
        match mode {
            "success" => assert!(result.unwrap().contains("stdout-marker")),
            "failure" => {
                assert_eq!(result.unwrap_err(), "Git 命令未能完成");
                assert!(stderr.contains("stderr-marker"));
            }
            _ => assert_eq!(result.unwrap_err(), "Git 输出过多，已停止读取"),
        }
    }
}
