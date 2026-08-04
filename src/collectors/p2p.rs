//! NVIDIA GPU P2P 拓扑、能力矩阵和显式 NVBandwidth 实测。
//!
//! 默认路径只运行 `nvidia-smi topo` 只读查询。`nvbandwidth` 会产生 GPU
//! 负载，只能由 CLI 的显式 benchmark 入口调用。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::domain::{
    CollectionIssue, GpuP2pLinkSnapshot, HealthStatus, P2pBandwidthMeasurement,
    P2pBenchmarkSnapshot, P2pBenchmarkStatus, P2pCapabilityStatus, P2pSnapshot,
};

use super::command::{
    CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};
use super::CollectorError;

pub const DEFAULT_P2P_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_P2P_BENCHMARK_TIMEOUT: Duration = Duration::from_secs(120);
pub const NVBANDWIDTH_TESTCASE: &str = "device_to_device_memcpy_write_ce";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct P2pCollection {
    pub snapshot: P2pSnapshot,
    pub issues: Vec<CollectionIssue>,
}

#[derive(Debug, Clone)]
pub struct NvidiaP2pCollector<R = ProcessCommandRunner> {
    runner: R,
    timeout: Duration,
    benchmark_timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl NvidiaP2pCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            timeout: DEFAULT_P2P_TIMEOUT,
            benchmark_timeout: DEFAULT_P2P_BENCHMARK_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

impl Default for NvidiaP2pCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> NvidiaP2pCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            timeout: DEFAULT_P2P_TIMEOUT,
            benchmark_timeout: DEFAULT_P2P_BENCHMARK_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_benchmark_timeout(mut self, timeout: Duration) -> Self {
        self.benchmark_timeout = timeout;
        self
    }

    pub fn with_output_limits(mut self, stdout_limit: usize, stderr_limit: usize) -> Self {
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        self
    }
}

impl<R: CommandRunner> NvidiaP2pCollector<R> {
    pub fn collect_topology(&self) -> P2pCollection {
        let topology = match self.run_matrix(&["topo", "-m"], "拓扑") {
            Ok(matrix) => matrix,
            Err(issue) => {
                return P2pCollection {
                    snapshot: P2pSnapshot {
                        gpu_indices: Vec::new(),
                        links: Vec::new(),
                        benchmark: P2pBenchmarkSnapshot::default(),
                        status: issue.status,
                    },
                    issues: vec![issue],
                };
            }
        };

        let mut issues = Vec::new();
        let mut matrices = BTreeMap::new();
        for (key, label, argument) in [
            ("read", "读", "r"),
            ("write", "写", "w"),
            ("pcie", "PCIe", "p"),
            ("nvlink", "NVLink", "n"),
            ("atomics", "原子操作", "a"),
        ] {
            match self.run_matrix(&["topo", "-p2p", argument], label) {
                Ok(matrix) => {
                    matrices.insert(key, matrix);
                }
                Err(issue) => issues.push(issue),
            }
        }

        let mut gpu_indices = topology
            .gpu_indices
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for matrix in matrices.values() {
            gpu_indices.extend(matrix.gpu_indices.iter().copied());
        }
        let gpu_indices = gpu_indices.into_iter().collect::<Vec<_>>();
        let mut links = Vec::new();
        for source_gpu in &gpu_indices {
            for target_gpu in &gpu_indices {
                if source_gpu == target_gpu {
                    continue;
                }
                links.push(GpuP2pLinkSnapshot {
                    source_gpu: *source_gpu,
                    target_gpu: *target_gpu,
                    topology_path: topology
                        .values
                        .get(&(*source_gpu, *target_gpu))
                        .cloned()
                        .filter(|value| value != "X"),
                    read: capability_at(matrices.get("read"), *source_gpu, *target_gpu),
                    write: capability_at(matrices.get("write"), *source_gpu, *target_gpu),
                    pcie: capability_at(matrices.get("pcie"), *source_gpu, *target_gpu),
                    nvlink: capability_at(matrices.get("nvlink"), *source_gpu, *target_gpu),
                    atomics: capability_at(matrices.get("atomics"), *source_gpu, *target_gpu),
                });
            }
        }
        let status = if gpu_indices.is_empty() {
            HealthStatus::Unknown
        } else if issues.is_empty() {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unknown
        };
        P2pCollection {
            snapshot: P2pSnapshot {
                gpu_indices,
                links,
                benchmark: P2pBenchmarkSnapshot::default(),
                status,
            },
            issues,
        }
    }

    /// 显式运行固定的 NVBandwidth 单向 GPU-to-GPU copy-engine 测试。
    pub fn collect_with_benchmark(&self) -> P2pCollection {
        let mut collection = self.collect_topology();
        let (benchmark, issues) = self.run_benchmark();
        collection.snapshot.benchmark = benchmark;
        collection.issues.extend(issues);
        collection
    }

    pub fn run_benchmark(&self) -> (P2pBenchmarkSnapshot, Vec<CollectionIssue>) {
        let mut request = CommandRequest::new(
            "nvbandwidth",
            ["-t", NVBANDWIDTH_TESTCASE, "--format", "json"],
        );
        request.timeout = self.benchmark_timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;

        let measured_at = now_millis();
        let output = match self.runner.run(&request) {
            Ok(output) => output,
            Err(error) => {
                let message = format!("nvbandwidth 不可用：{}", error.message);
                return (
                    benchmark_result(
                        P2pBenchmarkStatus::Unavailable,
                        measured_at,
                        Vec::new(),
                        None,
                        Some(message.clone()),
                    ),
                    vec![p2p_issue(
                        "nvbandwidth_unavailable",
                        HealthStatus::Unavailable,
                        message,
                    )],
                );
            }
        };
        if output.timed_out {
            let message = "nvbandwidth 超时，GPU 负载已终止并回收".to_owned();
            return (
                benchmark_result(
                    P2pBenchmarkStatus::Failed,
                    measured_at,
                    Vec::new(),
                    None,
                    Some(message.clone()),
                ),
                vec![p2p_issue(
                    "nvbandwidth_timeout",
                    HealthStatus::Warning,
                    message,
                )],
            );
        }
        if output.stdout_truncated || output.stderr_truncated {
            let message = "nvbandwidth 输出超过安全上限，结果未采信".to_owned();
            return (
                benchmark_result(
                    P2pBenchmarkStatus::Failed,
                    measured_at,
                    Vec::new(),
                    None,
                    Some(message.clone()),
                ),
                vec![p2p_issue(
                    "nvbandwidth_output_too_large",
                    HealthStatus::Warning,
                    message,
                )],
            );
        }
        if !output.success {
            let detail = if output.stderr.trim().is_empty() {
                format!("退出码 {:?}", output.exit_code)
            } else {
                output.stderr.trim().to_owned()
            };
            let message = format!("nvbandwidth 执行失败：{detail}");
            return (
                benchmark_result(
                    P2pBenchmarkStatus::Failed,
                    measured_at,
                    Vec::new(),
                    None,
                    Some(message.clone()),
                ),
                vec![p2p_issue(
                    "nvbandwidth_failed",
                    HealthStatus::Warning,
                    message,
                )],
            );
        }

        match parse_nvbandwidth_json(&output.stdout, measured_at) {
            Ok(snapshot) => (snapshot, Vec::new()),
            Err(error) => {
                let message = error.message;
                (
                    benchmark_result(
                        P2pBenchmarkStatus::Failed,
                        measured_at,
                        Vec::new(),
                        None,
                        Some(message.clone()),
                    ),
                    vec![p2p_issue(
                        "nvbandwidth_parse_failed",
                        HealthStatus::Warning,
                        message,
                    )],
                )
            }
        }
    }

    fn run_matrix(
        &self,
        args: &[&str],
        capability: &str,
    ) -> Result<ParsedGpuMatrix, CollectionIssue> {
        let mut request = CommandRequest::new("nvidia-smi", args.iter().copied());
        request.timeout = self.timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;
        let output = self.runner.run(&request).map_err(|error| {
            p2p_issue(
                "nvidia_smi_unavailable",
                HealthStatus::Unavailable,
                format!("P2P {capability}采集不可用：{}", error.message),
            )
        })?;
        if output.timed_out {
            return Err(p2p_issue(
                "nvidia_smi_topo_timeout",
                HealthStatus::Unknown,
                format!("nvidia-smi P2P {capability} 查询超时"),
            ));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err(p2p_issue(
                "nvidia_smi_topo_output_too_large",
                HealthStatus::Unknown,
                format!("nvidia-smi P2P {capability} 输出超过安全上限"),
            ));
        }
        if !output.success {
            return Err(p2p_issue(
                "nvidia_smi_topo_failed",
                HealthStatus::Unknown,
                format!(
                    "nvidia-smi P2P {capability} 查询失败：{}",
                    output.stderr.trim()
                ),
            ));
        }
        parse_gpu_matrix(&output.stdout).map_err(|error| {
            p2p_issue(
                "nvidia_smi_topo_parse_failed",
                HealthStatus::Unknown,
                format!("P2P {capability} 矩阵解析失败：{}", error.message),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedGpuMatrix {
    gpu_indices: Vec<u32>,
    values: BTreeMap<(u32, u32), String>,
}

fn parse_gpu_matrix(output: &str) -> Result<ParsedGpuMatrix, CollectorError> {
    if output
        .to_ascii_lowercase()
        .contains("no devices were found")
    {
        return Ok(ParsedGpuMatrix {
            gpu_indices: Vec::new(),
            values: BTreeMap::new(),
        });
    }
    let lines = output.lines().collect::<Vec<_>>();
    let (header_index, gpu_indices) = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let indices = line
                .split_whitespace()
                .filter_map(parse_gpu_label)
                .collect::<Vec<_>>();
            (!indices.is_empty()).then_some((index, indices))
        })
        .ok_or_else(|| CollectorError::new("p2p", "parse_failed", "未找到 GPU 矩阵表头"))?;

    let mut values = BTreeMap::new();
    for line in lines.iter().skip(header_index + 1) {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        let Some(row_gpu) = tokens.first().and_then(|token| parse_gpu_label(token)) else {
            continue;
        };
        if !gpu_indices.contains(&row_gpu) || tokens.len() < gpu_indices.len() + 1 {
            continue;
        }
        for (column, target_gpu) in gpu_indices.iter().enumerate() {
            values.insert((row_gpu, *target_gpu), tokens[column + 1].to_owned());
        }
    }
    if !gpu_indices.is_empty() && values.is_empty() {
        return Err(CollectorError::new(
            "p2p",
            "parse_failed",
            "找到 GPU 表头但没有可解析的数据行",
        ));
    }
    Ok(ParsedGpuMatrix {
        gpu_indices,
        values,
    })
}

fn parse_gpu_label(value: &str) -> Option<u32> {
    value.strip_prefix("GPU")?.parse::<u32>().ok()
}

fn capability_at(
    matrix: Option<&ParsedGpuMatrix>,
    source_gpu: u32,
    target_gpu: u32,
) -> P2pCapabilityStatus {
    matrix
        .and_then(|matrix| matrix.values.get(&(source_gpu, target_gpu)))
        .map(|value| parse_capability(value))
        .unwrap_or_default()
}

fn parse_capability(value: &str) -> P2pCapabilityStatus {
    match value.trim().to_ascii_uppercase().as_str() {
        "OK" => P2pCapabilityStatus::Supported,
        "NS" => P2pCapabilityStatus::NotSupported,
        "CNS" => P2pCapabilityStatus::ChipsetNotSupported,
        "GNS" => P2pCapabilityStatus::GpuNotSupported,
        "TNS" => P2pCapabilityStatus::TopologyNotSupported,
        "DR" => P2pCapabilityStatus::DisabledByConfiguration,
        _ => P2pCapabilityStatus::Unknown,
    }
}

pub fn parse_nvbandwidth_json(
    output: &str,
    measured_at: u64,
) -> Result<P2pBenchmarkSnapshot, CollectorError> {
    let value: serde_json::Value = serde_json::from_str(output).map_err(|error| {
        CollectorError::new(
            "p2p",
            "parse_failed",
            format!("nvbandwidth JSON 无效：{error}"),
        )
    })?;
    let root = value
        .get("nvbandwidth")
        .ok_or_else(|| CollectorError::new("p2p", "parse_failed", "缺少 nvbandwidth 根对象"))?;
    if let Some(error) = root.get("error").and_then(|value| value.as_str()) {
        return Err(CollectorError::new(
            "p2p",
            "benchmark_failed",
            format!("nvbandwidth 报告错误：{error}"),
        ));
    }
    let testcase = root
        .get("testcases")
        .and_then(|value| value.as_array())
        .and_then(|testcases| {
            testcases.iter().find(|testcase| {
                testcase.get("name").and_then(|value| value.as_str()) == Some(NVBANDWIDTH_TESTCASE)
            })
        })
        .ok_or_else(|| {
            CollectorError::new(
                "p2p",
                "parse_failed",
                format!("缺少 {NVBANDWIDTH_TESTCASE} 测试结果"),
            )
        })?;
    let status = testcase
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown");
    if status != "Passed" {
        return Err(CollectorError::new(
            "p2p",
            "benchmark_failed",
            format!("nvbandwidth 测试状态为 {status}"),
        ));
    }
    let description = testcase
        .get("bandwidth_description")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let matrix = testcase
        .get("bandwidth_matrix")
        .and_then(|value| value.as_array())
        .ok_or_else(|| CollectorError::new("p2p", "parse_failed", "缺少 bandwidth_matrix"))?;

    let row_is_source = description
        .as_deref()
        .is_some_and(|value| value.contains("GPU(row) -> GPU(column)"));
    let mut measurements = Vec::new();
    for (row, columns) in matrix.iter().enumerate() {
        let Some(columns) = columns.as_array() else {
            continue;
        };
        for (column, raw) in columns.iter().enumerate() {
            if row == column {
                continue;
            }
            let bandwidth = raw
                .as_f64()
                .or_else(|| raw.as_str().and_then(|value| value.parse::<f64>().ok()));
            let Some(gigabytes_per_second) = bandwidth.filter(|value| *value > 0.0) else {
                continue;
            };
            let (source_gpu, target_gpu) = if row_is_source {
                (row as u32, column as u32)
            } else {
                (column as u32, row as u32)
            };
            measurements.push(P2pBandwidthMeasurement {
                source_gpu,
                target_gpu,
                gigabytes_per_second,
            });
        }
    }
    Ok(benchmark_result(
        P2pBenchmarkStatus::Succeeded,
        measured_at,
        measurements,
        description,
        None,
    ))
}

fn benchmark_result(
    status: P2pBenchmarkStatus,
    measured_at: u64,
    measurements: Vec<P2pBandwidthMeasurement>,
    direction_description: Option<String>,
    message: Option<String>,
) -> P2pBenchmarkSnapshot {
    P2pBenchmarkSnapshot {
        status,
        tool: "nvbandwidth".to_owned(),
        testcase: NVBANDWIDTH_TESTCASE.to_owned(),
        measured_at: Some(measured_at),
        direction_description,
        measurements,
        message,
    }
}

fn p2p_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "p2p".to_owned(),
        code: code.into(),
        status,
        message: message.into(),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::collectors::command::{CommandOutput, CommandRequest};

    const TOPOLOGY: &str = "        GPU0    GPU1    GPU2    CPU Affinity    NUMA Affinity\nGPU0     X      PIX     SYS     0-31            0\nGPU1    PIX      X      PHB     0-31            0\nGPU2    SYS     PHB      X      32-63           1\n";
    const P2P_OK: &str = "        GPU0    GPU1    GPU2\nGPU0     X       OK      NS\nGPU1     OK      X       OK\nGPU2     NS      OK      X\n\nLegend:\n";
    const P2P_NVLINK: &str = "        GPU0    GPU1    GPU2\nGPU0     X       NS      NS\nGPU1     NS      X       OK\nGPU2     NS      OK      X\n";

    type FixtureHandler =
        dyn Fn(&CommandRequest) -> Result<CommandOutput, CollectorError> + Send + Sync;

    #[derive(Clone)]
    struct FixtureRunner {
        handler: Arc<FixtureHandler>,
    }

    impl CommandRunner for FixtureRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            (self.handler)(request)
        }
    }

    fn output(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            exit_code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn parses_topology_paths_and_directional_capabilities() {
        let runner = FixtureRunner {
            handler: Arc::new(|request| {
                if request.args == ["topo", "-m"] {
                    Ok(output(TOPOLOGY))
                } else if request.args == ["topo", "-p2p", "n"] {
                    Ok(output(P2P_NVLINK))
                } else {
                    Ok(output(P2P_OK))
                }
            }),
        };
        let collection = NvidiaP2pCollector::with_runner(runner).collect_topology();
        assert!(collection.issues.is_empty(), "{:?}", collection.issues);
        assert_eq!(collection.snapshot.gpu_indices, vec![0, 1, 2]);
        assert_eq!(collection.snapshot.links.len(), 6);
        let link = collection
            .snapshot
            .links
            .iter()
            .find(|link| link.source_gpu == 0 && link.target_gpu == 1)
            .expect("GPU0 -> GPU1");
        assert_eq!(link.topology_path.as_deref(), Some("PIX"));
        assert_eq!(link.read, P2pCapabilityStatus::Supported);
        assert_eq!(link.nvlink, P2pCapabilityStatus::NotSupported);
        let nvlink = collection
            .snapshot
            .links
            .iter()
            .find(|link| link.source_gpu == 1 && link.target_gpu == 2)
            .expect("GPU1 -> GPU2");
        assert_eq!(nvlink.nvlink, P2pCapabilityStatus::Supported);
    }

    #[test]
    fn missing_nvidia_smi_is_one_structured_unavailable_result() {
        let runner = FixtureRunner {
            handler: Arc::new(|_| {
                Err(CollectorError::new(
                    "command",
                    "spawn_failed",
                    "nvidia-smi 不存在",
                ))
            }),
        };
        let collection = NvidiaP2pCollector::with_runner(runner).collect_topology();
        assert_eq!(collection.snapshot.status, HealthStatus::Unavailable);
        assert_eq!(collection.issues.len(), 1);
        assert_eq!(collection.issues[0].code, "nvidia_smi_unavailable");
    }

    #[test]
    fn parses_nvbandwidth_directional_json_matrix() {
        let fixture = r#"{
          "nvbandwidth": {
            "testcases": [{
              "name": "device_to_device_memcpy_write_ce",
              "status": "Passed",
              "bandwidth_description": "memcpy CE GPU(row) <- GPU(column) bandwidth (GB/s)",
              "bandwidth_matrix": [["N/A", "25.5"], ["24.75", "N/A"]]
            }]
          }
        }"#;
        let result = parse_nvbandwidth_json(fixture, 123).expect("nvbandwidth fixture");
        assert_eq!(result.status, P2pBenchmarkStatus::Succeeded);
        assert_eq!(result.measurements.len(), 2);
        assert_eq!(result.measurements[0].source_gpu, 1);
        assert_eq!(result.measurements[0].target_gpu, 0);
        assert_eq!(result.measurements[0].gigabytes_per_second, 25.5);
    }

    #[test]
    fn benchmark_is_never_run_by_default_topology_collection() {
        let runner = FixtureRunner {
            handler: Arc::new(|request| {
                assert_ne!(request.program, "nvbandwidth");
                if request.args == ["topo", "-m"] {
                    Ok(output(TOPOLOGY))
                } else {
                    Ok(output(P2P_OK))
                }
            }),
        };
        let collection = NvidiaP2pCollector::with_runner(runner).collect_topology();
        assert_eq!(
            collection.snapshot.benchmark.status,
            P2pBenchmarkStatus::NotRequested
        );
    }

    #[test]
    fn explicit_benchmark_timeout_is_reported_without_fake_rate() {
        let runner = FixtureRunner {
            handler: Arc::new(|request| {
                if request.program == "nvbandwidth" {
                    Ok(CommandOutput {
                        success: false,
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        timed_out: true,
                        stdout_truncated: false,
                        stderr_truncated: false,
                    })
                } else if request.args == ["topo", "-m"] {
                    Ok(output(TOPOLOGY))
                } else {
                    Ok(output(P2P_OK))
                }
            }),
        };
        let collection = NvidiaP2pCollector::with_runner(runner)
            .with_benchmark_timeout(Duration::from_millis(10))
            .collect_with_benchmark();
        assert_eq!(
            collection.snapshot.benchmark.status,
            P2pBenchmarkStatus::Failed
        );
        assert!(collection.snapshot.benchmark.measurements.is_empty());
        assert!(collection
            .issues
            .iter()
            .any(|issue| issue.code == "nvbandwidth_timeout"));
    }
}
