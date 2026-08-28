//! 远程 Agent：把 suanctl 自身（或指定二进制）部署到免密主机并作为 worker 执行。
//!
//! 与 `remote` 的"固定白名单命令"不同，agent 在远程以本地模式运行 suanctl，
//! 从而获得完整能力（内置 CUDA P2P 测速、NCCL、日志异常检测、本地存储等）。
//!
//! 安全约束：
//! - ssh 参数固定（BatchMode + ConnectTimeout + accept-new），别名经白名单校验。
//! - 二进制经 ssh stdin 管道传输（不经远端 shell 拼接）。
//! - 远程路径固定为 `~/.suanctl/agent/suanctl`，与本地数据目录同根。
//! - 远程命令由本进程 argv 直传（不拼接 shell）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 远程 Agent 固定安装目录（相对远程 $HOME）。
pub const AGENT_REL_DIR: &str = ".suanctl/agent";
/// 远程 Agent 可执行文件名。
pub const AGENT_BIN: &str = "suanctl";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployState {
    /// 远程已存在且版本匹配，未重新上传。
    AlreadyDeployed,
    /// 本次完成上传。
    Uploaded,
    /// 上传并校验失败。
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployOutcome {
    pub state: DeployState,
    pub remote_path: String,
    pub local_version: String,
    pub remote_version: Option<String>,
}

/// 解析远程 `suanctl --version` 输出为版本号（如 "0.1.0"）。
pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
}

/// 在远程主机上确保部署 suanctl agent。
/// `force` 为 true 时忽略版本匹配，总是重新上传。
pub fn ensure_deployed(alias: &str, local_binary: &Path, force: bool) -> DeployOutcome {
    let local_version = env!("CARGO_PKG_VERSION").to_owned();
    let remote_dir = format!("~/{AGENT_REL_DIR}");
    let remote_path = format!("{remote_dir}/{AGENT_BIN}");

    // 1. 探测远程是否已有 agent。
    let remote_version = if !force {
        run_ssh(alias, &[&remote_path, "--version"])
            .ok()
            .map(|output| parse_version(&output).unwrap_or_else(|| "未知".to_owned()))
    } else {
        None
    };
    if remote_version.as_deref() == Some(local_version.as_str()) {
        return DeployOutcome {
            state: DeployState::AlreadyDeployed,
            remote_path,
            local_version,
            remote_version,
        };
    }

    // 2. 上传：stdin 管道传输，不经 shell。
    match upload_binary(alias, local_binary, &remote_dir, &remote_path) {
        Ok(()) => {
            let verified = run_ssh(alias, &[&remote_path, "--version"])
                .ok()
                .and_then(|output| parse_version(&output));
            if verified.as_deref() == Some(local_version.as_str()) {
                DeployOutcome {
                    state: DeployState::Uploaded,
                    remote_path,
                    local_version,
                    remote_version: verified,
                }
            } else {
                // None 说明二进制在远端根本无法执行（常见于本机 glibc 新于远端）。
                let hint = if verified.is_none() {
                    "；远端可能无法运行该二进制（如 glibc 版本过旧），\
                     请改用 musl 静态构建（make dist-musl）的 suanctl 执行 agent"
                } else {
                    ""
                };
                DeployOutcome {
                    state: DeployState::Failed(format!(
                        "上传后校验失败：远程版本 {verified:?} != 本地 {local_version}{hint}"
                    )),
                    remote_path,
                    local_version,
                    remote_version: verified,
                }
            }
        }
        Err(message) => DeployOutcome {
            state: DeployState::Failed(message),
            remote_path,
            local_version,
            remote_version,
        },
    }
}

/// 在远程执行 agent 子命令，透传 stdout/stderr，返回退出码。
pub fn run_agent(alias: &str, args: &[String]) -> Result<i32, String> {
    let remote_path = format!("~/{AGENT_REL_DIR}/{AGENT_BIN}");
    let status = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
            alias,
            "--",
            &remote_path,
        ])
        .args(args)
        .status()
        .map_err(|error| format!("ssh 执行失败：{error}"))?;
    Ok(status.code().unwrap_or(1))
}

/// 在远程执行 agent 子命令并捕获 stdout（用于握手等需要解析输出的场景）。
pub fn run_agent_captured(alias: &str, args: &[String]) -> Result<String, String> {
    let remote_path = format!("~/{AGENT_REL_DIR}/{AGENT_BIN}");
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
            alias,
            "--",
            &remote_path,
        ])
        .args(args)
        .output()
        .map_err(|error| format!("ssh 执行失败：{error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "远程命令失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 把本地二进制传输到远程固定路径（失败自动重试 2 次）。
/// 优先使用 rsync（增量/断点续传，大二进制更稳）；rsync 不可用时回退 ssh 管道。
fn upload_binary(
    alias: &str,
    local_binary: &Path,
    remote_dir: &str,
    remote_path: &str,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match upload_once(alias, local_binary, remote_dir, remote_path) {
            Ok(()) => return Ok(()),
            Err(message) => {
                last_error = Some(message);
                if attempt < 3 {
                    eprintln!("上传失败（第 {attempt} 次），重试…");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "未知上传错误".to_owned()))
}

fn upload_once(
    alias: &str,
    local_binary: &Path,
    remote_dir: &str,
    remote_path: &str,
) -> Result<(), String> {
    // rsync 需要远端目录存在。
    run_ssh(alias, &[&format!("mkdir -p {remote_dir}")])?;
    match upload_rsync(alias, local_binary, remote_path) {
        Ok(()) => return Ok(()),
        Err(message) => eprintln!("{message}"),
    }
    // 回退：ssh 管道直传。
    upload_ssh_pipe(alias, local_binary, remote_path)
}

/// rsync 增量/断点续传（大二进制首选）。
fn upload_rsync(alias: &str, local_binary: &Path, remote_path: &str) -> Result<(), String> {
    let ssh_opts = "ssh -o BatchMode=yes -o ConnectTimeout=5 \
                    -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
                    -o StrictHostKeyChecking=accept-new";
    // --info=progress2：rsync 总进度（百分比/速率/剩余，单行 \r 刷新）。
    // 注意：rsync 的进度写到 stdout，会污染 agent 子命令的 JSON 管道；
    // 这里把子进程 stdout 重定向到本进程 stderr，进度照常可见而管道保持干净。
    let size_mib = std::fs::metadata(local_binary)
        .map(|meta| meta.len() as f64 / 1048576.0)
        .unwrap_or(0.0);
    eprintln!("传输 {local_binary:?}（{size_mib:.1} MiB）到 {alias}:{remote_path} …");
    let started = std::time::Instant::now();
    let status = Command::new("rsync")
        .args([
            "-az",
            "--partial",
            "--inplace",
            "--info=progress2",
            "-e",
            ssh_opts,
        ])
        .arg(local_binary)
        .arg(format!("{alias}:{remote_path}"))
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|error| format!("rsync 不可用（{error}），回退 ssh 管道"))?;
    if status.success() {
        let elapsed = started.elapsed();
        eprintln!(
            "传输完成：{size_mib:.1} MiB / {:.1} s（约 {:.0} MiB/s）",
            elapsed.as_secs_f64(),
            size_mib / elapsed.as_secs_f64().max(0.001)
        );
        Ok(())
    } else {
        Err(format!(
            "rsync 传输失败（退出码 {:?}），回退 ssh 管道",
            status.code()
        ))
    }
}

/// 回退传输：二进制经 ssh stdin 管道直写远程路径。
fn upload_ssh_pipe(alias: &str, local_binary: &Path, remote_path: &str) -> Result<(), String> {
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "StrictHostKeyChecking=accept-new",
            alias,
            "--",
            &format!("cat > {remote_path} && chmod +x {remote_path}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("启动 ssh 上传失败：{error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "无法获取 ssh 标准输入".to_owned())?;
    let mut file = std::fs::File::open(local_binary)
        .map_err(|error| format!("读取本地二进制 {} 失败：{error}", local_binary.display()))?;
    std::io::copy(&mut file, &mut stdin).map_err(|error| format!("上传传输失败：{error}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|error| format!("等待上传完成失败：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "上传失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 固定参数 ssh 执行并返回 stdout（成功时）。
fn run_ssh(alias: &str, remote_args: &[&str]) -> Result<String, String> {
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
            alias,
            "--",
        ])
        .args(remote_args)
        .output()
        .map_err(|error| format!("ssh 失败：{error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "远程命令失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 当前进程的可执行文件路径（agent 上传源）。
pub fn current_binary() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("无法定位自身二进制：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_version_output() {
        assert_eq!(parse_version("suanctl 0.1.0\n"), Some("0.1.0".to_owned()));
        assert_eq!(parse_version("suanctl 0.1.0"), Some("0.1.0".to_owned()));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("unexpected\n"), None);
    }

    #[test]
    fn remote_path_layout_is_fixed() {
        assert_eq!(AGENT_REL_DIR, ".suanctl/agent");
        assert_eq!(AGENT_BIN, "suanctl");
        // 路径不包含任何 shell 元字符（经 ssh argv 直传的安全前提）。
        let full = format!("~/{AGENT_REL_DIR}/{AGENT_BIN}");
        assert!(full
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '~' | '/' | '.')));
    }
}
