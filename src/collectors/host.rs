//! Linux 主机只读采集。
//!
//! 文件读取和文本解析分开：解析器只接收 [`HostText`]，因此固定 fixture
//! 测试不依赖运行平台，也不会触碰测试机器的真实 `/proc`。

use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{DiskKind, DiskSnapshot, HealthStatus, HostSnapshot, NetInterfaceSnapshot};

use super::{CollectorError, HostCollector};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostText {
    pub hostname: Option<String>,
    pub os_release: Option<String>,
    pub kernel_version: Option<String>,
    pub architecture: Option<String>,
    pub cpuinfo: Option<String>,
    pub loadavg: Option<String>,
    pub meminfo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LinuxHostCollector {
    root: PathBuf,
}

impl Default for LinuxHostCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxHostCollector {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// 仅用于 Linux fixture/受控根目录；不会创建或修改任何文件。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn parse(text: &HostText) -> Result<HostSnapshot, CollectorError> {
        parse_host_snapshot(text)
    }

    pub fn read_text(&self) -> Result<HostText, CollectorError> {
        read_host_text(&self.root)
    }
}

impl HostCollector for LinuxHostCollector {
    fn collect_host(&self) -> Result<HostSnapshot, CollectorError> {
        if !cfg!(target_os = "linux") {
            return Err(CollectorError::new(
                "host",
                "unsupported_platform",
                "Linux 主机采集器只能在 Linux 上读取 /proc 和 /etc/os-release",
            ));
        }
        let mut snapshot = parse_host_snapshot(&read_host_text(&self.root)?)?;
        if self.root == Path::new("/") {
            enrich_host(&mut snapshot);
        }
        Ok(snapshot)
    }
}

/// 真机扩展盘点（fixture 根目录不触发）：块设备（lsblk）、内存条规格
/// （dmidecode，需 root）、网卡与 IP。全部只读；任一失败只留空，不影响
/// 基础快照。
fn enrich_host(snapshot: &mut HostSnapshot) {
    snapshot.disks = lsblk_disks();
    snapshot.memory_modules = dmidecode_memory_summary();
    snapshot.interfaces = interface_snapshots();
}

fn lsblk_disks() -> Vec<DiskSnapshot> {
    let Ok(output) = std::process::Command::new("lsblk")
        .args(["-b", "-J", "-o", "NAME,SIZE,TYPE,MODEL,FSTYPE,MOUNTPOINTS"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_lsblk_disks(&String::from_utf8_lossy(&output.stdout))
}

fn dmidecode_memory_summary() -> Option<String> {
    let output = std::process::Command::new("dmidecode")
        .args(["-t", "memory"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_dmidecode_memory(&String::from_utf8_lossy(&output.stdout))
}

fn interface_snapshots() -> Vec<NetInterfaceSnapshot> {
    crate::net::list_interfaces()
        .unwrap_or_default()
        .into_iter()
        .map(|iface| NetInterfaceSnapshot {
            config_mode: crate::net::netplan_mode(Path::new("/etc/netplan"), &iface.name)
                .map(str::to_owned),
            name: iface.name,
            mac: iface.mac,
            state: iface.state,
            addresses: iface.addresses,
        })
        .collect()
}

/// 解析 `lsblk -b -J -o NAME,SIZE,TYPE,MODEL,FSTYPE,MOUNTPOINTS`：只收
/// TYPE=disk 的整盘；系统盘 = 子树内挂载了 /；干净 = 无分区、无文件系统
/// 签名、未挂载。
pub fn parse_lsblk_disks(json: &str) -> Vec<DiskSnapshot> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(devices) = value.get("blockdevices").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    devices
        .iter()
        .filter(|device| device.get("type").and_then(|t| t.as_str()) == Some("disk"))
        .map(|device| {
            let name = device
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_owned();
            let model = device
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            let size_bytes = device.get("size").and_then(|v| v.as_u64());
            let fstype = device
                .get("fstype")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let mut mountpoints = mountpoints_of(device);
            let mut has_child = false;
            let mut child_has_fs = false;
            // 子树需递归遍历：根分区常在嵌套的 LVM/DM 设备上（part → lvm → /）。
            walk_children(device, &mut mountpoints, &mut has_child, &mut child_has_fs);
            let kind = if mountpoints.iter().any(|m| m == "/") {
                DiskKind::System
            } else {
                DiskKind::Data
            };
            let blank = fstype.is_none() && !has_child && !child_has_fs && mountpoints.is_empty();
            DiskSnapshot {
                name,
                model,
                size_bytes,
                kind,
                fstype,
                mountpoints,
                blank: Some(blank),
            }
        })
        .collect()
}

fn walk_children(
    device: &serde_json::Value,
    mountpoints: &mut Vec<String>,
    has_child: &mut bool,
    child_has_fs: &mut bool,
) {
    let Some(children) = device.get("children").and_then(|v| v.as_array()) else {
        return;
    };
    for child in children {
        *has_child = true;
        mountpoints.extend(mountpoints_of(child));
        if child.get("fstype").and_then(|v| v.as_str()).is_some() {
            *child_has_fs = true;
        }
        walk_children(child, mountpoints, has_child, child_has_fs);
    }
}

fn mountpoints_of(device: &serde_json::Value) -> Vec<String> {
    device
        .get("mountpoints")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|m| m.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 `dmidecode -t memory`：汇总已安装内存条为
/// "8×32GB DDR5 4800MT/s" 形式；空插槽与未知规格跳过，一个都没有返回 None。
pub fn parse_dmidecode_memory(text: &str) -> Option<String> {
    let mut modules: Vec<(String, String, String)> = Vec::new();
    let mut in_device = false;
    let mut size: Option<String> = None;
    let mut memory_type: Option<String> = None;
    let mut speed: Option<String> = None;
    let flush = |size: &mut Option<String>,
                 memory_type: &mut Option<String>,
                 speed: &mut Option<String>,
                 modules: &mut Vec<(String, String, String)>| {
        if let Some(value) = size.take() {
            if !value.contains("No Module") && !value.contains("Unknown") {
                modules.push((
                    value,
                    memory_type.take().unwrap_or_default(),
                    speed.take().unwrap_or_default(),
                ));
            }
        }
        memory_type.take();
        speed.take();
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Memory Device" {
            flush(&mut size, &mut memory_type, &mut speed, &mut modules);
            in_device = true;
            continue;
        }
        if !in_device {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Size:") {
            size = Some(rest.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("Type:") {
            memory_type = Some(rest.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("Speed:") {
            // "4800 MT/s" → "4800MT/s"
            speed = Some(rest.trim().replace(' ', ""));
        } else if trimmed.is_empty() {
            flush(&mut size, &mut memory_type, &mut speed, &mut modules);
            in_device = false;
        }
    }
    flush(&mut size, &mut memory_type, &mut speed, &mut modules);
    if modules.is_empty() {
        return None;
    }
    let mut groups: std::collections::BTreeMap<(String, String, String), usize> =
        std::collections::BTreeMap::new();
    for module in &modules {
        *groups.entry(module.clone()).or_insert(0) += 1;
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|((size, memory_type, speed), count)| {
            let spec = [memory_type.as_str(), speed.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty() && *part != "Unknown")
                .collect::<Vec<_>>()
                .join(" ");
            let size = size.replace(' ', "");
            if spec.is_empty() {
                format!("{count}×{size}")
            } else {
                format!("{count}×{size} {spec}")
            }
        })
        .collect();
    Some(parts.join(" + "))
}

pub fn read_host_text(root: &Path) -> Result<HostText, CollectorError> {
    Ok(HostText {
        hostname: Some(read_file(root, "proc/sys/kernel/hostname")?),
        os_release: Some(read_file(root, "etc/os-release")?),
        kernel_version: Some(read_file(root, "proc/sys/kernel/osrelease")?),
        architecture: Some(std::env::consts::ARCH.to_owned()),
        cpuinfo: Some(read_file(root, "proc/cpuinfo")?),
        loadavg: Some(read_file(root, "proc/loadavg")?),
        meminfo: Some(read_file(root, "proc/meminfo")?),
    })
}

pub fn parse_host_snapshot(text: &HostText) -> Result<HostSnapshot, CollectorError> {
    let hostname = text
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("未知主机")
        .to_owned();
    let os = parse_os_release(text.os_release.as_deref()).unwrap_or_else(|| "未知系统".to_owned());
    let kernel_version = scalar(text.kernel_version.as_deref());
    let architecture = scalar(text.architecture.as_deref());
    let (cpu_model, logical_cpu_count) = parse_cpuinfo(text.cpuinfo.as_deref());
    let load_1m = text
        .loadavg
        .as_deref()
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok());

    let (memory_used_mib, memory_total_mib) = parse_meminfo(text.meminfo.as_deref());
    let cpu_status = if logical_cpu_count.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    };
    let memory_status = if memory_total_mib.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    };

    Ok(HostSnapshot {
        hostname,
        os,
        kernel_version,
        architecture,
        cpu_model,
        logical_cpu_count,
        load_1m,
        memory_used_mib,
        memory_total_mib,
        memory_modules: None,
        disks: Vec::new(),
        interfaces: Vec::new(),
        status: combine_status(cpu_status, memory_status),
        cpu_status,
        memory_status,
    })
}

fn read_file(root: &Path, relative: &str) -> Result<String, CollectorError> {
    fs::read_to_string(root.join(relative)).map_err(|error| {
        CollectorError::new(
            "host",
            "read_failed",
            format!("读取 {relative} 失败：{error}"),
        )
    })
}

fn scalar(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_os_release(value: Option<&str>) -> Option<String> {
    let fields = value?.lines().filter_map(|line| {
        let (key, raw_value) = line.split_once('=')?;
        Some((key.trim(), unquote(raw_value.trim())))
    });
    let mut pretty = None;
    let mut name = None;
    for (key, value) in fields {
        match key {
            "PRETTY_NAME" => pretty = Some(value),
            "NAME" => name = Some(value),
            _ => {}
        }
    }
    pretty.or(name).filter(|value| !value.is_empty())
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
        .to_owned()
}

fn parse_cpuinfo(value: Option<&str>) -> (Option<String>, Option<u32>) {
    let Some(value) = value else {
        return (None, None);
    };
    let mut model = None;
    let mut processors = 0_u32;
    for line in value.lines() {
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "model name" | "Hardware" | "Processor" if model.is_none() => {
                model = scalar(Some(raw_value));
            }
            "processor" if raw_value.trim().parse::<u32>().is_ok() => {
                processors = processors.saturating_add(1);
            }
            _ => {}
        }
    }
    (model, (processors > 0).then_some(processors))
}

fn parse_meminfo(value: Option<&str>) -> (Option<u64>, Option<u64>) {
    let Some(value) = value else {
        return (None, None);
    };
    let mut values = std::collections::HashMap::new();
    for line in value.lines() {
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = raw_value.split_whitespace();
        let Some(number) = parts.next().and_then(|item| item.parse::<u64>().ok()) else {
            continue;
        };
        let kib = match parts.next() {
            Some("kB") | None => number,
            Some("mB") => number.saturating_mul(1024),
            _ => continue,
        };
        values.insert(key.trim(), kib);
    }

    let total_kib = values.get("MemTotal").copied();
    let available_kib = values
        .get("MemAvailable")
        .copied()
        .or_else(|| approximate_available_kib(&values));
    let total_mib = total_kib.map(kib_to_mib);
    let used_mib = total_kib
        .zip(available_kib)
        .map(|(total, available)| kib_to_mib(total.saturating_sub(available.min(total))));
    (used_mib, total_mib)
}

fn approximate_available_kib(values: &std::collections::HashMap<&str, u64>) -> Option<u64> {
    let free = values.get("MemFree").copied()?;
    Some(
        free.saturating_add(values.get("Buffers").copied().unwrap_or(0))
            .saturating_add(values.get("Cached").copied().unwrap_or(0)),
    )
}

fn kib_to_mib(value: u64) -> u64 {
    value / 1024
}

fn combine_status(left: HealthStatus, right: HealthStatus) -> HealthStatus {
    use HealthStatus::{Healthy, Unknown};
    if left == HealthStatus::Critical || right == HealthStatus::Critical {
        HealthStatus::Critical
    } else if left == HealthStatus::Warning || right == HealthStatus::Warning {
        HealthStatus::Warning
    } else if left == HealthStatus::Unavailable || right == HealthStatus::Unavailable {
        HealthStatus::Unavailable
    } else if left == Unknown || right == Unknown {
        Unknown
    } else {
        Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_host_snapshot, HostText};
    use crate::collectors::HostCollector;
    use crate::domain::DiskKind;

    const CPUINFO: &str = include_str!("fixtures/host_cpuinfo.txt");
    const MEMINFO: &str = include_str!("fixtures/host_meminfo.txt");
    const OS_RELEASE: &str = include_str!("fixtures/host_os-release.txt");

    #[test]
    fn parses_real_rx_box_host() {
        // 真实设备 rx-box（172.18.5.123）：Hygon C86 3350、16 逻辑 CPU、Ubuntu 24.04。
        let root = std::env::temp_dir().join(format!("suanctl-host-real-{}", std::process::id()));
        std::fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(
            root.join("proc/sys/kernel/hostname"),
            include_str!("fixtures/real-rx-box/rx_hostname.txt"),
        )
        .unwrap();
        std::fs::write(
            root.join("etc/os-release"),
            include_str!("fixtures/real-rx-box/rx_os_release.txt"),
        )
        .unwrap();
        std::fs::write(
            root.join("proc/sys/kernel/osrelease"),
            include_str!("fixtures/real-rx-box/rx_osrelease.txt"),
        )
        .unwrap();
        std::fs::write(
            root.join("proc/cpuinfo"),
            include_str!("fixtures/real-rx-box/rx_cpuinfo.txt"),
        )
        .unwrap();
        std::fs::write(
            root.join("proc/loadavg"),
            include_str!("fixtures/real-rx-box/rx_loadavg.txt"),
        )
        .unwrap();
        std::fs::write(
            root.join("proc/meminfo"),
            include_str!("fixtures/real-rx-box/rx_meminfo.txt"),
        )
        .unwrap();

        let snapshot = super::LinuxHostCollector::with_root(root.clone())
            .collect_host()
            .expect("real host chain");
        assert_eq!(snapshot.hostname, "rx-box");
        assert!(snapshot.os.contains("Ubuntu"), "os={}", snapshot.os);
        assert_eq!(snapshot.kernel_version.as_deref(), Some("6.8.0-90-generic"));
        assert_eq!(snapshot.logical_cpu_count, Some(16));
        let cpu_model = snapshot.cpu_model.as_deref().unwrap_or("");
        assert!(
            cpu_model.contains("Hygon C86 3350"),
            "cpu_model={cpu_model}"
        );
        assert_eq!(snapshot.load_1m, Some(0.94));
        let memory_gib = snapshot.memory_total_mib.unwrap_or(0) / 1024;
        assert!(
            (62..=68).contains(&memory_gib),
            "内存约 64GiB，实际 {memory_gib}GiB"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collectors_host_parses_normal_fixture() {
        let snapshot = parse_host_snapshot(&HostText {
            hostname: Some("gpu-node-01\n".to_owned()),
            os_release: Some(OS_RELEASE.to_owned()),
            kernel_version: Some("6.8.0-31-generic\n".to_owned()),
            architecture: Some("x86_64".to_owned()),
            cpuinfo: Some(CPUINFO.to_owned()),
            loadavg: Some("1.25 0.90 0.80 3/500 1234\n".to_owned()),
            meminfo: Some(MEMINFO.to_owned()),
        })
        .expect("host fixture");
        assert_eq!(snapshot.hostname, "gpu-node-01");
        assert_eq!(snapshot.os, "Ubuntu 24.04.1 LTS");
        assert_eq!(snapshot.logical_cpu_count, Some(2));
        assert_eq!(snapshot.memory_total_mib, Some(16384));
        assert_eq!(snapshot.memory_used_mib, Some(6144));
        assert_eq!(snapshot.load_1m, Some(1.25));
    }

    #[test]
    fn collectors_host_keeps_missing_optional_fields_unknown() {
        let snapshot = parse_host_snapshot(&HostText {
            hostname: None,
            os_release: Some("NAME=Linux\n".to_owned()),
            kernel_version: None,
            architecture: Some("aarch64".to_owned()),
            cpuinfo: Some("model name: test\n".to_owned()),
            loadavg: Some("not-a-load\n".to_owned()),
            meminfo: Some("MemFree: 10 kB\n".to_owned()),
        })
        .expect("partial host fixture");
        assert_eq!(snapshot.hostname, "未知主机");
        assert_eq!(snapshot.os, "Linux");
        assert_eq!(snapshot.kernel_version, None);
        assert_eq!(snapshot.logical_cpu_count, None);
        assert_eq!(snapshot.memory_total_mib, None);
        assert_eq!(snapshot.status.label(), "未知");
    }

    #[test]
    fn collectors_host_full_chain_from_injected_root() {
        // with_root 注入 fixture 根目录：验证 read_text + parse 完整链路，
        // 不触碰真实 /proc 与 /etc。
        let root = std::env::temp_dir().join(format!("suanctl-host-chain-{}", std::process::id()));
        std::fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(root.join("proc")).unwrap();
        std::fs::write(root.join("proc/sys/kernel/hostname"), "chain-test\n").unwrap();
        std::fs::write(root.join("etc/os-release"), OS_RELEASE).unwrap();
        std::fs::write(root.join("proc/sys/kernel/osrelease"), "6.8.0-fixture\n").unwrap();
        std::fs::write(root.join("proc/cpuinfo"), CPUINFO).unwrap();
        std::fs::write(root.join("proc/loadavg"), "0.50 0.30 0.20 1/10 99\n").unwrap();
        std::fs::write(root.join("proc/meminfo"), MEMINFO).unwrap();

        let collector = super::LinuxHostCollector::with_root(root.clone());
        let snapshot = collector.collect_host().expect("host chain");
        assert_eq!(snapshot.hostname, "chain-test");
        assert_eq!(snapshot.os, "Ubuntu 24.04.1 LTS");
        assert_eq!(snapshot.kernel_version.as_deref(), Some("6.8.0-fixture"));
        assert_eq!(snapshot.logical_cpu_count, Some(2));
        assert_eq!(snapshot.load_1m, Some(0.5));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collectors_host_reports_unreadable_root_structurally() {
        let collector = super::LinuxHostCollector::with_root("/nonexistent-suanctl-root");
        let error = collector.collect_host().expect_err("missing root");
        assert_eq!(error.code, "read_failed");
    }

    #[test]
    fn parse_lsblk_disks_marks_system_data_and_blank() {
        let json = r#"{"blockdevices": [
            {"name":"sda","size":480103981056,"type":"disk","model":"Samsung SSD 870 ","fstype":null,"mountpoints":[null],
             "children":[
                {"name":"sda1","size":1073741824,"type":"part","fstype":"vfat","mountpoints":["/boot/efi"]},
                {"name":"sda2","size":479030000000,"type":"part","fstype":"ext4","mountpoints":["/"]}
             ]},
            {"name":"sdb","size":1920383410176,"type":"disk","model":null,"fstype":null,"mountpoints":[null]},
            {"name":"sdc","size":1920383410176,"type":"disk","model":"ST2000NM","fstype":"xfs","mountpoints":[null],
             "children":[
                {"name":"sdc1","size":1920383000000,"type":"part","fstype":"xfs","mountpoints":["/data"]}
             ]},
            {"name":"sr0","size":1073741312,"type":"rom","model":null,"fstype":null,"mountpoints":[null]},
            {"name":"loop0","size":67108864,"type":"loop","model":null,"fstype":"squashfs","mountpoints":["/snap/core/1"]}
        ]}"#;
        let disks = super::parse_lsblk_disks(json);
        assert_eq!(disks.len(), 3, "rom/loop 不收");

        let sda = &disks[0];
        assert_eq!(sda.name, "sda");
        assert_eq!(sda.model.as_deref(), Some("Samsung SSD 870"));
        assert_eq!(sda.kind, DiskKind::System);
        assert_eq!(sda.blank, Some(false));
        assert!(sda.mountpoints.contains(&"/".to_owned()));

        let sdb = &disks[1];
        assert_eq!(sdb.kind, DiskKind::Data);
        assert_eq!(sdb.blank, Some(true), "无分区无签名即干净");

        let sdc = &disks[2];
        assert_eq!(sdc.kind, DiskKind::Data);
        assert_eq!(sdc.blank, Some(false), "有分区/文件系统不干净");
    }

    #[test]
    fn parse_lsblk_disks_finds_root_on_nested_lvm() {
        // 真实形态（172.18.4.199）：根分区在 part → lvm 的嵌套子树上。
        let json = r#"{"blockdevices": [
            {"name":"nvme0n1","size":4096805658624,"type":"disk","model":"BIWIN","fstype":null,"mountpoints":[null],
             "children":[
                {"name":"nvme0n1p1","size":1127219200,"type":"part","fstype":"vfat","mountpoints":["/boot/efi"]},
                {"name":"nvme0n1p3","size":4093528506368,"type":"part","fstype":null,"mountpoints":[null],
                 "children":[
                    {"name":"ubuntu--vg-lv--0","size":4093527457792,"type":"lvm","fstype":"ext4","mountpoints":["/srv/data","/"]}
                 ]}
             ]}
        ]}"#;
        let disks = super::parse_lsblk_disks(json);
        assert_eq!(disks.len(), 1);
        let disk = &disks[0];
        assert_eq!(disk.kind, DiskKind::System, "嵌套 LVM 上的 / 也要认");
        assert_eq!(disk.blank, Some(false));
        assert!(disk.mountpoints.contains(&"/".to_owned()));
    }

    #[test]
    fn parse_dmidecode_memory_summarizes_installed_modules() {
        let text = r#"
Handle 0x0000, DMI type 16, 23 bytes
Physical Memory Array
	Maximum Capacity: 2 TB

Handle 0x0001, DMI type 17, 84 bytes
Memory Device
	Array Handle: 0x0000
	Total Width: 64 bits
	Size: 32 GB
	Form Factor: DIMM
	Type: DDR5
	Speed: 4800 MT/s
	Manufacturer: Samsung
	Part Number: M321R4GA3BB6-CQK

Handle 0x0002, DMI type 17, 84 bytes
Memory Device
	Array Handle: 0x0000
	Size: 32 GB
	Type: DDR5
	Speed: 4800 MT/s

Handle 0x0003, DMI type 17, 84 bytes
Memory Device
	Array Handle: 0x0000
	Size: No Module Installed
	Type: Unknown
	Speed: Unknown
"#;
        assert_eq!(
            super::parse_dmidecode_memory(text).as_deref(),
            Some("2×32GB DDR5 4800MT/s")
        );
        assert_eq!(super::parse_dmidecode_memory(""), None);
    }
}
