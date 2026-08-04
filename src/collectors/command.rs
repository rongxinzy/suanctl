//! 受控的只读外部命令执行。
//!
//! 这里故意只接受程序和参数，不经过 shell。stdout/stderr 在独立线程中持续
//! 排空并限制大小，避免命令因为管道写满而无法退出。

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

use super::CollectorError;

pub const DEFAULT_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
pub const DEFAULT_STDERR_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

impl CommandRequest {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: Duration::from_secs(2),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub trait CommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
        run_process(request)
    }
}

fn run_process(request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
    let mut child = Command::new(&request.program)
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            CollectorError::new(
                "command",
                "spawn_failed",
                format!("无法启动 {}：{error}", request.program),
            )
        })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        CollectorError::new("command", "stdout_unavailable", "子进程 stdout 不可用")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CollectorError::new("command", "stderr_unavailable", "子进程 stderr 不可用")
    })?;

    let stdout_limit = request.stdout_limit;
    let stderr_limit = request.stderr_limit;
    let stdout_thread = thread::spawn(move || read_limited(stdout, stdout_limit));
    let stderr_thread = thread::spawn(move || read_limited(stderr, stderr_limit));

    let status = match child.wait_timeout(request.timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            // 超时后必须 kill 并 wait，避免留下 nvidia-smi 子进程。
            let _ = child.kill();
            let _ = child.wait();
            let stdout = join_reader(stdout_thread)?;
            let stderr = join_reader(stderr_thread)?;
            return Ok(CommandOutput {
                success: false,
                exit_code: None,
                stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
                stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
                timed_out: true,
                stdout_truncated: stdout.truncated,
                stderr_truncated: stderr.truncated,
            });
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_reader(stdout_thread);
            let _ = join_reader(stderr_thread);
            return Err(CollectorError::new(
                "command",
                "wait_failed",
                format!("等待 {} 结束失败：{error}", request.program),
            ));
        }
    };

    let stdout = join_reader(stdout_thread)?;
    let stderr = join_reader(stderr_thread)?;
    Ok(CommandOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        timed_out: false,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

#[derive(Debug)]
struct LimitedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> io::Result<LimitedBytes> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        if remaining > 0 {
            bytes.extend_from_slice(&buffer[..count.min(remaining)]);
        }
        if count > remaining {
            truncated = true;
        }
    }
    Ok(LimitedBytes { bytes, truncated })
}

fn join_reader(
    handle: thread::JoinHandle<io::Result<LimitedBytes>>,
) -> Result<LimitedBytes, CollectorError> {
    handle
        .join()
        .map_err(|_| CollectorError::new("command", "reader_failed", "读取子进程输出线程异常"))?
        .map_err(|error| {
            CollectorError::new(
                "command",
                "read_failed",
                format!("读取子进程输出失败：{error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{CommandRequest, CommandRunner, ProcessCommandRunner};

    #[test]
    fn collectors_command_runner_rejects_missing_program_structurally() {
        let request = CommandRequest::new("suanctl-command-that-does-not-exist", ["--version"]);
        let error = ProcessCommandRunner
            .run(&request)
            .expect_err("missing command");
        assert_eq!(error.collector, "command");
        assert_eq!(error.code, "spawn_failed");
    }

    #[cfg(unix)]
    #[test]
    fn collectors_command_runner_times_out_and_reports_timeout() {
        let mut request = CommandRequest::new("sleep", ["2"]);
        request.timeout = std::time::Duration::from_millis(20);
        let output = ProcessCommandRunner.run(&request).expect("timeout output");
        assert!(output.timed_out);
        assert!(!output.success);
    }
}
