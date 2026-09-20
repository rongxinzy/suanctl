use crate::domain::{
    DashboardSnapshot, DataSource, DiagnosisFinding, DiskKind, DiskSnapshot, EngineKind,
    GpuSnapshot, HealthStatus, HostSnapshot, LocalProbeStatus, LogPatternMatch, LogSnapshot,
    LogSourceSnapshot, NetInterfaceSnapshot, ProbeResult, ProbeStatus, RemoteScanSnapshot,
    ServiceSnapshot,
};

/// TUI 开发/演示来源。此函数不读取系统状态，也不属于真实采集接口。
/// 数值形态基于真实设备 rx-box（172.18.5.123，2× NVIDIA L20 / Hygon C86 3350）采集。
pub fn snapshot() -> DashboardSnapshot {
    DashboardSnapshot {
        captured_at: 0,
        source: DataSource::Demo,
        host: HostSnapshot {
            hostname: "rx-box（演示）".to_owned(),
            os: "Ubuntu 24.04.3 LTS".to_owned(),
            kernel_version: Some("6.8.0-90-generic".to_owned()),
            architecture: Some("x86_64".to_owned()),
            cpu_model: Some("Hygon C86 3350  8-core Processor".to_owned()),
            logical_cpu_count: Some(16),
            load_1m: Some(0.94),
            memory_used_mib: Some(12204),
            memory_total_mib: Some(64037),
            status: HealthStatus::Healthy,
            cpu_status: HealthStatus::Healthy,
            memory_status: HealthStatus::Warning,
            memory_modules: Some("2×32GB DDR5 4800MT/s".to_owned()),
            disks: vec![
                DiskSnapshot {
                    name: "sda".to_owned(),
                    model: Some("SAMSUNG MZ1L23T8HCLS-00A07".to_owned()),
                    size_bytes: Some(3_840_755_982_336),
                    kind: DiskKind::System,
                    fstype: Some("ext4".to_owned()),
                    mountpoints: vec!["/".to_owned()],
                    blank: Some(false),
                },
                DiskSnapshot {
                    name: "nvme0n1".to_owned(),
                    model: Some("SAMSUNG MZQLB3T8HALS-00007".to_owned()),
                    size_bytes: Some(3_840_755_982_336),
                    kind: DiskKind::Data,
                    fstype: None,
                    mountpoints: Vec::new(),
                    blank: Some(true),
                },
            ],
            interfaces: vec![NetInterfaceSnapshot {
                name: "eno1".to_owned(),
                mac: Some("b4:05:5d:8f:aa:01".to_owned()),
                state: "UP".to_owned(),
                addresses: vec!["172.18.5.123/24".to_owned()],
                config_mode: Some("static".to_owned()),
            }],
        },
        gpus: vec![
            GpuSnapshot {
                index: 0,
                name: "NVIDIA L20".to_owned(),
                uuid: Some("GPU-2da85921-1a58-7e3c-5166-47d742d5fe72".to_owned()),
                pci_address: Some("00000000:0C:00.0".to_owned()),
                status: HealthStatus::Healthy,
                temperature_celsius: Some(56),
                utilization_percent: Some(0),
                memory_used_mib: Some(42248),
                memory_total_mib: Some(46068),
                power_draw_watts: Some(87.95),
                power_limit_watts: Some(350.0),
                pstate: Some("P0".to_owned()),
                numa_node: Some(0),
                reset_required: Some(false),
                xid_codes: Some(Vec::new()),
                smi_tool: None,
                vendor: Some("NVIDIA".to_owned()),
                serial_number: None,
                driver_version: None,
                ecc_enabled: None,
                error_details: Default::default(),
                reset_count: None,
            },
            GpuSnapshot {
                index: 1,
                name: "NVIDIA L20".to_owned(),
                uuid: Some("GPU-272445e2-6643-78d8-f636-ba8881fd789d".to_owned()),
                pci_address: Some("00000000:0F:00.0".to_owned()),
                status: HealthStatus::Warning,
                temperature_celsius: Some(57),
                utilization_percent: Some(0),
                memory_used_mib: Some(42248),
                memory_total_mib: Some(46068),
                power_draw_watts: Some(89.53),
                power_limit_watts: Some(350.0),
                pstate: Some("P0".to_owned()),
                numa_node: Some(0),
                reset_required: Some(false),
                xid_codes: Some(Vec::new()),
                smi_tool: None,
                vendor: Some("NVIDIA".to_owned()),
                serial_number: None,
                driver_version: None,
                ecc_enabled: None,
                error_details: Default::default(),
                reset_count: None,
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
            suggestion: None,
        }],
        // 演示数据不伪造 PCIe/IOMMU/驱动/CUDA 现场。
        platform: None,
        // 演示日志：包含一条演示的 Xid 异常，用于展示日志页。
        logs: Some(demo_logs()),
        // 演示远程：一台可达（无 sudo 降级）与一台不可达主机。
        remote: Some(demo_remote()),
    }
}

/// 演示用的远程扫描快照，不真实连接任何主机。
fn demo_remote() -> RemoteScanSnapshot {
    use crate::domain::RemoteHostSnapshot;
    let reachable = RemoteHostSnapshot {
        alias: "k1".to_owned(),
        hostname: Some("172.18.5.123".to_owned()),
        reachable: true,
        sudo_available: false,
        degraded: true,
        host_info: Some(
            "k1  Linux 6.8.0-55-generic  Ubuntu 24.04  load: 0.42 0.31 0.20".to_owned(),
        ),
        gpu_summary: Some("0, NVIDIA A100-SXM4-80GB, 12, 42, 11264MiB".to_owned()),
        kernel_log_tail: Vec::new(),
        issues: vec![crate::domain::CollectionIssue {
            collector: "remote".to_owned(),
            code: "sudo_unavailable".to_owned(),
            status: HealthStatus::Warning,
            message: "k1 当前用户无法执行 sudo，已降级：跳过内核日志（dmesg）采集".to_owned(),
        }],
        status: HealthStatus::Warning,
    };
    let unreachable = RemoteHostSnapshot {
        alias: "jumpserver".to_owned(),
        hostname: Some("172.18.5.233".to_owned()),
        reachable: false,
        sudo_available: false,
        degraded: false,
        host_info: None,
        gpu_summary: None,
        kernel_log_tail: Vec::new(),
        issues: vec![crate::domain::CollectionIssue {
            collector: "remote".to_owned(),
            code: "ssh_unreachable".to_owned(),
            status: HealthStatus::Unavailable,
            message: "jumpserver 免密连接失败（未配置免密或主机不可达）".to_owned(),
        }],
        status: HealthStatus::Unavailable,
    };
    RemoteScanSnapshot {
        scanned_at: 1_800_000_000_000,
        hosts: vec![reachable, unreachable],
        issues: Vec::new(),
        status: HealthStatus::Warning,
    }
}

/// 演示用的系统日志快照，不读取真实主机日志。
fn demo_logs() -> LogSnapshot {
    let dmesg = LogSourceSnapshot {
        name: "dmesg".to_owned(),
        probe_status: LocalProbeStatus::Succeeded,
        command: Some("dmesg -T".to_owned()),
        path: None,
        lines_tail: vec![
            "2026-08-04T10:12:31.000+08:00 kernel: Linux version 6.8.0-demo".to_owned(),
            "2026-08-04T10:13:02.000+08:00 kernel: NVRM: Xid (PCI:0000:03:00): 31, pid=4242, name=python".to_owned(),
            "2026-08-04T10:13:02.000+08:00 kernel: NVRM: GPU 0000:03:00.0: fallen off the bus".to_owned(),
        ],
        truncated: false,
        match_count: 2,
    };
    let journal_kernel = LogSourceSnapshot {
        name: "journalctl_kernel".to_owned(),
        probe_status: LocalProbeStatus::Succeeded,
        command: Some("journalctl -k -n 500 --no-pager -o short-iso".to_owned()),
        path: None,
        lines_tail: vec![
            "2026-08-04T10:12:31.000+08:00 kernel: systemd: Reached target".to_owned(),
            "2026-08-04T10:13:02.000+08:00 kernel: NVRM: Xid (PCI:0000:03:00): 31".to_owned(),
        ],
        truncated: false,
        match_count: 1,
    };
    let kern_log = LogSourceSnapshot {
        name: "kern.log".to_owned(),
        probe_status: LocalProbeStatus::Unavailable,
        command: None,
        path: Some("/var/log/kern.log".to_owned()),
        lines_tail: Vec::new(),
        truncated: false,
        match_count: 0,
    };
    LogSnapshot {
        sources: vec![dmesg, journal_kernel, kern_log],
        matches: vec![
            LogPatternMatch {
                pattern: "xid".to_owned(),
                severity: HealthStatus::Critical,
                count: 2,
                sources: vec!["dmesg".to_owned(), "journalctl_kernel".to_owned()],
                examples: vec![
                    "2026-08-04T10:13:02.000+08:00 kernel: NVRM: Xid (PCI:0000:03:00): 31, pid=4242, name=python".to_owned(),
                ],
            },
            LogPatternMatch {
                pattern: "gpu_fallen_off".to_owned(),
                severity: HealthStatus::Critical,
                count: 1,
                sources: vec!["dmesg".to_owned()],
                examples: vec![
                    "2026-08-04T10:13:02.000+08:00 kernel: NVRM: GPU 0000:03:00.0: fallen off the bus".to_owned(),
                ],
            },
        ],
        issues: Vec::new(),
        status: HealthStatus::Critical,
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
