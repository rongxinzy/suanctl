//! GPU 厂商画像：把厂商相关的话术、展示与诊断规则收敛到统一接口。
//!
//! GPU 厂商众多（NVIDIA、燧原 Enflame、海光 DCU……），各家的电源状态术语、
//! 错误语义和平台信息差异很大。接入新厂商 = 实现 `VendorProfile` 并在
//! `profile_for` 注册，UI / 报告 / 诊断层无需再写厂商分支。

use std::collections::BTreeSet;

use crate::diagnosis::{finding, finding_with_suggestion};
use crate::domain::{
    DashboardSnapshot, DiagnosisFinding, GpuSnapshot, HealthStatus, PlatformSnapshot,
};

/// 「平台与健康」段的一行事实：项目、值、状态。
pub type PlatformFact = (String, String, HealthStatus);

/// 厂商画像：所有随厂商变化的行为都从这里取，调用方不出现厂商名字符串。
pub trait VendorProfile {
    /// 厂商标识，与 `GpuSnapshot.vendor` 对应（"NVIDIA" / "Enflame"）。
    fn name(&self) -> &'static str;
    /// GPU 表「电源/性能状态」列的表头（NVIDIA 的 P 态、Enflame 的 DPM 等）。
    fn power_state_header(&self) -> &'static str;
    /// GPU 表「备注」列内容。
    fn gpu_remark(&self, gpu: &GpuSnapshot) -> String;
    /// 「平台与健康」段的厂商事实（TUI 与 Markdown 报告共用同一份数据）。
    fn platform_facts(
        &self,
        platform: Option<&PlatformSnapshot>,
        gpus: &[GpuSnapshot],
    ) -> Vec<PlatformFact>;
    /// 厂商诊断规则（只读，输入仅来自快照）。
    fn diagnose(&self, snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>);
}

/// 按 `GpuSnapshot.vendor` 取画像；未知厂商回退 Generic（不伪造任何厂商语义）。
pub fn profile_for(vendor: Option<&str>) -> &'static dyn VendorProfile {
    match vendor {
        Some("NVIDIA") => &NvidiaProfile,
        Some("Enflame") => &EnflameProfile,
        _ => &GenericProfile,
    }
}

/// 快照中实际出现的厂商画像（按首次出现顺序去重）。
pub fn present_profiles(gpus: &[GpuSnapshot]) -> Vec<&'static dyn VendorProfile> {
    let mut profiles: Vec<&'static dyn VendorProfile> = Vec::new();
    for gpu in gpus {
        let profile = profile_for(gpu.vendor.as_deref());
        if !profiles.iter().any(|p| p.name() == profile.name()) {
            profiles.push(profile);
        }
    }
    profiles
}

/// GPU 表「电源状态」列表头：单厂商用该厂商术语，混插/无卡用中性词。
pub fn power_state_header(gpus: &[GpuSnapshot]) -> &'static str {
    let profiles = present_profiles(gpus);
    if profiles.len() == 1 {
        profiles[0].power_state_header()
    } else {
        GenericProfile.power_state_header()
    }
}

/// 「平台与健康」段的厂商事实：单厂商直接展示，混插逐厂商加前缀，
/// 无 GPU 但平台层有 NVIDIA 痕迹（如驱动加载失败导致采不到卡）仍按 NVIDIA 展示。
pub fn platform_facts(
    platform: Option<&PlatformSnapshot>,
    gpus: &[GpuSnapshot],
) -> Vec<PlatformFact> {
    let mut profiles = present_profiles(gpus);
    if profiles.is_empty()
        && platform.is_some_and(|p| {
            p.nvidia_driver.module_loaded.is_some()
                || p.nvidia_driver.kernel_module_version.is_some()
        })
    {
        profiles.push(&NvidiaProfile);
    }
    let multi = profiles.len() > 1;
    profiles
        .into_iter()
        .flat_map(|profile| {
            profile
                .platform_facts(platform, gpus)
                .into_iter()
                .map(move |(item, value, status)| {
                    if multi {
                        (format!("[{}] {item}", profile.name()), value, status)
                    } else {
                        (item, value, status)
                    }
                })
        })
        .collect()
}

fn vendor_gpus<'a>(gpus: &'a [GpuSnapshot], vendor: &str) -> Vec<&'a GpuSnapshot> {
    gpus.iter()
        .filter(|gpu| gpu.vendor.as_deref() == Some(vendor))
        .collect()
}

// ---------------------------------------------------------------------------
// NVIDIA
// ---------------------------------------------------------------------------

pub struct NvidiaProfile;

impl VendorProfile for NvidiaProfile {
    fn name(&self) -> &'static str {
        "NVIDIA"
    }

    fn power_state_header(&self) -> &'static str {
        "P态"
    }

    fn gpu_remark(&self, gpu: &GpuSnapshot) -> String {
        let reset = gpu.reset_required.map_or("未知".to_owned(), |v| {
            if v {
                "需reset".to_owned()
            } else {
                "正常".to_owned()
            }
        });
        let xid = gpu.xid_codes.as_ref().map_or("Xid未知".into(), |codes| {
            if codes.is_empty() {
                String::from("无Xid")
            } else {
                let codes = codes
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("Xid:{codes}")
            }
        });
        format!("{reset}/{xid}")
    }

    fn platform_facts(
        &self,
        platform: Option<&PlatformSnapshot>,
        _gpus: &[GpuSnapshot],
    ) -> Vec<PlatformFact> {
        let Some(p) = platform else { return Vec::new() };
        let driver = &p.nvidia_driver;
        let cuda = &p.cuda;
        let unknown = |value: &Option<String>| value.clone().unwrap_or_else(|| "未知".to_owned());
        let module = match (driver.module_loaded, &driver.kernel_module_version) {
            (Some(true), Some(version)) => format!("已加载（{version}）"),
            (Some(true), None) => "已加载".to_owned(),
            (Some(false), _) => "未加载".to_owned(),
            (None, _) => "未知".to_owned(),
        };
        let smi = match (&driver.nvidia_smi_driver_version, driver.version_match) {
            (Some(version), Some(true)) => format!("{version}（与内核模块匹配）"),
            (Some(version), Some(false)) => format!("{version}（与内核模块不匹配）"),
            (Some(version), None) => version.clone(),
            (None, _) => "未知".to_owned(),
        };
        vec![
            ("NVIDIA 内核模块".to_owned(), module, driver.status),
            ("NVIDIA 驱动（nvidia-smi）".to_owned(), smi, driver.status),
            (
                "CUDA 驱动报告最高版本".to_owned(),
                unknown(&cuda.driver_reported_max_cuda),
                cuda.status,
            ),
            (
                "CUDA Toolkit（nvcc）".to_owned(),
                unknown(&cuda.nvcc_toolkit_version),
                cuda.status,
            ),
            (
                "libcuda / libcudart".to_owned(),
                format!(
                    "{} / {}",
                    cuda.libcuda.presence.label(),
                    cuda.libcudart.presence.label()
                ),
                cuda.status,
            ),
        ]
    }

    fn diagnose(&self, snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
        for gpu in vendor_gpus(&snapshot.gpus, "NVIDIA") {
            let object = format!("gpu-{}", gpu.index);
            if gpu.reset_required == Some(true) {
                findings.push(finding(
                    "gpu-reset-required",
                    &object,
                    HealthStatus::Critical,
                    "GPU 需要复位",
                    [format!("GPU {} reset_required=true", gpu.index)],
                ));
            }
            if let Some(codes) = gpu.xid_codes.as_ref().filter(|codes| !codes.is_empty()) {
                findings.push(finding(
                    "gpu-xid",
                    &object,
                    HealthStatus::Critical,
                    "GPU 存在 Xid 错误码",
                    [format!("GPU {} Xid={:?}", gpu.index, codes)],
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 燧原 Enflame
// ---------------------------------------------------------------------------

pub struct EnflameProfile;

impl VendorProfile for EnflameProfile {
    fn name(&self) -> &'static str {
        "Enflame"
    }

    fn power_state_header(&self) -> &'static str {
        "DPM"
    }

    fn gpu_remark(&self, gpu: &GpuSnapshot) -> String {
        let ecc = gpu.ecc_enabled.map_or("未知".to_owned(), |on| {
            if on { "开" } else { "关" }.to_owned()
        });
        let errors: u64 = gpu.error_details.values().sum();
        format!("ECC:{ecc} 错误:{errors}")
    }

    fn platform_facts(
        &self,
        _platform: Option<&PlatformSnapshot>,
        gpus: &[GpuSnapshot],
    ) -> Vec<PlatformFact> {
        let gpus = vendor_gpus(gpus, "Enflame");
        if gpus.is_empty() {
            return Vec::new();
        }
        let versions: BTreeSet<&str> = gpus
            .iter()
            .filter_map(|gpu| gpu.driver_version.as_deref())
            .collect();
        let driver = if versions.is_empty() {
            "未知".to_owned()
        } else {
            versions.into_iter().collect::<Vec<_>>().join("、")
        };

        let ecc_known: Vec<bool> = gpus.iter().filter_map(|gpu| gpu.ecc_enabled).collect();
        let ecc_on = ecc_known.iter().filter(|on| **on).count();
        let (ecc, ecc_status) = if ecc_known.is_empty() {
            ("未知".to_owned(), HealthStatus::Unknown)
        } else {
            let status = if ecc_on == ecc_known.len() {
                HealthStatus::Healthy
            } else {
                HealthStatus::Warning
            };
            (format!("开启 {}/{} 张", ecc_on, ecc_known.len()), status)
        };

        let mut total_errors = 0_u64;
        let mut categories: Vec<(&str, u64)> = Vec::new();
        for gpu in &gpus {
            for (category, count) in &gpu.error_details {
                if *count > 0 {
                    total_errors += count;
                    match categories.iter_mut().find(|(name, _)| name == category) {
                        Some((_, sum)) => *sum += count,
                        None => categories.push((category.as_str(), *count)),
                    }
                }
            }
        }
        let (errors, errors_status) = if total_errors == 0 {
            ("0".to_owned(), HealthStatus::Healthy)
        } else {
            let breakdown = categories
                .iter()
                .map(|(name, count)| format!("{name}={count}"))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!("总计 {total_errors}（{breakdown}）"),
                HealthStatus::Warning,
            )
        };

        let resets: u64 = gpus.iter().filter_map(|gpu| gpu.reset_count).sum();
        vec![
            (
                "Enflame 驱动（efsmi）".to_owned(),
                driver,
                HealthStatus::Unknown,
            ),
            ("ECC".to_owned(), ecc, ecc_status),
            ("分类错误计数".to_owned(), errors, errors_status),
            (
                "累计复位".to_owned(),
                format!("{resets} 次"),
                HealthStatus::Unknown,
            ),
        ]
    }

    fn diagnose(&self, snapshot: &DashboardSnapshot, findings: &mut Vec<DiagnosisFinding>) {
        for gpu in vendor_gpus(&snapshot.gpus, "Enflame") {
            let object = format!("gpu-{}", gpu.index);
            let nonzero: Vec<String> = gpu
                .error_details
                .iter()
                .filter(|(_, count)| **count > 0)
                .map(|(category, count)| format!("{category}={count}"))
                .collect();
            if !nonzero.is_empty() {
                findings.push(finding_with_suggestion(
                    "gpu-error-details",
                    &object,
                    HealthStatus::Warning,
                    "GPU 存在硬件错误计数（efsmi Error Details）",
                    [format!("GPU {} {}", gpu.index, nonzero.join(", "))],
                    "按类别排查：DRAM HBM/PCIE/SIP 等计数增长通常指向硬件、固件或链路问题，\
                     建议结合 efsmi 与厂商工具进一步定位",
                ));
            }
            if gpu.ecc_enabled == Some(false) {
                findings.push(finding_with_suggestion(
                    "gpu-ecc-disabled",
                    &object,
                    HealthStatus::Warning,
                    "GPU ECC 未开启",
                    [format!("GPU {} ECC Mode Current=Disable", gpu.index)],
                    "生产环境建议开启 ECC（经 efsmi/固件设置），关闭状态下显存位翻转无法被纠正",
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 未知 / 其他厂商
// ---------------------------------------------------------------------------

pub struct GenericProfile;

impl VendorProfile for GenericProfile {
    fn name(&self) -> &'static str {
        "Generic"
    }

    fn power_state_header(&self) -> &'static str {
        "电源状态"
    }

    fn gpu_remark(&self, gpu: &GpuSnapshot) -> String {
        gpu.smi_tool.clone().unwrap_or_else(|| "—".into())
    }

    fn platform_facts(
        &self,
        _platform: Option<&PlatformSnapshot>,
        gpus: &[GpuSnapshot],
    ) -> Vec<PlatformFact> {
        let tools: BTreeSet<&str> = gpus
            .iter()
            .filter_map(|gpu| gpu.smi_tool.as_deref())
            .collect();
        if tools.is_empty() {
            return Vec::new();
        }
        vec![(
            "GPU 采集工具".to_owned(),
            tools.into_iter().collect::<Vec<_>>().join("、"),
            HealthStatus::Unknown,
        )]
    }

    fn diagnose(&self, _snapshot: &DashboardSnapshot, _findings: &mut Vec<DiagnosisFinding>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::gcu::parse_efsmi_query;
    use crate::domain::{DataSource, HostSnapshot};
    use std::path::Path;

    const EFSMI_QUERY: &str = include_str!("collectors/fixtures/efsmi_query.txt");

    fn enflame_gpus() -> Vec<GpuSnapshot> {
        parse_efsmi_query(EFSMI_QUERY, Path::new("/nonexistent")).expect("fixture parses")
    }

    fn snapshot_with_gpus(gpus: Vec<GpuSnapshot>) -> DashboardSnapshot {
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
            gpus,
            services: Vec::new(),
            findings: Vec::new(),
            platform: None,
            logs: None,
            remote: None,
        }
    }

    #[test]
    fn power_state_header_follows_vendor() {
        assert_eq!(power_state_header(&enflame_gpus()), "DPM");
        let mut mixed = enflame_gpus();
        mixed[0].vendor = Some("NVIDIA".to_owned());
        assert_eq!(power_state_header(&mixed), "电源状态");
        assert_eq!(power_state_header(&[]), "电源状态");
    }

    #[test]
    fn enflame_facts_cover_driver_ecc_errors_and_resets() {
        let facts = platform_facts(None, &enflame_gpus());
        let items: Vec<&str> = facts.iter().map(|(item, _, _)| item.as_str()).collect();
        assert_eq!(
            items,
            ["Enflame 驱动（efsmi）", "ECC", "分类错误计数", "累计复位"]
        );
        assert_eq!(facts[0].1, "1.5.20260710");
        assert_eq!(facts[1].1, "开启 10/10 张");
        assert_eq!(facts[1].2, HealthStatus::Healthy);
        assert_eq!(facts[2].1, "0");
        assert_eq!(facts[2].2, HealthStatus::Healthy);
    }

    #[test]
    fn enflame_diagnose_flags_error_details_and_ecc_off() {
        let mut gpus = enflame_gpus();
        gpus.truncate(2);
        gpus[0].error_details.insert("SIP Error".to_owned(), 2);
        gpus[0].error_details.insert("PCIE Error".to_owned(), 1);
        gpus[1].ecc_enabled = Some(false);
        let snapshot = snapshot_with_gpus(gpus);
        let mut findings = Vec::new();
        EnflameProfile.diagnose(&snapshot, &mut findings);
        assert_eq!(findings.len(), 2);
        assert!(findings[0].id.starts_with("gpu-error-details.gpu-0"));
        assert!(findings[0].evidence[0].contains("SIP Error=2"));
        assert!(findings[1].id.starts_with("gpu-ecc-disabled.gpu-1"));
    }

    #[test]
    fn nvidia_diagnose_keeps_xid_and_reset_rules() {
        let mut gpus = enflame_gpus();
        gpus.truncate(1);
        gpus[0].vendor = Some("NVIDIA".to_owned());
        gpus[0].xid_codes = Some(vec![79]);
        gpus[0].reset_required = Some(true);
        let snapshot = snapshot_with_gpus(gpus);
        let mut findings = Vec::new();
        NvidiaProfile.diagnose(&snapshot, &mut findings);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.id.starts_with("gpu-xid.")));
        assert!(findings
            .iter()
            .any(|f| f.id.starts_with("gpu-reset-required.")));
    }
}
