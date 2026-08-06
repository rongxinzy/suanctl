//! 基于真实设备 rx-box（172.18.5.123）采集数据的快照 round-trip 测试。
//!
//! 覆盖：真实形态快照（2× NVIDIA L20 / Hygon C86 3350 / 真实日志尾部）
//! 经 serde JSON 序列化 → 反序列化后关键数据不丢失 —— 与存储/报告链路同路径。

use suanctl::collectors::demo;
use suanctl::domain::{
    DashboardSnapshot, HealthStatus, LocalProbeStatus, LogSnapshot, LogSourceSnapshot,
};

/// 真实日志尾部（rx-box dmesg/syslog），注入快照后验证 round-trip 保留原文。
const REAL_DMESG: &str = include_str!("../src/collectors/fixtures/real-rx-box/rx_dmesg_tail.log");
const REAL_SYSLOG: &str = include_str!("../src/collectors/fixtures/real-rx-box/rx_syslog_tail.log");

fn with_real_logs(mut snapshot: DashboardSnapshot) -> DashboardSnapshot {
    let dmesg_lines: Vec<String> = REAL_DMESG.lines().map(str::to_owned).collect();
    let syslog_lines: Vec<String> = REAL_SYSLOG.lines().map(str::to_owned).collect();
    snapshot.logs = Some(LogSnapshot {
        status: HealthStatus::Healthy,
        sources: vec![
            LogSourceSnapshot {
                name: "dmesg".to_owned(),
                probe_status: LocalProbeStatus::Succeeded,
                command: Some("dmesg -T".to_owned()),
                path: None,
                lines_tail: dmesg_lines,
                truncated: false,
                match_count: 0,
            },
            LogSourceSnapshot {
                name: "syslog".to_owned(),
                probe_status: LocalProbeStatus::Succeeded,
                command: None,
                path: Some("/var/log/syslog".to_owned()),
                lines_tail: syslog_lines,
                truncated: false,
                match_count: 0,
            },
        ],
        matches: Vec::new(),
        issues: Vec::new(),
    });
    snapshot
}

#[test]
fn real_rx_box_shaped_snapshot_roundtrips_through_json() {
    let snapshot = with_real_logs(demo::snapshot());
    let json = serde_json::to_string(&snapshot).expect("序列化");
    let back: DashboardSnapshot = serde_json::from_str(&json).expect("反序列化");

    // 主机（真实采集形态）
    assert_eq!(back.host.hostname, "rx-box（演示）");
    assert!(
        back.host.os.contains("Ubuntu 24.04.3"),
        "os={}",
        back.host.os
    );
    assert_eq!(back.host.logical_cpu_count, Some(16));
    assert!(back
        .host
        .cpu_model
        .as_deref()
        .is_some_and(|model| model.contains("Hygon C86 3350")));
    assert_eq!(back.host.memory_total_mib, Some(64037));

    // GPU（2× NVIDIA L20，真实数值）
    assert_eq!(back.gpus.len(), 2);
    assert!(back.gpus.iter().all(|gpu| gpu.name == "NVIDIA L20"));
    assert_eq!(back.gpus[0].memory_total_mib, Some(46068));
    assert_eq!(back.gpus[0].temperature_celsius, Some(56));
    assert_eq!(back.gpus[1].temperature_celsius, Some(57));

    // 日志尾部原文保留（含中文/特殊字符不丢失）
    let logs = back.logs.expect("logs 应保留");
    assert_eq!(logs.status, HealthStatus::Healthy);
    let dmesg = logs
        .sources
        .iter()
        .find(|source| source.name == "dmesg")
        .expect("dmesg 源");
    let syslog = logs
        .sources
        .iter()
        .find(|source| source.name == "syslog")
        .expect("syslog 源");
    assert_eq!(dmesg.lines_tail.len(), REAL_DMESG.lines().count());
    assert_eq!(syslog.lines_tail.len(), REAL_SYSLOG.lines().count());
    assert!(dmesg.lines_tail.iter().all(|line| line.len() <= 300));
    // 与原始 fixture 完全一致
    let joined = dmesg.lines_tail.join("\n");
    assert_eq!(joined, REAL_DMESG.trim_end());
}

#[test]
fn real_log_tails_contain_no_gpu_anomaly_patterns() {
    // 真实设备日志不应命中内置异常模式（否则是误报）。
    let snapshot = with_real_logs(demo::snapshot());
    let logs = snapshot.logs.expect("logs");
    assert!(
        logs.matches.is_empty(),
        "真实日志误报异常：{:?}",
        logs.matches
    );
}
