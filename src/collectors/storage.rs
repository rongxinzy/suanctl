//! Storage-fabric aggregation for PCIe controllers, SCSI/SAS and mdraid.
//!
//! This module is deliberately read-only. It combines already-collected PCIe
//! evidence with sysfs/proc observations and best-effort vendor CLI inventory;
//! it does not infer a physical disk health state from a block device alone.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::domain::{
    DriveVisibility, HealthStatus, PciDeviceRole, PciDeviceSnapshot, StorageControllerKind,
    StorageControllerSnapshot, StorageFabricSnapshot,
};

use super::command::{
    CommandRunner, ProcessCommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT,
};
use super::raid::{collect_software_raid, VendorCliCollector, VendorControllerObservation};
use super::sas::collect_sas_sysfs;
use super::{CollectorError, StorageCollector};

pub const DEFAULT_STORAGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct LinuxStorageCollector<R = ProcessCommandRunner> {
    runner: R,
    proc_root: PathBuf,
    sysfs_root: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl LinuxStorageCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            proc_root: PathBuf::from("/proc"),
            sysfs_root: PathBuf::from("/sys"),
            timeout: DEFAULT_STORAGE_COMMAND_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }
}

impl Default for LinuxStorageCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> LinuxStorageCollector<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            proc_root: PathBuf::from("/proc"),
            sysfs_root: PathBuf::from("/sys"),
            timeout: DEFAULT_STORAGE_COMMAND_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_proc_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.proc_root = root.into();
        self
    }

    pub fn with_sysfs_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.sysfs_root = root.into();
        self
    }

    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        self.proc_root = root.join("proc");
        self.sysfs_root = root.join("sys");
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
}

impl<R: CommandRunner + Clone> LinuxStorageCollector<R> {
    pub fn collect_snapshot(&self, pci_devices: &[PciDeviceSnapshot]) -> StorageFabricSnapshot {
        let mut issues = Vec::new();
        let sas = collect_sas_sysfs(&self.sysfs_root);
        issues.extend(sas.issues);
        let (software_raid, raid_issues) = collect_software_raid(&self.proc_root, &self.sysfs_root);
        issues.extend(raid_issues);
        let vendor_collector = VendorCliCollector::new(self.runner.clone())
            .with_timeout(self.timeout)
            .with_output_limits(self.stdout_limit, self.stderr_limit);
        let (vendor_clis, vendor_observations, vendor_issues) = vendor_collector.collect();
        issues.extend(vendor_issues);

        let mut controllers = controllers_from_pci(pci_devices);
        merge_scsi_hosts(&mut controllers, &sas.scsi_hosts);
        merge_vendor_observations(&mut controllers, &vendor_observations);
        add_orphan_hosts(&mut controllers, &sas.scsi_hosts);
        let status = combine_storage_status(
            controllers
                .iter()
                .map(|controller| controller.status)
                .chain(sas.scsi_hosts.iter().map(|host| host.status))
                .chain(sas.sas_phys.iter().map(|phy| phy.status))
                .chain(software_raid.iter().map(|array| array.status))
                // An absent optional vendor CLI is a capability gap, not a
                // storage-health failure. Executable failures still surface
                // through the collector issue/status paths above.
                .chain(
                    vendor_clis
                        .iter()
                        .filter(|cli| cli.status != HealthStatus::Unavailable)
                        .map(|cli| cli.status),
                )
                .chain(issues.iter().map(|issue| issue.status)),
        );
        StorageFabricSnapshot {
            controllers,
            scsi_hosts: sas.scsi_hosts,
            sas_phys: sas.sas_phys,
            software_raid,
            vendor_clis,
            issues,
            status,
        }
    }
}

impl<R: CommandRunner + Clone> StorageCollector for LinuxStorageCollector<R> {
    fn collect_storage(&self, pci_devices: &[PciDeviceSnapshot]) -> StorageFabricSnapshot {
        self.collect_snapshot(pci_devices)
    }
}

/// Keeps the controller role based on the PCI class code. In particular, a
/// SAS controller is not automatically declared a RAID controller.
pub fn controllers_from_pci(pci_devices: &[PciDeviceSnapshot]) -> Vec<StorageControllerSnapshot> {
    pci_devices
        .iter()
        .filter_map(|device| {
            let kind = match device.role? {
                PciDeviceRole::Raid => StorageControllerKind::Raid,
                PciDeviceRole::Sas => StorageControllerKind::Sas,
                PciDeviceRole::Sata => StorageControllerKind::Sata,
                PciDeviceRole::Nvme => StorageControllerKind::Nvme,
                PciDeviceRole::Scsi => StorageControllerKind::Scsi,
                PciDeviceRole::Bridge | PciDeviceRole::Other => return None,
            };
            let physical_drive_visibility = if kind == StorageControllerKind::Raid {
                DriveVisibility::Opaque
            } else {
                DriveVisibility::Unknown
            };
            Some(StorageControllerSnapshot {
                id: format!("pci:{}", device.bdf),
                bdf: Some(device.bdf.clone()),
                kind,
                vendor: device.vendor_name.clone().or_else(|| device.vendor.clone()),
                model: device.device_name.clone(),
                driver: device.driver.clone(),
                firmware_version: None,
                virtual_drive_count: None,
                physical_drive_count: None,
                physical_drive_visibility,
                evidence: Vec::new(),
                source: vec!["pci_sysfs".to_owned()],
                status: device.status,
            })
        })
        .collect()
}

fn merge_scsi_hosts(
    controllers: &mut [StorageControllerSnapshot],
    hosts: &[crate::domain::ScsiHostSnapshot],
) {
    for host in hosts {
        let Some(bdf) = host.bdf.as_deref() else {
            continue;
        };
        if let Some(controller) = controllers
            .iter_mut()
            .find(|controller| controller.bdf.as_deref() == Some(bdf))
        {
            controller.evidence.push(format!("scsi_host:{}", host.host));
            if controller.driver.is_none() {
                controller.driver = host.proc_name.clone();
            }
            if controller.kind == StorageControllerKind::Sas {
                controller.kind = StorageControllerKind::Hba;
            }
            unique_push(&mut controller.source, "scsi_sysfs");
            controller.status = combine_storage_status([controller.status, host.status]);
        }
    }
}

fn merge_vendor_observations(
    controllers: &mut [StorageControllerSnapshot],
    observations: &[VendorControllerObservation],
) {
    for (index, observation) in observations.iter().enumerate() {
        let target_index = observation
            .controller_id
            .as_deref()
            .and_then(|id| {
                controllers
                    .iter()
                    .position(|controller| controller.id.ends_with(id))
            })
            .or_else(|| {
                controllers
                    .iter()
                    .enumerate()
                    .filter(|(_, controller)| {
                        controller.kind == StorageControllerKind::Raid
                            || controller.kind == StorageControllerKind::Hba
                    })
                    .nth(index)
                    .map(|(controller_index, _)| controller_index)
            });
        let Some(target_index) = target_index else {
            continue;
        };
        let controller = &mut controllers[target_index];
        if controller.model.is_none() {
            controller.model = observation.model.clone();
        }
        if controller.firmware_version.is_none() {
            controller.firmware_version = observation.firmware_version.clone();
        }
        controller.virtual_drive_count = observation
            .virtual_drive_count
            .or(controller.virtual_drive_count);
        controller.physical_drive_count = observation
            .physical_drive_count
            .or(controller.physical_drive_count);
        if observation.physical_drive_count.is_some() {
            controller.physical_drive_visibility = DriveVisibility::Visible;
        }
        unique_push(&mut controller.source, &observation.provider);
    }
}

fn add_orphan_hosts(
    controllers: &mut Vec<StorageControllerSnapshot>,
    hosts: &[crate::domain::ScsiHostSnapshot],
) {
    for host in hosts {
        if host.bdf.is_some()
            && controllers
                .iter()
                .any(|controller| controller.bdf == host.bdf)
        {
            continue;
        }
        controllers.push(StorageControllerSnapshot {
            id: format!("scsi:{}", host.host),
            bdf: host.bdf.clone(),
            kind: if host.sas_host.is_some() {
                StorageControllerKind::Hba
            } else {
                StorageControllerKind::Scsi
            },
            vendor: None,
            model: None,
            driver: host.proc_name.clone(),
            firmware_version: host.firmware_version.clone(),
            virtual_drive_count: None,
            physical_drive_count: None,
            physical_drive_visibility: DriveVisibility::Unknown,
            evidence: vec![format!("scsi_host:{}", host.host)],
            source: vec!["scsi_sysfs".to_owned()],
            status: host.status,
        });
    }
}

fn unique_push(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_owned());
    }
}

fn combine_storage_status(statuses: impl IntoIterator<Item = HealthStatus>) -> HealthStatus {
    let mut has_unavailable = false;
    let mut has_unknown = false;
    for status in statuses {
        match status {
            HealthStatus::Critical => return HealthStatus::Critical,
            HealthStatus::Warning => return HealthStatus::Warning,
            HealthStatus::Unavailable => has_unavailable = true,
            HealthStatus::Unknown => has_unknown = true,
            HealthStatus::Healthy => {}
        }
    }
    if has_unavailable {
        HealthStatus::Unavailable
    } else if has_unknown {
        HealthStatus::Unknown
    } else {
        HealthStatus::Healthy
    }
}

#[allow(dead_code)]
fn storage_evidence_set(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}

#[allow(dead_code)]
fn storage_error(message: impl Into<String>) -> CollectorError {
    CollectorError::new("storage", "collection_failed", message)
}

#[cfg(test)]
mod tests {
    use super::controllers_from_pci;
    use crate::domain::{
        DriveVisibility, HealthStatus, PciDeviceRole, PciDeviceSnapshot, StorageControllerKind,
    };

    fn device(role: PciDeviceRole) -> PciDeviceSnapshot {
        PciDeviceSnapshot {
            bdf: "0000:01:00.0".to_owned(),
            vendor: Some("0x1000".to_owned()),
            device: Some("0x005d".to_owned()),
            class: Some("0x010400".to_owned()),
            driver: Some("megaraid_sas".to_owned()),
            numa_node: Some(0),
            iommu_group: Some(7),
            current_link_speed: Some("16.0 GT/s".to_owned()),
            current_link_width: Some("x8".to_owned()),
            current_link_gen: Some(4),
            current_theoretical_bandwidth_mb_s: Some(15_754),
            max_link_speed: Some("16.0 GT/s".to_owned()),
            max_link_width: Some("x8".to_owned()),
            max_link_gen: Some(4),
            max_theoretical_bandwidth_mb_s: Some(15_754),
            acs: None,
            role: Some(role),
            class_name: None,
            vendor_name: Some("Broadcom/LSI".to_owned()),
            device_name: Some("MegaRAID".to_owned()),
            subsystem_vendor: None,
            subsystem_device: None,
            subsystem_name: None,
            parent_bdf: None,
            downstream_bdfs: Vec::new(),
            status: HealthStatus::Healthy,
        }
    }

    #[test]
    fn raid_controller_is_opaque_until_vendor_evidence_exposes_physical_disks() {
        let controllers = controllers_from_pci(&[device(PciDeviceRole::Raid)]);
        assert_eq!(controllers[0].kind, StorageControllerKind::Raid);
        assert_eq!(
            controllers[0].physical_drive_visibility,
            DriveVisibility::Opaque
        );
        assert_eq!(controllers[0].status, HealthStatus::Healthy);
    }
}
