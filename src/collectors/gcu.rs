//! 燧原 Enflame GCU 的只读 `efsmi` 采集。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{GpuSnapshot, HealthStatus};

use super::command::{
    CommandOutput, CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT,
    DEFAULT_STDOUT_LIMIT,
};
use super::gpu::{read_numa_node, NvidiaSmiCollector};
use super::{CollectorError, GpuCollector};

const EFSMI_QUERY_FIELDS: &str = "DEVICE,POWER,TEMP,MEMORY,USAGE,PCIE,DRIVER,ECC,ERROR";

#[derive(Debug, Clone)]
pub struct EnflameSmiCollector<R = ProcessCommandRunner> {
    runner: R,
    sysfs_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl EnflameSmiCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self::with_runner(ProcessCommandRunner)
    }
}

impl Default for EnflameSmiCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> EnflameSmiCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            sysfs_root: PathBuf::from("/sys"),
            timeout: Duration::from_secs(5),
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

impl<R: CommandRunner> EnflameSmiCollector<R> {
    pub fn parse_output(&self, output: &str) -> Result<Vec<GpuSnapshot>, CollectorError> {
        parse_efsmi_query(output, &self.sysfs_root)
    }

    fn run_efsmi(&self) -> Result<CommandOutput, CollectorError> {
        let mut request = CommandRequest::new(
            "efsmi",
            [
                "-q".to_owned(),
                "-d".to_owned(),
                EFSMI_QUERY_FIELDS.to_owned(),
            ],
        );
        request.timeout = self.timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;

        let output = self.runner.run(&request).map_err(|error| {
            CollectorError::new(
                "gpu",
                "command_failed",
                format!("efsmi 不可用：{}", error.message),
            )
        })?;
        if output.timed_out {
            return Err(CollectorError::new(
                "gpu",
                "command_timeout",
                "efsmi 超时，子进程已终止并回收",
            ));
        }
        if output.stdout_truncated || output.stderr_truncated {
            return Err(CollectorError::new(
                "gpu",
                "output_too_large",
                "efsmi 输出超过安全上限",
            ));
        }
        if !output.success {
            let detail = if output.stderr.trim().is_empty() {
                format!("efsmi 退出码 {:?}", output.exit_code)
            } else {
                format!("efsmi 失败：{}", output.stderr.trim())
            };
            return Err(CollectorError::new("gpu", "command_failed", detail));
        }
        Ok(output)
    }
}

impl<R: CommandRunner> GpuCollector for EnflameSmiCollector<R> {
    fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
        if !cfg!(target_os = "linux") {
            return Err(CollectorError::new(
                "gpu",
                "unsupported_platform",
                "Enflame GCU 采集器只能在 Linux 上运行",
            ));
        }
        let output = self.run_efsmi()?;
        self.parse_output(&output.stdout)
    }
}

/// GPU 采集回退链：NVIDIA（nvidia-smi → querygpu）失败后尝试 Enflame（efsmi）。
/// 各采集器内部已按自身工具链回退，这里只做跨厂商兜底。
#[derive(Debug, Clone)]
pub struct ChainGpuCollector<N = NvidiaSmiCollector, E = EnflameSmiCollector> {
    nvidia: N,
    enflame: E,
}

impl ChainGpuCollector<NvidiaSmiCollector, EnflameSmiCollector> {
    pub fn new() -> Self {
        Self {
            nvidia: NvidiaSmiCollector::new(),
            enflame: EnflameSmiCollector::new(),
        }
    }
}

impl Default for ChainGpuCollector<NvidiaSmiCollector, EnflameSmiCollector> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N: GpuCollector, E: GpuCollector> GpuCollector for ChainGpuCollector<N, E> {
    fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
        match self.nvidia.collect_gpus() {
            Ok(gpus) => Ok(gpus),
            Err(nvidia_error) => match self.enflame.collect_gpus() {
                Ok(gpus) => Ok(gpus),
                Err(enflame_error) => Err(CollectorError::new(
                    "gpu",
                    nvidia_error.code,
                    format!(
                        "NVIDIA 与 Enflame 工具链均不可用（{}；{}）",
                        nvidia_error.message, enflame_error.message
                    ),
                )),
            },
        }
    }
}

/// 解析 `efsmi -q -d ...` 的分块 key-value 输出。
pub fn parse_efsmi_query(
    output: &str,
    sysfs_root: &Path,
) -> Result<Vec<GpuSnapshot>, CollectorError> {
    let mut blocks: Vec<(u32, BTreeMap<String, String>)> = Vec::new();
    let mut current: Option<(u32, BTreeMap<String, String>)> = None;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("DEV ID ") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            let index = rest
                .split_whitespace()
                .next()
                .and_then(|token| token.parse::<u32>().ok())
                .ok_or_else(|| {
                    CollectorError::new("gpu", "parse_failed", format!("efsmi DEV ID 无效：{rest}"))
                })?;
            current = Some((index, BTreeMap::new()));
            continue;
        }
        let Some((_, fields)) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        // 小节标题（如 "PCIe Info"）无冒号不会进来；"Link Info" 等空值行跳过。
        if key.is_empty() || value.trim().is_empty() {
            continue;
        }
        fields.insert(key.to_owned(), value.trim().to_owned());
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }

    if blocks.is_empty() {
        return Err(CollectorError::new(
            "gpu",
            "parse_failed",
            "efsmi 输出中未找到任何 DEV ID 块",
        ));
    }

    Ok(blocks
        .into_iter()
        .map(|(index, fields)| build_snapshot(index, &fields, sysfs_root))
        .collect())
}

fn build_snapshot(index: u32, fields: &BTreeMap<String, String>, sysfs_root: &Path) -> GpuSnapshot {
    let get = |key: &str| fields.get(key).map(String::as_str);

    let healthy = get("Health").is_none_or(|value| value.eq_ignore_ascii_case("true"));
    let error_count = get("Total Error Count").and_then(parse_leading_u64);
    let status = if !healthy {
        HealthStatus::Critical
    } else if error_count.is_some_and(|count| count > 0) {
        HealthStatus::Warning
    } else {
        HealthStatus::Healthy
    };

    let pci_address = match (get("Domain"), get("Bus"), get("Dev"), get("Func")) {
        (Some(domain), Some(bus), Some(dev), Some(func)) => {
            Some(format!("{domain}:{bus}:{dev}.{func}"))
        }
        _ => None,
    };

    GpuSnapshot {
        index,
        name: get("Dev Name").unwrap_or("未知 GCU").to_owned(),
        uuid: get("Dev UUID").map(str::to_owned),
        pci_address: pci_address.clone(),
        status,
        temperature_celsius: get("GCU Temp")
            .and_then(parse_leading_u64)
            .map(|v| v as u32),
        utilization_percent: get("GCU Usage")
            .and_then(parse_leading_f64)
            .map(|value| value.round() as u32),
        memory_used_mib: get("Used Size").and_then(parse_leading_u64),
        memory_total_mib: get("Total Size").and_then(parse_leading_u64),
        power_draw_watts: get("Cur Power").and_then(parse_leading_f64),
        power_limit_watts: get("Power Capa").and_then(parse_leading_f64),
        pstate: get("Dpm Level").map(str::to_owned),
        numa_node: pci_address
            .as_deref()
            .and_then(|address| read_numa_node(sysfs_root, address)),
        // Enflame 无 Xid / reset 对应语义，保持 None 不伪造。
        reset_required: None,
        xid_codes: None,
        smi_tool: Some("efsmi".to_owned()),
        vendor: Some("Enflame".to_owned()),
    }
}

/// 取值的第一个空白分隔 token 解析为 u64（容忍 "42976 MiB"、"35 ℃" 等单位后缀）。
fn parse_leading_u64(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse().ok()
}

fn parse_leading_f64(value: &str) -> Option<f64> {
    value.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::super::command::{CommandOutput, CommandRequest, CommandRunner};
    use super::{parse_efsmi_query, ChainGpuCollector, EnflameSmiCollector};
    use crate::collectors::{CollectorError, GpuCollector};
    use crate::domain::HealthStatus;

    const QUERY: &str = include_str!("fixtures/efsmi_query.txt");

    struct FailingRunner;

    impl CommandRunner for FailingRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            Err(CollectorError::new(
                "command",
                "spawn_failed",
                format!("{} 不存在", request.program),
            ))
        }
    }

    struct EfsmiOnlyRunner;

    impl CommandRunner for EfsmiOnlyRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            if request.program == "efsmi" {
                Ok(CommandOutput {
                    stdout: QUERY.to_owned(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    success: true,
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })
            } else {
                Err(CollectorError::new(
                    "command",
                    "spawn_failed",
                    format!("{} 不存在", request.program),
                ))
            }
        }
    }

    #[test]
    fn gcu_parses_real_efsmi_query_fixture() {
        let gpus =
            parse_efsmi_query(QUERY, std::path::Path::new("/nonexistent")).expect("efsmi fixture");
        assert_eq!(gpus.len(), 10);
        let first = &gpus[0];
        assert_eq!(first.index, 0);
        assert_eq!(first.name, "Enflame S60");
        assert_eq!(first.uuid.as_deref(), Some("TPUH90200803"));
        assert_eq!(first.pci_address.as_deref(), Some("0000:0d:00.0"));
        assert_eq!(first.status, HealthStatus::Healthy);
        assert_eq!(first.temperature_celsius, Some(35));
        assert_eq!(first.utilization_percent, Some(0));
        assert_eq!(first.memory_used_mib, Some(38934));
        assert_eq!(first.memory_total_mib, Some(42976));
        assert_eq!(first.power_draw_watts, Some(99.0));
        assert_eq!(first.power_limit_watts, Some(300.0));
        assert_eq!(first.pstate.as_deref(), Some("Sleep"));
        assert_eq!(first.smi_tool.as_deref(), Some("efsmi"));
        assert_eq!(first.vendor.as_deref(), Some("Enflame"));
        assert_eq!(first.xid_codes, None);
        assert_eq!(first.reset_required, None);
        assert_eq!(gpus[9].index, 9);
    }

    #[test]
    fn gcu_maps_health_false_to_critical() {
        let output = QUERY.replacen(
            "Health                  : True",
            "Health                  : False",
            1,
        );
        let gpus = parse_efsmi_query(&output, std::path::Path::new("/nonexistent"))
            .expect("health fixture");
        assert_eq!(gpus[0].status, HealthStatus::Critical);
        assert_eq!(gpus[1].status, HealthStatus::Healthy);
    }

    #[test]
    fn gcu_maps_error_count_to_warning() {
        let output = QUERY.replacen(
            "Total Error Count       : 0",
            "Total Error Count       : 3",
            1,
        );
        let gpus = parse_efsmi_query(&output, std::path::Path::new("/nonexistent"))
            .expect("error-count fixture");
        assert_eq!(gpus[0].status, HealthStatus::Warning);
        assert_eq!(gpus[1].status, HealthStatus::Healthy);
    }

    #[test]
    fn gcu_tolerates_missing_optional_fields() {
        let block = "DEV ID 2\n    Device Info\n        Dev Name                : Enflame S60\n        Health                  : True\n";
        let gpus =
            parse_efsmi_query(block, std::path::Path::new("/nonexistent")).expect("sparse block");
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].index, 2);
        assert_eq!(gpus[0].pci_address, None);
        assert_eq!(gpus[0].temperature_celsius, None);
        assert_eq!(gpus[0].status, HealthStatus::Healthy);
    }

    #[test]
    fn gcu_rejects_output_without_dev_blocks() {
        let error = parse_efsmi_query("no devices here", std::path::Path::new("/nonexistent"))
            .expect_err("no DEV ID");
        assert_eq!(error.code, "parse_failed");
    }

    #[test]
    fn gcu_reports_missing_efsmi_structurally() {
        let collector = EnflameSmiCollector::with_runner(FailingRunner);
        let error = collector.collect_gpus().expect_err("missing efsmi");
        assert_eq!(error.code, "command_failed");
        assert!(error.message.contains("efsmi"));
    }

    #[test]
    fn chain_falls_back_to_efsmi_when_nvidia_tools_missing() {
        let collector = ChainGpuCollector {
            nvidia: crate::collectors::gpu::NvidiaSmiCollector::with_runner(EfsmiOnlyRunner),
            enflame: EnflameSmiCollector::with_runner(EfsmiOnlyRunner),
        };
        let gpus = collector.collect_gpus().expect("efsmi fallback");
        assert_eq!(gpus.len(), 10);
        assert_eq!(gpus[0].smi_tool.as_deref(), Some("efsmi"));
    }

    #[test]
    fn chain_aggregates_error_when_all_tools_missing() {
        let collector = ChainGpuCollector {
            nvidia: crate::collectors::gpu::NvidiaSmiCollector::with_runner(FailingRunner),
            enflame: EnflameSmiCollector::with_runner(FailingRunner),
        };
        let error = collector.collect_gpus().expect_err("all missing");
        assert!(error.message.contains("nvidia-smi") && error.message.contains("efsmi"));
    }

    #[test]
    fn chain_prefers_nvidia_when_available() {
        struct NvidiaRunner;
        impl CommandRunner for NvidiaRunner {
            fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
                assert_eq!(request.program, "nvidia-smi");
                Ok(CommandOutput {
                    stdout: include_str!("fixtures/nvidia_smi_normal.csv").to_owned(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    success: true,
                    timed_out: false,
                    stdout_truncated: false,
                    stderr_truncated: false,
                })
            }
        }
        let collector = ChainGpuCollector {
            nvidia: crate::collectors::gpu::NvidiaSmiCollector::with_runner(NvidiaRunner),
            enflame: EnflameSmiCollector::with_runner(NvidiaRunner),
        };
        let gpus = collector.collect_gpus().expect("nvidia path");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].vendor.as_deref(), Some("NVIDIA"));
    }
}
