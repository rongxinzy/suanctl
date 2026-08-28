//! 远程设备扫描：解析 `~/.ssh/config` 的免密主机，执行系统级只读采集。
//!
//! 流程（权限预检测在前，避免采集到一半才提示）：
//! 1. 解析 ssh config，得到候选主机列表（跳过通配/含特殊字符的别名）。
//! 2. 每台主机：免密可达性探测（`ssh -o BatchMode=yes ... true`）。
//! 3. 权限预检测：`sudo -n true` 判定是否可执行 sudo。
//! 4. 采集：先执行无需 sudo 的项；sudo 可用才执行 sudo 项（dmesg 等）；
//!    无 sudo 时跳过并标记降级（degraded），结果中提示。
//!
//! 安全约束：ssh 参数固定（BatchMode + ConnectTimeout），别名经白名单校验
//! （仅字母数字 `-_.`，拒绝含 shell 元字符的别名）；远程命令为常量字符串，
//! 不拼接任何用户输入。

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::command::{CommandRequest, CommandRunner, ProcessCommandRunner};
use crate::domain::{CollectionIssue, HealthStatus, RemoteHostSnapshot, RemoteScanSnapshot};

/// 每条远程命令执行超时。
const REMOTE_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// ssh config 中的主机条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHostConfig {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// 解析 ~/.ssh/config 的 Host 块。支持缩进与注释；跳过含通配符的
/// Host 条目（如 `Host *`、`Host *.example`）与含 shell 元字符的别名。
pub fn parse_ssh_config(path: &Path) -> Vec<SshHostConfig> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    let mut current: Option<SshHostConfig> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Host" => {
                if let Some(host) = current.take() {
                    if is_safe_alias(&host.alias) {
                        hosts.push(host);
                    }
                }
                // 只取第一个别名；`Host a b` 取 a。
                let alias = value.split_whitespace().next().unwrap_or_default();
                current = Some(SshHostConfig {
                    alias: alias.to_owned(),
                    hostname: None,
                    user: None,
                    port: None,
                });
            }
            "HostName" => {
                if let Some(host) = current.as_mut() {
                    host.hostname = Some(value.to_owned());
                }
            }
            "User" => {
                if let Some(host) = current.as_mut() {
                    host.user = Some(value.to_owned());
                }
            }
            "Port" => {
                if let Some(host) = current.as_mut() {
                    host.port = value.parse().ok();
                }
            }
            _ => {}
        }
    }
    if let Some(host) = current.take() {
        if is_safe_alias(&host.alias) {
            hosts.push(host);
        }
    }
    hosts
}

/// 别名白名单：字母数字与 `-_.`，且不能是 `*` 通配。
fn is_safe_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias != "*"
        && !alias.contains('*')
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 远程只读采集器。`runner` 可注入以便测试。
pub struct RemoteScanner<R = ProcessCommandRunner> {
    runner: R,
    /// 解析后的候选主机（测试可注入）。
    hosts: Vec<SshHostConfig>,
}

impl RemoteScanner<ProcessCommandRunner> {
    /// 从 ~/.ssh/config 构建扫描器。
    pub fn from_ssh_config(config_path: &Path) -> Self {
        Self {
            runner: ProcessCommandRunner,
            hosts: parse_ssh_config(config_path),
        }
    }
}

impl<R: CommandRunner> RemoteScanner<R> {
    pub fn with_hosts(runner: R, hosts: Vec<SshHostConfig>) -> Self {
        Self { runner, hosts }
    }

    /// 主机总数（含不可达）。
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// 候选主机列表（只读展示用，不触发连接）。
    pub fn candidates(&self) -> &[SshHostConfig] {
        &self.hosts
    }

    /// 扫描全部主机（并行），返回完整扫描快照。
    pub fn scan_all(&self) -> RemoteScanSnapshot
    where
        R: Sync,
    {
        let mut hosts: Vec<RemoteHostSnapshot> = std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .hosts
                .iter()
                .map(|host| scope.spawn(move || self.scan_host(host)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        hosts.sort_by(|a, b| a.alias.cmp(&b.alias));
        let mut issues = Vec::new();
        let status = if hosts.is_empty() {
            issues.push(CollectionIssue {
                collector: "remote".to_owned(),
                code: "no_hosts".to_owned(),
                status: HealthStatus::Unavailable,
                message: "~/.ssh/config 中未找到可用主机（或均被跳过）".to_owned(),
            });
            HealthStatus::Unavailable
        } else if hosts.iter().any(|host| host.reachable) {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unavailable
        };
        RemoteScanSnapshot {
            scanned_at: now_millis(),
            hosts,
            issues,
            status,
        }
    }

    /// 扫描指定别名（不在候选列表则返回不可达结果）。
    pub fn scan_alias(&self, alias: &str) -> RemoteScanSnapshot {
        let host = self
            .hosts
            .iter()
            .find(|host| host.alias == alias)
            .cloned()
            .unwrap_or_else(|| SshHostConfig {
                alias: alias.to_owned(),
                hostname: None,
                user: None,
                port: None,
            });
        RemoteScanSnapshot {
            scanned_at: now_millis(),
            hosts: vec![self.scan_host(&host)],
            issues: Vec::new(),
            status: HealthStatus::Healthy,
        }
    }

    fn scan_host(&self, host: &SshHostConfig) -> RemoteHostSnapshot {
        let mut issues = Vec::new();
        if !self.reachable(host) {
            issues.push(CollectionIssue {
                collector: "remote".to_owned(),
                code: "ssh_unreachable".to_owned(),
                status: HealthStatus::Unavailable,
                message: format!("{} 免密连接失败（未配置免密或主机不可达）", host.alias),
            });
            return RemoteHostSnapshot {
                alias: host.alias.clone(),
                hostname: host.hostname.clone(),
                reachable: false,
                sudo_available: false,
                degraded: false,
                host_info: None,
                gpu_summary: None,
                kernel_log_tail: Vec::new(),
                issues,
                status: HealthStatus::Unavailable,
            };
        }

        // 权限预检测：先判 sudo，再决定采集项，避免采集到一半才提示。
        let sudo_available = self.sudo_available(host);

        // 无需 sudo 的采集项。
        let host_info = self
            .run_remote(host, HOST_INFO_CMD)
            .map(|output| truncate_summary(&output, 400));
        let gpu_summary = self
            .run_remote(host, GPU_SUMMARY_CMD)
            .map(|output| truncate_summary(&output, 400));

        // sudo 相关项：内核日志尾部。无 sudo 时尝试普通 dmesg，仍不可读则降级。
        let mut degraded = false;
        let mut kernel_log_tail = Vec::new();
        let dmesg_cmd = if sudo_available {
            "sudo -n dmesg -T 2>/dev/null | tail -n 30"
        } else {
            "dmesg -T 2>/dev/null | tail -n 30"
        };
        if let Some(output) = self.run_remote(host, dmesg_cmd) {
            let lines: Vec<String> = output.lines().map(|l| l.trim_end().to_owned()).collect();
            if lines.is_empty() {
                if !sudo_available {
                    degraded = true;
                    issues.push(CollectionIssue {
                        collector: "remote".to_owned(),
                        code: "sudo_unavailable".to_owned(),
                        status: HealthStatus::Warning,
                        message: format!(
                            "{} 当前用户无法执行 sudo，已降级：跳过内核日志（dmesg）采集",
                            host.alias
                        ),
                    });
                }
            } else {
                kernel_log_tail = lines;
            }
        } else if !sudo_available {
            degraded = true;
            issues.push(CollectionIssue {
                collector: "remote".to_owned(),
                code: "sudo_unavailable".to_owned(),
                status: HealthStatus::Warning,
                message: format!(
                    "{} 当前用户无法执行 sudo，已降级：跳过内核日志（dmesg）采集",
                    host.alias
                ),
            });
        }

        if sudo_available {
            issues.push(CollectionIssue {
                collector: "remote".to_owned(),
                code: "sudo_ok".to_owned(),
                status: HealthStatus::Healthy,
                message: format!("{} sudo 权限可用，系统级采集完整", host.alias),
            });
        }

        RemoteHostSnapshot {
            alias: host.alias.clone(),
            hostname: host.hostname.clone(),
            reachable: true,
            sudo_available,
            degraded,
            host_info,
            gpu_summary,
            kernel_log_tail,
            issues,
            status: if degraded {
                HealthStatus::Warning
            } else {
                HealthStatus::Healthy
            },
        }
    }

    /// 免密可达性：`ssh -o BatchMode=yes -o ConnectTimeout=5 <alias> -- true`。
    fn reachable(&self, host: &SshHostConfig) -> bool {
        self.run_remote(host, "true").is_some()
    }

    /// sudo 权限预检测：`ssh <alias> -- sudo -n true`。
    fn sudo_available(&self, host: &SshHostConfig) -> bool {
        self.run_remote(host, "sudo -n true").is_some()
    }

    fn run_remote(&self, host: &SshHostConfig, remote_cmd: &str) -> Option<String> {
        let mut request = CommandRequest::new(
            "ssh",
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=3",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &host.alias,
                "--",
                remote_cmd,
            ],
        );
        request.timeout = REMOTE_CMD_TIMEOUT;
        let output = self.runner.run(&request).ok()?;
        if output.success {
            Some(output.stdout)
        } else {
            None
        }
    }
}

/// 固定只读命令：主机与负载信息。
const HOST_INFO_CMD: &str =
    "hostname; uname -srm; head -n 2 /etc/os-release 2>/dev/null; uptime; cat /proc/loadavg 2>/dev/null";
/// 固定只读命令：GPU 摘要（nvidia-smi 不可用时尝试 querygpu 改名体，再尝试 efsmi）。
const GPU_SUMMARY_CMD: &str =
    "(nvidia-smi --query-gpu=index,name,utilization.gpu,temperature.gpu,memory.used --format=csv,noheader 2>/dev/null || querygpu --query-gpu=index,name,utilization.gpu,temperature.gpu,memory.used --format=csv,noheader 2>/dev/null || efsmi -q -d DEVICE,POWER,TEMP,MEMORY,USAGE 2>/dev/null | grep -E '^(DEV ID|[[:space:]]+(Dev Name|GCU Temp|Cur Power|Total Size|Used Size|GCU Usage)[[:space:]]*:)') || echo gpu-smi-unavailable";

fn truncate_summary(output: &str, max_chars: usize) -> String {
    let trimmed = output.trim();
    if trimmed.chars().count() > max_chars {
        let mut result: String = trimmed.chars().take(max_chars).collect();
        result.push('…');
        result
    } else {
        trimmed.to_owned()
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_config_hosts_and_skips_wildcards() {
        let dir = std::env::temp_dir().join(format!("suanctl-ssh-parse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(
            &path,
            r#"# comment
Host github.com
    HostName github.com
    User git

Host k1
    HostName 172.18.5.123
    User root
    Port 22

Host *
    User root

Host *.example
    User root
"#,
        )
        .unwrap();

        let hosts = parse_ssh_config(&path);
        assert_eq!(hosts.len(), 2, "通配条目应被跳过：{hosts:?}");
        assert_eq!(hosts[0].alias, "github.com");
        assert_eq!(hosts[1].alias, "k1");
        assert_eq!(hosts[1].hostname.as_deref(), Some("172.18.5.123"));
        assert_eq!(hosts[1].user.as_deref(), Some("root"));
        assert_eq!(hosts[1].port, Some(22));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsafe_alias_is_rejected() {
        assert!(is_safe_alias("k1"));
        assert!(is_safe_alias("node-2"));
        assert!(!is_safe_alias("*"));
        assert!(!is_safe_alias("a; rm -rf"));
        assert!(!is_safe_alias("x|y"));
        assert!(!is_safe_alias(""));
    }

    #[test]
    fn missing_config_yields_empty_hosts() {
        assert!(parse_ssh_config(Path::new("/nonexistent/ssh/config")).is_empty());
    }

    /// 模拟 ssh：按远程命令内容返回不同结果，支持开关 sudo 与可达性。
    struct FakeSsh {
        sudo: bool,
        reachable: bool,
    }

    impl super::super::command::CommandRunner for FakeSsh {
        fn run(
            &self,
            request: &super::super::command::CommandRequest,
        ) -> Result<super::super::command::CommandOutput, super::super::CollectorError> {
            assert_eq!(request.program, "ssh");
            let cmd = request.args.last().map(String::as_str).unwrap_or("");
            let success = match cmd {
                "true" => self.reachable,
                "sudo -n true" => self.reachable && self.sudo,
                _ => self.reachable,
            };
            let stdout = if !success {
                String::new()
            } else if cmd.contains("dmesg") {
                if self.sudo {
                    "2026-08-04T10:00:00 kernel: NVRM: Xid 31\n2026-08-04T10:00:01 kernel: boot ok\n"
                        .to_owned()
                } else {
                    String::new() // 无 sudo 时 dmesg 不可读
                }
            } else if cmd.starts_with("hostname") {
                "k1\nLinux 6.8.0 x86_64\nNAME=\"Ubuntu\"\n".to_owned()
            } else if cmd.contains("nvidia-smi") {
                "0, NVIDIA A100, 12, 42, 11264MiB\n".to_owned()
            } else {
                String::new()
            };
            Ok(super::super::command::CommandOutput {
                success,
                exit_code: Some(if success { 0 } else { 255 }),
                stdout,
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    fn host(alias: &str) -> SshHostConfig {
        SshHostConfig {
            alias: alias.to_owned(),
            hostname: Some("172.18.5.123".to_owned()),
            user: Some("root".to_owned()),
            port: None,
        }
    }

    #[test]
    fn sudo_unavailable_degrades_and_warns_early() {
        // 可达但无 sudo：权限预检测失败 → 跳过 dmesg，标记降级并在结果中提示。
        let scanner = RemoteScanner::with_hosts(
            FakeSsh {
                sudo: false,
                reachable: true,
            },
            vec![host("k1")],
        );
        let snapshot = scanner.scan_all();
        assert_eq!(snapshot.hosts.len(), 1);
        let host = &snapshot.hosts[0];
        assert!(host.reachable);
        assert!(!host.sudo_available);
        assert!(host.degraded);
        assert!(host.kernel_log_tail.is_empty());
        assert!(host.host_info.is_some());
        assert!(host.gpu_summary.is_some());
        assert!(host
            .issues
            .iter()
            .any(|issue| issue.code == "sudo_unavailable"));
        assert_eq!(host.status, HealthStatus::Warning);
    }

    #[test]
    fn sudo_available_collects_system_level_items() {
        let scanner = RemoteScanner::with_hosts(
            FakeSsh {
                sudo: true,
                reachable: true,
            },
            vec![host("k1")],
        );
        let snapshot = scanner.scan_all();
        let host = &snapshot.hosts[0];
        assert!(host.reachable);
        assert!(host.sudo_available);
        assert!(!host.degraded);
        assert_eq!(host.kernel_log_tail.len(), 2, "sudo 可用时采集 dmesg 尾部");
        assert!(host.kernel_log_tail[0].contains("NVRM: Xid"));
        assert_eq!(host.status, HealthStatus::Healthy);
    }

    #[test]
    fn unreachable_host_is_reported_without_panicking() {
        let scanner = RemoteScanner::with_hosts(
            FakeSsh {
                sudo: true,
                reachable: false,
            },
            vec![host("k1")],
        );
        let snapshot = scanner.scan_all();
        let host = &snapshot.hosts[0];
        assert!(!host.reachable);
        assert_eq!(host.status, HealthStatus::Unavailable);
        assert!(host
            .issues
            .iter()
            .any(|issue| issue.code == "ssh_unreachable"));
        assert!(!host.degraded);
    }
}
