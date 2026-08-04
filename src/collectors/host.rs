//! Linux 主机只读采集。
//!
//! 文件读取和文本解析分开：解析器只接收 [`HostText`]，因此固定 fixture
//! 测试不依赖运行平台，也不会触碰测试机器的真实 `/proc`。

use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{HealthStatus, HostSnapshot};

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
        parse_host_snapshot(&read_host_text(&self.root)?)
    }
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

    const CPUINFO: &str = include_str!("fixtures/host_cpuinfo.txt");
    const MEMINFO: &str = include_str!("fixtures/host_meminfo.txt");
    const OS_RELEASE: &str = include_str!("fixtures/host_os-release.txt");

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
}
