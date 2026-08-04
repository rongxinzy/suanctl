use std::fmt;

use crate::domain::{
    CollectionIssue, DashboardSnapshot, GpuSnapshot, HostSnapshot, PlatformSnapshot,
    ServiceSnapshot,
};

pub mod command;
pub mod cuda;
pub mod demo;
pub mod gpu;
pub mod host;
pub mod p2p;
pub mod pcie;
pub mod platform;
pub mod raid;
pub mod runtime;
pub mod sas;
pub mod storage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorError {
    pub collector: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl CollectorError {
    pub fn unavailable(collector: &'static str, message: impl Into<String>) -> Self {
        Self::new(collector, "unavailable", message)
    }

    pub fn new(collector: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            collector,
            code,
            message: message.into(),
        }
    }

    pub fn issue(&self) -> CollectionIssue {
        CollectionIssue {
            collector: self.collector.to_owned(),
            code: self.code.to_owned(),
            status: crate::domain::HealthStatus::Unavailable,
            message: self.message.clone(),
        }
    }
}

impl fmt::Display for CollectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.collector, self.message)
    }
}

impl std::error::Error for CollectorError {}

pub trait HostCollector {
    fn collect_host(&self) -> Result<HostSnapshot, CollectorError>;
}

pub trait GpuCollector {
    fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError>;
}

pub trait ServiceCollector {
    fn collect_services(&self) -> Result<Vec<ServiceSnapshot>, CollectorError>;
}

pub trait PlatformCollector {
    fn collect_platform(&self) -> Result<PlatformSnapshot, CollectorError>;
}

pub trait StorageCollector {
    fn collect_storage(
        &self,
        pci_devices: &[crate::domain::PciDeviceSnapshot],
    ) -> crate::domain::StorageFabricSnapshot;
}

pub trait DashboardCollector: HostCollector + GpuCollector + ServiceCollector {
    fn collect_dashboard(&self) -> Result<DashboardSnapshot, CollectorError>;
}

/// 显式占位实现：供调用方在未配置某类 collector 时返回明确不可用。
pub struct UnavailableCollector;

impl HostCollector for UnavailableCollector {
    fn collect_host(&self) -> Result<HostSnapshot, CollectorError> {
        Err(CollectorError::unavailable(
            "host",
            "当前调用路径未配置主机采集器",
        ))
    }
}

impl GpuCollector for UnavailableCollector {
    fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
        Err(CollectorError::unavailable(
            "gpu",
            "当前调用路径未配置 NVIDIA GPU 采集器",
        ))
    }
}

impl ServiceCollector for UnavailableCollector {
    fn collect_services(&self) -> Result<Vec<ServiceSnapshot>, CollectorError> {
        Err(CollectorError::unavailable(
            "service",
            "当前调用路径未配置服务发现器",
        ))
    }
}

impl PlatformCollector for UnavailableCollector {
    fn collect_platform(&self) -> Result<PlatformSnapshot, CollectorError> {
        Err(CollectorError::unavailable(
            "platform",
            "当前调用路径未配置平台采集器",
        ))
    }
}

impl DashboardCollector for UnavailableCollector {
    fn collect_dashboard(&self) -> Result<DashboardSnapshot, CollectorError> {
        Err(CollectorError::unavailable(
            "dashboard",
            "当前调用路径未配置 dashboard 聚合器",
        ))
    }
}
