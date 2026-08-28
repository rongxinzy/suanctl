//! NVIDIA GPU 的只读 `nvidia-smi` 采集。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{GpuSnapshot, HealthStatus};

use super::command::{
    CommandOutput, CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT,
    DEFAULT_STDOUT_LIMIT,
};
use super::{CollectorError, GpuCollector};

const NVIDIA_SMI_QUERY_FIELDS: &str = "index,uuid,name,pci.bus_id,temperature.gpu,utilization.gpu,memory.used,memory.total,power.draw,power.limit,pstate";

#[derive(Debug, Clone)]
pub struct NvidiaSmiCollector<R = ProcessCommandRunner> {
    runner: R,
    sysfs_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl NvidiaSmiCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            sysfs_root: PathBuf::from("/sys"),
            timeout: Duration::from_secs(2),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

impl Default for NvidiaSmiCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> NvidiaSmiCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            sysfs_root: PathBuf::from("/sys"),
            timeout: Duration::from_secs(2),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_sysfs_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.sysfs_root = root.into();
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

impl<R: CommandRunner> NvidiaSmiCollector<R> {
    pub fn parse_output(&self, output: &str) -> Result<Vec<GpuSnapshot>, CollectorError> {
        parse_query_output(output, &self.sysfs_root)
    }

    fn collect_gpus_unchecked(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
        // nvidia-smi 不可用时尝试 querygpu（改名体）。
        let (output, tool) = match self.run_smi("nvidia-smi") {
            Ok(output) => (output, "nvidia-smi".to_owned()),
            Err(primary_error) => match self.run_smi("querygpu") {
                Ok(output) => (output, "querygpu".to_owned()),
                Err(_) => {
                    return Err(CollectorError::new(
                        "gpu",
                        "command_failed",
                        format!("nvidia-smi 与 querygpu 均不可用：{}", primary_error.message),
                    ));
                }
            },
        };
        let mut gpus = parse_query_output(&output.stdout, &self.sysfs_root)?;
        for gpu in &mut gpus {
            gpu.smi_tool = Some(tool.clone());
        }
        Ok(gpus)
    }

    /// 运行一个 SMI 工具（nvidia-smi 或其改名体 querygpu）并统一错误处理。
    fn run_smi(&self, tool: &str) -> Result<CommandOutput, CollectorError> {
        let mut request = CommandRequest::new(
            tool,
            [
                format!("--query-gpu={NVIDIA_SMI_QUERY_FIELDS}"),
                "--format=csv,noheader,nounits".to_owned(),
            ],
        );
        request.timeout = self.timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;

        let output = self.runner.run(&request)?;
        if output.timed_out {
            return Err(CollectorError::new(
                "gpu",
                "command_timeout",
                format!("{tool} 超时，子进程已终止并回收"),
            ));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err(CollectorError::new(
                "gpu",
                "output_too_large",
                format!("{tool} 输出超过安全上限"),
            ));
        }
        if !output.success {
            let detail = if output.stderr.trim().is_empty() {
                format!("{tool} 退出码 {:?}", output.exit_code)
            } else {
                format!("{tool} 失败：{}", output.stderr.trim())
            };
            return Err(CollectorError::new("gpu", "command_failed", detail));
        }
        Ok(output)
    }
}

impl<R: CommandRunner> GpuCollector for NvidiaSmiCollector<R> {
    fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
        if !cfg!(target_os = "linux") {
            return Err(CollectorError::new(
                "gpu",
                "unsupported_platform",
                "NVIDIA GPU 采集器只能在 Linux 上运行",
            ));
        }
        self.collect_gpus_unchecked()
    }
}

pub fn parse_query_output(
    output: &str,
    sysfs_root: &Path,
) -> Result<Vec<GpuSnapshot>, CollectorError> {
    let mut snapshots = Vec::new();
    for (line_number, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = normalize_fields(parse_csv_line(line), line_number + 1)?;
        snapshots.push(parse_gpu(fields, sysfs_root, line_number + 1)?);
    }
    Ok(snapshots)
}

fn normalize_fields(
    mut fields: Vec<String>,
    line_number: usize,
) -> Result<Vec<String>, CollectorError> {
    // nvidia-smi 通常会对含逗号的名称加引号；同时兼容某些版本输出未加引号的名称，
    // 此时固定的尾部 8 列可以确定哪些逗号属于 name。
    if fields.len() > 11 {
        let name_end = fields.len() - 8;
        let tail = fields.split_off(name_end);
        let name = fields.drain(2..).collect::<Vec<_>>().join(", ");
        let mut normalized = Vec::with_capacity(11);
        normalized.extend(fields);
        normalized.push(name);
        normalized.extend(tail);
        fields = normalized;
    }
    if fields.len() != 11 {
        return Err(CollectorError::new(
            "gpu",
            "parse_failed",
            format!(
                "nvidia-smi 第 {line_number} 行字段数为 {}，预期 11",
                fields.len()
            ),
        ));
    }
    Ok(fields)
}

fn parse_gpu(
    fields: Vec<String>,
    sysfs_root: &Path,
    line_number: usize,
) -> Result<GpuSnapshot, CollectorError> {
    let reset_required = fields
        .iter()
        .any(|field| field.to_ascii_lowercase().contains("gpu requires reset"));
    let has_error = fields.iter().any(|field| is_error(field));
    let has_unknown = fields.iter().any(|field| is_unknown(field));
    let status = if reset_required {
        HealthStatus::Critical
    } else if has_error {
        HealthStatus::Warning
    } else if has_unknown {
        HealthStatus::Unknown
    } else {
        HealthStatus::Healthy
    };

    let index = parse_required_u32(&fields[0], "index", line_number)?;
    let pci_address = parse_optional_string(&fields[3]);
    Ok(GpuSnapshot {
        index,
        name: parse_optional_string(&fields[2]).unwrap_or_else(|| "未知 GPU".to_owned()),
        uuid: parse_optional_string(&fields[1]),
        pci_address: pci_address.clone(),
        status,
        temperature_celsius: parse_optional_u32(&fields[4]),
        utilization_percent: parse_optional_u32(&fields[5]),
        memory_used_mib: parse_optional_u64(&fields[6]),
        memory_total_mib: parse_optional_u64(&fields[7]),
        power_draw_watts: parse_optional_f64(&fields[8]),
        power_limit_watts: parse_optional_f64(&fields[9]),
        pstate: parse_optional_string(&fields[10]),
        numa_node: pci_address
            .as_deref()
            .and_then(|address| read_numa_node(sysfs_root, address)),
        reset_required: if reset_required {
            Some(true)
        } else if has_error {
            None
        } else {
            Some(false)
        },
        // Xid 属于 journal/kernel 证据，按任务边界本采集器不读取。
        xid_codes: None,
        smi_tool: None,
        vendor: Some("NVIDIA".to_owned()),
    })
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(current.trim().to_owned());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    fields.push(current.trim().to_owned());
    fields
}

fn parse_required_u32(value: &str, field: &str, line_number: usize) -> Result<u32, CollectorError> {
    value.trim().parse::<u32>().map_err(|error| {
        CollectorError::new(
            "gpu",
            "parse_failed",
            format!("nvidia-smi 第 {line_number} 行 {field} 无效：{error}"),
        )
    })
}

fn parse_optional_string(value: &str) -> Option<String> {
    (!is_unknown(value) && !is_error(value) && !is_reset_marker(value))
        .then(|| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_optional_u32(value: &str) -> Option<u32> {
    parse_optional_value(value).and_then(|value| value.parse().ok())
}

fn parse_optional_u64(value: &str) -> Option<u64> {
    parse_optional_value(value).and_then(|value| value.parse().ok())
}

fn parse_optional_f64(value: &str) -> Option<f64> {
    parse_optional_value(value).and_then(|value| value.parse().ok())
}

fn parse_optional_value(value: &str) -> Option<&str> {
    let value = value.trim();
    (!is_unknown(value) && !is_error(value) && !is_reset_marker(value)).then_some(value)
}

fn is_unknown(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("n/a")
}

fn is_error(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("err!")
}

fn is_reset_marker(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("gpu requires reset")
}

pub(crate) fn read_numa_node(sysfs_root: &Path, pci_address: &str) -> Option<u32> {
    let candidates = [pci_address.to_owned(), normalize_pci_address(pci_address)];
    candidates.iter().find_map(|address| {
        let path = sysfs_root
            .join("bus/pci/devices")
            .join(address)
            .join("numa_node");
        let raw = fs::read_to_string(path).ok()?;
        let value = raw.trim().parse::<i32>().ok()?;
        (value >= 0).then_some(value as u32)
    })
}

pub(crate) fn normalize_pci_address(value: &str) -> String {
    let mut pieces = value.splitn(3, ':');
    let Some(domain) = pieces.next() else {
        return value.to_owned();
    };
    let Some(bus) = pieces.next() else {
        return value.to_owned();
    };
    let Some(function) = pieces.next() else {
        return value.to_owned();
    };
    if domain.len() > 4 {
        format!("{}:{bus}:{function}", &domain[domain.len() - 4..])
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::command::{CommandOutput, CommandRequest, CommandRunner};
    use super::{parse_query_output, NvidiaSmiCollector};
    use crate::collectors::CollectorError;
    use crate::collectors::GpuCollector;

    const NORMAL: &str = include_str!("fixtures/nvidia_smi_normal.csv");
    const EDGE: &str = include_str!("fixtures/nvidia_smi_edge.csv");

    #[derive(Clone)]
    struct FakeRunner {
        result: Arc<Result<CommandOutput, CollectorError>>,
        /// 按程序名分流的响应（fallback 测试用）。
        by_program: std::collections::BTreeMap<String, Arc<Result<CommandOutput, CollectorError>>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            if let Some(response) = self.by_program.get(&request.program) {
                return response.as_ref().clone();
            }
            self.result.as_ref().clone()
        }
    }

    #[test]
    fn collectors_gpu_parses_normal_and_comma_name() {
        let gpus = parse_query_output(NORMAL, std::path::Path::new("/nonexistent"))
            .expect("normal GPU fixture");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4060 Ti, Test");
        assert_eq!(gpus[0].memory_used_mib, Some(8192));
        assert_eq!(gpus[0].status, crate::domain::HealthStatus::Healthy);
        assert_eq!(gpus[0].xid_codes, None);
    }

    #[test]
    fn collectors_gpu_keeps_na_err_and_reset_as_unknown_values() {
        let gpus = parse_query_output(EDGE, std::path::Path::new("/nonexistent"))
            .expect("edge GPU fixture");
        assert_eq!(gpus.len(), 3);
        assert_eq!(gpus[0].temperature_celsius, None);
        assert_eq!(gpus[0].memory_total_mib, None);
        assert_eq!(gpus[0].status, crate::domain::HealthStatus::Unknown);
        assert_eq!(gpus[0].reset_required, Some(false));
        assert_eq!(gpus[1].power_draw_watts, None);
        assert_eq!(gpus[1].status, crate::domain::HealthStatus::Warning);
        assert_eq!(gpus[1].reset_required, None);
        assert_eq!(gpus[2].status, crate::domain::HealthStatus::Critical);
        assert_eq!(gpus[2].reset_required, Some(true));
    }

    #[test]
    fn collectors_gpu_reports_missing_nvidia_smi_structurally() {
        let result = Err(CollectorError::new(
            "command",
            "spawn_failed",
            "nvidia-smi 不存在",
        ));
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(result),
            by_program: std::collections::BTreeMap::new(),
        });
        let error = collector
            .collect_gpus_unchecked()
            .expect_err("missing GPU command");
        // nvidia-smi 与 querygpu 均不可用 → 汇总错误。
        assert_eq!(error.code, "command_failed");
        assert!(error.message.contains("nvidia-smi") && error.message.contains("querygpu"));
    }

    #[test]
    fn collectors_gpu_maps_command_timeout_to_gpu_error() {
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
            by_program: std::collections::BTreeMap::new(),
        });
        let error = collector.collect_gpus_unchecked().expect_err("timeout");
        assert_eq!(error.collector, "gpu");
        // nvidia-smi 与 querygpu 均超时 → 汇总错误（保留超时提示）。
        assert_eq!(error.code, "command_failed");
        assert!(error.message.contains("nvidia-smi") && error.message.contains("querygpu"));
    }

    #[test]
    fn parses_real_rx_box_l20_output() {
        // 真实设备 rx-box（172.18.5.123）的 nvidia-smi 输出：2× NVIDIA L20。
        const REAL: &str = include_str!("fixtures/real-rx-box/rx_nvidia_smi_l20.csv");
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: REAL.to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
            by_program: std::collections::BTreeMap::new(),
        });
        let gpus = collector.collect_gpus().expect("real gpu parse");
        assert_eq!(gpus.len(), 2);
        for (index, gpu) in gpus.iter().enumerate() {
            assert_eq!(gpu.name, "NVIDIA L20");
            assert_eq!(
                gpu.uuid.as_ref().map(String::len),
                Some(40),
                "真实 UUID（含 GPU- 前缀）"
            );
            assert_eq!(
                gpu.pci_address.as_deref(),
                Some(if index == 0 {
                    "00000000:0C:00.0"
                } else {
                    "00000000:0F:00.0"
                })
            );
            assert_eq!(gpu.memory_total_mib, Some(46068));
            assert!(gpu.temperature_celsius.unwrap_or(0) >= 40, "L20 工作温度");
            assert_eq!(gpu.power_limit_watts, Some(350.0));
            assert_eq!(gpu.status, crate::domain::HealthStatus::Healthy);
        }
        // 第一张卡：56°C / 87.95W / P0
        assert_eq!(gpus[0].temperature_celsius, Some(56));
        assert_eq!(gpus[0].utilization_percent, Some(0));
        assert_eq!(gpus[0].pstate.as_deref(), Some("P0"));
    }

    #[test]
    fn collectors_gpu_full_chain_via_trait_entry() {
        // GpuCollector trait 入口（collect_gpus）完整链路：runner → 解析 → 快照。
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: NORMAL.to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
            by_program: std::collections::BTreeMap::new(),
        });
        let gpus = collector.collect_gpus().expect("gpu trait chain");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4060 Ti, Test");
        assert_eq!(gpus[0].status, crate::domain::HealthStatus::Healthy);
    }

    #[test]
    fn collectors_gpu_nonzero_exit_maps_to_command_failed() {
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: false,
                exit_code: Some(9),
                stdout: String::new(),
                stderr: "NVML_ERROR_DRIVER_NOT_LOADED\n".to_owned(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
            by_program: std::collections::BTreeMap::new(),
        });
        let error = collector.collect_gpus_unchecked().expect_err("failed");
        assert_eq!(error.code, "command_failed");
        assert!(error.message.contains("NVML_ERROR_DRIVER_NOT_LOADED"));
    }

    #[test]
    fn falls_back_to_querygpu_when_nvidia_smi_is_unavailable() {
        // nvidia-smi 不存在（spawn 失败）→ 尝试 querygpu（改名体）并标记工具。
        let unavailable = Arc::new(Err(CollectorError::new(
            "command",
            "spawn_failed",
            "无法启动 nvidia-smi：No such file or directory",
        )));
        let available = Arc::new(Ok(CommandOutput {
            success: true,
            exit_code: Some(0),
            stdout: NORMAL.to_owned(),
            stderr: String::new(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        }));
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: unavailable.clone(),
            by_program: std::collections::BTreeMap::from([
                ("nvidia-smi".to_owned(), unavailable),
                ("querygpu".to_owned(), available),
            ]),
        });
        let gpus = collector
            .collect_gpus_unchecked()
            .expect("querygpu fallback");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4060 Ti, Test");
        assert_eq!(
            gpus[0].smi_tool.as_deref(),
            Some("querygpu"),
            "应记录实际使用的工具"
        );
        assert_eq!(gpus[1].smi_tool.as_deref(), Some("querygpu"));
    }

    #[test]
    fn marks_nvidia_smi_as_tool_when_primary_succeeds() {
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: NORMAL.to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
            by_program: std::collections::BTreeMap::new(),
        });
        let gpus = collector.collect_gpus_unchecked().expect("nvidia-smi");
        assert_eq!(gpus[0].smi_tool.as_deref(), Some("nvidia-smi"));
    }

    #[test]
    fn reports_both_tools_unavailable_in_error() {
        // nvidia-smi 与 querygpu 均不可用 → 错误信息应同时提及两者。
        let unavailable = Arc::new(Err(CollectorError::new(
            "command",
            "spawn_failed",
            "无法启动 nvidia-smi：No such file or directory",
        )));
        let collector = NvidiaSmiCollector::with_runner(FakeRunner {
            result: unavailable.clone(),
            by_program: std::collections::BTreeMap::from([
                ("nvidia-smi".to_owned(), unavailable.clone()),
                ("querygpu".to_owned(), unavailable),
            ]),
        });
        let error = collector
            .collect_gpus_unchecked()
            .expect_err("both missing");
        assert_eq!(error.code, "command_failed");
        assert!(
            error.message.contains("nvidia-smi") && error.message.contains("querygpu"),
            "错误应提及两个工具：{}",
            error.message
        );
    }
}
