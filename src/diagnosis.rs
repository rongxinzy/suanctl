use std::collections::BTreeMap;

use crate::domain::{
    AcsControl, DashboardSnapshot, DiagnosisFinding, DriveVisibility, HealthStatus, PciDeviceRole,
    ProbeStatus, ServiceSnapshot, StorageControllerKind,
};

const MAX_EVIDENCE_CHARS: usize = 160;
const MAX_ID_CHARS: usize = 96;
const MAX_OBJECT_CHARS: usize = 56;
const MAX_SUMMARY_CHARS: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceEvent {
    pub timestamp: u64,
    pub source: String,
    pub object: String,
    pub message: String,
    pub status: HealthStatus,
}

pub trait DiagnosisRule {
    fn evaluate(&self, snapshot: &DashboardSnapshot) -> Option<DiagnosisFinding>;
}

pub trait DiagnosisEngine {
    fn evaluate(
        &self,
        snapshot: &DashboardSnapshot,
        events: &[EvidenceEvent],
    ) -> Vec<DiagnosisFinding>;
}

/// 对一个已采集快照执行确定性的只读诊断。
///
/// 诊断层不读取系统、不执行命令，也不尝试修复。所有输入均来自快照，
/// 因而可在离线报告和构造测试中复用。
pub fn diagnose(snapshot: &DashboardSnapshot) -> Vec<DiagnosisFinding> {
    let mut findings = Vec::new();
    diagnose_gpus(snapshot, &mut findings);
    // 厂商专属规则（Xid/reset 属 NVIDIA，分类错误计数属 Enflame……）收敛在
    // 厂商画像里；这里只对快照中实际出现的厂商各跑一次。
    for profile in crate::vendors::present_profiles(&snapshot.gpus) {
        profile.diagnose(snapshot, &mut findings);
    }
    diagnose_p2p_topology(snapshot, &mut findings);
    diagnose_services(snapshot, &mut findings);
    diagnose_platform(snapshot, &mut findings);
    diagnose_storage(snapshot, &mut findings);
    diagnose_logs(snapshot, &mut findings);
    normalize_findings(findings)
}

/// v0.1 的兼容包装；events 暂不参与无状态快照规则。
pub struct ReadOnlyDiagnosisEngine;

impl DiagnosisEngine for ReadOnlyDiagnosisEngine {
    fn evaluate(
        &self,
        snapshot: &DashboardSnapshot,
        _events: &[EvidenceEvent],
    ) -> Vec<DiagnosisFinding> {
        diagnose(snapshot)
    }
}

fn diagnose_gpus(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    for gpu in &snapshot.gpus {
        let object = format!("gpu-{}", gpu.index);
        match gpu.status {
            HealthStatus::Critical => findings.push(finding(
                "gpu-status",
                &object,
                HealthStatus::Critical,
                "GPU 采集状态为严重",
                [format!("GPU {} status=critical", gpu.index)],
            )),
            HealthStatus::Unavailable => findings.push(finding(
                "gpu-status",
                &object,
                HealthStatus::Unavailable,
                "GPU 状态不可用",
                [format!("GPU {} status=unavailable", gpu.index)],
            )),
            _ => {}
        }
    }
}

fn diagnose_p2p_topology(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    let Some(p2p) = snapshot
        .platform
        .as_ref()
        .and_then(|platform| platform.p2p.as_ref())
    else {
        return;
    };
    let numa_of = |index: u32| {
        snapshot
            .gpus
            .iter()
            .find(|gpu| gpu.index == index)
            .and_then(|gpu| gpu.numa_node)
    };
    for link in &p2p.links {
        // 链路是有向全展开的，只看 source<target 避免成对重复。
        if link.source_gpu >= link.target_gpu {
            continue;
        }
        let Some((a, b)) = numa_of(link.source_gpu).zip(numa_of(link.target_gpu)) else {
            continue;
        };
        if a == b {
            continue;
        }
        // 只在 P2P 确有路径/能力时报告：两卡本无通路时跨 NUMA 无意义。
        let has_path =
            link.topology_path.is_some() || link.read.is_supported() || link.write.is_supported();
        if !has_path {
            continue;
        }
        findings.push(finding(
            "gpu-p2p-cross-numa",
            &format!("gpu-{}-{}", link.source_gpu, link.target_gpu),
            HealthStatus::Warning,
            "GPU P2P 路径跨 NUMA 节点",
            [format!(
                "GPU{}(NUMA{}) -> GPU{}(NUMA{}) path={} meeting={}",
                link.source_gpu,
                a,
                link.target_gpu,
                b,
                link.topology_path.as_deref().unwrap_or("未知"),
                link.upstream_meeting_bdf.as_deref().unwrap_or("未知")
            )],
        ));
    }
}

fn diagnose_services(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    for service in &snapshot.services {
        let object = service_object(service);
        if is_discovered(service) && service.endpoint.is_none() {
            findings.push(finding(
                "service-endpoint-missing",
                &object,
                HealthStatus::Warning,
                "已发现推理服务，但没有可探测端点",
                [format!("服务={} endpoint=None", service.name)],
            ));
        }
        if service.endpoint_reachable == Some(false) {
            findings.push(finding(
                "service-endpoint-unreachable",
                &object,
                HealthStatus::Warning,
                "推理服务端点不可达",
                [format!("服务={} endpoint_reachable=false", service.name)],
            ));
        }
        if service.health_probe.status == ProbeStatus::Succeeded
            && service.models_probe.status == ProbeStatus::Failed
        {
            findings.push(finding(
                "service-fake-alive",
                &object,
                HealthStatus::Warning,
                "健康检查成功但模型接口失败，存在假活风险",
                [format!(
                    "服务={} health=succeeded models=failed",
                    service.name
                )],
            ));
        }
        if service.health_probe.status == ProbeStatus::Succeeded
            && matches!(
                service.metrics_probe.status,
                ProbeStatus::Failed | ProbeStatus::Unavailable
            )
        {
            findings.push(finding(
                "service-metrics-unavailable",
                &object,
                HealthStatus::Warning,
                "服务健康，但指标端点不可用，无法判断指标新鲜度",
                [format!(
                    "服务={} health=succeeded metrics={:?}",
                    service.name, service.metrics_probe.status
                )],
            ));
        }
        if service.health_probe.status == ProbeStatus::Failed
            || service.health_probe.http_status == Some(503)
        {
            let status = if service.health_probe.http_status == Some(503) {
                HealthStatus::Critical
            } else {
                HealthStatus::Warning
            };
            findings.push(finding(
                "service-health-failed",
                &object,
                status,
                "推理服务健康检查失败",
                [format!(
                    "服务={} health={:?} http_status={:?}",
                    service.name, service.health_probe.status, service.health_probe.http_status
                )],
            ));
        }
        if configured_model_missing(service) {
            findings.push(finding(
                "service-model-mismatch",
                &object,
                HealthStatus::Warning,
                "配置模型不在服务返回的模型列表中",
                [format!(
                    "服务={} configured_model={} observed_models={:?}",
                    service.name,
                    service.model.as_deref().unwrap_or(""),
                    service.observed_models
                )],
            ));
        }
    }
}

fn diagnose_platform(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    let Some(platform) = snapshot.platform.as_ref() else {
        return;
    };

    if platform
        .iommu
        .requested
        .as_ref()
        .and_then(|requested| requested.enabled)
        == Some(true)
        && platform.iommu.effective == Some(false)
    {
        findings.push(finding_with_suggestion(
            "iommu-not-effective",
            "iommu",
            HealthStatus::Warning,
            "已请求启用 IOMMU，但当前观测未生效",
            ["requested.enabled=true effective=false".to_owned()],
            "在内核命令行加入 iommu=pt（Intel 另加 intel_iommu=on，AMD 另加 amd_iommu=on）并重启生效",
        ));
    }

    if let Some(override_state) = platform
        .iommu
        .acs_override
        .as_ref()
        .filter(|state| state.enabled)
    {
        findings.push(finding_with_suggestion(
            "acs-override",
            "iommu",
            risk_status(override_state.risk),
            "检测到 ACS override，PCIe 隔离边界可能被改变",
            [format!(
                "pcie_acs_override enabled=true value={:?}",
                override_state.value
            )],
            "若为预期配置（直通/P2P）可忽略；否则移除 pcie_acs_override 内核参数并重启",
        ));
    }

    if platform.nvidia_driver.version_match == Some(false) {
        findings.push(finding(
            "nvidia-driver-mismatch",
            "nvidia-driver",
            HealthStatus::Critical,
            "NVIDIA 内核驱动与用户态驱动版本不匹配",
            [format!(
                "kernel={:?} nvidia_smi={:?}",
                platform.nvidia_driver.kernel_module_version,
                platform.nvidia_driver.nvidia_smi_driver_version
            )],
        ));
    }

    let nvidia_user_or_pci_evidence = platform.nvidia_driver.nvidia_smi_driver_version.is_some()
        || platform.pci_devices.iter().any(|device| {
            device.vendor.as_deref() == Some("0x10de")
                || device.vendor_name.as_deref() == Some("NVIDIA")
                || device.driver.as_deref() == Some("nvidia")
        });
    if platform.nvidia_driver.module_loaded == Some(false) && nvidia_user_or_pci_evidence {
        findings.push(finding(
            "nvidia-module-not-loaded",
            "nvidia-driver",
            HealthStatus::Warning,
            "存在 NVIDIA 用户态或 PCI 设备证据，但内核模块未加载",
            [format!(
                "module_loaded=false nvidia_smi_driver={:?}",
                platform.nvidia_driver.nvidia_smi_driver_version
            )],
        ));
    }

    let toolkit_present = platform.cuda.nvcc_toolkit_version.is_some();
    let libcuda_absent = platform.cuda.libcuda.presence == crate::domain::PresenceStatus::Absent;
    let driver_module_absent = platform.nvidia_driver.module_loaded == Some(false);
    if toolkit_present && (libcuda_absent || driver_module_absent) {
        findings.push(finding(
            "cuda-runtime-incomplete",
            "cuda",
            HealthStatus::Critical,
            "检测到 CUDA toolkit，但驱动运行时不完整",
            [format!(
                "nvcc={:?} libcuda={:?} nvidia_module_loaded={:?}",
                platform.cuda.nvcc_toolkit_version,
                platform.cuda.libcuda.presence,
                platform.nvidia_driver.module_loaded
            )],
        ));
    }

    for device in &platform.pci_devices {
        if device.role == Some(PciDeviceRole::Bridge)
            && acs_isolation_redirect_disabled(
                device.acs.as_ref().and_then(|acs| acs.control.as_ref()),
            )
        {
            findings.push(finding_with_suggestion(
                "acs-isolation-disabled",
                &device.bdf,
                HealthStatus::Warning,
                "桥设备 ACS 隔离重定向明确关闭，存在隔离风险",
                [format!(
                    "PCIe {} ACS control 中重定向位为 false",
                    device.bdf
                )],
                "如需 GPU 直通/P2P 隔离：启用设备 ACS 或在内核命令行加入 pcie_acs_override=downstream 后重启",
            ));
        }
        if let (Some(current), Some(maximum)) = (
            parse_link_width(device.current_link_width.as_deref()),
            parse_link_width(device.max_link_width.as_deref()),
        ) {
            if current < maximum {
                findings.push(finding(
                    "pcie-link-width",
                    &device.bdf,
                    HealthStatus::Warning,
                    "PCIe 当前链路宽度低于最大宽度",
                    [format!(
                        "PCIe {} current_width={} max_width={}",
                        device.bdf, current, maximum
                    )],
                ));
            }
        }
    }
}

fn diagnose_storage(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    let Some(storage) = snapshot
        .platform
        .as_ref()
        .and_then(|platform| platform.storage.as_ref())
    else {
        return;
    };

    for array in &storage.software_raid {
        if array.degraded.is_some_and(|degraded| degraded > 0) {
            findings.push(finding(
                "mdraid-degraded",
                &array.name,
                HealthStatus::Critical,
                "软件 RAID 阵列已降级",
                [format!("{} degraded={:?}", array.name, array.degraded)],
            ));
        }
        if array.sync_action.as_deref().is_some_and(|action| {
            !action.trim().is_empty() && !action.trim().eq_ignore_ascii_case("idle")
        }) {
            findings.push(finding(
                "mdraid-sync",
                &array.name,
                HealthStatus::Warning,
                "软件 RAID 阵列正在同步或重建",
                [format!(
                    "{} sync_action={:?}",
                    array.name, array.sync_action
                )],
            ));
        }
        if array.array_state.as_deref().is_some_and(|state| {
            let state = state.to_ascii_lowercase();
            state.contains("recover") || state.contains("rebuild") || state.contains("resync")
        }) {
            findings.push(finding(
                "mdraid-recovery-state",
                &array.name,
                HealthStatus::Warning,
                "软件 RAID 阵列状态显示正在恢复或重建",
                [format!(
                    "{} array_state={:?}",
                    array.name, array.array_state
                )],
            ));
        }
    }

    for phy in &storage.sas_phys {
        let counters = [
            ("invalid_dword", phy.invalid_dword_count),
            ("running_disparity", phy.running_disparity_error_count),
            ("loss_of_dword_sync", phy.loss_of_dword_sync_count),
            ("phy_reset_problem", phy.phy_reset_problem_count),
        ];
        let errors: Vec<_> = counters
            .into_iter()
            .filter_map(|(name, value)| value.filter(|value| *value > 0).map(|value| (name, value)))
            .collect();
        if !errors.is_empty() {
            findings.push(finding(
                "sas-errors",
                &phy.phy,
                HealthStatus::Warning,
                "SAS PHY 存在累计链路错误",
                [format!("SAS {} errors={errors:?}", phy.phy)],
            ));
        }
    }

    for controller in &storage.controllers {
        if matches!(
            controller.status,
            HealthStatus::Warning | HealthStatus::Critical
        ) {
            let object = controller.bdf.as_deref().unwrap_or(&controller.id);
            findings.push(finding(
                "storage-controller-status",
                object,
                controller.status,
                "存储控制器报告异常状态",
                [format!(
                    "controller={} kind={:?} status={:?}",
                    controller.id, controller.kind, controller.status
                )],
            ));
        }
        if controller.kind == StorageControllerKind::Raid
            && controller.physical_drive_visibility == DriveVisibility::Opaque
        {
            let object = controller.bdf.as_deref().unwrap_or(&controller.id);
            findings.push(finding(
                "raid-physical-drive-visibility",
                object,
                HealthStatus::Warning,
                "物理盘可观测性缺口",
                [format!(
                    "RAID controller={} physical_drive_visibility=opaque",
                    controller.id
                )],
            ));
        }
    }
}

fn diagnose_logs(snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
    let Some(logs) = snapshot.logs.as_ref() else {
        return;
    };
    for matched in &logs.matches {
        findings.push(finding(
            "log-pattern",
            &matched.pattern,
            matched.severity,
            "系统日志检测到异常模式",
            [format!(
                "模式={} 命中={} 来源={:?}",
                matched.pattern, matched.count, matched.sources
            )],
        ));
    }
}

fn is_discovered(service: &ServiceSnapshot) -> bool {
    service.process_present == Some(true)
        || service.pid.is_some()
        || service.endpoint.is_some()
        || !service.discovery.sources.is_empty()
}

fn configured_model_missing(service: &ServiceSnapshot) -> bool {
    let Some(model) = service
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    else {
        return false;
    };
    !service.observed_models.is_empty()
        && !service
            .observed_models
            .iter()
            .any(|observed| observed == model)
}

fn service_object(service: &ServiceSnapshot) -> String {
    if !service.name.trim().is_empty() {
        service.name.clone()
    } else {
        service.engine.label().to_owned()
    }
}

fn acs_isolation_redirect_disabled(control: Option<&AcsControl>) -> bool {
    let Some(control) = control else {
        return false;
    };
    [
        control.p2p_request_redirect,
        control.completion_redirect,
        control.upstream_forwarding,
        control.egress_control,
    ]
    .into_iter()
    .any(|value| value == Some(false))
}

fn parse_link_width(value: Option<&str>) -> Option<u32> {
    let value = value?.trim().to_ascii_lowercase();
    let digits = value.strip_prefix('x').unwrap_or(&value);
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn risk_status(status: HealthStatus) -> HealthStatus {
    match status {
        HealthStatus::Critical => HealthStatus::Critical,
        _ => HealthStatus::Warning,
    }
}

pub(crate) fn finding<const N: usize>(
    rule: &str,
    object: &str,
    status: HealthStatus,
    summary: &str,
    evidence: [String; N],
) -> DiagnosisFinding {
    let normalized_object = normalize_object(object);
    let id = bounded(&format!("{rule}.{normalized_object}"), MAX_ID_CHARS);
    DiagnosisFinding {
        id,
        status,
        object: bounded(object, MAX_OBJECT_CHARS),
        summary: bounded(summary, MAX_SUMMARY_CHARS),
        evidence: evidence
            .into_iter()
            .map(|item| bounded(&item, MAX_EVIDENCE_CHARS))
            .collect(),
        suggestion: None,
    }
}

/// 带修复建议的诊断结论（只读建议，不代执行）。
pub(crate) fn finding_with_suggestion<const N: usize>(
    rule: &str,
    object: &str,
    status: HealthStatus,
    summary: &str,
    evidence: [String; N],
    suggestion: &str,
) -> DiagnosisFinding {
    let mut result = finding(rule, object, status, summary, evidence);
    result.suggestion = Some(bounded(suggestion, MAX_SUMMARY_CHARS));
    result
}

fn normalize_findings(findings: Vec<DiagnosisFinding>) -> Vec<DiagnosisFinding> {
    let mut by_id = BTreeMap::<String, DiagnosisFinding>::new();
    for finding in findings {
        match by_id.get_mut(&finding.id) {
            Some(existing) if status_rank(finding.status) > status_rank(existing.status) => {
                *existing = finding;
            }
            Some(_) => {}
            None => {
                by_id.insert(finding.id.clone(), finding);
            }
        }
    }
    let mut findings = by_id.into_values().collect::<Vec<_>>();
    findings.sort_by(|left, right| {
        status_rank(right.status)
            .cmp(&status_rank(left.status))
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.summary.cmp(&right.summary))
            .then_with(|| left.evidence.cmp(&right.evidence))
    });
    findings
}

fn normalize_object(value: &str) -> String {
    let mut normalized = String::new();
    let mut last_separator = false;
    for character in value.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            normalized.push(character);
            last_separator = false;
        } else if !last_separator {
            normalized.push('-');
            last_separator = true;
        }
        if normalized.chars().count() >= MAX_OBJECT_CHARS {
            break;
        }
    }
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "unknown".to_owned()
    } else {
        normalized.to_owned()
    }
}

fn bounded(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn status_rank(status: HealthStatus) -> u8 {
    match status {
        HealthStatus::Critical => 4,
        HealthStatus::Warning => 3,
        HealthStatus::Unavailable => 2,
        HealthStatus::Unknown => 1,
        HealthStatus::Healthy => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AcsCapability, AcsSnapshot, CudaLibrarySnapshot, CudaStackSnapshot, DataSource, EngineKind,
        GpuSnapshot, HostSnapshot, IommuRequested, IommuSnapshot, NvidiaDriverSnapshot,
        PciDeviceSnapshot, PlatformSnapshot, ProbeResult, SasPhySnapshot, SoftwareRaidSnapshot,
        StorageControllerSnapshot, StorageFabricSnapshot, VendorCliSnapshot,
    };

    fn snapshot() -> DashboardSnapshot {
        DashboardSnapshot {
            captured_at: 0,
            source: DataSource::Runtime,
            host: HostSnapshot {
                hostname: "host".to_owned(),
                os: "Linux".to_owned(),
                kernel_version: None,
                architecture: None,
                cpu_model: None,
                logical_cpu_count: None,
                load_1m: None,
                memory_used_mib: None,
                memory_total_mib: None,
                status: HealthStatus::Healthy,
                cpu_status: HealthStatus::Healthy,
                memory_status: HealthStatus::Healthy,
            },
            gpus: Vec::new(),
            services: Vec::new(),
            findings: Vec::new(),
            platform: None,
            logs: None,
            remote: None,
        }
    }

    fn gpu(index: u32, status: HealthStatus, reset: Option<bool>, xid: &[u32]) -> GpuSnapshot {
        GpuSnapshot {
            index,
            name: "GPU".to_owned(),
            uuid: None,
            pci_address: None,
            status,
            temperature_celsius: None,
            utilization_percent: None,
            memory_used_mib: None,
            memory_total_mib: None,
            power_draw_watts: None,
            power_limit_watts: None,
            pstate: None,
            numa_node: None,
            reset_required: reset,
            xid_codes: Some(xid.to_vec()),
            smi_tool: None,
            vendor: Some("NVIDIA".to_owned()),
            serial_number: None,
            driver_version: None,
            ecc_enabled: None,
            error_details: Default::default(),
            reset_count: None,
        }
    }

    fn service(name: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.to_owned(),
            engine: EngineKind::Vllm,
            model: None,
            pid: Some(42),
            port: None,
            status: HealthStatus::Unknown,
            process_present: Some(true),
            endpoint: None,
            endpoint_reachable: None,
            health_probe: ProbeResult::not_attempted(),
            models_probe: ProbeResult::not_attempted(),
            metrics_probe: ProbeResult::not_attempted(),
            observed_models: Vec::new(),
            observed_metrics: Vec::new(),
            gpu_indices: Vec::new(),
            last_error: None,
            discovery: Default::default(),
        }
    }

    fn platform() -> PlatformSnapshot {
        PlatformSnapshot {
            iommu: IommuSnapshot {
                requested: Some(IommuRequested {
                    enabled: Some(true),
                    vendor: None,
                    mode: None,
                    parameters: Vec::new(),
                }),
                effective: Some(false),
                effective_mode: None,
                groups_present: None,
                group_count: None,
                acs_override: Some(crate::domain::IommuAcsOverride {
                    enabled: true,
                    value: Some("downstream".to_owned()),
                    risk: HealthStatus::Warning,
                    warning: "override".to_owned(),
                }),
                status: HealthStatus::Warning,
            },
            nvidia_driver: NvidiaDriverSnapshot {
                module_loaded: Some(false),
                kernel_module_version: Some("535".to_owned()),
                nvidia_smi_driver_version: Some("550".to_owned()),
                device_driver_names: Vec::new(),
                version_match: Some(false),
                status: HealthStatus::Critical,
            },
            cuda: CudaStackSnapshot {
                driver_reported_max_cuda: None,
                nvcc_toolkit_version: Some("12.4".to_owned()),
                nvcc_probe_status: crate::domain::LocalProbeStatus::Succeeded,
                libcudart: library("libcudart", crate::domain::PresenceStatus::Present),
                libcuda: library("libcuda", crate::domain::PresenceStatus::Absent),
                status: HealthStatus::Critical,
            },
            pci_devices: vec![PciDeviceSnapshot {
                bdf: "0000:03:00.0".to_owned(),
                vendor: None,
                device: None,
                class: Some("0x060400".to_owned()),
                driver: None,
                numa_node: None,
                iommu_group: None,
                current_link_speed: Some("8.0 GT/s".to_owned()),
                current_link_width: Some("x8".to_owned()),
                current_link_gen: Some(3),
                current_theoretical_bandwidth_mb_s: Some(7_877),
                max_link_speed: Some("16.0 GT/s".to_owned()),
                max_link_width: Some("x16".to_owned()),
                max_link_gen: Some(4),
                max_theoretical_bandwidth_mb_s: Some(31_508),
                acs: Some(AcsSnapshot {
                    capability: Some(AcsCapability {
                        present: true,
                        source_validation: None,
                        translation_blocking: None,
                        p2p_request_redirect: Some(true),
                        completion_redirect: Some(true),
                        upstream_forwarding: Some(true),
                        egress_control: Some(true),
                        direct_translated_p2p: None,
                    }),
                    control: Some(AcsControl {
                        source_validation: None,
                        translation_blocking: None,
                        p2p_request_redirect: Some(false),
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
            }],
            p2p: None,
            storage: Some(StorageFabricSnapshot {
                controllers: vec![StorageControllerSnapshot {
                    id: "raid0".to_owned(),
                    bdf: Some("0000:04:00.0".to_owned()),
                    kind: StorageControllerKind::Raid,
                    vendor: None,
                    model: None,
                    driver: None,
                    firmware_version: None,
                    virtual_drive_count: None,
                    physical_drive_count: None,
                    physical_drive_visibility: DriveVisibility::Opaque,
                    evidence: Vec::new(),
                    source: Vec::new(),
                    status: HealthStatus::Warning,
                }],
                scsi_hosts: Vec::new(),
                sas_phys: vec![SasPhySnapshot {
                    phy: "phy-0:0".to_owned(),
                    sas_host: None,
                    bdf: None,
                    port_identifier: None,
                    port_state: None,
                    phy_state: None,
                    negotiated_link_rate: None,
                    minimum_link_rate: None,
                    maximum_link_rate: None,
                    invalid_dword_count: Some(1),
                    running_disparity_error_count: None,
                    loss_of_dword_sync_count: None,
                    phy_reset_problem_count: None,
                    status: HealthStatus::Warning,
                }],
                software_raid: vec![SoftwareRaidSnapshot {
                    name: "md0".to_owned(),
                    level: Some("raid1".to_owned()),
                    raid_disks: Some(2),
                    degraded: Some(1),
                    sync_action: Some("resync".to_owned()),
                    array_state: Some("recovering".to_owned()),
                    status: HealthStatus::Critical,
                }],
                vendor_clis: vec![VendorCliSnapshot {
                    provider: "storcli".to_owned(),
                    executable: None,
                    probe_status: crate::domain::LocalProbeStatus::Unavailable,
                    parsed: false,
                    controllers_observed: None,
                    status: HealthStatus::Unavailable,
                }],
                issues: Vec::new(),
                status: HealthStatus::Warning,
            }),
            plugins: Vec::new(),
            acs_summary: None,
            issues: Vec::new(),
            status: HealthStatus::Warning,
        }
    }

    fn library(name: &str, presence: crate::domain::PresenceStatus) -> CudaLibrarySnapshot {
        CudaLibrarySnapshot {
            name: name.to_owned(),
            presence,
            probe_status: crate::domain::LocalProbeStatus::Succeeded,
            path: None,
            source: None,
        }
    }

    #[test]
    fn covers_gpu_service_platform_pcie_and_storage_rules() {
        let mut snapshot = snapshot();
        snapshot.gpus = vec![
            gpu(0, HealthStatus::Healthy, Some(true), &[31]),
            gpu(1, HealthStatus::Unavailable, Some(false), &[]),
        ];

        let missing_endpoint = service("容器服务");
        let mut unhealthy = service("vllm");
        unhealthy.endpoint = Some("http://127.0.0.1:8000".to_owned());
        unhealthy.endpoint_reachable = Some(false);
        unhealthy.model = Some("configured".to_owned());
        unhealthy.observed_models = vec!["served".to_owned()];
        unhealthy.health_probe = ProbeResult {
            status: ProbeStatus::Succeeded,
            http_status: Some(200),
            message: None,
        };
        unhealthy.models_probe = ProbeResult {
            status: ProbeStatus::Failed,
            http_status: Some(503),
            message: None,
        };
        unhealthy.metrics_probe = ProbeResult {
            status: ProbeStatus::Unavailable,
            http_status: None,
            message: None,
        };
        let mut failed = service("failed");
        failed.endpoint = Some("http://127.0.0.1:8001".to_owned());
        failed.health_probe = ProbeResult {
            status: ProbeStatus::Failed,
            http_status: Some(503),
            message: None,
        };
        snapshot.services = vec![
            missing_endpoint.clone(),
            unhealthy,
            failed,
            missing_endpoint,
        ];
        snapshot.platform = Some(platform());

        let findings = diagnose(&snapshot);
        for prefix in [
            "gpu-reset-required.",
            "gpu-xid.",
            "gpu-status.",
            "service-endpoint-missing.",
            "service-endpoint-unreachable.",
            "service-fake-alive.",
            "service-metrics-unavailable.",
            "service-health-failed.",
            "service-model-mismatch.",
            "iommu-not-effective.",
            "acs-override.",
            "acs-isolation-disabled.",
            "nvidia-driver-mismatch.",
            "nvidia-module-not-loaded.",
            "cuda-runtime-incomplete.",
            "pcie-link-width.",
            "mdraid-degraded.",
            "mdraid-sync.",
            "mdraid-recovery-state.",
            "sas-errors.",
            "storage-controller-status.",
            "raid-physical-drive-visibility.",
        ] {
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.id.starts_with(prefix)),
                "missing {prefix}"
            );
        }
        assert!(findings.iter().all(|finding| finding
            .evidence
            .iter()
            .all(|evidence| evidence.chars().count() <= MAX_EVIDENCE_CHARS)));
    }

    #[test]
    fn acs_and_iommu_findings_carry_actionable_suggestions() {
        let mut snapshot = snapshot();
        let mut platform = platform();
        // 默认 platform()：iommu-not-effective + acs-override + acs-isolation-disabled 均触发。
        snapshot.platform = Some(platform.clone());
        let findings = diagnose(&snapshot);
        let by_id = |id: &str| {
            findings
                .iter()
                .find(|finding| finding.id == id)
                .expect("finding 应存在")
                .clone()
        };
        let iommu = by_id("iommu-not-effective.iommu");
        assert!(
            iommu
                .suggestion
                .as_deref()
                .is_some_and(|text| text.contains("iommu=pt")),
            "IOMMU 建议应含 iommu=pt：{:?}",
            iommu.suggestion
        );
        let override_finding = by_id("acs-override.iommu");
        assert!(
            override_finding
                .suggestion
                .as_deref()
                .is_some_and(|text| text.contains("pcie_acs_override")),
            "ACS override 建议应含参数名：{:?}",
            override_finding.suggestion
        );
        let isolation = by_id("acs-isolation-disabled.0000-03-00.0");
        assert!(
            isolation
                .suggestion
                .as_deref()
                .is_some_and(|text| text.contains("pcie_acs_override=downstream")),
            "ACS 关闭建议应含 downstream：{:?}",
            isolation.suggestion
        );
        // 无 ACS/IOMMU 问题时正常 finding 不携带建议。
        platform.iommu.requested.as_mut().unwrap().enabled = Some(false);
        platform.iommu.effective = Some(false);
        platform.iommu.acs_override = None;
        platform.pci_devices[0].acs = None;
        snapshot.platform = Some(platform);
        let clean = diagnose(&snapshot);
        assert!(
            clean.iter().all(|finding| finding.suggestion.is_none()),
            "无相关问题时不应有建议：{:?}",
            clean
                .iter()
                .filter_map(|finding| finding.suggestion.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn does_not_report_missing_evidence_or_speed_only_degradation() {
        let mut snapshot = snapshot();
        let mut platform = platform();
        platform.iommu.requested.as_mut().unwrap().enabled = Some(false);
        platform.iommu.effective = Some(false);
        platform.iommu.acs_override = None;
        platform.nvidia_driver.version_match = None;
        platform.nvidia_driver.module_loaded = None;
        platform.cuda.nvcc_toolkit_version = None;
        platform.pci_devices[0].current_link_width = Some("x16".to_owned());
        platform.pci_devices[0].max_link_width = Some("x16".to_owned());
        platform.pci_devices[0].current_link_speed = Some("2.5 GT/s".to_owned());
        platform.pci_devices[0].max_link_speed = Some("16.0 GT/s".to_owned());
        platform.pci_devices[0].acs = None;
        platform.storage.as_mut().unwrap().vendor_clis[0].probe_status =
            crate::domain::LocalProbeStatus::Unavailable;
        platform.storage.as_mut().unwrap().controllers[0].status = HealthStatus::Healthy;
        platform.storage.as_mut().unwrap().controllers[0].physical_drive_visibility =
            DriveVisibility::Visible;
        platform.storage.as_mut().unwrap().software_raid[0].degraded = Some(0);
        platform.storage.as_mut().unwrap().software_raid[0].sync_action = Some("idle".to_owned());
        platform.storage.as_mut().unwrap().software_raid[0].array_state = Some("clean".to_owned());
        platform.storage.as_mut().unwrap().sas_phys[0].invalid_dword_count = Some(0);
        snapshot.platform = Some(platform);
        let findings = diagnose(&snapshot);
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn real_rx_box_shaped_snapshot_diagnoses_clean() {
        // 用真实设备 rx-box（172.18.5.123）采集形态的数据驱动诊断：
        // 2× L20 healthy、Hygon CPU、真实日志尾部无异常 → 不应产生任何 finding。
        let mut snapshot = snapshot();
        snapshot.host = HostSnapshot {
            hostname: "rx-box".to_owned(),
            os: "Ubuntu 24.04.3 LTS".to_owned(),
            kernel_version: Some("6.8.0-90-generic".to_owned()),
            architecture: Some("x86_64".to_owned()),
            cpu_model: Some("Hygon C86 3350  8-core Processor".to_owned()),
            logical_cpu_count: Some(16),
            load_1m: Some(0.94),
            memory_used_mib: Some(12204),
            memory_total_mib: Some(64037),
            status: HealthStatus::Healthy,
            cpu_status: HealthStatus::Healthy,
            memory_status: HealthStatus::Healthy,
        };
        let l20 = |index: u32, temperature: u32, power: f64| GpuSnapshot {
            index,
            name: "NVIDIA L20".to_owned(),
            uuid: Some(format!("GPU-2da85921-1a58-7e3c-5166-47d742d5fe{index}")),
            pci_address: Some(if index == 0 {
                "00000000:0C:00.0".to_owned()
            } else {
                "00000000:0F:00.0".to_owned()
            }),
            status: HealthStatus::Healthy,
            temperature_celsius: Some(temperature),
            utilization_percent: Some(0),
            memory_used_mib: Some(42248),
            memory_total_mib: Some(46068),
            power_draw_watts: Some(power),
            power_limit_watts: Some(350.0),
            pstate: Some("P0".to_owned()),
            numa_node: None,
            reset_required: None,
            xid_codes: Some(Vec::new()),
            smi_tool: None,
            vendor: Some("NVIDIA".to_owned()),
            serial_number: None,
            driver_version: None,
            ecc_enabled: None,
            error_details: Default::default(),
            reset_count: None,
        };
        snapshot.gpus = vec![l20(0, 56, 87.95), l20(1, 57, 89.53)];
        snapshot.logs = Some(crate::domain::LogSnapshot {
            status: HealthStatus::Healthy,
            sources: vec![
                crate::domain::LogSourceSnapshot {
                    name: "dmesg".to_owned(),
                    probe_status: crate::domain::LocalProbeStatus::Succeeded,
                    command: Some("dmesg -T".to_owned()),
                    path: None,
                    lines_tail: vec!["kern log line".to_owned()],
                    truncated: false,
                    match_count: 0,
                },
                crate::domain::LogSourceSnapshot {
                    name: "syslog".to_owned(),
                    probe_status: crate::domain::LocalProbeStatus::Succeeded,
                    command: None,
                    path: Some("/var/log/syslog".to_owned()),
                    lines_tail: vec!["syslog line".to_owned()],
                    truncated: false,
                    match_count: 0,
                },
            ],
            matches: Vec::new(),
            issues: Vec::new(),
        });
        let findings = diagnose(&snapshot);
        assert!(findings.is_empty(), "真实形态数据不应误报：{findings:?}");
    }

    #[test]
    fn sorts_deduplicates_and_bounds_ids() {
        let mut snapshot = snapshot();
        let service_name = "服务/".to_owned() + &"很长".repeat(100);
        let first = service(&service_name);
        snapshot.services = vec![first.clone(), first];
        let findings = diagnose(&snapshot);
        assert_eq!(findings.len(), 1);
        assert!(findings.windows(2).all(|pair| pair[0].id <= pair[1].id));
        assert!(findings[0].id.chars().count() <= MAX_ID_CHARS);
        assert!(findings[0]
            .id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')));
    }
}
