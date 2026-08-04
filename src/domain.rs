use serde::{Deserialize, Serialize};

pub type TimestampMillis = u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Warning,
    Critical,
    Unavailable,
    #[default]
    Unknown,
}

impl HealthStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "正常",
            Self::Warning => "警告",
            Self::Critical => "严重",
            Self::Unavailable => "不可用",
            Self::Unknown => "未知",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSource {
    Demo,
    Runtime,
    Unavailable,
}

impl DataSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Demo => "演示数据",
            Self::Runtime => "运行时采集",
            Self::Unavailable => "不可用",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPage {
    Overview,
    Gpu,
    Services,
    Diagnosis,
    Reports,
}

impl UiPage {
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Gpu,
        Self::Services,
        Self::Diagnosis,
        Self::Reports,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "总览",
            Self::Gpu => "GPU",
            Self::Services => "服务",
            Self::Diagnosis => "诊断",
            Self::Reports => "报告",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Gpu => 1,
            Self::Services => 2,
            Self::Diagnosis => 3,
            Self::Reports => 4,
        }
    }

    pub const fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::Overview),
            '2' => Some(Self::Gpu),
            '3' => Some(Self::Services),
            '4' => Some(Self::Diagnosis),
            '5' => Some(Self::Reports),
            _ => None,
        }
    }

    pub const fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub const fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub hostname: String,
    pub os: String,
    #[serde(default)]
    pub kernel_version: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub cpu_model: Option<String>,
    #[serde(default)]
    pub logical_cpu_count: Option<u32>,
    #[serde(default)]
    pub load_1m: Option<f64>,
    #[serde(default)]
    pub memory_used_mib: Option<u64>,
    #[serde(default)]
    pub memory_total_mib: Option<u64>,
    #[serde(default)]
    pub status: HealthStatus,
    pub cpu_status: HealthStatus,
    pub memory_status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuSnapshot {
    pub index: u32,
    pub name: String,
    #[serde(default)]
    pub uuid: Option<String>,
    pub pci_address: Option<String>,
    pub status: HealthStatus,
    pub temperature_celsius: Option<u32>,
    pub utilization_percent: Option<u32>,
    pub memory_used_mib: Option<u64>,
    pub memory_total_mib: Option<u64>,
    #[serde(default)]
    pub power_draw_watts: Option<f64>,
    #[serde(default)]
    pub power_limit_watts: Option<f64>,
    #[serde(default)]
    pub pstate: Option<String>,
    #[serde(default)]
    pub numa_node: Option<u32>,
    #[serde(default)]
    pub reset_required: Option<bool>,
    #[serde(default)]
    pub xid_codes: Option<Vec<u32>>,
}

/// 文件、命令或 sysfs 观测到的存在性。`Unknown` 表示没有足够证据，
/// 不把缺失的工具或不可读路径编码成数值零。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    Present,
    Absent,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalProbeStatus {
    NotAttempted,
    Succeeded,
    Failed,
    Unavailable,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IommuRequested {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IommuAcsOverride {
    pub enabled: bool,
    #[serde(default)]
    pub value: Option<String>,
    pub risk: HealthStatus,
    pub warning: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IommuSnapshot {
    /// Kernel command-line intent, kept separate from observed effectiveness.
    #[serde(default)]
    pub requested: Option<IommuRequested>,
    /// Derived only from the observable IOMMU groups directory.
    #[serde(default)]
    pub effective: Option<bool>,
    #[serde(default)]
    pub effective_mode: Option<String>,
    #[serde(default)]
    pub groups_present: Option<bool>,
    #[serde(default)]
    pub group_count: Option<u32>,
    #[serde(default)]
    pub acs_override: Option<IommuAcsOverride>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvidiaDriverSnapshot {
    #[serde(default)]
    pub module_loaded: Option<bool>,
    #[serde(default)]
    pub kernel_module_version: Option<String>,
    /// Version reported by the user-space `nvidia-smi` executable.
    #[serde(default)]
    pub nvidia_smi_driver_version: Option<String>,
    #[serde(default)]
    pub device_driver_names: Vec<String>,
    /// `None` means one side was unavailable; `Some(false)` is an observed mismatch.
    #[serde(default)]
    pub version_match: Option<bool>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CudaLibrarySnapshot {
    pub name: String,
    pub presence: PresenceStatus,
    pub probe_status: LocalProbeStatus,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CudaStackSnapshot {
    /// This value comes only from the default `nvidia-smi` header and is not
    /// treated as proof that a CUDA toolkit is installed.
    #[serde(default)]
    pub driver_reported_max_cuda: Option<String>,
    #[serde(default)]
    pub nvcc_toolkit_version: Option<String>,
    #[serde(default)]
    pub nvcc_probe_status: LocalProbeStatus,
    pub libcudart: CudaLibrarySnapshot,
    pub libcuda: CudaLibrarySnapshot,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcsCapability {
    pub present: bool,
    #[serde(default)]
    pub source_validation: Option<bool>,
    #[serde(default)]
    pub translation_blocking: Option<bool>,
    #[serde(default)]
    pub p2p_request_redirect: Option<bool>,
    #[serde(default)]
    pub completion_redirect: Option<bool>,
    #[serde(default)]
    pub upstream_forwarding: Option<bool>,
    #[serde(default)]
    pub egress_control: Option<bool>,
    #[serde(default)]
    pub direct_translated_p2p: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcsControl {
    #[serde(default)]
    pub source_validation: Option<bool>,
    #[serde(default)]
    pub translation_blocking: Option<bool>,
    #[serde(default)]
    pub p2p_request_redirect: Option<bool>,
    #[serde(default)]
    pub completion_redirect: Option<bool>,
    #[serde(default)]
    pub upstream_forwarding: Option<bool>,
    #[serde(default)]
    pub egress_control: Option<bool>,
    #[serde(default)]
    pub direct_translated_p2p: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcsSnapshot {
    #[serde(default)]
    pub capability: Option<AcsCapability>,
    #[serde(default)]
    pub control: Option<AcsControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PciDeviceRole {
    Bridge,
    Raid,
    Sas,
    Sata,
    Nvme,
    Scsi,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PciDeviceSnapshot {
    pub bdf: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub numa_node: Option<u32>,
    #[serde(default)]
    pub iommu_group: Option<u32>,
    #[serde(default)]
    pub current_link_speed: Option<String>,
    #[serde(default)]
    pub current_link_width: Option<String>,
    /// Parsed PCIe generation for the currently negotiated link speed.
    #[serde(default)]
    pub current_link_gen: Option<u8>,
    /// Nominal one-direction payload ceiling in decimal MB/s. This is derived
    /// from link speed, width and PCIe encoding, not a workload measurement.
    #[serde(default)]
    pub current_theoretical_bandwidth_mb_s: Option<u64>,
    #[serde(default)]
    pub max_link_speed: Option<String>,
    #[serde(default)]
    pub max_link_width: Option<String>,
    #[serde(default)]
    pub max_link_gen: Option<u8>,
    #[serde(default)]
    pub max_theoretical_bandwidth_mb_s: Option<u64>,
    #[serde(default)]
    pub acs: Option<AcsSnapshot>,
    /// PCI class based role. A generic bridge is intentionally not called a
    /// switch unless the available topology/evidence supports that claim.
    #[serde(default)]
    pub role: Option<PciDeviceRole>,
    #[serde(default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub vendor_name: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub subsystem_vendor: Option<String>,
    #[serde(default)]
    pub subsystem_device: Option<String>,
    #[serde(default)]
    pub subsystem_name: Option<String>,
    #[serde(default)]
    pub parent_bdf: Option<String>,
    #[serde(default)]
    pub downstream_bdfs: Vec<String>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pCapabilityStatus {
    Supported,
    NotSupported,
    ChipsetNotSupported,
    GpuNotSupported,
    TopologyNotSupported,
    DisabledByConfiguration,
    #[default]
    Unknown,
}

impl P2pCapabilityStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Supported => "支持",
            Self::NotSupported => "不支持",
            Self::ChipsetNotSupported => "芯片组不支持",
            Self::GpuNotSupported => "GPU不支持",
            Self::TopologyNotSupported => "拓扑不支持",
            Self::DisabledByConfiguration => "配置禁用",
            Self::Unknown => "未知",
        }
    }

    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuP2pLinkSnapshot {
    pub source_gpu: u32,
    pub target_gpu: u32,
    /// Raw NVIDIA topology token: PIX/PXB/PHB/NODE/SYS/NV#.
    #[serde(default)]
    pub topology_path: Option<String>,
    #[serde(default)]
    pub read: P2pCapabilityStatus,
    #[serde(default)]
    pub write: P2pCapabilityStatus,
    #[serde(default)]
    pub pcie: P2pCapabilityStatus,
    #[serde(default)]
    pub nvlink: P2pCapabilityStatus,
    #[serde(default)]
    pub atomics: P2pCapabilityStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pBenchmarkStatus {
    #[default]
    NotRequested,
    Succeeded,
    Failed,
    Unavailable,
}

impl P2pBenchmarkStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NotRequested => "未执行",
            Self::Succeeded => "已完成",
            Self::Failed => "失败",
            Self::Unavailable => "不可用",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct P2pBandwidthMeasurement {
    pub source_gpu: u32,
    pub target_gpu: u32,
    pub gigabytes_per_second: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct P2pBenchmarkSnapshot {
    #[serde(default)]
    pub status: P2pBenchmarkStatus,
    pub tool: String,
    pub testcase: String,
    #[serde(default)]
    pub measured_at: Option<TimestampMillis>,
    #[serde(default)]
    pub direction_description: Option<String>,
    #[serde(default)]
    pub measurements: Vec<P2pBandwidthMeasurement>,
    #[serde(default)]
    pub message: Option<String>,
}

impl Default for P2pBenchmarkSnapshot {
    fn default() -> Self {
        Self {
            status: P2pBenchmarkStatus::NotRequested,
            tool: "nvbandwidth".to_owned(),
            testcase: "device_to_device_memcpy_write_ce".to_owned(),
            measured_at: None,
            direction_description: None,
            measurements: Vec::new(),
            message: Some("未请求运行 GPU 负载".to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct P2pSnapshot {
    #[serde(default)]
    pub gpu_indices: Vec<u32>,
    #[serde(default)]
    pub links: Vec<GpuP2pLinkSnapshot>,
    /// Default means no workload was run. Only an explicit CLI flag may
    /// replace this field with measured NVBandwidth results.
    #[serde(default)]
    pub benchmark: P2pBenchmarkSnapshot,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageControllerKind {
    Raid,
    Hba,
    Sas,
    Sata,
    Nvme,
    Scsi,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveVisibility {
    Visible,
    Opaque,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageControllerSnapshot {
    pub id: String,
    #[serde(default)]
    pub bdf: Option<String>,
    pub kind: StorageControllerKind,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub firmware_version: Option<String>,
    #[serde(default)]
    pub virtual_drive_count: Option<u32>,
    #[serde(default)]
    pub physical_drive_count: Option<u32>,
    /// Hardware RAID commonly hides the physical disk layer from Linux. In
    /// that case it must remain opaque/unknown instead of being called healthy.
    #[serde(default)]
    pub physical_drive_visibility: DriveVisibility,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub source: Vec<String>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScsiHostSnapshot {
    pub host: String,
    #[serde(default)]
    pub bdf: Option<String>,
    #[serde(default)]
    pub proc_name: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub firmware_version: Option<String>,
    #[serde(default)]
    pub sas_host: Option<String>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SasPhySnapshot {
    pub phy: String,
    #[serde(default)]
    pub sas_host: Option<String>,
    #[serde(default)]
    pub bdf: Option<String>,
    #[serde(default)]
    pub port_identifier: Option<String>,
    #[serde(default)]
    pub port_state: Option<String>,
    #[serde(default)]
    pub phy_state: Option<String>,
    #[serde(default)]
    pub negotiated_link_rate: Option<String>,
    #[serde(default)]
    pub minimum_link_rate: Option<String>,
    #[serde(default)]
    pub maximum_link_rate: Option<String>,
    #[serde(default)]
    pub invalid_dword_count: Option<u64>,
    #[serde(default)]
    pub running_disparity_error_count: Option<u64>,
    #[serde(default)]
    pub loss_of_dword_sync_count: Option<u64>,
    #[serde(default)]
    pub phy_reset_problem_count: Option<u64>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareRaidSnapshot {
    pub name: String,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub raid_disks: Option<u32>,
    #[serde(default)]
    pub degraded: Option<u32>,
    #[serde(default)]
    pub sync_action: Option<String>,
    #[serde(default)]
    pub array_state: Option<String>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorCliSnapshot {
    pub provider: String,
    #[serde(default)]
    pub executable: Option<String>,
    pub probe_status: LocalProbeStatus,
    #[serde(default)]
    pub parsed: bool,
    #[serde(default)]
    pub controllers_observed: Option<u32>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageFabricSnapshot {
    #[serde(default)]
    pub controllers: Vec<StorageControllerSnapshot>,
    #[serde(default)]
    pub scsi_hosts: Vec<ScsiHostSnapshot>,
    #[serde(default)]
    pub sas_phys: Vec<SasPhySnapshot>,
    #[serde(default)]
    pub software_raid: Vec<SoftwareRaidSnapshot>,
    #[serde(default)]
    pub vendor_clis: Vec<VendorCliSnapshot>,
    #[serde(default)]
    pub issues: Vec<CollectionIssue>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformSnapshot {
    pub iommu: IommuSnapshot,
    pub nvidia_driver: NvidiaDriverSnapshot,
    pub cuda: CudaStackSnapshot,
    pub pci_devices: Vec<PciDeviceSnapshot>,
    /// NVIDIA driver-reported peer capability/topology and optional explicit
    /// NVBandwidth result. Missing on older snapshots and non-NVIDIA hosts.
    #[serde(default)]
    pub p2p: Option<P2pSnapshot>,
    /// Storage is optional so older snapshots and non-storage hosts remain
    /// backwards compatible.
    #[serde(default)]
    pub storage: Option<StorageFabricSnapshot>,
    #[serde(default)]
    pub issues: Vec<CollectionIssue>,
    #[serde(default)]
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    LlamaCpp,
    Vllm,
    Sglang,
    Unknown,
}

impl EngineKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vLLM",
            Self::Sglang => "SGLang",
            Self::Unknown => "未知引擎",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    #[default]
    NotAttempted,
    Succeeded,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    #[serde(default)]
    pub status: ProbeStatus,
    #[serde(default)]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub message: Option<String>,
}

impl ProbeResult {
    pub const fn not_attempted() -> Self {
        Self {
            status: ProbeStatus::NotAttempted,
            http_status: None,
            message: None,
        }
    }
}

impl Default for ProbeResult {
    fn default() -> Self {
        Self::not_attempted()
    }
}

/// HTTP `/metrics` 中有限白名单标量的观测。只保存名称和值字符串，不保存
/// 无界的 Prometheus label map。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedMetric {
    pub name: String,
    pub value: String,
}

/// 服务发现的来源。来源只描述“看到了什么”，不代表 HTTP 探针已经成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySourceKind {
    HostProcess,
    Container,
    Configured,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryConfidence {
    High,
    Medium,
    Low,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    Observed,
    Published,
    Configured,
    Inferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    Docker,
    Podman,
    Nerdctl,
    Cri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedPort {
    #[serde(default)]
    pub host_ip: Option<String>,
    #[serde(default)]
    pub host_port: Option<u16>,
    pub container_port: u16,
    #[serde(default)]
    pub protocol: Option<String>,
}

/// 对外暴露的容器元数据白名单。不会保存环境变量、mount、label 或原始
/// inspect/ps JSON。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerMetadata {
    pub runtime: ContainerRuntime,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub published_ports: Vec<PublishedPort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySource {
    pub kind: DiscoverySourceKind,
    /// 固定的匹配规则或安全摘要；禁止放入完整命令行。
    pub match_reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub confidence: DiscoveryConfidence,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub container: Option<ContainerMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointProvenance {
    pub endpoint: String,
    pub kind: EndpointKind,
    /// 例如“进程显式 --host/--port”或“容器 published port”。
    pub derivation: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryProvenance {
    #[serde(default)]
    pub sources: Vec<DiscoverySource>,
    #[serde(default)]
    pub endpoints: Vec<EndpointProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub name: String,
    pub engine: EngineKind,
    pub model: Option<String>,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub status: HealthStatus,
    #[serde(default)]
    pub process_present: Option<bool>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub endpoint_reachable: Option<bool>,
    #[serde(default)]
    pub health_probe: ProbeResult,
    #[serde(default)]
    pub models_probe: ProbeResult,
    #[serde(default)]
    pub metrics_probe: ProbeResult,
    #[serde(default)]
    pub observed_models: Vec<String>,
    #[serde(default)]
    pub observed_metrics: Vec<ObservedMetric>,
    #[serde(default)]
    pub gpu_indices: Vec<u32>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub discovery: DiscoveryProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosisFinding {
    pub id: String,
    pub status: HealthStatus,
    pub object: String,
    pub summary: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionIssue {
    pub collector: String,
    pub code: String,
    pub status: HealthStatus,
    pub message: String,
}

impl CollectionIssue {
    pub fn unavailable(
        collector: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            collector: collector.into(),
            code: code.into(),
            status: HealthStatus::Unavailable,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityIssue {
    pub capability: String,
    pub code: String,
    pub status: HealthStatus,
    pub message: String,
}

impl CapabilityIssue {
    pub fn unavailable(
        capability: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            code: code.into(),
            status: HealthStatus::Unavailable,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "issue", rename_all = "snake_case")]
pub enum DoctorIssue {
    Collection(CollectionIssue),
    Capability(CapabilityIssue),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub captured_at: TimestampMillis,
    pub source: DataSource,
    pub host: HostSnapshot,
    pub gpus: Vec<GpuSnapshot>,
    pub services: Vec<ServiceSnapshot>,
    pub findings: Vec<DiagnosisFinding>,
    /// Real platform collection is optional so old/demo snapshots remain valid.
    #[serde(default)]
    pub platform: Option<PlatformSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: String,
    pub source: DataSource,
    pub timestamp: TimestampMillis,
    pub status: HealthStatus,
    pub message: String,
    #[serde(default)]
    pub snapshot: Option<DashboardSnapshot>,
    #[serde(default)]
    pub issues: Vec<DoctorIssue>,
}

impl DoctorReport {
    pub fn from_runtime(collection: crate::collectors::runtime::RuntimeCollection) -> Self {
        let snapshot = collection.snapshot;
        Self {
            schema_version: "suanctl.doctor/v0.1".to_owned(),
            source: DataSource::Runtime,
            timestamp: snapshot.captured_at,
            status: collection.status,
            message: format!(
                "已完成运行时采集：{} 项服务，{} 条问题",
                snapshot.services.len(),
                collection.issues.len()
            ),
            snapshot: Some(snapshot),
            issues: collection
                .issues
                .into_iter()
                .map(DoctorIssue::Collection)
                .collect(),
        }
    }

    pub fn unavailable(timestamp: TimestampMillis) -> Self {
        Self {
            schema_version: "suanctl.doctor/v0.1".to_owned(),
            source: DataSource::Unavailable,
            timestamp,
            status: HealthStatus::Unavailable,
            message: "当前调用路径未提供运行时快照，未伪造硬件状态".to_owned(),
            snapshot: None,
            issues: vec![
                DoctorIssue::Collection(CollectionIssue::unavailable(
                    "runtime",
                    "collector_not_implemented",
                    "当前调用路径未提供主机、GPU 和服务快照",
                )),
                DoctorIssue::Capability(CapabilityIssue::unavailable(
                    "runtime_collection",
                    "capability_not_implemented",
                    "请使用 RuntimeCollector 获取本机只读运行时快照",
                )),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DataSource, DoctorIssue, DoctorReport, HealthStatus, ProbeStatus, UiPage};

    #[test]
    fn health_status_serializes_as_stable_snake_case() {
        let json = serde_json::to_string(&HealthStatus::Unavailable).expect("status json");
        assert_eq!(json, "\"unavailable\"");
    }

    #[test]
    fn doctor_report_contains_unavailable_source_without_fake_hardware() {
        let report = DoctorReport::unavailable(123);
        let value = serde_json::to_value(report).expect("doctor json");
        assert_eq!(value["schema_version"], "suanctl.doctor/v0.1");
        assert_eq!(value["source"], "unavailable");
        assert_eq!(value["timestamp"], 123);
        assert_eq!(value["status"], "unavailable");
        assert!(value["snapshot"].is_null());
        assert_eq!(value["issues"].as_array().map(Vec::len), Some(2));
        assert!(matches!(report_issues(), DoctorIssue::Collection(_)));
    }

    fn report_issues() -> DoctorIssue {
        DoctorReport::unavailable(0)
            .issues
            .into_iter()
            .next()
            .expect("unavailable report issue")
    }

    #[test]
    fn unavailable_report_does_not_encode_fake_zero_hardware_values() {
        let report = DoctorReport::unavailable(123);
        assert!(report.snapshot.is_none());
        assert!(!serde_json::to_string(&report)
            .expect("doctor json")
            .contains("memory_total_mib"));
    }

    #[test]
    fn service_health_is_separate_from_endpoint_reachability() {
        let service = &crate::collectors::demo::snapshot().services[1];
        assert_eq!(service.process_present, Some(true));
        assert_eq!(service.endpoint_reachable, Some(true));
        assert_eq!(service.health_probe.status, ProbeStatus::Failed);
        assert_eq!(service.health_probe.http_status, Some(503));
        assert_eq!(service.status, HealthStatus::Warning);
    }

    #[test]
    fn page_order_and_labels_are_stable() {
        assert_eq!(UiPage::ALL.len(), 5);
        assert_eq!(UiPage::from_digit('3'), Some(UiPage::Services));
        assert_eq!(UiPage::Overview.next(), UiPage::Gpu);
        assert_eq!(UiPage::Overview.previous(), UiPage::Reports);
        assert_eq!(DataSource::Demo.label(), "演示数据");
    }
}
