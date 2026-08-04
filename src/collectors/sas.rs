//! SCSI host and SAS PHY sysfs collection.
//!
//! The collector only reads `/sys/class/scsi_host`, `/sys/class/sas_host` and
//! `/sys/class/sas_phy`. Missing attributes are represented as `None`; a
//! missing SAS class is a capability gap, not evidence that the HBA is healthy.

use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::{CollectionIssue, HealthStatus, SasPhySnapshot, ScsiHostSnapshot};

use super::pcie::is_valid_bdf;

pub const DEFAULT_MAX_SCSI_HOSTS: usize = 4096;
pub const DEFAULT_MAX_SAS_PHYS: usize = 8192;

#[derive(Debug, Clone, PartialEq)]
pub struct SasCollection {
    pub scsi_hosts: Vec<ScsiHostSnapshot>,
    pub sas_phys: Vec<SasPhySnapshot>,
    pub issues: Vec<CollectionIssue>,
}

pub fn collect_sas_sysfs(root: &Path) -> SasCollection {
    let mut issues = Vec::new();
    let scsi_hosts = collect_scsi_hosts(root, &mut issues);
    let sas_hosts = collect_sas_host_names(root, &mut issues);
    let sas_phys = collect_sas_phys(root, &sas_hosts, &mut issues);
    let mut scsi_hosts = scsi_hosts;
    for host in &mut scsi_hosts {
        if host.sas_host.is_none() {
            host.sas_host = sas_hosts
                .iter()
                .find(|name| name.as_str() == host.host)
                .cloned();
        }
    }
    SasCollection {
        scsi_hosts,
        sas_phys,
        issues,
    }
}

fn collect_scsi_hosts(root: &Path, issues: &mut Vec<CollectionIssue>) -> Vec<ScsiHostSnapshot> {
    let path = root.join("class/scsi_host");
    let entries = match sorted_entries(&path, "sas_scsi_host_unavailable", issues) {
        Some(entries) => entries,
        None => return Vec::new(),
    };
    let mut hosts = Vec::new();
    for entry in entries.into_iter().take(DEFAULT_MAX_SCSI_HOSTS) {
        let Some(host) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if !host.starts_with("host") {
            continue;
        }
        let proc_name = read_first_text(&entry, &["proc_name"]);
        let state = read_first_text(&entry, &["state"]);
        let firmware_version = read_first_text(
            &entry,
            &[
                "version_fw",
                "fw_version",
                "firmware_version",
                "firmware_rev",
            ],
        );
        let canonical = canonical_or_original(&entry);
        let bdf = find_bdf_in_path(&canonical);
        let sas_host = find_sas_host_in_path(&canonical);
        let status = scsi_host_status(proc_name.as_deref(), state.as_deref());
        hosts.push(ScsiHostSnapshot {
            host,
            bdf,
            proc_name,
            state,
            firmware_version,
            sas_host,
            status,
        });
    }
    hosts
}

fn collect_sas_host_names(root: &Path, issues: &mut Vec<CollectionIssue>) -> Vec<String> {
    let path = root.join("class/sas_host");
    let Some(entries) = sorted_entries(&path, "sas_host_unavailable", issues) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .filter(|name| name.starts_with("host"))
        .take(DEFAULT_MAX_SCSI_HOSTS)
        .collect()
}

fn collect_sas_phys(
    root: &Path,
    sas_hosts: &[String],
    issues: &mut Vec<CollectionIssue>,
) -> Vec<SasPhySnapshot> {
    let path = root.join("class/sas_phy");
    let Some(entries) = sorted_entries(&path, "sas_phy_unavailable", issues) else {
        return Vec::new();
    };
    let mut phys = Vec::new();
    for entry in entries.into_iter().take(DEFAULT_MAX_SAS_PHYS) {
        let Some(phy) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if !phy.starts_with("phy") {
            continue;
        }
        let canonical = canonical_or_original(&entry);
        let bdf = find_bdf_in_path(&canonical);
        let sas_host = find_sas_host_in_path(&canonical).or_else(|| {
            sas_hosts.iter().find_map(|host| {
                canonical
                    .components()
                    .any(|component| component.as_os_str() == std::ffi::OsStr::new(host))
                    .then(|| host.clone())
            })
        });
        let negotiated_link_rate = read_first_text(
            &entry,
            &[
                "negotiated_linkrate",
                "negotiated_link_rate",
                "negotiated_rate",
            ],
        );
        let minimum_link_rate = read_first_text(
            &entry,
            &["minimum_linkrate", "minimum_link_rate", "minimum_rate"],
        );
        let maximum_link_rate = read_first_text(
            &entry,
            &["maximum_linkrate", "maximum_link_rate", "maximum_rate"],
        );
        let port_state = read_first_text(&entry, &["port/port_state", "port_state"]);
        let phy_state = read_first_text(&entry, &["phy_state"]);
        let port_identifier = read_first_text(&entry, &["port_identifier"]);
        let invalid_dword_count = read_counter(&entry, "invalid_dword_count");
        let running_disparity_error_count = read_counter(&entry, "running_disparity_error_count");
        let loss_of_dword_sync_count = read_counter(&entry, "loss_of_dword_sync_count");
        let phy_reset_problem_count = read_counter(&entry, "phy_reset_problem_count");
        let status = sas_phy_status(
            port_state.as_deref(),
            phy_state.as_deref(),
            [
                invalid_dword_count,
                running_disparity_error_count,
                loss_of_dword_sync_count,
                phy_reset_problem_count,
            ],
        );
        phys.push(SasPhySnapshot {
            phy,
            sas_host,
            bdf,
            port_identifier,
            port_state,
            phy_state,
            negotiated_link_rate,
            minimum_link_rate,
            maximum_link_rate,
            invalid_dword_count,
            running_disparity_error_count,
            loss_of_dword_sync_count,
            phy_reset_problem_count,
            status,
        });
    }
    phys
}

fn sorted_entries(
    path: &Path,
    unavailable_code: &str,
    issues: &mut Vec<CollectionIssue>,
) -> Option<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            issues.push(sas_issue(
                unavailable_code,
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", path.display()),
            ));
            return None;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => issues.push(sas_issue(
                "sas_sysfs_entry_unavailable",
                HealthStatus::Warning,
                format!("读取 {} 目录项失败：{error}", path.display()),
            )),
        }
    }
    paths.sort();
    Some(paths)
}

fn read_first_text(path: &Path, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        fs::read_to_string(path.join(name))
            .ok()
            .and_then(|value| scalar(&value))
    })
}

fn read_counter(path: &Path, name: &str) -> Option<u64> {
    read_first_text(path, &[name]).and_then(|value| parse_counter(&value))
}

pub fn parse_counter(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .find_map(|token| token.trim().parse::<u64>().ok())
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn find_bdf_in_path(path: &Path) -> Option<String> {
    path.components()
        .rev()
        .filter_map(|component| component.as_os_str().to_str())
        .find(|component| is_valid_bdf(component))
        .map(ToOwned::to_owned)
}

fn find_sas_host_in_path(path: &Path) -> Option<String> {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .find(|component| component.starts_with("host"))
        .map(ToOwned::to_owned)
}

fn scsi_host_status(proc_name: Option<&str>, state: Option<&str>) -> HealthStatus {
    if state.is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("offline") || value.contains("failed") || value.contains("blocked")
    }) {
        HealthStatus::Warning
    } else if proc_name.is_some() || state.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    }
}

fn sas_phy_status(
    port_state: Option<&str>,
    phy_state: Option<&str>,
    counters: [Option<u64>; 4],
) -> HealthStatus {
    if counters.into_iter().flatten().any(|counter| counter > 0) {
        return HealthStatus::Warning;
    }
    if port_state.is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("offline") || value.contains("failed")
    }) || phy_state.is_some_and(|value| value.to_ascii_lowercase().contains("failed"))
    {
        HealthStatus::Warning
    } else if port_state.is_some() || phy_state.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    }
}

fn sas_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "sas".to_owned(),
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

    use super::{collect_sas_sysfs, find_bdf_in_path, parse_counter, sas_phy_status};
    use crate::domain::HealthStatus;

    const MPT3SAS_PROC_NAME: &str = include_str!("fixtures/scsi_host_mpt3sas_proc_name.txt");
    const ONLINE_STATE: &str = include_str!("fixtures/scsi_host_online_state.txt");

    #[test]
    fn sas_counter_parser_does_not_turn_invalid_values_into_zero() {
        assert_eq!(parse_counter("42\n"), Some(42));
        assert_eq!(parse_counter("unknown\n"), None);
    }

    #[test]
    fn sas_phy_errors_raise_warning() {
        assert_eq!(
            sas_phy_status(
                Some("online"),
                Some("running"),
                [Some(0), Some(2), None, None]
            ),
            HealthStatus::Warning
        );
    }

    #[test]
    fn bdf_is_recovered_from_canonical_style_path() {
        let path =
            std::path::Path::new("/sys/devices/pci0000:00/0000:00:03.0/0000:03:00.0/host0/phy-3:0");
        assert_eq!(find_bdf_in_path(path).as_deref(), Some("0000:03:00.0"));
    }

    #[test]
    #[cfg(unix)]
    fn sas_sysfs_fixture_maps_mpt3sas_and_reports_phy_errors() {
        let root = fixture_root("sas-mpt3sas");
        let host_target = root.join("devices/pci0000:00/0000:02:00.0/host0");
        let target = host_target.join("phy-0:0");
        fs::create_dir_all(&target).expect("sas phy target");
        fs::write(host_target.join("proc_name"), MPT3SAS_PROC_NAME).expect("proc name");
        fs::write(host_target.join("state"), ONLINE_STATE).expect("host state");
        fs::write(target.join("negotiated_linkrate"), "12.0 Gbit\n").expect("rate");
        fs::write(target.join("minimum_linkrate"), "3.0 Gbit\n").expect("min rate");
        fs::write(target.join("maximum_linkrate"), "12.0 Gbit\n").expect("max rate");
        fs::write(target.join("port_state"), "online\n").expect("port state");
        fs::write(target.join("phy_state"), "running\n").expect("phy state");
        fs::write(target.join("invalid_dword_count"), "7\n").expect("invalid dword");
        fs::write(target.join("running_disparity_error_count"), "2\n").expect("disparity");
        fs::write(target.join("loss_of_dword_sync_count"), "0\n").expect("sync");
        fs::write(target.join("phy_reset_problem_count"), "1\n").expect("reset");
        fs::create_dir_all(root.join("class/scsi_host")).expect("scsi class");
        fs::create_dir_all(root.join("class/sas_host")).expect("sas host class");
        fs::create_dir_all(root.join("class/sas_phy")).expect("sas phy class");
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/0000:02:00.0/host0",
            root.join("class/scsi_host/host0"),
        )
        .expect("scsi host link");
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/0000:02:00.0/host0",
            root.join("class/sas_host/host0"),
        )
        .expect("sas host link");
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/0000:02:00.0/host0/phy-0:0",
            root.join("class/sas_phy/phy-0:0"),
        )
        .expect("sas phy link");

        let collection = collect_sas_sysfs(&root);
        assert_eq!(
            collection.scsi_hosts[0].proc_name.as_deref(),
            Some("mpt3sas")
        );
        assert_eq!(
            collection.scsi_hosts[0].bdf.as_deref(),
            Some("0000:02:00.0")
        );
        let phy = &collection.sas_phys[0];
        assert_eq!(phy.negotiated_link_rate.as_deref(), Some("12.0 Gbit"));
        assert_eq!(phy.invalid_dword_count, Some(7));
        assert_eq!(phy.status, HealthStatus::Warning);
        let _ = fs::remove_dir_all(root);
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
