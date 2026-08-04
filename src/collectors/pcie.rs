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
    PciDeviceSnapshot,
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
        status,
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
        class_role, is_valid_bdf, parse_link_width, parse_lspci_acs, parse_lspci_details,
        pcie_generation, theoretical_bandwidth_mb_s, LinuxPcieCollector,
    };
    use crate::collectors::CollectorError;
    use crate::domain::PciDeviceRole;

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
}
