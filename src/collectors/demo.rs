use crate::domain::{
    DashboardSnapshot, DataSource, DiagnosisFinding, EngineKind, GpuSnapshot, HealthStatus,
    HostSnapshot, ProbeResult, ProbeStatus, ServiceSnapshot,
};

/// TUI 开发/演示来源。此函数不读取系统状态，也不属于真实采集接口。
pub fn snapshot() -> DashboardSnapshot {
    DashboardSnapshot {
        captured_at: 0,
        source: DataSource::Demo,
        host: HostSnapshot {
            hostname: "demo-host".to_owned(),
            os: "Linux（演示）".to_owned(),
            kernel_version: Some("6.8.0-demo".to_owned()),
            architecture: Some("x86_64".to_owned()),
            cpu_model: Some("演示 CPU".to_owned()),
            logical_cpu_count: Some(32),
            load_1m: Some(1.25),
            memory_used_mib: Some(32768),
            memory_total_mib: Some(65536),
            status: HealthStatus::Healthy,
            cpu_status: HealthStatus::Healthy,
            memory_status: HealthStatus::Warning,
        },
        gpus: vec![
            GpuSnapshot {
                index: 0,
                name: "演示 GPU A".to_owned(),
                uuid: Some("GPU-demo-a".to_owned()),
                pci_address: Some("demo:00:01.0".to_owned()),
                status: HealthStatus::Healthy,
                temperature_celsius: Some(58),
                utilization_percent: Some(72),
                memory_used_mib: Some(8192),
                memory_total_mib: Some(16384),
                power_draw_watts: Some(145.0),
                power_limit_watts: Some(160.0),
                pstate: Some("P2".to_owned()),
                numa_node: Some(0),
                reset_required: Some(false),
                xid_codes: Some(Vec::new()),
            },
            GpuSnapshot {
                index: 1,
                name: "演示 GPU B".to_owned(),
                uuid: Some("GPU-demo-b".to_owned()),
                pci_address: Some("demo:00:02.0".to_owned()),
                status: HealthStatus::Warning,
                temperature_celsius: Some(76),
                utilization_percent: Some(94),
                memory_used_mib: Some(15360),
                memory_total_mib: Some(16384),
                power_draw_watts: Some(160.0),
                power_limit_watts: Some(160.0),
                pstate: Some("P0".to_owned()),
                numa_node: Some(0),
                reset_required: Some(false),
                xid_codes: Some(Vec::new()),
            },
        ],
        services: vec![
            ServiceSnapshot {
                name: "demo-llama".to_owned(),
                engine: EngineKind::LlamaCpp,
                model: Some("演示模型（非真实服务）".to_owned()),
                pid: Some(10001),
                port: Some(18080),
                status: HealthStatus::Healthy,
                process_present: Some(true),
                endpoint: Some("http://127.0.0.1:18080".to_owned()),
                endpoint_reachable: Some(true),
                health_probe: ProbeResult {
                    status: ProbeStatus::Succeeded,
                    http_status: Some(200),
                    message: Some("演示探针结果".to_owned()),
                },
                models_probe: ProbeResult {
                    status: ProbeStatus::Succeeded,
                    http_status: Some(200),
                    message: Some("演示模型已匹配".to_owned()),
                },
                metrics_probe: ProbeResult::not_attempted(),
                observed_models: Vec::new(),
                observed_metrics: Vec::new(),
                gpu_indices: vec![0],
                last_error: None,
                discovery: Default::default(),
            },
            ServiceSnapshot {
                name: "demo-vllm".to_owned(),
                engine: EngineKind::Vllm,
                model: Some("演示模型（非真实服务）".to_owned()),
                pid: Some(10002),
                port: Some(18081),
                status: HealthStatus::Warning,
                process_present: Some(true),
                endpoint: Some("http://127.0.0.1:18081".to_owned()),
                endpoint_reachable: Some(true),
                health_probe: ProbeResult {
                    status: ProbeStatus::Failed,
                    http_status: Some(503),
                    message: Some("演示服务未就绪".to_owned()),
                },
                models_probe: ProbeResult::not_attempted(),
                metrics_probe: ProbeResult::not_attempted(),
                observed_models: Vec::new(),
                observed_metrics: Vec::new(),
                gpu_indices: vec![1],
                last_error: Some("演示服务未就绪".to_owned()),
                discovery: Default::default(),
            },
        ],
        findings: vec![DiagnosisFinding {
            id: "demo-source".to_owned(),
            status: HealthStatus::Warning,
            object: "数据来源".to_owned(),
            summary: "当前页面使用演示数据，不代表真实主机状态".to_owned(),
            evidence: vec!["collectors::demo".to_owned()],
        }],
        // 演示数据不伪造 PCIe/IOMMU/驱动/CUDA 现场。
        platform: None,
    }
}

#[cfg(test)]
mod tests {
    use super::snapshot;
    use crate::domain::DataSource;

    #[test]
    fn demo_snapshot_is_explicitly_marked_as_demo() {
        assert_eq!(snapshot().source, DataSource::Demo);
    }
}
