//! CUDA stack 的只读探针。
//!
//! 这里严格区分三类事实：驱动头部宣称的最高 CUDA 兼容版本、`nvcc`
//! 工具链版本，以及常见运行库文件是否存在。不会因为 `nvidia-smi`
//! 输出 CUDA Version 就声称 CUDA Toolkit 已安装，也不会运行 GPU workload。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{
    CollectionIssue, CudaLibrarySnapshot, CudaStackSnapshot, HealthStatus, LocalProbeStatus,
    PresenceStatus,
};

use super::command::{
    CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};

pub const DEFAULT_NVCC_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvidiaSmiHeader {
    pub driver_version: Option<String>,
    pub cuda_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CudaCollection {
    pub snapshot: CudaStackSnapshot,
    pub issues: Vec<CollectionIssue>,
}

#[derive(Debug, Clone)]
pub struct CudaStackCollector<R = ProcessCommandRunner> {
    runner: R,
    filesystem_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl CudaStackCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            filesystem_root: PathBuf::from("/"),
            timeout: DEFAULT_NVCC_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

impl Default for CudaStackCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> CudaStackCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            filesystem_root: PathBuf::from("/"),
            timeout: DEFAULT_NVCC_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_filesystem_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.filesystem_root = root.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_output_limits(mut self, stdout_limit: usize, stderr_limit: usize) -> Self {
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        self
    }
}

impl<R: CommandRunner> CudaStackCollector<R> {
    pub fn collect(&self, driver_reported_max_cuda: Option<String>) -> CudaCollection {
        let (nvcc_toolkit_version, nvcc_probe_status, mut issues) = self.probe_nvcc();
        let (libcudart, libcudart_issue) = scan_library(
            &self.filesystem_root,
            "libcudart",
            common_library_dirs(&self.filesystem_root),
        );
        if let Some(issue) = libcudart_issue {
            issues.push(issue);
        }
        let (libcuda, libcuda_issue) = scan_library(
            &self.filesystem_root,
            "libcuda",
            common_library_dirs(&self.filesystem_root),
        );
        if let Some(issue) = libcuda_issue {
            issues.push(issue);
        }

        let status = cuda_status(
            driver_reported_max_cuda.as_deref(),
            &nvcc_probe_status,
            &libcudart,
            &libcuda,
        );
        CudaCollection {
            snapshot: CudaStackSnapshot {
                driver_reported_max_cuda,
                nvcc_toolkit_version,
                nvcc_probe_status,
                libcudart,
                libcuda,
                status,
            },
            issues,
        }
    }

    fn probe_nvcc(&self) -> (Option<String>, LocalProbeStatus, Vec<CollectionIssue>) {
        let mut request = CommandRequest::new("nvcc", ["--version"]);
        request.timeout = self.timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;
        let output = match self.runner.run(&request) {
            Ok(output) => output,
            Err(error) => {
                return (
                    None,
                    LocalProbeStatus::Unavailable,
                    vec![cuda_issue(
                        "nvcc_unavailable",
                        HealthStatus::Unavailable,
                        format!("nvcc 不可用：{}", error.message),
                    )],
                )
            }
        };
        if output.timed_out {
            return (
                None,
                LocalProbeStatus::Failed,
                vec![cuda_issue(
                    "nvcc_timeout",
                    HealthStatus::Warning,
                    "nvcc --version 超时，未运行任何 CUDA workload",
                )],
            );
        }
        if output.stdout_truncated || output.stderr_truncated {
            return (
                None,
                LocalProbeStatus::Failed,
                vec![cuda_issue(
                    "nvcc_output_too_large",
                    HealthStatus::Warning,
                    "nvcc --version 输出超过安全上限",
                )],
            );
        }
        if !output.success {
            return (
                None,
                LocalProbeStatus::Failed,
                vec![cuda_issue(
                    "nvcc_failed",
                    HealthStatus::Warning,
                    format!("nvcc --version 失败：{}", output.stderr.trim()),
                )],
            );
        }
        let text = if output.stdout.trim().is_empty() {
            output.stderr.as_str()
        } else {
            output.stdout.as_str()
        };
        match parse_nvcc_version(text) {
            Some(version) => (Some(version), LocalProbeStatus::Succeeded, Vec::new()),
            None => (
                None,
                LocalProbeStatus::Failed,
                vec![cuda_issue(
                    "nvcc_version_unparsed",
                    HealthStatus::Warning,
                    "nvcc 命令成功，但未解析到 Toolkit 版本",
                )],
            ),
        }
    }
}

pub fn parse_nvidia_smi_header(output: &str) -> NvidiaSmiHeader {
    let tokens = output.split_whitespace().collect::<Vec<_>>();
    NvidiaSmiHeader {
        driver_version: value_after_pair(&tokens, "Driver", "Version:"),
        cuda_version: value_after_pair(&tokens, "CUDA", "Version:"),
    }
}

fn value_after_pair(tokens: &[&str], first: &str, second: &str) -> Option<String> {
    tokens
        .windows(3)
        .find(|window| window[0].eq_ignore_ascii_case(first) && window[1] == second)
        .and_then(|window| valid_version(window[2]))
}

pub fn parse_nvcc_version(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(value) = line.split_once("release ").map(|(_, value)| value) {
            if let Some(version) = valid_version(value.split([',', ' ', '\t']).next()?) {
                return Some(version);
            }
        }
    }
    output.split_whitespace().find_map(|token| {
        let candidate = token.strip_prefix('V')?;
        valid_version(candidate)
    })
}

fn valid_version(value: &str) -> Option<String> {
    let value = value.trim_matches(|character: char| matches!(character, ',' | ';' | ':'));
    (!value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
        }))
    .then(|| value.to_owned())
}

fn common_library_dirs(root: &Path) -> Vec<PathBuf> {
    [
        "usr/local/cuda/lib64",
        "usr/local/cuda/lib",
        "usr/lib/x86_64-linux-gnu",
        "usr/lib/aarch64-linux-gnu",
        "usr/lib64",
        "lib/x86_64-linux-gnu",
        "lib/aarch64-linux-gnu",
        "lib64",
        "usr/lib/wsl/lib",
    ]
    .into_iter()
    .map(|path| root.join(path))
    .collect()
}

fn scan_library(
    root: &Path,
    name: &str,
    directories: Vec<PathBuf>,
) -> (CudaLibrarySnapshot, Option<CollectionIssue>) {
    let mut saw_directory = false;
    let mut read_error = None;
    let mut found = None;
    for directory in directories {
        if !directory.is_dir() {
            continue;
        }
        saw_directory = true;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                read_error = Some(format!("读取 {} 失败：{error}", directory.display()));
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if file_name.starts_with(name) && file_name.contains(".so") && path.is_file() {
                found = Some(path);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }

    let snapshot = if let Some(path) = found {
        CudaLibrarySnapshot {
            name: name.to_owned(),
            presence: PresenceStatus::Present,
            probe_status: LocalProbeStatus::Succeeded,
            path: Some(path.display().to_string()),
            source: Some("filesystem:common_library_paths".to_owned()),
        }
    } else if let Some(error) = read_error {
        return (
            CudaLibrarySnapshot {
                name: name.to_owned(),
                presence: PresenceStatus::Unknown,
                probe_status: LocalProbeStatus::Failed,
                path: None,
                source: Some("filesystem:common_library_paths".to_owned()),
            },
            Some(cuda_issue(
                "library_probe_failed",
                HealthStatus::Warning,
                error,
            )),
        );
    } else if saw_directory {
        CudaLibrarySnapshot {
            name: name.to_owned(),
            presence: PresenceStatus::Absent,
            probe_status: LocalProbeStatus::Succeeded,
            path: None,
            source: Some("filesystem:common_library_paths".to_owned()),
        }
    } else {
        CudaLibrarySnapshot {
            name: name.to_owned(),
            presence: PresenceStatus::Unknown,
            probe_status: LocalProbeStatus::NotAttempted,
            path: None,
            source: Some("filesystem:common_library_paths".to_owned()),
        }
    };
    let _ = root;
    (snapshot, None)
}

fn cuda_status(
    driver_reported_max_cuda: Option<&str>,
    nvcc_probe_status: &LocalProbeStatus,
    libcudart: &CudaLibrarySnapshot,
    libcuda: &CudaLibrarySnapshot,
) -> HealthStatus {
    if matches!(nvcc_probe_status, LocalProbeStatus::Failed)
        || matches!(libcudart.probe_status, LocalProbeStatus::Failed)
        || matches!(libcuda.probe_status, LocalProbeStatus::Failed)
    {
        return HealthStatus::Warning;
    }
    if driver_reported_max_cuda.is_some()
        || libcudart.presence == PresenceStatus::Present
        || libcuda.presence == PresenceStatus::Present
        || !matches!(
            nvcc_probe_status,
            LocalProbeStatus::Unknown | LocalProbeStatus::NotAttempted
        )
    {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    }
}

fn cuda_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "cuda".to_owned(),
        code: code.into(),
        status,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::super::command::{CommandOutput, CommandRequest, CommandRunner};
    use super::{parse_nvcc_version, parse_nvidia_smi_header, CudaStackCollector};
    use crate::collectors::CollectorError;
    use crate::domain::{LocalProbeStatus, PresenceStatus};

    #[derive(Clone)]
    struct FakeRunner {
        result: Arc<Result<CommandOutput, CollectorError>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            self.result.as_ref().clone()
        }
    }

    #[test]
    fn cuda_parser_keeps_driver_reported_cuda_separate_from_nvcc_toolkit() {
        let header = parse_nvidia_smi_header(
            "NVIDIA-SMI 550.54.14 Driver Version: 550.54.14 CUDA Version: 12.2\n",
        );
        assert_eq!(header.driver_version.as_deref(), Some("550.54.14"));
        assert_eq!(header.cuda_version.as_deref(), Some("12.2"));
        assert_eq!(
            parse_nvcc_version("Cuda compilation tools, release 12.4, V12.4.99"),
            Some("12.4".to_owned())
        );
    }

    #[test]
    fn cuda_collector_reports_library_presence_without_running_workload() {
        let root = fixture_root("cuda");
        let library_dir = root.join("usr/local/cuda/lib64");
        fs::create_dir_all(&library_dir).expect("library fixture");
        fs::write(library_dir.join("libcudart.so.12"), b"fixture").expect("runtime fixture");
        fs::write(library_dir.join("libcuda.so.550"), b"fixture").expect("driver fixture");
        let runner = FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: "Cuda compilation tools, release 12.4, V12.4.99\n".to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
        };
        let collection = CudaStackCollector::with_runner(runner)
            .with_filesystem_root(&root)
            .collect(Some("12.2".to_owned()));
        assert_eq!(
            collection.snapshot.driver_reported_max_cuda.as_deref(),
            Some("12.2")
        );
        assert_eq!(
            collection.snapshot.nvcc_toolkit_version.as_deref(),
            Some("12.4")
        );
        assert_eq!(
            collection.snapshot.libcudart.presence,
            PresenceStatus::Present
        );
        assert_eq!(
            collection.snapshot.libcuda.presence,
            PresenceStatus::Present
        );
        assert_eq!(
            collection.snapshot.nvcc_probe_status,
            LocalProbeStatus::Succeeded
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cuda_collector_distinguishes_missing_nvcc_from_unknown_toolkit() {
        let root = fixture_root("cuda-no-nvcc");
        let runner = FakeRunner {
            result: Arc::new(Err(CollectorError::new(
                "command",
                "spawn_failed",
                "nvcc 不存在",
            ))),
        };
        let collection = CudaStackCollector::with_runner(runner)
            .with_filesystem_root(&root)
            .collect(None);
        assert_eq!(collection.snapshot.nvcc_toolkit_version, None);
        assert_eq!(
            collection.snapshot.nvcc_probe_status,
            LocalProbeStatus::Unavailable
        );
        assert!(collection
            .issues
            .iter()
            .any(|issue| issue.code == "nvcc_unavailable"));
        let _ = fs::remove_dir_all(root);
    }

    fn fixture_root(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("suanctl-{label}-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }
}
