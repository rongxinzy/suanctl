//! 真实运行时快照装配。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::collectors::{GpuCollector, HostCollector, PlatformCollector};
use crate::diagnosis::diagnose;
use crate::domain::{
    CollectionIssue, DashboardSnapshot, DataSource, EngineKind, HealthStatus, HostSnapshot,
    TimestampMillis,
};
use crate::engines::{
    adapter_for, merge_results, ConfiguredEndpoint, ConfiguredProvider, ContainerProvider,
    DiscoveryProvider, HostProcessProvider,
};

pub const MAX_SERVICES: usize = 64;
pub const MAX_PROBED_SERVICES: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCollection {
    pub snapshot: DashboardSnapshot,
    pub issues: Vec<CollectionIssue>,
    pub status: HealthStatus,
}

pub struct RuntimeCollector {
    host: Box<dyn HostCollector>,
    gpu: Box<dyn GpuCollector>,
    platform: Box<dyn PlatformCollector>,
    providers: Vec<Box<dyn DiscoveryProvider>>,
    pub configured_endpoints: Vec<ConfiguredEndpoint>,
    probe_services: bool,
}

impl Default for RuntimeCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeCollector {
    pub fn new() -> Self {
        Self {
            host: Box::new(crate::collectors::host::LinuxHostCollector::default()),
            gpu: Box::new(crate::collectors::gpu::NvidiaSmiCollector::default()),
            platform: Box::new(crate::collectors::platform::LinuxPlatformCollector::default()),
            providers: vec![
                Box::new(HostProcessProvider::default()),
                Box::new(ContainerProvider::default()),
            ],
            configured_endpoints: Vec::new(),
            probe_services: true,
        }
    }

    pub fn with_configured_endpoints(mut self, endpoints: Vec<ConfiguredEndpoint>) -> Self {
        if !endpoints.is_empty() {
            self.configured_endpoints.extend(endpoints.iter().cloned());
            self.providers
                .push(Box::new(ConfiguredProvider::new(endpoints)));
        }
        self
    }

    /// 供纯构造测试和上层受控装配使用；不会读取系统或启动探针。
    pub fn from_sources(
        host: Box<dyn HostCollector>,
        gpu: Box<dyn GpuCollector>,
        platform: Box<dyn PlatformCollector>,
        providers: Vec<Box<dyn DiscoveryProvider>>,
    ) -> Self {
        Self {
            host,
            gpu,
            platform,
            providers,
            configured_endpoints: Vec::new(),
            probe_services: false,
        }
    }

    pub fn collect(&self) -> RuntimeCollection {
        let captured_at = now_millis();
        let mut issues = Vec::new();

        let host = match self.host.collect_host() {
            Ok(host) => host,
            Err(error) => {
                issues.push(collection_issue(error.issue()));
                unavailable_host()
            }
        };
        let gpus = match self.gpu.collect_gpus() {
            Ok(gpus) => gpus,
            Err(error) => {
                issues.push(collection_issue(error.issue()));
                Vec::new()
            }
        };
        let platform = match self.platform.collect_platform() {
            Ok(platform) => Some(platform),
            Err(error) => {
                issues.push(collection_issue(error.issue()));
                None
            }
        };

        let discovery = merge_results(self.providers.iter().map(|provider| provider.discover()));
        issues.extend(discovery.issues.into_iter().map(collection_issue));
        let mut services = discovery.services;
        if services.len() > MAX_SERVICES {
            issues.push(collection_issue(CollectionIssue {
                collector: "service_discovery".to_owned(),
                code: "service_limit".to_owned(),
                status: HealthStatus::Warning,
                message: format!("发现服务超过上限，已限制为 {MAX_SERVICES} 项"),
            }));
            services.truncate(MAX_SERVICES);
        }

        if self.probe_services {
            probe_services_bounded(&mut services, &mut issues);
        }

        let mut snapshot = DashboardSnapshot {
            captured_at,
            source: DataSource::Runtime,
            host,
            gpus,
            services,
            findings: Vec::new(),
            platform,
        };
        snapshot.findings = diagnose(&snapshot);
        let status = runtime_status(&snapshot, &issues);
        RuntimeCollection {
            snapshot,
            issues,
            status,
        }
    }
}

fn probe_services_bounded(
    services: &mut [crate::domain::ServiceSnapshot],
    issues: &mut Vec<CollectionIssue>,
) {
    let jobs = services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.endpoint.is_some() && service.engine != EngineKind::Unknown)
        .map(|(index, service)| (index, service.clone()))
        .collect::<Vec<_>>();
    if jobs.len() > MAX_PROBED_SERVICES {
        issues.push(CollectionIssue {
            collector: "service_probe".to_owned(),
            code: "probe_limit".to_owned(),
            status: HealthStatus::Warning,
            message: format!("可探测服务超过上限，本轮只探测前 {MAX_PROBED_SERVICES} 项"),
        });
    }

    let results = std::thread::scope(|scope| {
        jobs.into_iter()
            .take(MAX_PROBED_SERVICES)
            .map(|(index, mut service)| {
                scope.spawn(move || {
                    let result = adapter_for(service.engine)
                        .map(|adapter| adapter.probe_service(&mut service));
                    (index, service, result)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect::<Vec<_>>()
    });

    for (index, service, result) in results {
        services[index] = service;
        if let Some(result) = result.filter(|result| result.status == HealthStatus::Unavailable) {
            issues.push(CollectionIssue {
                collector: "service_probe".to_owned(),
                code: "probe_unavailable".to_owned(),
                status: HealthStatus::Unavailable,
                message: format!(
                    "{} 服务端点不可用：{}",
                    services[index].name,
                    result.last_error.unwrap_or_default()
                ),
            });
        }
    }
}

fn collection_issue(issue: CollectionIssue) -> CollectionIssue {
    issue
}

fn unavailable_host() -> HostSnapshot {
    HostSnapshot {
        hostname: "未知主机".to_owned(),
        os: "未知系统".to_owned(),
        kernel_version: None,
        architecture: None,
        cpu_model: None,
        logical_cpu_count: None,
        load_1m: None,
        memory_used_mib: None,
        memory_total_mib: None,
        status: HealthStatus::Unavailable,
        cpu_status: HealthStatus::Unknown,
        memory_status: HealthStatus::Unknown,
    }
}

pub fn runtime_status(snapshot: &DashboardSnapshot, issues: &[CollectionIssue]) -> HealthStatus {
    let mut statuses = vec![snapshot.host.status];
    statuses.extend(snapshot.gpus.iter().map(|gpu| gpu.status));
    statuses.extend(snapshot.services.iter().map(|service| service.status));
    if let Some(platform) = &snapshot.platform {
        statuses.push(platform.status);
    } else {
        statuses.push(HealthStatus::Unavailable);
    }
    statuses.extend(snapshot.findings.iter().map(|finding| finding.status));
    statuses.extend(
        issues
            .iter()
            .filter(|issue| issue.code != "runtime_missing")
            .map(|issue| issue.status),
    );
    statuses
        .into_iter()
        .max_by_key(|status| status_rank(*status))
        .unwrap_or(HealthStatus::Unknown)
}

fn status_rank(status: HealthStatus) -> u8 {
    match status {
        HealthStatus::Healthy => 1,
        HealthStatus::Unknown => 2,
        HealthStatus::Unavailable => 3,
        HealthStatus::Warning => 4,
        HealthStatus::Critical => 5,
    }
}

pub fn now_millis() -> TimestampMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as TimestampMillis)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{RuntimeCollector, MAX_SERVICES};
    use crate::collectors::{CollectorError, GpuCollector, HostCollector, PlatformCollector};
    use crate::domain::{
        DataSource, DiscoveryConfidence, DiscoveryProvenance, DiscoverySource, DiscoverySourceKind,
        EngineKind, GpuSnapshot, HealthStatus, HostSnapshot, PlatformSnapshot, ProbeResult,
    };
    use crate::engines::{DiscoveryProvider, DiscoveryResult};

    struct FailingHost;
    impl HostCollector for FailingHost {
        fn collect_host(&self) -> Result<HostSnapshot, CollectorError> {
            Err(CollectorError::unavailable("host", "fixture host failure"))
        }
    }
    struct FailingGpu;
    impl GpuCollector for FailingGpu {
        fn collect_gpus(&self) -> Result<Vec<GpuSnapshot>, CollectorError> {
            Err(CollectorError::unavailable("gpu", "fixture gpu failure"))
        }
    }
    struct FailingPlatform;
    impl PlatformCollector for FailingPlatform {
        fn collect_platform(&self) -> Result<PlatformSnapshot, CollectorError> {
            Err(CollectorError::unavailable(
                "platform",
                "fixture platform failure",
            ))
        }
    }
    struct ManyServices;
    impl DiscoveryProvider for ManyServices {
        fn discover(&self) -> DiscoveryResult {
            let mut result = DiscoveryResult::default();
            for index in 0..(MAX_SERVICES + 1) {
                result.services.push(crate::domain::ServiceSnapshot {
                    name: format!("service-{index}"),
                    engine: EngineKind::Unknown,
                    model: None,
                    pid: None,
                    port: None,
                    status: HealthStatus::Unknown,
                    process_present: None,
                    endpoint: None,
                    endpoint_reachable: None,
                    health_probe: Default::default(),
                    models_probe: Default::default(),
                    metrics_probe: Default::default(),
                    observed_models: Vec::new(),
                    observed_metrics: Vec::new(),
                    gpu_indices: Vec::new(),
                    last_error: None,
                    discovery: Default::default(),
                });
            }
            result
        }
    }

    struct OneUnderspecifiedService;
    impl DiscoveryProvider for OneUnderspecifiedService {
        fn discover(&self) -> DiscoveryResult {
            DiscoveryResult {
                services: vec![crate::domain::ServiceSnapshot {
                    name: "fixture-vllm".to_owned(),
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
                    discovery: DiscoveryProvenance {
                        sources: vec![DiscoverySource {
                            kind: DiscoverySourceKind::HostProcess,
                            match_reason: "fixture".to_owned(),
                            evidence: vec!["fixture".to_owned()],
                            confidence: DiscoveryConfidence::High,
                            pid: Some(42),
                            container: None,
                        }],
                        endpoints: Vec::new(),
                    },
                }],
                issues: Vec::new(),
            }
        }
    }

    #[test]
    fn failed_components_still_return_snapshot_and_issues() {
        let result = RuntimeCollector::from_sources(
            Box::new(FailingHost),
            Box::new(FailingGpu),
            Box::new(FailingPlatform),
            Vec::new(),
        )
        .collect();
        assert_eq!(result.snapshot.source, crate::domain::DataSource::Runtime);
        assert_eq!(result.snapshot.host.logical_cpu_count, None);
        assert!(result.snapshot.gpus.is_empty());
        assert!(result.snapshot.platform.is_none());
        assert!(result.issues.len() >= 3);
        assert_eq!(result.status, HealthStatus::Unavailable);

        let report = crate::domain::DoctorReport::from_runtime(result);
        let value = serde_json::to_value(&report).expect("runtime report json");
        assert_eq!(value["schema_version"], "suanctl.doctor/v0.1");
        assert_eq!(value["source"], "runtime");
        assert!(value["snapshot"].is_object());
    }

    #[test]
    fn service_limit_is_applied_without_system_access() {
        let result = RuntimeCollector::from_sources(
            Box::new(FailingHost),
            Box::new(FailingGpu),
            Box::new(FailingPlatform),
            vec![Box::new(ManyServices)],
        )
        .collect();
        assert_eq!(result.snapshot.services.len(), MAX_SERVICES);
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "service_limit"));
    }

    #[test]
    fn diagnosis_is_written_back_to_runtime_snapshot() {
        let result = RuntimeCollector::from_sources(
            Box::new(FailingHost),
            Box::new(FailingGpu),
            Box::new(FailingPlatform),
            vec![Box::new(OneUnderspecifiedService)],
        )
        .collect();

        assert_eq!(result.snapshot.source, DataSource::Runtime);
        assert!(result
            .snapshot
            .findings
            .iter()
            .any(|finding| finding.id.starts_with("service-endpoint-missing.")));
    }
}
