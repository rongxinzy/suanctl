//! 出厂检测清单：从一次快照推导验收检查项（纯函数，便于测试）。
//!
//! 语义对齐"过了打勾、没过不打勾"：Pass 打勾；Fail 与 Unknown 都不打勾，
//! Unknown 在说明里注明是未采集/未运行，而不是验收不通过。

use crate::domain::{
    AcsSummary, DashboardSnapshot, DiskKind, DiskSnapshot, GpuSnapshot, HealthStatus, HostSnapshot,
    IommuSnapshot, LogSnapshot, P2pBenchmarkStatus, P2pSnapshot, PciDeviceSnapshot,
    PlatformSnapshot, StorageControllerKind, StorageFabricSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Unknown,
}

impl CheckStatus {
    pub const fn checked(self) -> bool {
        matches!(self, Self::Pass)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

impl FactoryCheck {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }

    fn unknown(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Unknown,
            detail: detail.into(),
        }
    }
}

/// 出厂检测全量检查项（顺序即展示顺序）。
pub fn factory_checks(snapshot: &DashboardSnapshot) -> Vec<FactoryCheck> {
    let platform = snapshot.platform.as_ref();
    vec![
        operating_system(&snapshot.host),
        kernel_version(&snapshot.host),
        cpu(&snapshot.host),
        memory(&snapshot.host),
        disks(&snapshot.host),
        raid(platform.and_then(|p| p.storage.as_ref())),
        network_config(&snapshot.host),
        gpu_count(&snapshot.gpus),
        gpu_health(&snapshot.gpus),
        gpu_ecc(&snapshot.gpus),
        pcie_link_width(&snapshot.gpus, platform.map(|p| p.pci_devices.as_slice())),
        driver(&snapshot.gpus, platform),
        p2p_capability(platform.and_then(|p| p.p2p.as_ref())),
        p2p_benchmark(platform.and_then(|p| p.p2p.as_ref())),
        iommu(platform.map(|p| &p.iommu)),
        acs(platform.and_then(|p| p.acs_summary.as_ref())),
        system_logs(snapshot.logs.as_ref()),
        diagnosis_findings(snapshot),
    ]
}

/// 通过数 / 总项数（Unknown 不算通过）。
pub fn checked_count(checks: &[FactoryCheck]) -> (usize, usize) {
    (
        checks.iter().filter(|check| check.status.checked()).count(),
        checks.len(),
    )
}

fn operating_system(host: &HostSnapshot) -> FactoryCheck {
    let os = host.os.trim();
    if os.is_empty() || os == "未知系统" {
        return FactoryCheck::unknown("操作系统", "未采集到系统信息");
    }
    match host.architecture.as_deref() {
        Some(arch) => FactoryCheck::pass("操作系统", format!("{os}（{arch}）")),
        None => FactoryCheck::pass("操作系统", os.to_owned()),
    }
}

fn kernel_version(host: &HostSnapshot) -> FactoryCheck {
    match host.kernel_version.as_deref() {
        Some(kernel) => FactoryCheck::pass("内核版本", kernel.to_owned()),
        None => FactoryCheck::unknown("内核版本", "未采集到内核版本"),
    }
}

fn cpu(host: &HostSnapshot) -> FactoryCheck {
    match (&host.cpu_model, host.logical_cpu_count) {
        (Some(model), Some(count)) => {
            FactoryCheck::pass("CPU", format!("{model}（{count} 逻辑核）"))
        }
        (Some(model), None) => FactoryCheck::pass("CPU", model.clone()),
        (None, Some(count)) => FactoryCheck::pass("CPU", format!("{count} 逻辑核")),
        (None, None) => FactoryCheck::unknown("CPU", "未采集到 CPU 信息"),
    }
}

fn memory(host: &HostSnapshot) -> FactoryCheck {
    let Some(total_mib) = host.memory_total_mib else {
        return FactoryCheck::unknown("内存", "未采集到内存信息");
    };
    let total = format!("{:.0} GiB", total_mib as f64 / 1024.0);
    match &host.memory_modules {
        Some(modules) => FactoryCheck::pass("内存", format!("{total} · {modules}")),
        None => FactoryCheck::pass("内存", total),
    }
}

/// 十进制容量（lsblk 原始字节数）：480 GB / 3.5 TB。
fn format_capacity(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000_000_000_000;
    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.0} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    }
}

fn disk_label(disk: &DiskSnapshot) -> String {
    let mut label = disk.name.clone();
    let mut extra: Vec<String> = Vec::new();
    if let Some(model) = &disk.model {
        extra.push(model.clone());
    }
    if let Some(size) = disk.size_bytes {
        extra.push(format_capacity(size));
    }
    if !extra.is_empty() {
        label.push_str(&format!("（{}）", extra.join(" ")));
    }
    label
}

fn disks(host: &HostSnapshot) -> FactoryCheck {
    if host.disks.is_empty() {
        return FactoryCheck::fail("硬盘", "未发现硬盘");
    }
    let system: Vec<&DiskSnapshot> = host
        .disks
        .iter()
        .filter(|disk| disk.kind == DiskKind::System)
        .collect();
    let data: Vec<&DiskSnapshot> = host
        .disks
        .iter()
        .filter(|disk| disk.kind == DiskKind::Data)
        .collect();
    if system.is_empty() {
        return FactoryCheck::fail("硬盘", "未识别系统盘");
    }
    let dirty: Vec<String> = data
        .iter()
        .filter(|disk| disk.blank == Some(false))
        .map(|disk| disk.name.clone())
        .collect();
    if !dirty.is_empty() {
        return FactoryCheck::fail(
            "硬盘",
            format!("数据盘 {} 已有分区或文件系统，非干净盘", dirty.join("、")),
        );
    }
    let total_bytes: u64 = host.disks.iter().filter_map(|disk| disk.size_bytes).sum();
    let data_summary = if data.is_empty() {
        "无数据盘".to_owned()
    } else {
        format!("数据盘 {} 块（干净）", data.len())
    };
    FactoryCheck::pass(
        "硬盘",
        format!(
            "{} 块 · 共 {} · 系统盘 {} · {}",
            host.disks.len(),
            format_capacity(total_bytes),
            system
                .iter()
                .map(|disk| disk_label(disk))
                .collect::<Vec<_>>()
                .join("、"),
            data_summary
        ),
    )
}

fn raid(storage: Option<&StorageFabricSnapshot>) -> FactoryCheck {
    let Some(storage) = storage else {
        return FactoryCheck::unknown("RAID", "存储未采集");
    };
    let raid_controllers: Vec<&str> = storage
        .controllers
        .iter()
        .filter(|controller| controller.kind == StorageControllerKind::Raid)
        .map(|controller| {
            controller
                .model
                .as_deref()
                .unwrap_or(controller.id.as_str())
        })
        .collect();
    if raid_controllers.is_empty() && storage.software_raid.is_empty() {
        return FactoryCheck::pass("RAID", "无 RAID（磁盘直通）");
    }
    let degraded: Vec<String> = storage
        .software_raid
        .iter()
        .filter(|array| {
            array.degraded.is_some_and(|count| count > 0)
                || array
                    .array_state
                    .as_deref()
                    .is_some_and(|state| !matches!(state, "clean" | "active" | "active-idle"))
        })
        .map(|array| array.name.clone())
        .collect();
    if !degraded.is_empty() {
        return FactoryCheck::fail("RAID", format!("阵列降级：{}", degraded.join("、")));
    }
    let mut parts: Vec<String> = Vec::new();
    if !raid_controllers.is_empty() {
        parts.push(format!("硬 RAID：{}", raid_controllers.join("、")));
    }
    if !storage.software_raid.is_empty() {
        let arrays: Vec<String> = storage
            .software_raid
            .iter()
            .map(|array| {
                format!(
                    "{}（{}，正常）",
                    array.name,
                    array.level.as_deref().unwrap_or("未知级别")
                )
            })
            .collect();
        parts.push(format!("软 RAID：{}", arrays.join("、")));
    }
    FactoryCheck::pass("RAID", parts.join(" · "))
}

fn network_config(host: &HostSnapshot) -> FactoryCheck {
    if host.interfaces.is_empty() {
        return FactoryCheck::unknown("网络配置", "未采集到网卡信息");
    }
    // 优先展示 netplan 管理的物理口；docker/veth/flannel 等虚拟口是噪音。
    let managed: Vec<&crate::domain::NetInterfaceSnapshot> = host
        .interfaces
        .iter()
        .filter(|iface| iface.config_mode.is_some())
        .collect();
    let shown: &[&crate::domain::NetInterfaceSnapshot] = if managed.is_empty() {
        &host.interfaces.iter().collect::<Vec<_>>()
    } else {
        &managed
    };
    if shown.iter().all(|iface| iface.addresses.is_empty()) {
        return FactoryCheck::fail("网络配置", "所有网卡均无 IP 地址");
    }
    let parts: Vec<String> = shown
        .iter()
        .map(|iface| {
            if iface.addresses.is_empty() {
                return format!("{} 无地址", iface.name);
            }
            let mode = match iface.config_mode.as_deref() {
                Some("static") => "静态",
                Some("dhcp") => "DHCP",
                _ => "未知配置",
            };
            format!("{} {}（{}）", iface.name, iface.addresses.join(" "), mode)
        })
        .collect();
    FactoryCheck::pass("网络配置", parts.join(" · "))
}

fn iommu(iommu: Option<&IommuSnapshot>) -> FactoryCheck {
    let Some(iommu) = iommu else {
        return FactoryCheck::unknown("IOMMU", "平台未采集");
    };
    match iommu.effective {
        Some(true) => {
            let mode = iommu.effective_mode.as_deref().unwrap_or("未知模式");
            match iommu.group_count {
                Some(groups) => {
                    FactoryCheck::pass("IOMMU", format!("已启用（{mode}，{groups} 组）"))
                }
                None => FactoryCheck::pass("IOMMU", format!("已启用（{mode}）")),
            }
        }
        // 信息项：未启用不算验收失败（部分机型按需关闭）。
        Some(false) => FactoryCheck::pass("IOMMU", "未启用"),
        None => FactoryCheck::unknown("IOMMU", "未采集到 IOMMU 状态"),
    }
}

fn acs(summary: Option<&AcsSummary>) -> FactoryCheck {
    let Some(summary) = summary else {
        return FactoryCheck::unknown("ACS", "未采集 ACS 信息");
    };
    FactoryCheck::pass(
        "ACS",
        format!(
            "支持 {} · 启用 {} · 全关 {}",
            summary.supported, summary.enabled, summary.disabled
        ),
    )
}

fn gpu_count(gpus: &[GpuSnapshot]) -> FactoryCheck {
    if gpus.is_empty() {
        FactoryCheck::fail("GPU 数量", "未发现 GPU")
    } else {
        FactoryCheck::pass("GPU 数量", format!("发现 {} 张", gpus.len()))
    }
}

fn gpu_health(gpus: &[GpuSnapshot]) -> FactoryCheck {
    if gpus.is_empty() {
        return FactoryCheck::fail("GPU 健康", "未发现 GPU");
    }
    let unhealthy: Vec<String> = gpus
        .iter()
        .filter(|gpu| gpu.status != HealthStatus::Healthy)
        .map(|gpu| format!("GPU{}（{}）", gpu.index, gpu.status.label()))
        .collect();
    if unhealthy.is_empty() {
        FactoryCheck::pass("GPU 健康", format!("{} 张全部正常", gpus.len()))
    } else {
        FactoryCheck::fail("GPU 健康", unhealthy.join("、"))
    }
}

fn gpu_ecc(gpus: &[GpuSnapshot]) -> FactoryCheck {
    if gpus.is_empty() {
        return FactoryCheck::fail("ECC 校验", "未发现 GPU");
    }
    if gpus.iter().all(|gpu| gpu.ecc_enabled == Some(true)) {
        return FactoryCheck::pass("ECC 校验", "全部启用");
    }
    let disabled: Vec<String> = gpus
        .iter()
        .filter(|gpu| gpu.ecc_enabled == Some(false))
        .map(|gpu| format!("GPU{}", gpu.index))
        .collect();
    if disabled.is_empty() {
        FactoryCheck::unknown("ECC 校验", "未采集到 ECC 状态")
    } else {
        FactoryCheck::fail("ECC 校验", format!("{} 未启用", disabled.join("、")))
    }
}

fn pcie_link_width(gpus: &[GpuSnapshot], devices: Option<&[PciDeviceSnapshot]>) -> FactoryCheck {
    if gpus.is_empty() {
        return FactoryCheck::fail("PCIe 链路宽度", "未发现 GPU");
    }
    let Some(devices) = devices else {
        return FactoryCheck::unknown("PCIe 链路宽度", "平台未采集");
    };
    let mut degraded: Vec<String> = Vec::new();
    let mut unmeasured = 0_usize;
    for gpu in gpus {
        let Some(device) = crate::collectors::pcie::gpu_pcie_device(gpu, devices) else {
            unmeasured += 1;
            continue;
        };
        match (
            crate::collectors::pcie::parse_link_width(
                device.current_link_width.as_deref().unwrap_or(""),
            ),
            crate::collectors::pcie::parse_link_width(
                device.max_link_width.as_deref().unwrap_or(""),
            ),
        ) {
            (Some(current), Some(maximum)) if current < maximum => degraded.push(format!(
                "GPU{} x{}（最大 x{}）",
                gpu.index, current, maximum
            )),
            (Some(_), Some(_)) => {}
            _ => unmeasured += 1,
        }
    }
    if !degraded.is_empty() {
        return FactoryCheck::fail("PCIe 链路宽度", format!("降宽：{}", degraded.join("、")));
    }
    if unmeasured > 0 {
        return FactoryCheck::unknown(
            "PCIe 链路宽度",
            format!("{unmeasured} 张 GPU 链路宽度未采集"),
        );
    }
    FactoryCheck::pass("PCIe 链路宽度", "全部满宽")
}

fn driver(gpus: &[GpuSnapshot], platform: Option<&PlatformSnapshot>) -> FactoryCheck {
    let Some(platform) = platform else {
        return FactoryCheck::unknown("驱动版本", "平台未采集");
    };
    let nvidia = &platform.nvidia_driver;
    match nvidia.version_match {
        Some(true) => {
            let kernel = nvidia.kernel_module_version.as_deref().unwrap_or("未知");
            let user = nvidia
                .nvidia_smi_driver_version
                .as_deref()
                .unwrap_or("未知");
            FactoryCheck::pass("驱动版本", format!("内核 {kernel} · 用户态 {user} 一致"))
        }
        Some(false) => FactoryCheck::fail("驱动版本", "内核模块与用户态驱动版本不一致"),
        None => {
            let versions: Vec<&str> = gpus
                .iter()
                .filter_map(|gpu| gpu.driver_version.as_deref())
                .collect();
            if !gpus.is_empty() && versions.len() == gpus.len() {
                FactoryCheck::pass("驱动版本", format!("驱动 {}", versions[0]))
            } else {
                FactoryCheck::unknown("驱动版本", "未采集到驱动版本")
            }
        }
    }
}

fn p2p_capability(p2p: Option<&P2pSnapshot>) -> FactoryCheck {
    let Some(p2p) = p2p else {
        return FactoryCheck::unknown("P2P 互联能力", "未采集 P2P 能力");
    };
    if p2p.links.is_empty() {
        return FactoryCheck::unknown("P2P 互联能力", "无 P2P 链路信息");
    }
    let blocked = p2p
        .links
        .iter()
        .filter(|link| !(link.read.is_supported() && link.write.is_supported()))
        .count();
    if blocked == 0 {
        FactoryCheck::pass(
            "P2P 互联能力",
            format!("{} 条链路全部支持读写", p2p.links.len()),
        )
    } else {
        FactoryCheck::fail(
            "P2P 互联能力",
            format!("{}/{} 条链路不支持读写", blocked, p2p.links.len()),
        )
    }
}

fn p2p_benchmark(p2p: Option<&P2pSnapshot>) -> FactoryCheck {
    let Some(p2p) = p2p else {
        return FactoryCheck::unknown("P2P 实测带宽", "未采集 P2P 信息");
    };
    match p2p.benchmark.status {
        P2pBenchmarkStatus::Succeeded => FactoryCheck::pass(
            "P2P 实测带宽",
            format!(
                "{} · {} 条链路已实测",
                p2p.benchmark.tool,
                p2p.benchmark.measurements.len()
            ),
        ),
        P2pBenchmarkStatus::Failed => FactoryCheck::fail(
            "P2P 实测带宽",
            p2p.benchmark
                .message
                .clone()
                .unwrap_or_else(|| "实测失败".to_owned()),
        ),
        P2pBenchmarkStatus::NotRequested => {
            FactoryCheck::unknown("P2P 实测带宽", "未运行（TUI 按 b 触发实测）")
        }
        P2pBenchmarkStatus::Unavailable => FactoryCheck::unknown("P2P 实测带宽", "实测工具不可用"),
    }
}

fn system_logs(logs: Option<&LogSnapshot>) -> FactoryCheck {
    let Some(logs) = logs else {
        return FactoryCheck::unknown("系统日志", "日志未采集");
    };
    let summarize = |severity: HealthStatus| -> Vec<String> {
        logs.matches
            .iter()
            .filter(|hit| hit.severity == severity)
            .map(|hit| format!("{} ×{}", hit.pattern, hit.count))
            .collect()
    };
    let critical = summarize(HealthStatus::Critical);
    if !critical.is_empty() {
        return FactoryCheck::fail("系统日志", format!("Critical：{}", critical.join("、")));
    }
    let warning = summarize(HealthStatus::Warning);
    if !warning.is_empty() {
        return FactoryCheck::fail("系统日志", format!("Warning：{}", warning.join("、")));
    }
    FactoryCheck::pass("系统日志", "无异常模式命中")
}

fn diagnosis_findings(snapshot: &DashboardSnapshot) -> FactoryCheck {
    let critical = snapshot
        .findings
        .iter()
        .filter(|finding| finding.status == HealthStatus::Critical)
        .count();
    let warning = snapshot
        .findings
        .iter()
        .filter(|finding| finding.status == HealthStatus::Warning)
        .count();
    if critical == 0 && warning == 0 {
        return FactoryCheck::pass(
            "诊断发现",
            if snapshot.findings.is_empty() {
                "无诊断发现".to_owned()
            } else {
                format!(
                    "无 Critical/Warning（共 {} 条提示）",
                    snapshot.findings.len()
                )
            },
        );
    }
    let first = snapshot
        .findings
        .iter()
        .find(|finding| {
            matches!(
                finding.status,
                HealthStatus::Critical | HealthStatus::Warning
            )
        })
        .map(|finding| finding.summary.clone())
        .unwrap_or_default();
    FactoryCheck::fail(
        "诊断发现",
        format!("Critical ×{critical} · Warning ×{warning}：{first}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        DriveVisibility, GpuP2pLinkSnapshot, LogPatternMatch, NetInterfaceSnapshot,
        P2pBenchmarkSnapshot, P2pCapabilityStatus, SoftwareRaidSnapshot, StorageControllerSnapshot,
    };

    fn host() -> HostSnapshot {
        HostSnapshot {
            hostname: "host".to_owned(),
            os: "Ubuntu 24.04.3 LTS".to_owned(),
            kernel_version: Some("6.8.0-90-generic".to_owned()),
            architecture: Some("x86_64".to_owned()),
            cpu_model: Some("Hygon C86 3350  8-core Processor".to_owned()),
            logical_cpu_count: Some(16),
            load_1m: None,
            memory_used_mib: None,
            memory_total_mib: Some(64037),
            memory_modules: Some("2×32GB DDR5 4800MT/s".to_owned()),
            disks: Vec::new(),
            interfaces: Vec::new(),
            status: HealthStatus::Healthy,
            cpu_status: HealthStatus::Healthy,
            memory_status: HealthStatus::Healthy,
        }
    }

    fn disk(name: &str, kind: DiskKind, blank: Option<bool>) -> DiskSnapshot {
        DiskSnapshot {
            name: name.to_owned(),
            model: None,
            size_bytes: Some(480_000_000_000),
            kind,
            fstype: None,
            mountpoints: Vec::new(),
            blank,
        }
    }

    fn demo_gpus() -> Vec<GpuSnapshot> {
        let mut gpus = crate::collectors::demo::snapshot().gpus;
        for gpu in &mut gpus {
            gpu.status = HealthStatus::Healthy;
        }
        gpus
    }

    fn width_device(bdf: &str, current: &str, maximum: &str) -> PciDeviceSnapshot {
        PciDeviceSnapshot {
            bdf: bdf.to_owned(),
            vendor: None,
            device: None,
            class: Some("0x030200".to_owned()),
            driver: None,
            numa_node: None,
            iommu_group: None,
            current_link_speed: None,
            current_link_width: Some(current.to_owned()),
            current_link_gen: None,
            current_theoretical_bandwidth_mb_s: None,
            max_link_speed: None,
            max_link_width: Some(maximum.to_owned()),
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
            parent_bdf: None,
            downstream_bdfs: Vec::new(),
            mmio_windows: Vec::new(),
            status: HealthStatus::Unknown,
        }
    }

    fn p2p_with_link(supported: bool) -> P2pSnapshot {
        let capability = if supported {
            P2pCapabilityStatus::Supported
        } else {
            P2pCapabilityStatus::NotSupported
        };
        P2pSnapshot {
            gpu_indices: vec![0, 1],
            links: vec![GpuP2pLinkSnapshot {
                source_gpu: 0,
                target_gpu: 1,
                topology_path: Some("PIX".to_owned()),
                upstream_meeting_bdf: None,
                read: capability,
                write: capability,
                pcie: capability,
                nvlink: P2pCapabilityStatus::Unknown,
                atomics: P2pCapabilityStatus::Unknown,
            }],
            benchmark: P2pBenchmarkSnapshot::default(),
            status: HealthStatus::Healthy,
        }
    }

    #[test]
    fn host_info_checks_cover_os_kernel_cpu_memory() {
        let host = host();
        let os = operating_system(&host);
        assert_eq!(os.status, CheckStatus::Pass);
        assert!(os.detail.contains("Ubuntu 24.04.3 LTS"));
        assert!(os.detail.contains("x86_64"));

        let kernel = kernel_version(&host);
        assert_eq!(kernel.status, CheckStatus::Pass);
        assert!(kernel.detail.contains("6.8.0-90-generic"));

        let cpu_check = cpu(&host);
        assert_eq!(cpu_check.status, CheckStatus::Pass);
        assert!(cpu_check.detail.contains("Hygon C86 3350"));
        assert!(cpu_check.detail.contains("16 逻辑核"));

        let memory_check = memory(&host);
        assert_eq!(memory_check.status, CheckStatus::Pass);
        assert!(memory_check.detail.contains("2×32GB DDR5 4800MT/s"));

        let mut bare = host.clone();
        bare.cpu_model = None;
        assert!(cpu(&bare).detail.contains("16 逻辑核"));
        bare.logical_cpu_count = None;
        assert_eq!(cpu(&bare).status, CheckStatus::Unknown);

        let mut no_mem = host.clone();
        no_mem.memory_total_mib = None;
        assert_eq!(memory(&no_mem).status, CheckStatus::Unknown);

        let mut no_os = host.clone();
        no_os.os = "未知系统".to_owned();
        assert_eq!(operating_system(&no_os).status, CheckStatus::Unknown);
    }

    #[test]
    fn disk_checks_cover_capacity_system_and_cleanliness() {
        let mut host = host();
        assert_eq!(disks(&host).status, CheckStatus::Fail);

        host.disks = vec![disk("sda", DiskKind::System, Some(false))];
        let check = disks(&host);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("系统盘 sda"));
        assert!(check.detail.contains("480 GB"));

        host.disks = vec![
            disk("sda", DiskKind::System, Some(false)),
            disk("sdb", DiskKind::Data, Some(true)),
            disk("sdc", DiskKind::Data, Some(true)),
        ];
        let check = disks(&host);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("3 块"));
        assert!(check.detail.contains("数据盘 2 块（干净）"));

        host.disks[2].blank = Some(false);
        let check = disks(&host);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("sdc"));

        host.disks = vec![disk("sdb", DiskKind::Data, Some(true))];
        assert_eq!(disks(&host).status, CheckStatus::Fail);

        assert_eq!(format_capacity(3_840_755_982_336), "3.8 TB");
        assert_eq!(format_capacity(480_000_000_000), "480 GB");
    }

    #[test]
    fn raid_checks_cover_absent_healthy_and_degraded() {
        assert_eq!(raid(None).status, CheckStatus::Unknown);

        let mut storage = StorageFabricSnapshot {
            controllers: Vec::new(),
            scsi_hosts: Vec::new(),
            sas_phys: Vec::new(),
            software_raid: Vec::new(),
            vendor_clis: Vec::new(),
            issues: Vec::new(),
            status: HealthStatus::Healthy,
        };
        let check = raid(Some(&storage));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("无 RAID"));

        storage.software_raid.push(SoftwareRaidSnapshot {
            name: "md0".to_owned(),
            level: Some("raid1".to_owned()),
            raid_disks: Some(2),
            degraded: Some(0),
            sync_action: Some("idle".to_owned()),
            array_state: Some("clean".to_owned()),
            status: HealthStatus::Healthy,
        });
        let check = raid(Some(&storage));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("md0（raid1，正常）"));

        storage.software_raid[0].degraded = Some(1);
        let check = raid(Some(&storage));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("md0"));

        let mut hw = StorageFabricSnapshot {
            controllers: vec![StorageControllerSnapshot {
                id: "0000:81:00.0".to_owned(),
                bdf: None,
                kind: StorageControllerKind::Raid,
                vendor: None,
                model: Some("MegaRAID SAS-3 3008".to_owned()),
                driver: None,
                firmware_version: None,
                virtual_drive_count: None,
                physical_drive_count: None,
                physical_drive_visibility: DriveVisibility::Visible,
                evidence: Vec::new(),
                source: Vec::new(),
                status: HealthStatus::Healthy,
            }],
            scsi_hosts: Vec::new(),
            sas_phys: Vec::new(),
            software_raid: Vec::new(),
            vendor_clis: Vec::new(),
            issues: Vec::new(),
            status: HealthStatus::Healthy,
        };
        let check = raid(Some(&hw));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("MegaRAID SAS-3 3008"));
        hw.controllers.clear();
    }

    #[test]
    fn network_checks_cover_addresses_and_modes() {
        let mut host = host();
        assert_eq!(network_config(&host).status, CheckStatus::Unknown);

        host.interfaces = vec![NetInterfaceSnapshot {
            name: "eno1".to_owned(),
            mac: None,
            state: "UP".to_owned(),
            addresses: Vec::new(),
            config_mode: None,
        }];
        assert_eq!(network_config(&host).status, CheckStatus::Fail);

        host.interfaces = vec![
            NetInterfaceSnapshot {
                name: "eno1".to_owned(),
                mac: None,
                state: "UP".to_owned(),
                addresses: vec!["172.18.5.123/24".to_owned()],
                config_mode: Some("static".to_owned()),
            },
            NetInterfaceSnapshot {
                name: "eno2".to_owned(),
                mac: None,
                state: "DOWN".to_owned(),
                addresses: Vec::new(),
                config_mode: Some("dhcp".to_owned()),
            },
        ];
        let check = network_config(&host);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("eno1 172.18.5.123/24（静态）"));
        assert!(check.detail.contains("eno2 无地址"));

        // 有 netplan 管理口时，docker/veth 等虚拟口不进入展示与判定。
        host.interfaces.push(NetInterfaceSnapshot {
            name: "docker0".to_owned(),
            mac: None,
            state: "UP".to_owned(),
            addresses: vec!["172.17.0.1/16".to_owned()],
            config_mode: None,
        });
        let check = network_config(&host);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(!check.detail.contains("docker0"));
    }

    #[test]
    fn iommu_and_acs_checks() {
        assert_eq!(iommu(None).status, CheckStatus::Unknown);

        let mut snap = IommuSnapshot {
            requested: None,
            effective: Some(true),
            effective_mode: Some("pt".to_owned()),
            groups_present: Some(true),
            group_count: Some(48),
            acs_override: None,
            status: HealthStatus::Healthy,
        };
        let check = iommu(Some(&snap));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("pt"));
        assert!(check.detail.contains("48 组"));

        snap.effective = Some(false);
        let check = iommu(Some(&snap));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("未启用"));

        snap.effective = None;
        assert_eq!(iommu(Some(&snap)).status, CheckStatus::Unknown);

        assert_eq!(acs(None).status, CheckStatus::Unknown);
        let check = acs(Some(&AcsSummary {
            supported: 20,
            enabled: 4,
            disabled: 16,
        }));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("支持 20"));
        assert!(check.detail.contains("全关 16"));
    }

    #[test]
    fn factory_checks_covers_all_items_in_order() {
        let snapshot = crate::collectors::demo::snapshot();
        let checks = factory_checks(&snapshot);
        let names: Vec<&str> = checks.iter().map(|check| check.name).collect();
        assert_eq!(
            names,
            vec![
                "操作系统",
                "内核版本",
                "CPU",
                "内存",
                "硬盘",
                "RAID",
                "网络配置",
                "GPU 数量",
                "GPU 健康",
                "ECC 校验",
                "PCIe 链路宽度",
                "驱动版本",
                "P2P 互联能力",
                "P2P 实测带宽",
                "IOMMU",
                "ACS",
                "系统日志",
                "诊断发现",
            ]
        );
    }

    #[test]
    fn gpu_checks_cover_count_health_ecc() {
        let mut gpus = demo_gpus();
        assert_eq!(gpu_count(&gpus).status, CheckStatus::Pass);
        assert_eq!(gpu_health(&gpus).status, CheckStatus::Pass);
        assert_eq!(gpu_ecc(&gpus).status, CheckStatus::Unknown);

        gpus[0].status = HealthStatus::Warning;
        assert_eq!(gpu_health(&gpus).status, CheckStatus::Fail);
        gpus[0].status = HealthStatus::Healthy;
        for gpu in &mut gpus {
            gpu.ecc_enabled = Some(true);
        }
        assert_eq!(gpu_ecc(&gpus).status, CheckStatus::Pass);
        gpus[1].ecc_enabled = Some(false);
        assert_eq!(gpu_ecc(&gpus).status, CheckStatus::Fail);

        assert_eq!(gpu_count(&[]).status, CheckStatus::Fail);
    }

    #[test]
    fn pcie_width_flags_degraded_and_unknown() {
        let gpus = demo_gpus();
        let bdfs: Vec<String> = gpus
            .iter()
            .map(|gpu| {
                crate::collectors::gpu::normalize_pci_address(
                    gpu.pci_address.as_deref().expect("demo 有 PCI 地址"),
                )
            })
            .collect();
        let ok: Vec<PciDeviceSnapshot> = bdfs
            .iter()
            .map(|bdf| width_device(bdf, "x8", "x8"))
            .collect();
        assert_eq!(pcie_link_width(&gpus, Some(&ok)).status, CheckStatus::Pass);

        let mut degraded = ok.clone();
        degraded[0] = width_device(&bdfs[0], "x8", "x16");
        let check = pcie_link_width(&gpus, Some(&degraded));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("x8（最大 x16）"));

        assert_eq!(
            pcie_link_width(&gpus, Some(&[])).status,
            CheckStatus::Unknown
        );
        assert_eq!(pcie_link_width(&gpus, None).status, CheckStatus::Unknown);
    }

    #[test]
    fn p2p_checks_cover_capability_and_benchmark() {
        let empty = P2pSnapshot {
            gpu_indices: Vec::new(),
            links: Vec::new(),
            benchmark: P2pBenchmarkSnapshot::default(),
            status: HealthStatus::Unknown,
        };
        assert_eq!(p2p_capability(Some(&empty)).status, CheckStatus::Unknown);
        assert_eq!(p2p_capability(None).status, CheckStatus::Unknown);

        let mut p2p = p2p_with_link(true);
        assert_eq!(p2p_capability(Some(&p2p)).status, CheckStatus::Pass);
        p2p.links[0].read = P2pCapabilityStatus::NotSupported;
        assert_eq!(p2p_capability(Some(&p2p)).status, CheckStatus::Fail);

        assert_eq!(p2p_benchmark(Some(&p2p)).status, CheckStatus::Unknown);
        p2p.benchmark.status = P2pBenchmarkStatus::Succeeded;
        assert_eq!(p2p_benchmark(Some(&p2p)).status, CheckStatus::Pass);
        p2p.benchmark.status = P2pBenchmarkStatus::Failed;
        assert_eq!(p2p_benchmark(Some(&p2p)).status, CheckStatus::Fail);
    }

    #[test]
    fn logs_and_findings_checks() {
        assert_eq!(system_logs(None).status, CheckStatus::Unknown);
        let mut logs = LogSnapshot {
            sources: Vec::new(),
            matches: Vec::new(),
            issues: Vec::new(),
            status: HealthStatus::Healthy,
        };
        assert_eq!(system_logs(Some(&logs)).status, CheckStatus::Pass);
        logs.matches.push(LogPatternMatch {
            pattern: "xid".to_owned(),
            severity: HealthStatus::Critical,
            count: 3,
            sources: vec!["dmesg".to_owned()],
            examples: Vec::new(),
        });
        let check = system_logs(Some(&logs));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("xid ×3"));

        let mut snapshot = crate::collectors::demo::snapshot();
        // demo 自带一条 Warning 发现（数据来源提示）→ 不通过。
        assert_eq!(diagnosis_findings(&snapshot).status, CheckStatus::Fail);
        snapshot.findings.clear();
        assert_eq!(diagnosis_findings(&snapshot).status, CheckStatus::Pass);
    }
}
