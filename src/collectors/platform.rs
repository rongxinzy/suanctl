//! 智算服务器平台基线的只读编排：IOMMU、NVIDIA 驱动和 CUDA。
//!
//! 本模块只读取 `/proc`、`/sys` 和受控外部命令。它不会执行 sudo、modprobe、
//! reset、bind/unbind 或任何 GPU workload。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{
    AcsSummary, CollectionIssue, HealthStatus, IommuAcsOverride, IommuRequested, IommuSnapshot,
    NvidiaDriverSnapshot, PciDeviceSnapshot, PlatformSnapshot,
};

use super::command::{
    CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};
use super::cuda::{parse_nvidia_smi_header, CudaStackCollector, NvidiaSmiHeader};
use super::p2p::NvidiaP2pCollector;
use super::pcie::LinuxPcieCollector;
use super::storage::LinuxStorageCollector;
use super::{CollectorError, PlatformCollector};

pub const DEFAULT_PLATFORM_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct LinuxPlatformCollector<R = ProcessCommandRunner> {
    runner: R,
    proc_root: PathBuf,
    sysfs_root: PathBuf,
    filesystem_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl LinuxPlatformCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            proc_root: PathBuf::from("/proc"),
            sysfs_root: PathBuf::from("/sys"),
            filesystem_root: PathBuf::from("/"),
            timeout: DEFAULT_PLATFORM_COMMAND_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

impl Default for LinuxPlatformCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> LinuxPlatformCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            proc_root: PathBuf::from("/proc"),
            sysfs_root: PathBuf::from("/sys"),
            filesystem_root: PathBuf::from("/"),
            timeout: DEFAULT_PLATFORM_COMMAND_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_proc_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.proc_root = root.into();
        self
    }

    pub fn with_sysfs_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.sysfs_root = root.into();
        self
    }

    pub fn with_filesystem_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.filesystem_root = root.into();
        self
    }

    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        self.proc_root = root.join("proc");
        self.sysfs_root = root.join("sys");
        self.filesystem_root = root;
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

impl<R: CommandRunner + Clone> LinuxPlatformCollector<R> {
    /// 不检查编译目标，便于在非 Linux 主机上使用固定根目录做测试。
    pub fn collect_snapshot(&self) -> Result<PlatformSnapshot, CollectorError> {
        let pcie = LinuxPcieCollector::with_runner(self.runner.clone())
            .with_sysfs_root(&self.sysfs_root)
            .with_timeout(self.timeout)
            .with_output_limits(self.stdout_limit, self.stderr_limit)
            .collect_snapshot();

        let storage = LinuxStorageCollector::with_runner(self.runner.clone())
            .with_proc_root(&self.proc_root)
            .with_sysfs_root(&self.sysfs_root)
            .with_timeout(self.timeout)
            .with_output_limits(self.stdout_limit, self.stderr_limit)
            .collect_snapshot(&pcie.devices);

        let p2p = NvidiaP2pCollector::with_runner(self.runner.clone())
            .with_timeout(self.timeout)
            .with_output_limits(self.stdout_limit, self.stderr_limit)
            .collect_topology();

        let (iommu, mut issues) = collect_iommu_snapshot(&self.proc_root, &self.sysfs_root);
        issues.extend(pcie.issues);
        issues.extend(storage.issues.iter().cloned());
        issues.extend(p2p.issues);

        let (smi_header, smi_issues) = collect_nvidia_smi_header(
            &self.runner,
            self.timeout,
            self.stdout_limit,
            self.stderr_limit,
        );
        issues.extend(smi_issues);

        let (nvidia_driver, driver_issues) =
            collect_nvidia_driver_snapshot(&self.proc_root, &pcie.devices, smi_header.as_ref());
        issues.extend(driver_issues);

        let cuda = CudaStackCollector::with_runner(self.runner.clone())
            .with_filesystem_root(&self.filesystem_root)
            .with_timeout(self.timeout)
            .with_output_limits(self.stdout_limit, self.stderr_limit)
            .collect(smi_header.and_then(|header| header.cuda_version));
        issues.extend(cuda.issues);

        let status = combine_status(
            [
                iommu.status,
                nvidia_driver.status,
                cuda.snapshot.status,
                p2p.snapshot.status,
                storage.status,
            ]
            .into_iter()
            .chain(issues.iter().map(|issue| issue.status)),
        );
        let acs_summary = summarize_acs(&pcie.devices);
        Ok(PlatformSnapshot {
            iommu,
            nvidia_driver,
            cuda: cuda.snapshot,
            pci_devices: pcie.devices,
            p2p: Some(p2p.snapshot),
            storage: Some(storage),
            plugins: Vec::new(),
            acs_summary,
            issues,
            status,
        })
    }
}

impl<R: CommandRunner + Clone> PlatformCollector for LinuxPlatformCollector<R> {
    fn collect_platform(&self) -> Result<PlatformSnapshot, CollectorError> {
        if !cfg!(target_os = "linux") {
            return Err(CollectorError::new(
                "platform",
                "unsupported_platform",
                "智算服务器平台采集器只能在 Linux 上读取 /proc 和 /sys",
            ));
        }
        self.collect_snapshot()
    }
}

/// 汇总 PCI 设备的 ACS 状态：支持 ACS 能力（含已知无 ACS）的设备中，
/// 开启任一隔离机制（ACSCtl 任意位为 true）与全部关闭的计数。
fn summarize_acs(devices: &[PciDeviceSnapshot]) -> Option<AcsSummary> {
    if devices.is_empty() {
        return None;
    }
    let mut supported = 0usize;
    let mut enabled = 0usize;
    let mut disabled = 0usize;
    for device in devices {
        let Some(capability) = device.acs.as_ref().and_then(|acs| acs.capability.as_ref()) else {
            continue;
        };
        if !capability.present {
            continue;
        }
        supported += 1;
        let controls = device.acs.as_ref().and_then(|acs| acs.control.as_ref());
        let any_enabled = controls.is_some_and(|control| {
            control.source_validation == Some(true)
                || control.translation_blocking == Some(true)
                || control.p2p_request_redirect == Some(true)
                || control.completion_redirect == Some(true)
                || control.upstream_forwarding == Some(true)
                || control.egress_control == Some(true)
                || control.direct_translated_p2p == Some(true)
        });
        if any_enabled {
            enabled += 1;
        } else {
            disabled += 1;
        }
    }
    if supported == 0 && enabled == 0 && disabled == 0 {
        None
    } else {
        Some(AcsSummary {
            supported,
            enabled,
            disabled,
        })
    }
}

fn collect_nvidia_smi_header<R: CommandRunner>(
    runner: &R,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> (Option<NvidiaSmiHeader>, Vec<CollectionIssue>) {
    // nvidia-smi 不可用时尝试 querygpu（改名体）。
    match run_smi_header(runner, "nvidia-smi", timeout, stdout_limit, stderr_limit) {
        (Some(header), issues) => (Some(header), issues),
        (None, mut primary_issues) => {
            let (fallback, fallback_issues) =
                run_smi_header(runner, "querygpu", timeout, stdout_limit, stderr_limit);
            if fallback.is_some() {
                return (fallback, fallback_issues);
            }
            // 两者均失败：报出 nvidia-smi 的原始问题，并在其中补充 querygpu 信息。
            let querygpu_hint = fallback_issues
                .first()
                .map_or("querygpu 亦不可用", |issue| &issue.message);
            primary_issues.push(platform_issue(
                "nvidia_smi_unavailable",
                HealthStatus::Unavailable,
                format!("nvidia-smi 与 querygpu 均不可用（{querygpu_hint}）"),
            ));
            (None, primary_issues)
        }
    }
}

fn run_smi_header<R: CommandRunner>(
    runner: &R,
    tool: &str,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> (Option<NvidiaSmiHeader>, Vec<CollectionIssue>) {
    let mut request = CommandRequest::new(tool, std::iter::empty::<String>());
    request.timeout = timeout;
    request.stdout_limit = stdout_limit;
    request.stderr_limit = stderr_limit;
    let output = match runner.run(&request) {
        Ok(output) => output,
        Err(error) => {
            return (
                None,
                vec![platform_issue(
                    "nvidia_smi_unavailable",
                    HealthStatus::Unavailable,
                    format!("{tool} 不可用：{}", error.message),
                )],
            )
        }
    };
    if output.timed_out {
        return (
            None,
            vec![platform_issue(
                "nvidia_smi_timeout",
                HealthStatus::Warning,
                format!("{tool} 超时，驱动和 driver_reported_max_cuda 保持未知"),
            )],
        );
    }
    if output.stdout_truncated || output.stderr_truncated {
        return (
            None,
            vec![platform_issue(
                "nvidia_smi_output_too_large",
                HealthStatus::Warning,
                format!("{tool} 默认头部输出超过安全上限"),
            )],
        );
    }
    if !output.success {
        return (
            None,
            vec![platform_issue(
                "nvidia_smi_failed",
                HealthStatus::Warning,
                format!("{tool} 失败：{}", output.stderr.trim()),
            )],
        );
    }
    let header = parse_nvidia_smi_header(&output.stdout);
    if header.driver_version.is_none() && header.cuda_version.is_none() {
        return (
            None,
            vec![platform_issue(
                "nvidia_smi_header_unparsed",
                HealthStatus::Warning,
                format!("{tool} 命令成功，但默认头部没有可解析的 Driver/CUDA Version"),
            )],
        );
    }
    (Some(header), Vec::new())
}

pub fn collect_iommu_snapshot(
    proc_root: &Path,
    sysfs_root: &Path,
) -> (IommuSnapshot, Vec<CollectionIssue>) {
    let mut issues = Vec::new();
    let cmdline_path = proc_root.join("cmdline");
    let (requested, cmdline) = match fs::read_to_string(&cmdline_path) {
        Ok(value) => (Some(parse_iommu_requested(&value)), Some(value)),
        Err(error) => {
            issues.push(platform_issue(
                "iommu_cmdline_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", cmdline_path.display()),
            ));
            (None, None)
        }
    };
    let acs_override = cmdline.as_deref().and_then(parse_acs_override);

    let groups_path = sysfs_root.join("kernel/iommu_groups");
    let (groups_present, group_count) = match fs::read_dir(&groups_path) {
        Ok(entries) => {
            let mut count = 0_u32;
            for entry in entries {
                match entry {
                    Ok(entry)
                        if entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.parse::<u32>().is_ok()) =>
                    {
                        count = count.saturating_add(1);
                    }
                    Ok(_) => {}
                    Err(error) => issues.push(platform_issue(
                        "iommu_group_entry_unavailable",
                        HealthStatus::Warning,
                        format!("读取 IOMMU group 目录项失败：{error}"),
                    )),
                }
            }
            (Some(count > 0), Some(count))
        }
        Err(error) => {
            issues.push(platform_issue(
                "iommu_groups_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", groups_path.display()),
            ));
            (None, None)
        }
    };
    let effective = groups_present;
    let effective_mode = effective.and_then(|enabled| {
        enabled.then(|| {
            requested
                .as_ref()
                .and_then(|value| value.mode.clone())
                .unwrap_or_else(|| "iommu_groups".to_owned())
        })
    });

    if requested.as_ref().and_then(|value| value.enabled) == Some(true) && effective == Some(false)
    {
        issues.push(platform_issue(
            "iommu_requested_not_effective",
            HealthStatus::Warning,
            "内核命令行请求启用 IOMMU，但 /sys/kernel/iommu_groups 没有 group",
        ));
    }
    if acs_override.is_some() {
        issues.push(platform_issue(
            "pcie_acs_override",
            HealthStatus::Warning,
            "检测到 pcie_acs_override；这是风险覆盖项，不视为正常 ACS 状态",
        ));
    }
    let status = if acs_override.is_some()
        || (requested.as_ref().and_then(|value| value.enabled) == Some(true)
            && effective == Some(false))
    {
        HealthStatus::Warning
    } else if effective == Some(true) {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    };
    (
        IommuSnapshot {
            requested,
            effective,
            effective_mode,
            groups_present,
            group_count,
            acs_override,
            status,
        },
        issues,
    )
}

pub fn parse_iommu_requested(cmdline: &str) -> IommuRequested {
    let mut result = IommuRequested {
        enabled: None,
        vendor: None,
        mode: None,
        parameters: Vec::new(),
    };
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("intel_iommu=") {
            result.vendor = Some("intel".to_owned());
            result.enabled = parse_iommu_toggle(value);
            result.parameters.push(token.to_owned());
        } else if let Some(value) = token.strip_prefix("amd_iommu=") {
            result.vendor = Some("amd".to_owned());
            result.enabled = parse_iommu_toggle(value);
            result.parameters.push(token.to_owned());
        } else if let Some(value) = token.strip_prefix("iommu=") {
            result.mode = (!value.is_empty()).then(|| value.to_owned());
            result.enabled = Some(!value.eq_ignore_ascii_case("off"));
            result.parameters.push(token.to_owned());
        }
    }
    result
}

pub fn parse_acs_override(cmdline: &str) -> Option<IommuAcsOverride> {
    cmdline.split_whitespace().find_map(|token| {
        let value = token.strip_prefix("pcie_acs_override=")?;
        Some(IommuAcsOverride {
            enabled: true,
            value: Some(value.to_owned()),
            risk: HealthStatus::Warning,
            warning: "pcie_acs_override 会改变 PCIe 隔离语义，必须单独审查".to_owned(),
        })
    })
}

fn parse_iommu_toggle(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        "igfx_off" | "force" => Some(true),
        _ => None,
    }
}

fn collect_nvidia_driver_snapshot(
    proc_root: &Path,
    devices: &[PciDeviceSnapshot],
    smi_header: Option<&NvidiaSmiHeader>,
) -> (NvidiaDriverSnapshot, Vec<CollectionIssue>) {
    let mut issues = Vec::new();
    let modules_path = proc_root.join("modules");
    let module_text = match fs::read_to_string(&modules_path) {
        Ok(value) => Some(value),
        Err(error) => {
            issues.push(platform_issue(
                "proc_modules_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", modules_path.display()),
            ));
            None
        }
    };
    let kernel_version_path = proc_root.join("driver/nvidia/version");
    let kernel_version_text = match fs::read_to_string(&kernel_version_path) {
        Ok(value) => Some(value),
        Err(error) => {
            issues.push(platform_issue(
                "nvidia_kernel_version_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", kernel_version_path.display()),
            ));
            None
        }
    };

    let module_loaded = module_text.as_deref().map(parse_proc_modules);
    let kernel_module_version = kernel_version_text
        .as_deref()
        .and_then(parse_kernel_module_version);
    let nvidia_smi_driver_version = smi_header.and_then(|header| header.driver_version.clone());
    let mut driver_names = BTreeSet::new();
    for device in devices {
        let is_nvidia = device
            .vendor
            .as_deref()
            .map(|vendor| {
                vendor
                    .trim()
                    .trim_start_matches("0x")
                    .eq_ignore_ascii_case("10de")
            })
            .unwrap_or(false);
        if is_nvidia {
            if let Some(driver) = device.driver.as_deref() {
                driver_names.insert(driver.to_owned());
            }
        }
    }
    let version_match = kernel_module_version
        .as_deref()
        .zip(nvidia_smi_driver_version.as_deref())
        .map(|(kernel, smi)| kernel == smi);
    if version_match == Some(false) {
        issues.push(platform_issue(
            "driver_version_mismatch",
            HealthStatus::Warning,
            format!(
                "内核模块版本 {} 与 nvidia-smi/user-space 版本 {} 不一致",
                kernel_module_version.as_deref().unwrap_or("未知"),
                nvidia_smi_driver_version.as_deref().unwrap_or("未知")
            ),
        ));
    }
    let status = if version_match == Some(false) {
        HealthStatus::Warning
    } else if module_loaded == Some(true)
        || kernel_module_version.is_some()
        || nvidia_smi_driver_version.is_some()
        || !driver_names.is_empty()
    {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    };
    (
        NvidiaDriverSnapshot {
            module_loaded,
            kernel_module_version,
            nvidia_smi_driver_version,
            device_driver_names: driver_names.into_iter().collect(),
            version_match,
            status,
        },
        issues,
    )
}

pub fn parse_proc_modules(text: &str) -> bool {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|name| name == "nvidia")
}

pub fn parse_kernel_module_version(text: &str) -> Option<String> {
    text.split_whitespace().find_map(valid_version)
}

fn valid_version(value: &str) -> Option<String> {
    let value = value.trim_matches(|character: char| matches!(character, ',' | ';' | ':'));
    (!value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
        }))
    .then(|| value.to_owned())
}

fn combine_status(statuses: impl IntoIterator<Item = HealthStatus>) -> HealthStatus {
    let mut has_unknown = false;
    let mut has_unavailable = false;
    for status in statuses {
        match status {
            HealthStatus::Critical => return HealthStatus::Critical,
            HealthStatus::Warning => return HealthStatus::Warning,
            HealthStatus::Unavailable => has_unavailable = true,
            HealthStatus::Unknown => has_unknown = true,
            HealthStatus::Healthy => {}
        }
    }
    if has_unavailable {
        HealthStatus::Unavailable
    } else if has_unknown {
        HealthStatus::Unknown
    } else {
        HealthStatus::Healthy
    }
}

fn platform_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "platform".to_owned(),
        code: code.into(),
        status,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        collect_iommu_snapshot, collect_nvidia_smi_header, parse_acs_override,
        parse_iommu_requested, parse_kernel_module_version, parse_proc_modules, summarize_acs,
    };
    use crate::collectors::command::{CommandOutput, CommandRequest, CommandRunner};
    use crate::domain::{
        AcsCapability, AcsControl, AcsSnapshot, HealthStatus, PciDeviceRole, PciDeviceSnapshot,
    };

    /// 按程序名分流的 FakeRunner（nvidia-smi → querygpu fallback 测试用）。
    struct FakeRunner {
        by_program: std::collections::BTreeMap<
            String,
            std::sync::Arc<Result<CommandOutput, crate::collectors::CollectorError>>,
        >,
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            request: &CommandRequest,
        ) -> Result<CommandOutput, crate::collectors::CollectorError> {
            self.by_program
                .get(&request.program)
                .map(|response| response.as_ref().clone())
                .unwrap_or_else(|| {
                    Err(crate::collectors::CollectorError::new(
                        "command",
                        "spawn_failed",
                        format!("{} 不存在", request.program),
                    ))
                })
        }
    }

    fn smi_output(stdout: &str) -> CommandOutput {
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
    fn smi_header_falls_back_to_querygpu_when_nvidia_smi_missing() {
        let runner = FakeRunner {
            by_program: std::collections::BTreeMap::from([
                (
                    "querygpu".to_owned(),
                    std::sync::Arc::new(Ok(smi_output(
                        "Mon Aug  4 12:00:00 2026\n+-----------------------------------------------------------------------------+\n| NVIDIA-SMI 595.58.03    Driver Version: 595.58.03    CUDA Version: 12.4     |\n+-----------------------------------------------------------------------------+\n",
                    ))),
                ),
            ]),
        };
        let (header, issues) =
            collect_nvidia_smi_header(&runner, std::time::Duration::from_secs(2), 4096, 4096);
        let header = header.expect("querygpu 应提供头部");
        assert_eq!(
            header.driver_version.as_deref(),
            Some("595.58.03"),
            "querygpu 改名体输出应被解析"
        );
        assert!(issues.is_empty());
    }

    fn acs_device(bdf: &str, present: bool, p2p_redirect: Option<bool>) -> PciDeviceSnapshot {
        PciDeviceSnapshot {
            bdf: bdf.to_owned(),
            vendor: None,
            device: None,
            class: Some("0x060400".to_owned()),
            driver: None,
            numa_node: None,
            iommu_group: None,
            current_link_speed: None,
            current_link_width: None,
            current_link_gen: None,
            current_theoretical_bandwidth_mb_s: None,
            max_link_speed: None,
            max_link_width: None,
            max_link_gen: None,
            max_theoretical_bandwidth_mb_s: None,
            acs: Some(AcsSnapshot {
                capability: Some(AcsCapability {
                    present,
                    source_validation: None,
                    translation_blocking: None,
                    p2p_request_redirect: None,
                    completion_redirect: None,
                    upstream_forwarding: None,
                    egress_control: None,
                    direct_translated_p2p: None,
                }),
                control: Some(AcsControl {
                    source_validation: None,
                    translation_blocking: None,
                    p2p_request_redirect: p2p_redirect,
                    completion_redirect: None,
                    upstream_forwarding: None,
                    egress_control: None,
                    direct_translated_p2p: None,
                }),
            }),
            role: Some(PciDeviceRole::Bridge),
            class_name: None,
            vendor_name: None,
            device_name: None,
            subsystem_vendor: None,
            subsystem_device: None,
            subsystem_name: None,
            parent_bdf: None,
            downstream_bdfs: Vec::new(),
            mmio_windows: Vec::new(),
            status: HealthStatus::Healthy,
        }
    }

    #[test]
    fn acs_summary_counts_supported_enabled_and_disabled() {
        // 支持 ACS 的设备 3 台：2 台开启隔离（任一位 true），1 台全部关闭；1 台无 ACS 能力。
        let devices = vec![
            acs_device("0000:00:01.0", true, Some(true)),
            acs_device("0000:00:02.0", true, Some(false)),
            acs_device("0000:00:03.0", true, None),
            acs_device("0000:01:00.0", false, None),
        ];
        let summary = summarize_acs(&devices).expect("应有统计");
        assert_eq!(summary.supported, 3);
        assert_eq!(summary.enabled, 1);
        assert_eq!(summary.disabled, 2);
    }

    #[test]
    fn acs_summary_empty_devices_is_none() {
        assert!(summarize_acs(&[]).is_none());
    }

    #[test]
    fn iommu_requested_without_groups_is_not_reported_as_effective() {
        let root = fixture_root("iommu-no-groups");
        fs::create_dir_all(root.join("proc")).expect("proc fixture");
        fs::create_dir_all(root.join("sys/kernel/iommu_groups")).expect("groups fixture");
        fs::write(
            root.join("proc/cmdline"),
            include_str!("fixtures/platform_proc_cmdline_requested.txt"),
        )
        .expect("cmdline fixture");
        let (snapshot, issues) = collect_iommu_snapshot(&root.join("proc"), &root.join("sys"));
        assert_eq!(
            snapshot.requested.as_ref().and_then(|value| value.enabled),
            Some(true)
        );
        assert_eq!(snapshot.groups_present, Some(false));
        assert_eq!(snapshot.effective, Some(false));
        assert_eq!(snapshot.status, HealthStatus::Warning);
        assert!(issues
            .iter()
            .any(|issue| issue.code == "iommu_requested_not_effective"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn iommu_groups_and_acs_override_are_reported_separately() {
        let root = fixture_root("iommu-groups");
        fs::create_dir_all(root.join("proc")).expect("proc fixture");
        fs::create_dir_all(root.join("sys/kernel/iommu_groups/7")).expect("group fixture");
        fs::write(
            root.join("proc/cmdline"),
            "amd_iommu=on iommu=pt pcie_acs_override=downstream\n",
        )
        .expect("cmdline fixture");
        let (snapshot, issues) = collect_iommu_snapshot(&root.join("proc"), &root.join("sys"));
        assert_eq!(snapshot.groups_present, Some(true));
        assert_eq!(snapshot.group_count, Some(1));
        assert_eq!(snapshot.effective, Some(true));
        assert_eq!(snapshot.effective_mode.as_deref(), Some("pt"));
        assert_eq!(
            snapshot.acs_override.as_ref().map(|value| value.risk),
            Some(HealthStatus::Warning)
        );
        assert!(issues.iter().any(|issue| issue.code == "pcie_acs_override"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn driver_parsers_cover_module_and_version_match_inputs() {
        let modules = include_str!("fixtures/platform_proc_modules.txt");
        let version = include_str!("fixtures/platform_nvidia_version.txt");
        assert!(parse_proc_modules(modules));
        assert_eq!(
            parse_kernel_module_version(version).as_deref(),
            Some("550.54.14")
        );
        assert_eq!(
            parse_kernel_module_version("NVRM version: unavailable"),
            None
        );
    }

    #[test]
    fn iommu_cmdline_parser_keeps_requested_parameters_structured() {
        let requested = parse_iommu_requested("intel_iommu=on iommu=pt quiet");
        assert_eq!(requested.vendor.as_deref(), Some("intel"));
        assert_eq!(requested.enabled, Some(true));
        assert_eq!(requested.mode.as_deref(), Some("pt"));
        assert_eq!(requested.parameters, vec!["intel_iommu=on", "iommu=pt"]);
        assert_eq!(parse_acs_override("quiet"), None);
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
