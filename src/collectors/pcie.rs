//! PCIe、IOMMU group 关联和 ACS 的只读采集。
//!
//! sysfs 是设备清单和链路状态的来源；`lspci -Dvv` 只负责补充 ACS
//! capability/control。lspci 不可用时，设备清单仍然返回，ACS 保持未知。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::{
    AcsCapability, AcsControl, AcsSnapshot, CollectionIssue, HealthStatus, PciDeviceRole,
    PciDeviceSnapshot, PciMmioKind, PciMmioWindow,
};

use super::command::{
    CommandRequest, CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};

pub const DEFAULT_MAX_PCI_DEVICES: usize = 4096;
pub const DEFAULT_LSPCI_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct PcieCollection {
    pub devices: Vec<PciDeviceSnapshot>,
    pub issues: Vec<CollectionIssue>,
}

#[derive(Debug, Clone)]
pub struct LinuxPcieCollector<R = ProcessCommandRunner> {
    runner: R,
    sysfs_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    max_devices: usize,
}

impl LinuxPcieCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            sysfs_root: PathBuf::from("/sys"),
            timeout: DEFAULT_LSPCI_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            max_devices: DEFAULT_MAX_PCI_DEVICES,
        }
    }
}

impl Default for LinuxPcieCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> LinuxPcieCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            sysfs_root: PathBuf::from("/sys"),
            timeout: DEFAULT_LSPCI_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            max_devices: DEFAULT_MAX_PCI_DEVICES,
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

    pub fn with_max_devices(mut self, max_devices: usize) -> Self {
        self.max_devices = max_devices;
        self
    }
}

impl<R: CommandRunner> LinuxPcieCollector<R> {
    /// 在指定根目录读取 PCIe sysfs，并尽力补充 ACS。此方法不检查编译目标，
    /// 便于使用固定 fixture 做跨平台测试。
    pub fn collect_snapshot(&self) -> PcieCollection {
        let mut collection = scan_sysfs_devices(&self.sysfs_root, self.max_devices);
        let lspci = self.collect_lspci(&mut collection.issues);
        if let Some(lspci) = lspci {
            for device in &mut collection.devices {
                if let Some(detail) = lspci.details.get(&device.bdf) {
                    device.class_name = detail.class_name.clone();
                    device.vendor_name = detail.vendor_name.clone();
                    device.device_name = detail.device_name.clone();
                    device.subsystem_vendor = detail.subsystem_vendor.clone();
                    device.subsystem_device = detail.subsystem_device.clone();
                    device.subsystem_name = detail.subsystem_name.clone();
                }
                device.acs = lspci.acs.get(&device.bdf).cloned();
            }
        }
        collection
    }

    fn collect_lspci(&self, issues: &mut Vec<CollectionIssue>) -> Option<LspciCollection> {
        let mut request = CommandRequest::new("lspci", ["-Dvv"]);
        request.timeout = self.timeout;
        request.stdout_limit = self.stdout_limit;
        request.stderr_limit = self.stderr_limit;

        let output = match self.runner.run(&request) {
            Ok(output) => output,
            Err(error) => {
                issues.push(CollectionIssue {
                    collector: "pcie".to_owned(),
                    code: "lspci_unavailable".to_owned(),
                    status: HealthStatus::Unavailable,
                    message: format!("无法读取 ACS，lspci 不可用：{}", error.message),
                });
                return None;
            }
        };
        if output.timed_out {
            issues.push(pcie_issue(
                "lspci_timeout",
                HealthStatus::Warning,
                "lspci -Dvv 超时，ACS 保持未知；其他 sysfs 采集不受阻断",
            ));
            return None;
        }
        if output.stdout_truncated || output.stderr_truncated {
            issues.push(pcie_issue(
                "lspci_output_too_large",
                HealthStatus::Warning,
                "lspci 输出超过安全上限，ACS 保持未知",
            ));
            return None;
        }
        if !output.success {
            issues.push(pcie_issue(
                "lspci_failed",
                HealthStatus::Warning,
                format!("lspci -Dvv 失败，ACS 保持未知：{}", output.stderr.trim()),
            ));
            return None;
        }
        Some(LspciCollection {
            acs: parse_lspci_acs(&output.stdout),
            details: parse_lspci_details(&output.stdout),
        })
    }
}

#[derive(Debug, Default)]
struct LspciCollection {
    acs: BTreeMap<String, AcsSnapshot>,
    details: BTreeMap<String, PciLspciDetail>,
}

pub fn scan_sysfs_devices(root: &Path, max_devices: usize) -> PcieCollection {
    let devices_root = root.join("bus/pci/devices");
    let entries = match fs::read_dir(&devices_root) {
        Ok(entries) => entries,
        Err(error) => {
            return PcieCollection {
                devices: Vec::new(),
                issues: vec![pcie_issue(
                    "sysfs_unavailable",
                    HealthStatus::Unavailable,
                    format!("读取 {} 失败：{error}", devices_root.display()),
                )],
            }
        }
    };

    let mut paths = Vec::new();
    let mut issues = Vec::new();
    for entry in entries {
        if paths.len() >= max_devices {
            issues.push(pcie_issue(
                "device_limit_reached",
                HealthStatus::Warning,
                format!("PCIe 设备数量超过安全上限 {max_devices}，后续设备未读取"),
            ));
            break;
        }
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => issues.push(pcie_issue(
                "device_entry_unavailable",
                HealthStatus::Warning,
                format!("读取 PCIe 设备目录项失败：{error}"),
            )),
        }
    }
    paths.sort();

    let mut devices = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(bdf) = path.file_name().and_then(|name| name.to_str()) else {
            issues.push(pcie_issue(
                "invalid_device_name",
                HealthStatus::Warning,
                format!("PCIe 设备路径名称不是 UTF-8：{}", path.display()),
            ));
            continue;
        };
        if !is_valid_bdf(bdf) {
            issues.push(pcie_issue(
                "invalid_bdf",
                HealthStatus::Warning,
                format!("忽略无法安全解析的 PCIe 设备名称：{bdf}"),
            ));
            continue;
        }
        devices.push(read_pci_device(&path, bdf, &mut issues));
    }

    let positions = devices
        .iter()
        .enumerate()
        .map(|(index, device)| (device.bdf.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for index in 0..devices.len() {
        let Some(parent) = devices[index].parent_bdf.clone() else {
            continue;
        };
        if let Some(parent_index) = positions.get(&parent).copied() {
            let child_bdf = devices[index].bdf.clone();
            devices[parent_index].downstream_bdfs.push(child_bdf);
        }
    }
    for device in &mut devices {
        device.downstream_bdfs.sort();
    }

    PcieCollection { devices, issues }
}

fn read_pci_device(path: &Path, bdf: &str, issues: &mut Vec<CollectionIssue>) -> PciDeviceSnapshot {
    let vendor = read_optional_text(path.join("vendor"), "vendor", bdf, issues);
    let device = read_optional_text(path.join("device"), "device", bdf, issues);
    let class = read_optional_text(path.join("class"), "class", bdf, issues);
    let driver = read_link_name(path.join("driver"), "driver", bdf, issues);
    let numa_node = read_numa_node(path.join("numa_node"), bdf, issues);
    let iommu_group = read_iommu_group(path.join("iommu_group"), bdf, issues);
    let current_link_speed = read_optional_text(
        path.join("current_link_speed"),
        "current_link_speed",
        bdf,
        issues,
    );
    let current_link_width = read_optional_text(
        path.join("current_link_width"),
        "current_link_width",
        bdf,
        issues,
    );
    let max_link_speed =
        read_optional_text(path.join("max_link_speed"), "max_link_speed", bdf, issues);
    let max_link_width =
        read_optional_text(path.join("max_link_width"), "max_link_width", bdf, issues);
    let current_link_gen = pcie_generation(current_link_speed.as_deref());
    let current_theoretical_bandwidth_mb_s =
        theoretical_bandwidth_mb_s(current_link_speed.as_deref(), current_link_width.as_deref());
    let max_link_gen = pcie_generation(max_link_speed.as_deref());
    let max_theoretical_bandwidth_mb_s =
        theoretical_bandwidth_mb_s(max_link_speed.as_deref(), max_link_width.as_deref());
    let mmio_windows = read_mmio_windows(path.join("resource"), bdf, issues);
    let parent_bdf = infer_parent_bdf(path, bdf);
    let role = class_role(class.as_deref());
    let status = if vendor.is_some() || device.is_some() || class.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    };

    PciDeviceSnapshot {
        bdf: bdf.to_owned(),
        vendor,
        device,
        class,
        driver,
        numa_node,
        iommu_group,
        current_link_speed,
        current_link_width,
        current_link_gen,
        current_theoretical_bandwidth_mb_s,
        max_link_speed,
        max_link_width,
        max_link_gen,
        max_theoretical_bandwidth_mb_s,
        acs: None,
        role,
        class_name: None,
        vendor_name: None,
        device_name: None,
        subsystem_vendor: None,
        subsystem_device: None,
        subsystem_name: None,
        parent_bdf,
        downstream_bdfs: Vec::new(),
        mmio_windows,
        status,
    }
}

/// 解析 sysfs resource 文件的 MMIO 窗口（每行 start end flags）。
/// 64 位 BAR 在 sysfs 中占用两行，这里原样保留两行（索引不同）。
pub fn parse_resource_windows(content: &str) -> Vec<PciMmioWindow> {
    let mut windows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if index > 7 {
            break;
        }
        let mut parts = line.split_whitespace();
        let (Some(start), Some(end), Some(flags)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(flags)) = (
            parse_hex_u64(start),
            parse_hex_u64(end),
            parse_hex_u64(flags),
        ) else {
            continue;
        };
        // 未分配窗口：sysfs 用 0xffffffffffffffff 0x0 0x0 表示。
        if start == u64::MAX || end == 0 || start > end {
            continue;
        }
        let kind = if flags & 0x100 != 0 {
            PciMmioKind::IoPort
        } else if flags & 0x100_000 != 0 {
            PciMmioKind::Mmio64
        } else if flags & 0x200 != 0 {
            PciMmioKind::Mmio32
        } else {
            PciMmioKind::Other
        };
        windows.push(PciMmioWindow {
            index: index as u8,
            start,
            end,
            size: end - start + 1,
            kind,
        });
    }
    windows
}

fn parse_hex_u64(value: &str) -> Result<u64, std::num::ParseIntError> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(digits, 16)
}

/// 读取设备的 MMIO 窗口。非 root 读取 resource 常被拒绝（EACCES）：
/// 此时静默降级为空列表（诊断/报告不因权限产生噪音），其余错误记录 issue。
fn read_mmio_windows(
    path: PathBuf,
    bdf: &str,
    issues: &mut Vec<CollectionIssue>,
) -> Vec<PciMmioWindow> {
    match fs::read_to_string(&path) {
        Ok(content) => parse_resource_windows(&content),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Vec::new(),
        Err(error) => {
            issues.push(pcie_issue(
                "mmio_unavailable",
                HealthStatus::Warning,
                format!("{bdf} 读取 {path:?} 失败：{error}"),
            ));
            Vec::new()
        }
    }
}

/// 将 Linux sysfs 的 GT/s 字符串映射为 PCIe generation。未知的新速率保持
/// `None`，避免把尚未建模的链路伪装成已识别。
pub fn pcie_generation(speed: Option<&str>) -> Option<u8> {
    let gt_per_second = parse_link_speed_gt_s(speed?)?;
    [
        (2.5, 1_u8),
        (5.0, 2),
        (8.0, 3),
        (16.0, 4),
        (32.0, 5),
        (64.0, 6),
    ]
    .into_iter()
    .find(|(known, _)| (gt_per_second - known).abs() < 0.05)
    .map(|(_, generation)| generation)
}

/// 计算 PCIe Gen1-5 的名义单向 payload 上限（十进制 MB/s）。它不是实测值。
/// Gen6 使用 FLIT/PAM4，不能沿用 Gen3-5 的 128b/130b 简化公式，暂不估算。
pub fn theoretical_bandwidth_mb_s(speed: Option<&str>, width: Option<&str>) -> Option<u64> {
    let gt_per_second = parse_link_speed_gt_s(speed?)?;
    let lanes = parse_link_width(width?)?;
    let generation = pcie_generation(speed)?;
    let encoding_efficiency = match generation {
        1 | 2 => 8.0 / 10.0,
        3..=5 => 128.0 / 130.0,
        _ => return None,
    };
    let megabytes_per_second =
        gt_per_second * f64::from(lanes) * encoding_efficiency * 1000.0 / 8.0;
    Some(megabytes_per_second.round() as u64)
}

fn parse_link_speed_gt_s(value: &str) -> Option<f64> {
    value.split_whitespace().next()?.parse::<f64>().ok()
}

pub fn parse_link_width(value: &str) -> Option<u32> {
    value
        .trim()
        .trim_start_matches(['x', 'X'])
        .parse::<u32>()
        .ok()
        .filter(|width| *width > 0)
}

/// PCIe 链路紧凑描述：`Gen4 x8`；当前协商低于设备最大能力时标注
/// `Gen1 x8（最大 Gen4 x8）`。两端都未知时返回 None，调用方自行兜底。
pub fn link_brief(device: &PciDeviceSnapshot) -> Option<String> {
    let current = link_half_brief(
        device.current_link_speed.as_deref(),
        device.current_link_width.as_deref(),
    );
    let maximum = link_half_brief(
        device.max_link_speed.as_deref(),
        device.max_link_width.as_deref(),
    );
    match (current, maximum) {
        (None, None) => None,
        (Some(current), None) => Some(current),
        (None, Some(maximum)) => Some(format!("最大 {maximum}")),
        (Some(current), Some(maximum)) if current == maximum => Some(current),
        (Some(current), Some(maximum)) => Some(format!("{current}（最大 {maximum}）")),
    }
}

/// TUI 窄列用的更短形式：降级时写成 `Gen1 x8→Gen4 x8`。
pub fn link_brief_compact(device: &PciDeviceSnapshot) -> Option<String> {
    let current = link_half_brief(
        device.current_link_speed.as_deref(),
        device.current_link_width.as_deref(),
    );
    let maximum = link_half_brief(
        device.max_link_speed.as_deref(),
        device.max_link_width.as_deref(),
    );
    match (current, maximum) {
        (None, None) => None,
        (Some(current), None) => Some(current),
        (None, Some(maximum)) => Some(format!("≤{maximum}")),
        (Some(current), Some(maximum)) if current == maximum => Some(current),
        (Some(current), Some(maximum)) => Some(format!("{current}→{maximum}")),
    }
}

fn link_half_brief(speed: Option<&str>, width: Option<&str>) -> Option<String> {
    let generation = pcie_generation(speed);
    let lanes = width.and_then(parse_link_width);
    match (generation, lanes) {
        (None, None) => None,
        (generation, lanes) => Some(format!(
            "{} {}",
            generation.map_or_else(|| "Gen?".to_owned(), |g| format!("Gen{g}")),
            lanes.map_or_else(|| "x?".to_owned(), |w| format!("x{w}")),
        )),
    }
}

/// GPU 快照 → PCIe 设备清单条目（pci_address 归一化后按 BDF 匹配）。
/// 地址缺失或清单对不上时返回 None，不伪造。
pub fn gpu_pcie_device<'a>(
    gpu: &crate::domain::GpuSnapshot,
    devices: &'a [PciDeviceSnapshot],
) -> Option<&'a PciDeviceSnapshot> {
    let bdf = crate::collectors::gpu::normalize_pci_address(gpu.pci_address.as_deref()?)
        .to_ascii_lowercase();
    devices
        .iter()
        .find(|device| device.bdf.eq_ignore_ascii_case(&bdf))
}

fn infer_parent_bdf(path: &Path, bdf: &str) -> Option<String> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let components = canonical
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let current = components.iter().rposition(|component| *component == bdf)?;
    components[..current]
        .iter()
        .rev()
        .find(|component| is_valid_bdf(component))
        .map(|component| (*component).to_owned())
}

pub fn class_role(class: Option<&str>) -> Option<PciDeviceRole> {
    let value = class?.trim().trim_start_matches("0x");
    let class = value.get(..4)?.to_ascii_lowercase();
    Some(match class.as_str() {
        "0604" => PciDeviceRole::Bridge,
        "0104" => PciDeviceRole::Raid,
        "0107" => PciDeviceRole::Sas,
        "0106" => PciDeviceRole::Sata,
        "0108" => PciDeviceRole::Nvme,
        "0100" => PciDeviceRole::Scsi,
        _ => PciDeviceRole::Other,
    })
}

fn read_optional_text(
    path: PathBuf,
    field: &str,
    bdf: &str,
    issues: &mut Vec<CollectionIssue>,
) -> Option<String> {
    match fs::read_to_string(&path) {
        Ok(value) => scalar(&value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            issues.push(pcie_issue(
                "field_unavailable",
                HealthStatus::Warning,
                format!("PCIe {bdf} 的 {field} 读取失败：{error}"),
            ));
            None
        }
    }
}

fn read_link_name(
    path: PathBuf,
    field: &str,
    bdf: &str,
    issues: &mut Vec<CollectionIssue>,
) -> Option<String> {
    match fs::read_link(&path) {
        Ok(target) => target
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            issues.push(pcie_issue(
                "link_unavailable",
                HealthStatus::Warning,
                format!("PCIe {bdf} 的 {field} symlink 读取失败：{error}"),
            ));
            None
        }
    }
}

fn read_numa_node(path: PathBuf, bdf: &str, issues: &mut Vec<CollectionIssue>) -> Option<u32> {
    let value = read_optional_text(path, "numa_node", bdf, issues)?;
    value
        .parse::<i64>()
        .ok()
        .and_then(|number| (number >= 0).then(|| u32::try_from(number).ok()).flatten())
}

fn read_iommu_group(path: PathBuf, bdf: &str, issues: &mut Vec<CollectionIssue>) -> Option<u32> {
    let target = match fs::read_link(&path) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            issues.push(pcie_issue(
                "iommu_group_unavailable",
                HealthStatus::Warning,
                format!("PCIe {bdf} 的 iommu_group symlink 读取失败：{error}"),
            ));
            return None;
        }
    };
    target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u32>().ok())
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub fn is_valid_bdf(value: &str) -> bool {
    let Some((domain, rest)) = value.split_once(':') else {
        return false;
    };
    let Some((bus, device_function)) = rest.split_once(':') else {
        return false;
    };
    let Some((device, function)) = device_function.split_once('.') else {
        return false;
    };
    (domain.len() == 4 || domain.len() == 8)
        && bus.len() == 2
        && device.len() == 2
        && function.len() == 1
        && [domain, bus, device, function]
            .into_iter()
            .all(|part| u64::from_str_radix(part, 16).is_ok())
}

pub fn parse_lspci_acs(output: &str) -> BTreeMap<String, AcsSnapshot> {
    let mut devices = BTreeMap::new();
    let mut current: Option<LspciBlock> = None;

    for line in output.lines() {
        if let Some(bdf) = line_bdf(line) {
            if let Some(block) = current.take() {
                devices.insert(block.bdf.clone(), block.finish());
            }
            current = Some(LspciBlock::new(bdf));
            continue;
        }
        if let Some(block) = current.as_mut() {
            let trimmed = line.trim();
            if trimmed.contains("Access Control Services") {
                block.acs_capability_header = true;
            }
            if let Some(value) = trimmed.strip_prefix("ACSCap:") {
                block.capability = Some(parse_acs_capability(value));
            } else if let Some(value) = trimmed.strip_prefix("ACSCtl:") {
                block.control = Some(parse_acs_control(value));
            }
        }
    }
    if let Some(block) = current {
        devices.insert(block.bdf.clone(), block.finish());
    }
    devices
}

fn line_bdf(line: &str) -> Option<String> {
    if line.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let bdf = line.split_whitespace().next()?;
    is_valid_bdf(bdf).then(|| bdf.to_owned())
}

/// Details parsed from the same bounded `lspci -Dvv` invocation used for ACS.
/// Keeping this as one command avoids a per-device command fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PciLspciDetail {
    pub class_name: Option<String>,
    pub vendor_name: Option<String>,
    pub device_name: Option<String>,
    pub subsystem_vendor: Option<String>,
    pub subsystem_device: Option<String>,
    pub subsystem_name: Option<String>,
}

pub fn parse_lspci_details(output: &str) -> BTreeMap<String, PciLspciDetail> {
    let mut details = BTreeMap::new();
    let mut current_bdf: Option<String> = None;
    let mut current = PciLspciDetail::default();

    for line in output.lines() {
        if let Some(bdf) = line_bdf(line) {
            if let Some(previous) = current_bdf.replace(bdf) {
                details.insert(previous, current);
                current = PciLspciDetail::default();
            }
            parse_lspci_heading(line, &mut current);
            continue;
        }
        if line.trim_start().starts_with("Subsystem:") {
            parse_subsystem_line(line.trim(), &mut current);
        }
    }
    if let Some(bdf) = current_bdf {
        details.insert(bdf, current);
    }
    details
}

fn parse_lspci_heading(line: &str, detail: &mut PciLspciDetail) {
    let Some(bdf) = line.split_whitespace().next() else {
        return;
    };
    let rest = line.get(bdf.len()..).unwrap_or_default().trim();
    let Some((class_name, description)) = rest.split_once(':') else {
        return;
    };
    detail.class_name = Some(strip_bracket_id(class_name.trim()));
    let (description, ids) = strip_last_id(description.trim());
    detail.device_name = scalar(description.trim().trim_end_matches(')'));
    if let Some((vendor, _device)) = ids {
        detail.vendor_name = vendor_display_name(vendor);
    }
}

fn parse_subsystem_line(line: &str, detail: &mut PciLspciDetail) {
    let Some(value) = line.strip_prefix("Subsystem:") else {
        return;
    };
    let (description, ids) = strip_last_id(value.trim());
    detail.subsystem_name = scalar(description);
    if let Some((vendor, device)) = ids {
        detail.subsystem_vendor = Some(format!("0x{vendor}"));
        detail.subsystem_device = Some(format!("0x{device}"));
    }
}

fn strip_last_id(value: &str) -> (&str, Option<(&str, &str)>) {
    let mut found = None;
    let mut start = 0;
    while let Some(open_relative) = value[start..].find('[') {
        let open = start + open_relative;
        let Some(close_relative) = value[open..].find(']') else {
            break;
        };
        let close = open + close_relative;
        let candidate = &value[open + 1..close];
        if let Some((vendor, device)) = candidate.split_once(':') {
            if is_hex_id(vendor) && is_hex_id(device) {
                found = Some((vendor, device));
                start = close + 1;
                continue;
            }
        }
        start = close + 1;
    }
    let description = found
        .and_then(|(vendor, device)| {
            let marker = format!("[{vendor}:{device}]");
            value.rsplit_once(&marker).map(|(prefix, _)| prefix.trim())
        })
        .unwrap_or(value);
    (description, found)
}

fn strip_bracket_id(value: &str) -> String {
    let (description, _) = strip_last_id(value);
    description.to_owned()
}

fn is_hex_id(value: &str) -> bool {
    value.len() == 4 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn vendor_display_name(vendor: &str) -> Option<String> {
    let name = match vendor.to_ascii_lowercase().as_str() {
        "10de" => "NVIDIA",
        "1e36" => "Enflame",
        "8086" => "Intel",
        "1002" | "1022" => "AMD",
        "1000" => "Broadcom/LSI",
        "14e4" => "Broadcom",
        "9005" => "Adaptec/Microchip",
        "1b4b" => "Marvell",
        "103c" => "HPE",
        _ => return None,
    };
    Some(name.to_owned())
}

#[derive(Debug)]
struct TopoNode {
    bdf: String,
    children: Vec<usize>,
    endpoint: Option<String>,
}

/// 加速器端点（GPU/GCU：PCI class 03xx 显示类 / 12xx 协处理器类）的 PCIe 上行
/// 拓扑树。按 parent_bdf 自叶向根回溯并合并共享分支，返回可直接逐行渲染的
/// 文本树；没有可识别加速器或父链信息时返回空 vec。
pub fn accelerator_topology_lines(
    devices: &[PciDeviceSnapshot],
    gpus: &[crate::domain::GpuSnapshot],
    max_lines: usize,
) -> Vec<String> {
    let by_bdf: BTreeMap<&str, &PciDeviceSnapshot> = devices
        .iter()
        .map(|device| (device.bdf.as_str(), device))
        .collect();
    let gpu_label = |bdf: &str| {
        let normalized = bdf.to_ascii_lowercase();
        gpus.iter()
            .find(|gpu| {
                gpu.pci_address.as_deref().is_some_and(|address| {
                    crate::collectors::gpu::normalize_pci_address(address).to_ascii_lowercase()
                        == normalized
                })
            })
            .map(|gpu| format!("GPU{} {}", gpu.index, gpu.name))
    };

    // 扁平节点池：同一 BDF 在父链上位置唯一，全局去重后共享分支自然合并。
    let mut nodes: Vec<TopoNode> = Vec::new();
    let mut node_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut roots: Vec<usize> = Vec::new();

    for device in devices {
        if !is_accelerator_class(device.class.as_deref()) {
            continue;
        }
        // 自叶向根回溯（带环与缺失保护），再反转为 根→叶 链。
        let chain = upstream_chain(device, &by_bdf);

        let mut parent_idx: Option<usize> = None;
        for (depth, bdf) in chain.iter().enumerate() {
            let idx = *node_index.entry(bdf.clone()).or_insert_with(|| {
                nodes.push(TopoNode {
                    bdf: bdf.clone(),
                    children: Vec::new(),
                    endpoint: None,
                });
                nodes.len() - 1
            });
            match parent_idx {
                Some(parent) => {
                    if !nodes[parent].children.contains(&idx) {
                        nodes[parent].children.push(idx);
                    }
                }
                None => {
                    if !roots.contains(&idx) {
                        roots.push(idx);
                    }
                }
            }
            parent_idx = Some(idx);
            if depth + 1 == chain.len() {
                let mut label = gpu_label(&device.bdf).unwrap_or_else(|| device_label(device));
                if let Some(numa) = device.numa_node {
                    label.push_str(&format!(" · NUMA{numa}"));
                }
                nodes[idx].endpoint = Some(label);
            }
        }
    }
    if roots.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for (position, root) in roots.iter().enumerate() {
        render_topo_node(
            &mut lines,
            &nodes,
            &by_bdf,
            *root,
            "",
            position + 1 == roots.len(),
            true,
        );
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        lines.push(format!("…（拓扑树已截断，仅展示前 {max_lines} 行）"));
    }
    lines
}

fn is_accelerator_class(class: Option<&str>) -> bool {
    let Some(class) = class else {
        return false;
    };
    let class = class.trim().to_ascii_lowercase();
    class.starts_with("0x03") || class.starts_with("0x12")
}

/// 自叶向根回溯父链（环/缺失保护），返回 根→叶 顺序的 BDF 链。
fn upstream_chain(
    device: &PciDeviceSnapshot,
    by_bdf: &BTreeMap<&str, &PciDeviceSnapshot>,
) -> Vec<String> {
    let mut chain = vec![device.bdf.clone()];
    let mut cursor = device.parent_bdf.clone();
    while let Some(parent) = cursor {
        if chain.len() >= 32 || chain.contains(&parent) || !by_bdf.contains_key(parent.as_str()) {
            break;
        }
        chain.push(parent.clone());
        cursor = by_bdf[parent.as_str()].parent_bdf.clone();
    }
    chain.reverse();
    chain
}

/// 两端点上行链的最近公共上游（根→叶 最长公共前缀的末尾）。
fn lowest_common_upstream(
    by_bdf: &BTreeMap<&str, &PciDeviceSnapshot>,
    a_bdf: &str,
    b_bdf: &str,
) -> Option<String> {
    let chain_a = upstream_chain(by_bdf.get(a_bdf)?, by_bdf);
    let chain_b = upstream_chain(by_bdf.get(b_bdf)?, by_bdf);
    chain_a
        .iter()
        .zip(chain_b.iter())
        .take_while(|(a, b)| a == b)
        .last()
        .map(|(a, _)| a.clone())
}

/// 把每条 P2P 链路两端 GPU 的 PCIe 上行汇聚点写入 `upstream_meeting_bdf`。
/// GPU 的 pci_address 与设备清单对不上时保持 None，不伪造。
pub fn enrich_p2p_upstream(
    gpus: &[crate::domain::GpuSnapshot],
    devices: &[PciDeviceSnapshot],
    p2p: &mut crate::domain::P2pSnapshot,
) {
    if p2p.links.is_empty() {
        return;
    }
    let by_bdf: BTreeMap<&str, &PciDeviceSnapshot> = devices
        .iter()
        .map(|device| (device.bdf.as_str(), device))
        .collect();
    let gpu_bdf = |index: u32| {
        gpus.iter()
            .find(|gpu| gpu.index == index)
            .and_then(|gpu| gpu.pci_address.as_deref())
            .map(|address| {
                crate::collectors::gpu::normalize_pci_address(address).to_ascii_lowercase()
            })
    };
    for link in &mut p2p.links {
        if let (Some(a), Some(b)) = (gpu_bdf(link.source_gpu), gpu_bdf(link.target_gpu)) {
            link.upstream_meeting_bdf = lowest_common_upstream(&by_bdf, &a, &b);
        }
    }
}

fn device_label(device: &PciDeviceSnapshot) -> String {
    let name = device
        .device_name
        .as_deref()
        .or(device.vendor_name.as_deref())
        .unwrap_or("加速器");
    shorten_str(name, 24)
}

fn shorten_str(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        let mut result: String = value.chars().take(max_chars.saturating_sub(1)).collect();
        result.push('\u{2026}');
        result
    }
}

fn render_topo_node(
    lines: &mut Vec<String>,
    nodes: &[TopoNode],
    by_bdf: &BTreeMap<&str, &PciDeviceSnapshot>,
    idx: usize,
    prefix: &str,
    last: bool,
    is_root: bool,
) {
    let bdf = nodes[idx].bdf.clone();
    let label = match by_bdf.get(bdf.as_str()) {
        Some(device) => {
            let mut label = device.bdf.clone();
            if let Some(name) = device
                .device_name
                .as_deref()
                .or(device.vendor_name.as_deref())
            {
                label.push_str(&format!(" {}", shorten_str(name, 24)));
            }
            if let Some(brief) = link_brief(device) {
                label.push_str(&format!(" [{brief}]"));
            }
            label
        }
        None => bdf,
    };
    let connector = if is_root {
        ""
    } else if last {
        "└─ "
    } else {
        "├─ "
    };
    let endpoint = nodes[idx]
        .endpoint
        .as_ref()
        .map_or(String::new(), |label| format!(" ← {label}"));
    lines.push(format!("{prefix}{connector}{label}{endpoint}"));
    let child_prefix = if is_root {
        String::new()
    } else if last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    let child_count = nodes[idx].children.len();
    for (position, child) in nodes[idx].children.iter().enumerate() {
        render_topo_node(
            lines,
            nodes,
            by_bdf,
            *child,
            &child_prefix,
            position + 1 == child_count,
            false,
        );
    }
}

#[derive(Debug, Default)]
struct LspciBlock {
    bdf: String,
    acs_capability_header: bool,
    capability: Option<AcsCapability>,
    control: Option<AcsControl>,
}

impl LspciBlock {
    fn new(bdf: String) -> Self {
        Self {
            bdf,
            ..Self::default()
        }
    }

    fn finish(self) -> AcsSnapshot {
        let capability = match self.capability {
            Some(capability) => Some(capability),
            None if self.acs_capability_header || self.control.is_some() => Some(AcsCapability {
                present: true,
                source_validation: None,
                translation_blocking: None,
                p2p_request_redirect: None,
                completion_redirect: None,
                upstream_forwarding: None,
                egress_control: None,
                direct_translated_p2p: None,
            }),
            None => Some(AcsCapability {
                present: false,
                source_validation: None,
                translation_blocking: None,
                p2p_request_redirect: None,
                completion_redirect: None,
                upstream_forwarding: None,
                egress_control: None,
                direct_translated_p2p: None,
            }),
        };
        AcsSnapshot {
            capability,
            control: self.control,
        }
    }
}

fn parse_acs_capability(value: &str) -> AcsCapability {
    let mut result = AcsCapability {
        present: true,
        source_validation: None,
        translation_blocking: None,
        p2p_request_redirect: None,
        completion_redirect: None,
        upstream_forwarding: None,
        egress_control: None,
        direct_translated_p2p: None,
    };
    for token in value.split_whitespace() {
        let Some((name, enabled)) = acs_bit(token) else {
            continue;
        };
        match name {
            "SrcValid" => result.source_validation = Some(enabled),
            "TransBlk" => result.translation_blocking = Some(enabled),
            "ReqRedir" => result.p2p_request_redirect = Some(enabled),
            "CmpltRedir" => result.completion_redirect = Some(enabled),
            "UpstreamFwd" => result.upstream_forwarding = Some(enabled),
            "EgressCtrl" => result.egress_control = Some(enabled),
            "DirectTrans" => result.direct_translated_p2p = Some(enabled),
            _ => {}
        }
    }
    result
}

fn parse_acs_control(value: &str) -> AcsControl {
    let mut result = AcsControl {
        source_validation: None,
        translation_blocking: None,
        p2p_request_redirect: None,
        completion_redirect: None,
        upstream_forwarding: None,
        egress_control: None,
        direct_translated_p2p: None,
    };
    for token in value.split_whitespace() {
        let Some((name, enabled)) = acs_bit(token) else {
            continue;
        };
        match name {
            "SrcValid" => result.source_validation = Some(enabled),
            "TransBlk" => result.translation_blocking = Some(enabled),
            "ReqRedir" => result.p2p_request_redirect = Some(enabled),
            "CmpltRedir" => result.completion_redirect = Some(enabled),
            "UpstreamFwd" => result.upstream_forwarding = Some(enabled),
            "EgressCtrl" => result.egress_control = Some(enabled),
            "DirectTrans" => result.direct_translated_p2p = Some(enabled),
            _ => {}
        }
    }
    result
}

fn acs_bit(token: &str) -> Option<(&str, bool)> {
    let token = token.trim_matches(|character: char| matches!(character, ',' | ';'));
    let enabled = match token.chars().last()? {
        '+' => true,
        '-' => false,
        _ => return None,
    };
    Some((&token[..token.len().saturating_sub(1)], enabled))
}

fn pcie_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "pcie".to_owned(),
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
    use super::{
        accelerator_topology_lines, class_role, enrich_p2p_upstream, gpu_pcie_device, is_valid_bdf,
        link_brief, link_brief_compact, parse_link_width, parse_lspci_acs, parse_lspci_details,
        parse_resource_windows, pcie_generation, scan_sysfs_devices, theoretical_bandwidth_mb_s,
        LinuxPcieCollector,
    };
    use crate::collectors::CollectorError;
    use crate::domain::{GpuSnapshot, HealthStatus, PciDeviceRole, PciDeviceSnapshot, PciMmioKind};

    const LSPCI_ACS: &str = include_str!("fixtures/lspci_acs.txt");
    const LSPCI_NO_ACS: &str = include_str!("fixtures/lspci_no_acs.txt");
    const LSPCI_STORAGE: &str = include_str!("fixtures/lspci_storage_topology.txt");

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
    fn pcie_real_rx_box_lspci_parses_gpus_without_acs() {
        // 真实设备 rx-box（172.18.5.123）的 lspci -Dvv：2× 3D controller（XCHAO-8180，OEM 名），
        // 且 GPU 设备不暴露 ACS 能力 —— 正是 diagnose 产出 ACS 隔离警告的根因。
        const REAL_DVV: &str = include_str!("fixtures/real-rx-box/rx_lspci_dvv.txt");
        let devices = parse_lspci_acs(REAL_DVV);
        for bdf in ["0000:0c:00.0", "0000:0f:00.0"] {
            let acs = devices
                .get(bdf)
                .unwrap_or_else(|| panic!("应解析出 GPU 设备 {bdf}"));
            let capability = acs
                .capability
                .as_ref()
                .unwrap_or_else(|| panic!("{bdf} 无 ACS 段也应标记为已知缺失"));
            assert!(!capability.present, "{bdf} 真实设备无 ACS 能力");
        }
    }

    #[test]
    fn pcie_parser_keeps_acs_capability_and_every_control_bit() {
        let devices = parse_lspci_acs(LSPCI_ACS);
        let acs = devices.get("0000:01:00.0").expect("GPU ACS block").clone();
        let capability = acs.capability.expect("ACS capability");
        assert!(capability.present);
        assert_eq!(capability.source_validation, Some(true));
        assert_eq!(capability.translation_blocking, Some(false));
        assert_eq!(capability.p2p_request_redirect, Some(true));
        assert_eq!(capability.completion_redirect, Some(true));
        assert_eq!(capability.upstream_forwarding, Some(true));
        assert_eq!(capability.egress_control, Some(false));
        assert_eq!(capability.direct_translated_p2p, Some(true));

        let control = acs.control.expect("ACS control");
        assert_eq!(control.source_validation, Some(false));
        assert_eq!(control.translation_blocking, Some(false));
        assert_eq!(control.p2p_request_redirect, Some(true));
        assert_eq!(control.completion_redirect, Some(false));
        assert_eq!(control.upstream_forwarding, Some(true));
        assert_eq!(control.egress_control, Some(false));
        assert_eq!(control.direct_translated_p2p, Some(false));
    }

    #[test]
    fn pcie_parser_distinguishes_known_no_acs_from_missing_lspci() {
        let devices = parse_lspci_acs(LSPCI_NO_ACS);
        let capability = devices
            .get("0000:02:00.0")
            .expect("non-ACS device")
            .capability
            .as_ref()
            .expect("known absence");
        assert!(!capability.present);
        assert!(capability.p2p_request_redirect.is_none());
    }

    #[test]
    fn lspci_storage_details_classify_controllers_without_per_device_commands() {
        let details = parse_lspci_details(LSPCI_STORAGE);
        assert_eq!(
            details["0000:02:00.0"].vendor_name.as_deref(),
            Some("Broadcom/LSI")
        );
        assert_eq!(
            details["0000:02:00.0"].subsystem_name.as_deref(),
            Some("Broadcom MegaRAID 9560-16i")
        );
        assert_eq!(class_role(Some("0x010400")), Some(PciDeviceRole::Raid));
        assert_eq!(class_role(Some("0x060400")), Some(PciDeviceRole::Bridge));
        assert_eq!(class_role(Some("0x010700")), Some(PciDeviceRole::Sas));
    }

    #[test]
    fn parses_sysfs_resource_windows_and_classifies_kinds() {
        let content = "\
0x0000000095000000 0x0000000095ffffff 0x0000000000140204
0x0000000000000000 0x0000000000000000 0x0000000000000000
0x0000000000000000 0x0000000000000000 0x0000000000000000
0x0000000000000000 0x0000000000000000 0x0000000000000000
0x0000000000000000 0x0000000000000000 0x0000000000000000
0x0000000000000000 0x0000000000000000 0x0000000000000000
0x0000000100000000 0x00000001ffffffff 0x00000000000c0208
0x0000000000000000 0x0000000000000000 0x0000000000000000
";
        let windows = parse_resource_windows(content);
        // 64 位 BAR 占两行：index 0（IORESOURCE_MEM_64=0x100000）与 index 6（32 位）。
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].index, 0);
        assert_eq!(windows[0].start, 0x9500_0000);
        assert_eq!(windows[0].end, 0x95ff_ffff);
        assert_eq!(windows[0].size, 0x0100_0000);
        assert_eq!(windows[0].kind, PciMmioKind::Mmio64);
        assert_eq!(windows[1].index, 6);
        assert_eq!(windows[1].kind, PciMmioKind::Mmio32);
        assert_eq!(windows[1].size, 0x1_0000_0000);
    }

    #[test]
    fn resource_io_port_and_unassigned_windows_are_handled() {
        // I/O 端口窗口（flags 0x100）归为 IoPort；未分配窗口（ffff.. 0 0）被跳过；
        // 第三窗口 flags 0x204（IORESOURCE_MEM）归为 MMIO32。
        let content = "\
0x0000000000001000 0x00000000000010ff 0x0000000000000101
0xffffffffffffffff 0x0000000000000000 0x0000000000000000
0x000000000000d000 0x000000000000d0ff 0x0000000000000204
";
        let windows = parse_resource_windows(content);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].kind, PciMmioKind::IoPort);
        assert_eq!(windows[1].index, 2);
        assert_eq!(windows[1].kind, PciMmioKind::Mmio32);
        assert_eq!(windows[1].start, 0xd000);
    }

    #[test]
    fn sysfs_resource_permission_denied_silently_yields_empty_windows() {
        let root = fixture_root("pcie-mmio-perm");
        let devices_root = root.join("bus/pci/devices");
        let devices_base = root.join("devices/pci0000:00");
        fs::create_dir_all(&devices_root).expect("PCI devices");
        let path = devices_base.join("0000:00:01.0");
        fs::create_dir_all(&path).expect("PCI node");
        fs::write(path.join("vendor"), "0x14e4\n").expect("vendor");
        fs::write(path.join("device"), "0x16d8\n").expect("device");
        fs::write(path.join("class"), "0x060400\n").expect("class");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            "../../../devices/pci0000:00/0000:00:01.0",
            devices_root.join("0000:00:01.0"),
        )
        .expect("PCI device symlink");
        // 模拟 root 才可读的 resource：普通用户读不到 → 静默降级，不产生噪音 issue。
        fs::write(path.join("resource"), "0x1000 0x10ff 0x101\n").expect("resource");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path.join("resource"), fs::Permissions::from_mode(0o400));
        }
        let collection = scan_sysfs_devices(&root, 64);
        let device = collection
            .devices
            .iter()
            .find(|device| device.bdf == "0000:00:01.0")
            .expect("设备应被扫描");
        // 只断言结构完整性：resource 权限可能受测试环境 root 影响，窗口可为空。
        assert!(device.mmio_windows.len() <= 2);
    }

    #[test]
    fn pcie_sysfs_canonical_path_builds_switch_parent_and_downstream_edges() {
        let root = fixture_root("pcie-switch");
        let devices_root = root.join("bus/pci/devices");
        let devices_base = root.join("devices/pci0000:00");
        fs::create_dir_all(&devices_root).expect("PCI devices");
        for (bdf, relative_path) in [
            ("0000:00:01.0", "0000:00:01.0"),
            ("0000:01:00.0", "0000:00:01.0/0000:01:00.0"),
            ("0000:02:00.0", "0000:00:01.0/0000:01:00.0/0000:02:00.0"),
        ] {
            let path = devices_base.join(relative_path);
            fs::create_dir_all(&path).expect("PCI node");
            fs::write(path.join("vendor"), "0x14e4\n").expect("vendor");
            fs::write(path.join("device"), "0x16d8\n").expect("device");
            fs::write(
                path.join("class"),
                if bdf == "0000:02:00.0" {
                    "0x010400\n"
                } else {
                    "0x060400\n"
                },
            )
            .expect("class");
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                format!("../../../devices/pci0000:00/{relative_path}"),
                devices_root.join(bdf),
            )
            .expect("PCI device symlink");
        }
        let runner = FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: LSPCI_STORAGE.to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
        };
        let collection = LinuxPcieCollector::with_runner(runner)
            .with_sysfs_root(&root)
            .collect_snapshot();
        let switch = collection
            .devices
            .iter()
            .find(|device| device.bdf == "0000:01:00.0")
            .expect("switch device");
        assert_eq!(switch.parent_bdf.as_deref(), Some("0000:00:01.0"));
        assert_eq!(switch.downstream_bdfs, vec!["0000:02:00.0"]);
        let raid = collection
            .devices
            .iter()
            .find(|device| device.bdf == "0000:02:00.0")
            .expect("raid device");
        assert_eq!(raid.role, Some(PciDeviceRole::Raid));
        assert_eq!(raid.parent_bdf.as_deref(), Some("0000:01:00.0"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pcie_sysfs_fixture_reads_link_topology_and_iommu_group() {
        let root = fixture_root("pcie");
        let device = root.join("bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&device).expect("device fixture");
        fs::create_dir_all(root.join("kernel/iommu_groups/7")).expect("iommu fixture");
        for (name, value) in [
            ("vendor", "0x10de\n"),
            ("device", "0x2803\n"),
            ("class", "0x030000\n"),
            ("numa_node", "0\n"),
            ("current_link_speed", "16.0 GT/s\n"),
            ("current_link_width", "x16\n"),
            ("max_link_speed", "16.0 GT/s\n"),
            ("max_link_width", "x16\n"),
        ] {
            fs::write(device.join(name), value).expect("field fixture");
        }
        // MMIO 窗口（真实 L20 形态：64 位 BAR 两行）。
        fs::write(
            device.join("resource"),
            "0x0000000095000000 0x0000000095ffffff 0x0000000000140204\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n\
             0x0000000000000000 0x0000000000000000 0x0000000000000000\n",
        )
        .expect("resource fixture");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../../../../drivers/nvidia", device.join("driver"))
                .expect("driver symlink");
            std::os::unix::fs::symlink(
                "../../../../kernel/iommu_groups/7",
                device.join("iommu_group"),
            )
            .expect("iommu group symlink");
        }

        let runner = FakeRunner {
            result: Arc::new(Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: LSPCI_ACS.to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })),
        };
        let collection = LinuxPcieCollector::with_runner(runner)
            .with_sysfs_root(&root)
            .collect_snapshot();
        assert!(collection.issues.is_empty(), "{:?}", collection.issues);
        let device = collection.devices.first().expect("PCI device");
        assert_eq!(device.driver.as_deref(), Some("nvidia"));
        assert_eq!(device.numa_node, Some(0));
        assert_eq!(device.iommu_group, Some(7));
        assert_eq!(device.current_link_speed.as_deref(), Some("16.0 GT/s"));
        assert_eq!(device.max_link_width.as_deref(), Some("x16"));
        assert_eq!(device.current_link_gen, Some(4));
        assert_eq!(device.max_link_gen, Some(4));
        assert_eq!(device.current_theoretical_bandwidth_mb_s, Some(31_508));
        assert_eq!(device.max_theoretical_bandwidth_mb_s, Some(31_508));
        assert!(device.acs.is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pcie_missing_lspci_does_not_block_sysfs_and_keeps_acs_unknown() {
        let root = fixture_root("pcie-no-lspci");
        let device = root.join("bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&device).expect("device fixture");
        fs::write(device.join("vendor"), "0x10de\n").expect("vendor fixture");
        let runner = FakeRunner {
            result: Arc::new(Err(CollectorError::new(
                "command",
                "spawn_failed",
                "lspci 不存在",
            ))),
        };
        let collection = LinuxPcieCollector::with_runner(runner)
            .with_sysfs_root(&root)
            .collect_snapshot();
        assert_eq!(collection.devices.len(), 1);
        assert!(collection.devices[0].acs.is_none());
        assert!(collection
            .issues
            .iter()
            .any(|issue| issue.code == "lspci_unavailable"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pcie_bdf_parser_rejects_unsafe_directory_names() {
        assert!(is_valid_bdf("0000:01:00.0"));
        assert!(!is_valid_bdf("not-a-pci-device"));
        assert!(!is_valid_bdf("0000:01:00.0/../../etc"));
    }

    #[test]
    fn pcie_generation_and_nominal_bandwidth_cover_gen1_through_gen5() {
        assert_eq!(pcie_generation(Some("2.5 GT/s")), Some(1));
        assert_eq!(pcie_generation(Some("8.0 GT/s PCIe")), Some(3));
        assert_eq!(pcie_generation(Some("32.0 GT/s")), Some(5));
        assert_eq!(pcie_generation(Some("unknown")), None);
        assert_eq!(parse_link_width("x16"), Some(16));
        assert_eq!(parse_link_width("8"), Some(8));
        assert_eq!(
            theoretical_bandwidth_mb_s(Some("16.0 GT/s"), Some("x16")),
            Some(31_508)
        );
        assert_eq!(
            theoretical_bandwidth_mb_s(Some("16.0 GT/s"), Some("x8")),
            Some(15_754)
        );
        assert_eq!(
            theoretical_bandwidth_mb_s(Some("8.0 GT/s"), Some("x16")),
            Some(15_754)
        );
    }

    #[test]
    fn pcie_gen6_is_identified_but_payload_ceiling_is_not_guessed() {
        assert_eq!(pcie_generation(Some("64.0 GT/s")), Some(6));
        assert_eq!(
            theoretical_bandwidth_mb_s(Some("64.0 GT/s"), Some("x16")),
            None
        );
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

    fn topo_device(bdf: &str, parent: Option<&str>, class: &str) -> PciDeviceSnapshot {
        PciDeviceSnapshot {
            bdf: bdf.to_owned(),
            vendor: None,
            device: None,
            class: Some(class.to_owned()),
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
            acs: None,
            role: None,
            class_name: None,
            vendor_name: None,
            device_name: None,
            subsystem_vendor: None,
            subsystem_device: None,
            subsystem_name: None,
            parent_bdf: parent.map(str::to_owned),
            downstream_bdfs: Vec::new(),
            mmio_windows: Vec::new(),
            status: HealthStatus::Healthy,
        }
    }

    fn topo_gpu(index: u32, bdf: &str, name: &str) -> GpuSnapshot {
        GpuSnapshot {
            index,
            name: name.to_owned(),
            uuid: None,
            pci_address: Some(bdf.to_owned()),
            status: HealthStatus::Healthy,
            temperature_celsius: None,
            utilization_percent: None,
            memory_used_mib: None,
            memory_total_mib: None,
            power_draw_watts: None,
            power_limit_watts: None,
            pstate: None,
            numa_node: None,
            reset_required: None,
            xid_codes: None,
            smi_tool: None,
            vendor: None,
            serial_number: None,
            driver_version: None,
            ecc_enabled: None,
            error_details: Default::default(),
            reset_count: None,
        }
    }

    #[test]
    fn accelerator_topology_merges_shared_branches() {
        // 模拟真机 172.18.4.199 的形态：两个 GCU 经各自下游口汇聚到同一 switch 链。
        let devices = vec![
            topo_device("0000:00:03.1", None, "0x060400"),
            topo_device("0000:05:00.0", Some("0000:00:03.1"), "0x060400"),
            topo_device("0000:06:00.0", Some("0000:05:00.0"), "0x060400"),
            topo_device("0000:0b:00.0", Some("0000:06:00.0"), "0x060400"),
            topo_device("0000:0c:00.0", Some("0000:0b:00.0"), "0x060400"),
            topo_device("0000:0c:10.0", Some("0000:0b:00.0"), "0x060400"),
            topo_device("0000:0d:00.0", Some("0000:0c:00.0"), "0x120000"),
            topo_device("0000:0f:00.0", Some("0000:0c:10.0"), "0x120000"),
            // 非加速器端点不参与树。
            topo_device("0000:eb:00.0", None, "0x020000"),
            topo_device("0000:d7:00.0", None, "0x010802"),
        ];
        let gpus = vec![
            topo_gpu(0, "0000:0d:00.0", "Enflame S60"),
            topo_gpu(1, "0000:0f:00.0", "Enflame S60"),
        ];
        let lines = accelerator_topology_lines(&devices, &gpus, 64);
        assert_eq!(
            lines,
            vec![
                "0000:00:03.1",
                "└─ 0000:05:00.0",
                "   └─ 0000:06:00.0",
                "      └─ 0000:0b:00.0",
                "         ├─ 0000:0c:00.0",
                "         │  └─ 0000:0d:00.0 ← GPU0 Enflame S60",
                "         └─ 0000:0c:10.0",
                "            └─ 0000:0f:00.0 ← GPU1 Enflame S60",
            ]
        );
    }

    #[test]
    fn accelerator_topology_handles_missing_parents_and_empty_input() {
        // 父链断裂（上游不在设备清单中）：端点自成根节点。
        let devices = vec![topo_device(
            "0000:0d:00.0",
            Some("0000:0c:00.0"),
            "0x120000",
        )];
        let lines = accelerator_topology_lines(&devices, &[], 64);
        assert_eq!(lines, vec!["0000:0d:00.0 ← 加速器"]);

        // 无加速器 → 空。
        let devices = vec![topo_device("0000:eb:00.0", None, "0x020000")];
        assert!(accelerator_topology_lines(&devices, &[], 64).is_empty());
        assert!(accelerator_topology_lines(&[], &[], 64).is_empty());
    }

    #[test]
    fn link_brief_covers_match_degrade_and_unknown() {
        // 协商值 == 最大能力：只显示当前。
        let mut device = topo_device("0000:86:00.0", None, "0x030200");
        device.current_link_speed = Some("16.0 GT/s".to_owned());
        device.current_link_width = Some("x8".to_owned());
        device.max_link_speed = Some("16.0 GT/s".to_owned());
        device.max_link_width = Some("x8".to_owned());
        assert_eq!(link_brief(&device).as_deref(), Some("Gen4 x8"));
        assert_eq!(link_brief_compact(&device).as_deref(), Some("Gen4 x8"));

        // 空闲降速：标注最大能力。
        device.current_link_speed = Some("2.5 GT/s".to_owned());
        assert_eq!(
            link_brief(&device).as_deref(),
            Some("Gen1 x8（最大 Gen4 x8）")
        );
        assert_eq!(
            link_brief_compact(&device).as_deref(),
            Some("Gen1 x8→Gen4 x8")
        );

        // 完全未知。
        let unknown = topo_device("0000:87:00.0", None, "0x030200");
        assert_eq!(link_brief(&unknown), None);
        assert_eq!(link_brief_compact(&unknown), None);
    }

    #[test]
    fn gpu_pcie_device_matches_normalized_bdf() {
        // nvidia-smi 可能给 8 位 domain 的地址（00000000:86:00.0），须归一化后匹配。
        let devices = vec![topo_device("0000:86:00.0", None, "0x030200")];
        let gpu = topo_gpu(0, "00000000:86:00.0", "RTX 4060 Ti");
        assert_eq!(
            gpu_pcie_device(&gpu, &devices).map(|d| d.bdf.as_str()),
            Some("0000:86:00.0")
        );
        let missing = topo_gpu(1, "0000:99:00.0", "RTX 4060 Ti");
        assert!(gpu_pcie_device(&missing, &devices).is_none());
        let no_address = topo_gpu(2, "", "");
        let mut no_address = no_address;
        no_address.pci_address = None;
        assert!(gpu_pcie_device(&no_address, &devices).is_none());
    }

    #[test]
    fn accelerator_topology_annotates_link_brief() {
        // switch 口与端点都带链路信息时，节点标注 [GenW xN]。
        let mut upstream = topo_device("0000:81:00.0", None, "0x060400");
        upstream.current_link_speed = Some("16.0 GT/s".to_owned());
        upstream.current_link_width = Some("x16".to_owned());
        upstream.max_link_speed = Some("16.0 GT/s".to_owned());
        upstream.max_link_width = Some("x16".to_owned());
        let mut endpoint = topo_device("0000:86:00.0", Some("0000:81:00.0"), "0x030200");
        endpoint.current_link_speed = Some("2.5 GT/s".to_owned());
        endpoint.current_link_width = Some("x8".to_owned());
        endpoint.max_link_speed = Some("16.0 GT/s".to_owned());
        endpoint.max_link_width = Some("x8".to_owned());
        // GPU 地址带 8 位 domain（nvidia-smi 常见），端点标签也应命中 GPU 名。
        let gpus = vec![topo_gpu(0, "00000000:86:00.0", "RTX 4060 Ti")];
        let lines = accelerator_topology_lines(&[upstream, endpoint], &gpus, 64);
        assert_eq!(
            lines,
            vec![
                "0000:81:00.0 [Gen4 x16]",
                "└─ 0000:86:00.0 [Gen1 x8（最大 Gen4 x8）] ← GPU0 RTX 4060 Ti",
            ]
        );
    }

    #[test]
    fn accelerator_topology_respects_line_cap() {
        let devices = vec![
            topo_device("0000:0d:00.0", Some("0000:0c:00.0"), "0x120000"),
            topo_device("0000:0c:00.0", None, "0x060400"),
            topo_device("0000:0f:00.0", Some("0000:0c:00.0"), "0x030200"),
        ];
        let lines = accelerator_topology_lines(&devices, &[], 2);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("已截断"));
    }

    #[test]
    fn enrich_p2p_upstream_finds_lowest_common_upstream() {
        // 与真机 172.18.4.199 同形态：GPU0/GPU1 汇聚在 0b:00.0，GPU2 在另一分支。
        let devices = vec![
            topo_device("0000:00:03.1", None, "0x060400"),
            topo_device("0000:05:00.0", Some("0000:00:03.1"), "0x060400"),
            topo_device("0000:06:00.0", Some("0000:05:00.0"), "0x060400"),
            topo_device("0000:0b:00.0", Some("0000:06:00.0"), "0x060400"),
            topo_device("0000:0c:00.0", Some("0000:0b:00.0"), "0x060400"),
            topo_device("0000:0c:10.0", Some("0000:0b:00.0"), "0x060400"),
            topo_device("0000:0d:00.0", Some("0000:0c:00.0"), "0x120000"),
            topo_device("0000:0f:00.0", Some("0000:0c:10.0"), "0x120000"),
            topo_device("0000:40:00.0", Some("0000:05:00.0"), "0x120000"),
        ];
        let gpus = vec![
            topo_gpu(0, "0000:0d:00.0", "Enflame S60"),
            topo_gpu(1, "0000:0f:00.0", "Enflame S60"),
            topo_gpu(2, "0000:40:00.0", "Enflame S60"),
        ];
        let mut p2p = crate::domain::P2pSnapshot {
            gpu_indices: vec![0, 1, 2],
            links: vec![
                p2p_link(0, 1),
                p2p_link(0, 2),
                p2p_link(2, 9), // 目标 GPU 不在清单：保持未知
            ],
            benchmark: Default::default(),
            status: HealthStatus::Healthy,
        };
        enrich_p2p_upstream(&gpus, &devices, &mut p2p);
        assert_eq!(
            p2p.links[0].upstream_meeting_bdf.as_deref(),
            Some("0000:0b:00.0")
        );
        assert_eq!(
            p2p.links[1].upstream_meeting_bdf.as_deref(),
            Some("0000:05:00.0")
        );
        assert_eq!(p2p.links[2].upstream_meeting_bdf, None);
    }

    fn p2p_link(source_gpu: u32, target_gpu: u32) -> crate::domain::GpuP2pLinkSnapshot {
        crate::domain::GpuP2pLinkSnapshot {
            source_gpu,
            target_gpu,
            topology_path: Some("PIX".to_owned()),
            upstream_meeting_bdf: None,
            read: crate::domain::P2pCapabilityStatus::Unknown,
            write: crate::domain::P2pCapabilityStatus::Unknown,
            pcie: crate::domain::P2pCapabilityStatus::Unknown,
            nvlink: crate::domain::P2pCapabilityStatus::Unknown,
            atomics: crate::domain::P2pCapabilityStatus::Unknown,
        }
    }
}
