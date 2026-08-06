use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    collectors::runtime::RuntimeCollection,
    domain::{DashboardSnapshot, DoctorReport, HealthStatus, PciMmioKind},
};

pub const EVIDENCE_SCHEMA_VERSION: &str = "suanctl.evidence/v0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    pub code: &'static str,
    pub message: String,
}

impl StorageError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for StorageError {}

pub trait SnapshotSink {
    fn write_snapshot(&self, snapshot: &DashboardSnapshot) -> Result<(), StorageError>;
}
pub trait ReportSink {
    fn write_report(&self, snapshot: &DashboardSnapshot) -> Result<(), StorageError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Json,
    Jsonl,
    Markdown,
}

impl ReportFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Jsonl => "JSONL",
            Self::Markdown => "Markdown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceReport {
    pub schema_version: String,
    pub generated_at: u64,
    pub captured_at: u64,
    pub source: crate::domain::DataSource,
    pub status: HealthStatus,
    pub snapshot: DashboardSnapshot,
    pub collection_issues: Vec<crate::domain::CollectionIssue>,
}

impl EvidenceReport {
    pub fn from_runtime(collection: &RuntimeCollection) -> Self {
        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION.to_owned(),
            generated_at: now_millis(),
            captured_at: collection.snapshot.captured_at,
            source: collection.snapshot.source,
            status: collection.status,
            snapshot: collection.snapshot.clone(),
            collection_issues: collection.issues.clone(),
        }
    }

    pub fn from_doctor(report: &DoctorReport) -> Option<Self> {
        let snapshot = report.snapshot.clone()?;
        Some(Self {
            schema_version: EVIDENCE_SCHEMA_VERSION.to_owned(),
            generated_at: now_millis(),
            captured_at: snapshot.captured_at,
            source: report.source,
            status: report.status,
            snapshot,
            collection_issues: report
                .issues
                .iter()
                .filter_map(|issue| match issue {
                    crate::domain::DoctorIssue::Collection(issue) => Some(issue.clone()),
                    crate::domain::DoctorIssue::Capability(_) => None,
                })
                .collect(),
        })
    }
}

pub struct EvidenceWriter;

impl EvidenceWriter {
    pub fn render(report: &EvidenceReport, format: ReportFormat) -> Result<String, StorageError> {
        match format {
            ReportFormat::Json => serde_json::to_string_pretty(report)
                .map_err(|e| StorageError::new("serialize", e.to_string())),
            ReportFormat::Jsonl => render_jsonl(report),
            ReportFormat::Markdown => Ok(render_markdown(report)),
        }
    }

    pub fn write(
        path: impl AsRef<Path>,
        report: &EvidenceReport,
        format: ReportFormat,
        force: bool,
    ) -> Result<(), StorageError> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() || !parent.is_dir() {
            return Err(StorageError::new(
                "parent_missing",
                format!("输出目录不存在：{}", parent.display()),
            ));
        }
        let content = Self::render(report, format)?;
        let mut options = OpenOptions::new();
        options.write(true);
        if force {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        let mut file = options.open(path).map_err(|e| {
            let code = if !force && e.kind() == io::ErrorKind::AlreadyExists {
                "output_exists"
            } else {
                "open_output"
            };
            StorageError::new(code, format!("无法写入 {}：{}", path.display(), e))
        })?;
        file.write_all(content.as_bytes())
            .map_err(|e| StorageError::new("write_output", e.to_string()))?;
        file.flush()
            .map_err(|e| StorageError::new("flush_output", e.to_string()))?;
        Ok(())
    }
}

pub fn write_runtime_report(
    collection: &RuntimeCollection,
    path: impl AsRef<Path>,
    format: ReportFormat,
    force: bool,
) -> Result<(), StorageError> {
    let report = EvidenceReport::from_runtime(collection);
    EvidenceWriter::write(path, &report, format, force)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
fn json_line(kind: &str, value: impl Serialize) -> Result<String, StorageError> {
    let mut object = serde_json::Map::new();
    object.insert(
        "kind".to_owned(),
        serde_json::Value::String(kind.to_owned()),
    );
    let value =
        serde_json::to_value(value).map_err(|e| StorageError::new("serialize", e.to_string()))?;
    if let serde_json::Value::Object(fields) = value {
        object.extend(fields);
    }
    serde_json::to_string(&object).map_err(|e| StorageError::new("serialize", e.to_string()))
}
fn render_jsonl(report: &EvidenceReport) -> Result<String, StorageError> {
    let s = &report.snapshot;
    let mut lines = vec![
        json_line(
            "metadata",
            serde_json::json!({"schema_version": report.schema_version, "generated_at": report.generated_at, "captured_at": report.captured_at, "source": report.source, "status": report.status}),
        )?,
        json_line("host", &s.host)?,
    ];
    lines.extend(
        s.gpus
            .iter()
            .map(|v| json_line("gpu", v))
            .collect::<Result<Vec<_>, _>>()?,
    );
    lines.extend(
        s.services
            .iter()
            .map(|v| json_line("service", v))
            .collect::<Result<Vec<_>, _>>()?,
    );
    if let Some(platform) = &s.platform {
        lines.push(json_line("platform", platform)?);
    }
    if let Some(logs) = &s.logs {
        lines.push(json_line("logs", logs)?);
    }
    lines.extend(
        s.findings
            .iter()
            .map(|v| json_line("finding", v))
            .collect::<Result<Vec<_>, _>>()?,
    );
    lines.extend(
        report
            .collection_issues
            .iter()
            .map(|v| json_line("issue", v))
            .collect::<Result<Vec<_>, _>>()?,
    );
    Ok(format!("{}\n", lines.join("\n")))
}

fn unknown<T: fmt::Display>(value: Option<T>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "未知".to_owned())
}
fn esc(value: impl fmt::Display) -> String {
    value
        .to_string()
        .replace('|', "\\|")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
fn status(status: HealthStatus) -> &'static str {
    status.label()
}
fn limited<T: fmt::Display>(values: &[T], max: usize) -> String {
    let mut result = values
        .iter()
        .take(max)
        .map(esc)
        .collect::<Vec<_>>()
        .join("、");
    if values.len() > max {
        result.push_str(&format!("（已截断，共 {} 项）", values.len()));
    }
    if result.is_empty() {
        "未知".to_owned()
    } else {
        result
    }
}
fn row(field: &str, value: impl fmt::Display, state: HealthStatus) -> String {
    format!("| {} | {} | {} |\n", esc(field), esc(value), status(state))
}
fn render_markdown(report: &EvidenceReport) -> String {
    let s = &report.snapshot;
    let mut out = format!("# 智算服务器证据报告\n\n- Schema：`{}`\n- 生成时间（Unix ms）：{}\n- 采集时间（Unix ms）：{}\n- 来源：{}\n- 总体状态：{}\n- 采集问题：{} 条\n\n", report.schema_version, report.generated_at, report.captured_at, s.source.label(), status(report.status), report.collection_issues.len());
    out.push_str("## 主机\n\n| 字段 | 值 | 状态 |\n| --- | --- | --- |\n");
    out.push_str(&row("主机名", &s.host.hostname, s.host.status));
    out.push_str(&row("系统", &s.host.os, s.host.status));
    out.push_str(&row(
        "内核",
        unknown(s.host.kernel_version.as_ref()),
        s.host.status,
    ));
    out.push_str(&row(
        "架构",
        unknown(s.host.architecture.as_ref()),
        s.host.status,
    ));
    out.push_str(&row(
        "CPU",
        unknown(s.host.cpu_model.as_ref()),
        s.host.cpu_status,
    ));
    out.push_str(&row(
        "逻辑 CPU",
        unknown(s.host.logical_cpu_count),
        s.host.cpu_status,
    ));
    out.push_str(&row(
        "内存 MiB",
        format!(
            "{}/{}",
            unknown(s.host.memory_used_mib),
            unknown(s.host.memory_total_mib)
        ),
        s.host.memory_status,
    ));
    out.push_str("\n## GPU\n\n| GPU | 名称 | PCI | 温度 | 利用率 | 显存 | 状态 |\n| --- | --- | --- | --- | --- | --- | --- |\n");
    for gpu in &s.gpus {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {}/{} MiB | {} |\n",
            gpu.index,
            esc(&gpu.name),
            esc(unknown(gpu.pci_address.as_ref())),
            unknown(gpu.temperature_celsius),
            unknown(gpu.utilization_percent),
            unknown(gpu.memory_used_mib),
            unknown(gpu.memory_total_mib),
            status(gpu.status)
        ));
    }
    if s.gpus.is_empty() {
        out.push_str("| - | 未发现 GPU 或采集不可用 | 未知 | 未知 | 未知 | 未知 | 未知 |\n");
    }
    out.push_str("\n## 服务\n\n| 服务 | 引擎 | 端点 | 可达 | 模型 | 状态 |\n| --- | --- | --- | --- | --- | --- |\n");
    for service in &s.services {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            esc(&service.name),
            service.engine.label(),
            esc(unknown(service.endpoint.as_ref())),
            unknown(service.endpoint_reachable),
            esc(unknown(service.model.as_ref())),
            status(service.status)
        ));
    }
    if s.services.is_empty() {
        out.push_str("| - | 未发现服务或发现能力不可用 | 未知 | 未知 | 未知 | 未知 |\n");
    }
    if let Some(platform) = &s.platform {
        render_platform(&mut out, platform);
    } else {
        out.push_str("\n## 平台\n\n平台采集结果：未知。\n");
    }
    if let Some(logs) = &s.logs {
        render_logs(&mut out, logs);
    } else {
        out.push_str("\n## 系统日志\n\n日志采集结果：未知。\n");
    }
    out.push_str("\n## 诊断\n\n");
    if s.findings.is_empty() {
        out.push_str("无诊断发现。\n");
    } else {
        for finding in &s.findings {
            out.push_str(&format!(
                "- [{}] `{}` / {}：{}；证据：{}\n",
                status(finding.status),
                esc(&finding.id),
                esc(&finding.object),
                esc(&finding.summary),
                limited(&finding.evidence, 4)
            ));
            if let Some(suggestion) = &finding.suggestion {
                out.push_str(&format!("  - 建议：{}\n", esc(suggestion)));
            }
        }
    }
    out.push_str("\n## 采集问题\n\n");
    if report.collection_issues.is_empty() {
        out.push_str("无顶层采集问题。\n");
    } else {
        for issue in &report.collection_issues {
            out.push_str(&format!(
                "- [{}] `{}` / `{}`：{}\n",
                status(issue.status),
                esc(&issue.collector),
                esc(&issue.code),
                esc(&issue.message)
            ));
        }
    }
    out
}
fn render_platform(out: &mut String, p: &crate::domain::PlatformSnapshot) {
    out.push_str("\n## 平台\n\n| 项目 | 值 | 状态 |\n| --- | --- | --- |\n");
    out.push_str(&row(
        "IOMMU 生效",
        unknown(p.iommu.effective),
        p.iommu.status,
    ));
    out.push_str(&row(
        "IOMMU 模式",
        unknown(p.iommu.effective_mode.as_ref()),
        p.iommu.status,
    ));
    out.push_str(&row(
        "ACS override",
        p.iommu
            .acs_override
            .as_ref()
            .map(|v| v.enabled)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "未知".to_owned()),
        p.iommu.status,
    ));
    out.push_str(&row(
        "NVIDIA 内核模块",
        unknown(p.nvidia_driver.kernel_module_version.as_ref()),
        p.nvidia_driver.status,
    ));
    out.push_str(&row(
        "NVIDIA-smi 驱动",
        unknown(p.nvidia_driver.nvidia_smi_driver_version.as_ref()),
        p.nvidia_driver.status,
    ));
    out.push_str(&row(
        "CUDA 驱动报告最高版本（非 Toolkit）",
        unknown(p.cuda.driver_reported_max_cuda.as_ref()),
        p.cuda.status,
    ));
    out.push_str(&row(
        "已安装 CUDA Toolkit（nvcc）",
        unknown(p.cuda.nvcc_toolkit_version.as_ref()),
        p.cuda.status,
    ));
    out.push_str(&row("PCIe 设备数量", p.pci_devices.len(), p.status));
    if let Some(summary) = &p.acs_summary {
        out.push_str(&format!(
            "PCIe ACS 状态：支持 {} 台，开启隔离 {} 台，全部关闭 {} 台\n\n",
            summary.supported, summary.enabled, summary.disabled
        ));
    }
    out.push_str("\n### PCIe / Switch\n\n| BDF | 角色 | 厂商/设备 | 父级 | 下游 | 链路（当前 / 最大） | ACS | MMIO 窗口 | 状态 |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for d in p.pci_devices.iter().take(32) {
        out.push_str(&format!(
            "| {} | {} | {}/{} | {} | {} | {} / {} | {} | {} | {} |\n",
            esc(&d.bdf),
            esc(unknown(d.role.map(|role| format!("{role:?}")))),
            esc(unknown(d.vendor_name.as_ref())),
            esc(unknown(d.device_name.as_ref())),
            esc(unknown(d.parent_bdf.as_ref())),
            limited(&d.downstream_bdfs, 4),
            esc(pcie_link_report(
                d.current_link_gen,
                d.current_link_speed.as_deref(),
                d.current_link_width.as_deref(),
                d.current_theoretical_bandwidth_mb_s,
            )),
            esc(pcie_link_report(
                d.max_link_gen,
                d.max_link_speed.as_deref(),
                d.max_link_width.as_deref(),
                d.max_theoretical_bandwidth_mb_s,
            )),
            esc(if d.acs.is_some() {
                "已观测"
            } else {
                "未知"
            }),
            esc(match d.mmio_windows.len() {
                0 => "无".to_owned(),
                count => {
                    let mmio64 = d
                        .mmio_windows
                        .iter()
                        .filter(|window| window.kind == PciMmioKind::Mmio64)
                        .count();
                    let mmio32 = d
                        .mmio_windows
                        .iter()
                        .filter(|window| window.kind == PciMmioKind::Mmio32)
                        .count();
                    let summary = if mmio64 > 0 {
                        format!("{mmio64}×64位")
                    } else {
                        String::new()
                    };
                    let summary = if mmio32 > 0 {
                        let mut parts = vec![format!("{mmio32}×32位")];
                        if !summary.is_empty() {
                            parts.push(summary);
                        }
                        parts.join("+")
                    } else if !summary.is_empty() {
                        summary
                    } else {
                        format!("{count}个")
                    };
                    summary
                }
            }),
            status(d.status)
        ));
    }
    if p.pci_devices.len() > 32 {
        out.push_str(&format!(
            "\n> PCIe 列表已截断，仅展示 32/{} 项。\n",
            p.pci_devices.len()
        ));
    }
    render_p2p(out, p.p2p.as_ref());
    if let Some(storage) = &p.storage {
        out.push_str("\n### 存储控制器\n\n| ID | 类型 | 型号 | 驱动 | 固件 | 虚拟盘 | 物理盘可见性 | 状态 |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for c in storage.controllers.iter().take(64) {
            out.push_str(&format!(
                "| {} | {:?} | {} | {} | {} | {} | {:?} | {} |\n",
                esc(&c.id),
                c.kind,
                esc(unknown(c.model.as_ref())),
                esc(unknown(c.driver.as_ref())),
                esc(unknown(c.firmware_version.as_ref())),
                unknown(c.virtual_drive_count),
                c.physical_drive_visibility,
                status(c.status)
            ));
        }
        if storage.controllers.is_empty() {
            out.push_str("| - | 未知 | 未发现控制器 | 未知 | 未知 | 未知 | 未知 | 未知 |\n");
        } else if storage.controllers.len() > 64 {
            out.push_str(&format!(
                "\n> 存储控制器列表已截断，仅展示 64/{} 项。\n",
                storage.controllers.len()
            ));
        }
        out.push_str("\n### 软件 RAID\n\n| 阵列 | 级别 | 盘数 | 降级数 | 同步动作 | 状态 |\n| --- | --- | --- | --- | --- | --- |\n");
        for a in storage.software_raid.iter().take(64) {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                esc(&a.name),
                esc(unknown(a.level.as_ref())),
                unknown(a.raid_disks),
                unknown(a.degraded),
                esc(unknown(a.sync_action.as_ref())),
                status(a.status)
            ));
        }
        if storage.software_raid.is_empty() {
            out.push_str("| - | 未发现 mdraid | 未知 | 未知 | 未知 | 未知 |\n");
        } else if storage.software_raid.len() > 64 {
            out.push_str(&format!(
                "\n> mdraid 列表已截断，仅展示 64/{} 项。\n",
                storage.software_raid.len()
            ));
        }
        out.push_str("\n### SAS PHY\n\n| PHY | 主机 | 链路速率 | 状态 | 错误计数 |\n| --- | --- | --- | --- | --- |\n");
        for phy in storage.sas_phys.iter().take(128) {
            let errors = [
                phy.invalid_dword_count,
                phy.running_disparity_error_count,
                phy.loss_of_dword_sync_count,
                phy.phy_reset_problem_count,
            ]
            .iter()
            .filter_map(|v| *v)
            .sum::<u64>();
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                esc(&phy.phy),
                esc(unknown(phy.sas_host.as_ref())),
                esc(unknown(phy.negotiated_link_rate.as_ref())),
                status(phy.status),
                errors
            ));
        }
        if storage.sas_phys.is_empty() {
            out.push_str("| - | 未发现 SAS PHY | 未知 | 未知 | 未知 |\n");
        } else if storage.sas_phys.len() > 128 {
            out.push_str(&format!(
                "\n> SAS PHY 列表已截断，仅展示 128/{} 项。\n",
                storage.sas_phys.len()
            ));
        }
    } else {
        out.push_str("\n### 存储\n\n存储控制器、mdraid、SAS：未知。\n");
    }
    if !p.plugins.is_empty() {
        out.push_str("\n### 插件\n\n| 插件 | 路径 | 状态 |\n| --- | --- | --- |\n");
        for plugin in &p.plugins {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                esc(&plugin.name),
                esc(unknown(plugin.path.as_ref())),
                status(plugin.status)
            ));
        }
    }
}

fn render_logs(out: &mut String, logs: &crate::domain::LogSnapshot) {
    out.push_str("\n## 系统日志\n\n");
    out.push_str(&format!("- 总体状态：{}\n", status(logs.status)));
    out.push_str("- 日志来源：\n");
    if logs.sources.is_empty() {
        out.push_str("  - 无可用来源\n");
    } else {
        for source in &logs.sources {
            out.push_str(&format!(
                "  - `{}`：{}；命令：{}；路径：{}；尾部 {} 行；异常 {} 条{}\n",
                esc(&source.name),
                source.probe_status.label(),
                unknown(source.command.as_ref()),
                unknown(source.path.as_ref()),
                source.lines_tail.len(),
                source.match_count,
                if source.truncated {
                    "（尾部截断）"
                } else {
                    ""
                }
            ));
        }
    }
    out.push_str("- 异常模式：\n");
    if logs.matches.is_empty() {
        out.push_str("  - 未检测到异常模式\n");
    } else {
        for matched in &logs.matches {
            out.push_str(&format!(
                "  - [{}] `{}`：命中 {} 次（{}）\n",
                status(matched.severity),
                esc(&matched.pattern),
                matched.count,
                esc(matched.sources.join("、"))
            ));
            for example in &matched.examples {
                out.push_str(&format!("    - `{}`\n", esc(example)));
            }
        }
    }
}

fn render_p2p(out: &mut String, p2p: Option<&crate::domain::P2pSnapshot>) {
    out.push_str("\n### GPU P2P\n\n");
    let Some(p2p) = p2p else {
        out.push_str("P2P 拓扑与能力：未知。\n");
        return;
    };
    out.push_str("`nvidia-smi topo` 表示驱动识别的能力/路径，不是带宽实测。\n\n");
    out.push_str("| 源 GPU | 目标 GPU | 路径 | 读 | 写 | PCIe P2P | NVLink P2P | 原子操作 |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for link in p2p.links.iter().take(128) {
        out.push_str(&format!(
            "| GPU{} | GPU{} | {} | {} | {} | {} | {} | {} |\n",
            link.source_gpu,
            link.target_gpu,
            esc(link.topology_path.as_deref().unwrap_or("未知")),
            link.read.label(),
            link.write.label(),
            link.pcie.label(),
            link.nvlink.label(),
            link.atomics.label()
        ));
    }
    if p2p.links.is_empty() {
        out.push_str("| - | - | 未发现可比较的 GPU pair | 未知 | 未知 | 未知 | 未知 | 未知 |\n");
    } else if p2p.links.len() > 128 {
        out.push_str(&format!(
            "\n> P2P 列表已截断，仅展示 128/{} 项。\n",
            p2p.links.len()
        ));
    }

    let benchmark = &p2p.benchmark;
    out.push_str(&format!(
        "\n#### P2P 实测速率\n\n- 状态：{}\n- 工具：`{}`\n- 测试：`{}`\n",
        benchmark.status.label(),
        esc(&benchmark.tool),
        esc(&benchmark.testcase)
    ));
    if let Some(description) = &benchmark.direction_description {
        out.push_str(&format!("- 方向定义：{}\n", esc(description)));
    }
    if benchmark.measurements.is_empty() {
        out.push_str(
            "\n未保存实测速率；只有显式 `--p2p-benchmark`/`p2p --benchmark` 才运行 GPU 负载。\n",
        );
    } else {
        out.push_str("\n| 源 GPU | 目标 GPU | 实测 GB/s |\n| --- | --- | --- |\n");
        for measurement in benchmark.measurements.iter().take(128) {
            out.push_str(&format!(
                "| GPU{} | GPU{} | {:.2} |\n",
                measurement.source_gpu, measurement.target_gpu, measurement.gigabytes_per_second
            ));
        }
    }
}

fn pcie_link_report(
    generation: Option<u8>,
    speed: Option<&str>,
    width: Option<&str>,
    bandwidth_mb_s: Option<u64>,
) -> String {
    let generation = generation.map_or("Gen?".to_owned(), |value| format!("Gen{value}"));
    let speed = speed.unwrap_or("速率未知");
    let width = width.map_or("x?".to_owned(), |value| {
        if value.starts_with(['x', 'X']) {
            value.to_owned()
        } else {
            format!("x{value}")
        }
    });
    let bandwidth = bandwidth_mb_s.map_or("理论带宽未知".to_owned(), |value| {
        format!("理论单向 {:.2} GB/s", value as f64 / 1000.0)
    });
    format!("{generation} {speed} {width} {bandwidth}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        collectors::demo,
        domain::{
            CollectionIssue, GpuP2pLinkSnapshot, HealthStatus, P2pBandwidthMeasurement,
            P2pBenchmarkSnapshot, P2pBenchmarkStatus, P2pCapabilityStatus, P2pSnapshot,
        },
    };
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn report() -> EvidenceReport {
        EvidenceReport::from_runtime(&RuntimeCollection {
            snapshot: demo::snapshot(),
            issues: vec![CollectionIssue::unavailable("test", "fixture", "a|b\nc")],
            status: HealthStatus::Warning,
        })
    }
    fn temp_dir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "suanctl-report-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        p
    }

    #[test]
    fn three_formats_have_complete_sections() {
        let r = report();
        let json = EvidenceWriter::render(&r, ReportFormat::Json).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["snapshot"]["host"]
                .is_object()
        );
        let jsonl = EvidenceWriter::render(&r, ReportFormat::Jsonl).unwrap();
        assert!(jsonl.lines().count() >= 7);
        let markdown = EvidenceWriter::render(&r, ReportFormat::Markdown).unwrap();
        for section in ["主机", "GPU", "服务", "平台", "诊断", "采集问题"] {
            assert!(markdown.contains(section));
        }
        assert!(markdown.contains("\\|"));
        assert!(markdown.contains("\\n"));
    }

    #[test]
    fn markdown_p2p_separates_driver_capability_theory_and_measured_rate() {
        let p2p = P2pSnapshot {
            gpu_indices: vec![0, 1],
            links: vec![GpuP2pLinkSnapshot {
                source_gpu: 0,
                target_gpu: 1,
                topology_path: Some("PIX".to_owned()),
                read: P2pCapabilityStatus::Supported,
                write: P2pCapabilityStatus::Supported,
                pcie: P2pCapabilityStatus::Supported,
                nvlink: P2pCapabilityStatus::NotSupported,
                atomics: P2pCapabilityStatus::Unknown,
            }],
            benchmark: P2pBenchmarkSnapshot {
                status: P2pBenchmarkStatus::Succeeded,
                tool: "nvbandwidth".to_owned(),
                testcase: "device_to_device_memcpy_write_ce".to_owned(),
                measured_at: Some(123),
                direction_description: Some("GPU(row) <- GPU(column)".to_owned()),
                measurements: vec![P2pBandwidthMeasurement {
                    source_gpu: 0,
                    target_gpu: 1,
                    gigabytes_per_second: 25.5,
                }],
                message: None,
            },
            status: HealthStatus::Healthy,
        };
        let mut markdown = String::new();
        render_p2p(&mut markdown, Some(&p2p));
        assert!(markdown.contains("驱动识别的能力/路径，不是带宽实测"));
        assert!(markdown.contains("GPU0 | GPU1 | 25.50"));
        assert!(markdown.contains("NVLink P2P"));
    }

    #[test]
    fn pcie_report_labels_nominal_one_direction_bandwidth() {
        let text = pcie_link_report(Some(4), Some("16.0 GT/s"), Some("x16"), Some(31_508));
        assert!(text.contains("Gen4"));
        assert!(text.contains("理论单向 31.51 GB/s"));
    }
    #[test]
    fn jsonl_lines_are_json_with_stable_kinds() {
        let lines = EvidenceWriter::render(&report(), ReportFormat::Jsonl).unwrap();
        let kinds = lines
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<std::collections::BTreeSet<_>>();
        for kind in ["metadata", "host", "gpu", "service", "finding", "issue"] {
            assert!(kinds.contains(kind));
        }
    }
    #[test]
    fn create_new_rejects_overwrite_and_force_allows_it() {
        let dir = temp_dir();
        let path = dir.join("report.json");
        let r = report();
        EvidenceWriter::write(&path, &r, ReportFormat::Json, false).unwrap();
        assert_eq!(
            EvidenceWriter::write(&path, &r, ReportFormat::Json, false)
                .unwrap_err()
                .code,
            "output_exists"
        );
        EvidenceWriter::write(&path, &r, ReportFormat::Markdown, true).unwrap();
        assert!(fs::read_to_string(path).unwrap().starts_with("# 智算"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_parent_is_error() {
        let path = std::env::temp_dir().join("suanctl-report-parent-does-not-exist/report.json");
        let e = EvidenceWriter::write(path, &report(), ReportFormat::Json, false).unwrap_err();
        assert_eq!(e.code, "parent_missing");
    }
}
