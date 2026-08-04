use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::{
    app::{AppState, UiOperationState},
    domain::{DataSource, DiagnosisFinding, HealthStatus, ProbeResult, ProbeStatus, UiPage},
};

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let header_height = if state.snapshot.source == DataSource::Demo {
        5
    } else {
        4
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    let source = format!("来源：{}", state.snapshot.source.label());
    let mut header = vec![Line::from(vec![
        Span::styled(
            " 智算台 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("| 单机诊断监控 | "),
        Span::styled(source, Style::default().fg(Color::Yellow)),
        Span::raw(format!(" | 状态：{}", state.collection_status.label())),
    ])];
    if header_height > 2 {
        header.push(Line::from(format!(
            " 主机 {}  ·  采集时间 {}  ·  刷新 {} 次",
            state.snapshot.host.hostname,
            timestamp(state.snapshot.captured_at),
            state.refresh_count
        )));
    }
    if state.snapshot.source == DataSource::Demo {
        header.push(Line::from(Span::styled(
            " 演示数据集 · 界面预览",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(
        Paragraph::new(header)
            .block(Block::default().borders(Borders::ALL).title(" suanctl "))
            .wrap(Wrap { trim: true }),
        areas[0],
    );

    let titles: Vec<Line<'static>> = UiPage::ALL
        .iter()
        .map(|page| Line::from(format!(" {} ", page.label())))
        .collect();
    frame.render_widget(
        Tabs::new(titles)
            .select(state.page.index())
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(Span::raw("│"))
            .block(Block::default().borders(Borders::BOTTOM)),
        areas[1],
    );

    draw_page(frame, state, areas[2]);

    frame.render_widget(
        Paragraph::new(" 1-5/←→ 切页   r 刷新   b P2P测速   m 导出格式   e 导出   ? 帮助   q 退出")
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
        areas[3],
    );

    if state.show_help {
        draw_help(frame, area);
    } else if !matches!(state.operation, UiOperationState::Idle) {
        draw_operation(frame, area, state);
    }
}

fn draw_page(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    match state.page {
        UiPage::Overview => draw_overview(frame, state, area),
        UiPage::Gpu => draw_lines(frame, " GPU ", gpu_lines(state, area.width), area),
        UiPage::Services => draw_lines(frame, " 服务 ", service_lines(state, area.width), area),
        UiPage::Diagnosis => draw_lines(frame, " 诊断 ", diagnosis_lines(state), area),
        UiPage::Reports => draw_lines(frame, " 报告 ", report_lines(state), area),
    }
}

fn draw_overview(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    if area.width >= 110 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        draw_lines(
            frame,
            " 主机与资源 ",
            overview_host_lines(state),
            columns[0],
        );
        draw_lines(
            frame,
            " 平台与健康 ",
            overview_platform_lines(state),
            columns[1],
        );
    } else {
        draw_lines(
            frame,
            " 总览 ",
            [overview_host_lines(state), overview_platform_lines(state)].concat(),
            area,
        );
    }
}

fn draw_lines(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'static>>, area: Rect) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn overview_host_lines(state: &AppState) -> Vec<Line<'static>> {
    let host = &state.snapshot.host;
    let cpu = host.cpu_model.as_deref().unwrap_or("未知");
    let cpu_count = opt(host.logical_cpu_count);
    let load = host.load_1m.map_or("--".into(), |v| format!("{v:.2}"));
    let memory = match (host.memory_used_mib, host.memory_total_mib) {
        (Some(used), Some(total)) => format!("{used}/{total} MiB"),
        _ => "--".into(),
    };
    vec![
        Line::from(format!("主机：{}", host.hostname)),
        Line::from(format!("OS：{}", host.os)),
        Line::from(format!(
            "内核：{}  架构：{}",
            opt_ref(&host.kernel_version),
            opt_ref(&host.architecture)
        )),
        Line::from(format!("CPU：{}", cpu)),
        Line::from(format!("逻辑核：{}  Load(1m)：{}", cpu_count, load)),
        Line::from(format!(
            "内存：{}  状态：{}",
            memory,
            host.memory_status.label()
        )),
        Line::from(format!(
            "GPU：{} 张  ·  服务：{} 个",
            state.snapshot.gpus.len(),
            state.snapshot.services.len()
        )),
        Line::from(format!(
            "采集状态：{}  ·  采集问题：{} 条",
            state.collection_status.label(),
            state.collection_issues.len()
        )),
        Line::from(""),
        Line::from(format!("Finding：{} 条", state.snapshot.findings.len())),
    ]
}

fn overview_platform_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let counts = finding_counts(&state.snapshot.findings);
    lines.push(Line::from(format!(
        "健康摘要：正常 {}  警告 {}  严重 {}  未知 {}",
        counts.0, counts.1, counts.2, counts.3
    )));
    if let Some(platform) = &state.snapshot.platform {
        let iommu =
            platform
                .iommu
                .effective
                .map_or("未知", |v| if v { "已生效" } else { "未生效" });
        let requested = platform
            .iommu
            .requested
            .as_ref()
            .and_then(|v| v.enabled)
            .map_or("未知", |v| if v { "启用" } else { "未启用" });
        let override_text = platform.iommu.acs_override.as_ref().map_or("未知", |v| {
            if v.enabled {
                "已启用"
            } else {
                "未启用"
            }
        });
        lines.push(Line::from(format!(
            "IOMMU：requested={} effective={}  ACS override：{}",
            requested, iommu, override_text
        )));
        lines.push(Line::from(format!(
            "NVIDIA module：{}  内核版本：{}",
            opt_ref(&platform.nvidia_driver.module_loaded.map(|v| if v {
                "已加载"
            } else {
                "未加载"
            })),
            opt_ref(&platform.nvidia_driver.kernel_module_version)
        )));
        lines.push(Line::from(format!(
            "驱动：{}  匹配：{}",
            opt_ref(&platform.nvidia_driver.nvidia_smi_driver_version),
            platform
                .nvidia_driver
                .version_match
                .map_or("未知", |v| if v { "是" } else { "否" })
        )));
        lines.push(Line::from(format!(
            "CUDA：driver={} toolkit={} libcuda={} libcudart={}",
            opt_ref(&platform.cuda.driver_reported_max_cuda),
            opt_ref(&platform.cuda.nvcc_toolkit_version),
            platform.cuda.libcuda.presence.label(),
            platform.cuda.libcudart.presence.label()
        )));
        lines.push(Line::from(format!(
            "PCIe 设备：{}",
            platform.pci_devices.len()
        )));
        if let Some(p2p) = &platform.p2p {
            let supported = p2p
                .links
                .iter()
                .filter(|link| link.read.is_supported() && link.write.is_supported())
                .count();
            lines.push(Line::from(format!(
                "GPU P2P 读写可达：{}/{}（有向）  实测速率：{}",
                supported,
                p2p.links.len(),
                p2p.benchmark.status.label()
            )));
        } else {
            lines.push(Line::from("GPU P2P：未知"));
        }
        if let Some(storage) = &platform.storage {
            let mdraid = storage.software_raid.len();
            let sas = storage.sas_phys.len();
            lines.push(Line::from(format!(
                "存储控制器：{}  mdraid：{}  SAS PHY：{}",
                storage.controllers.len(),
                mdraid,
                sas
            )));
        } else {
            lines.push(Line::from("存储快照：未知"));
        }
    } else {
        lines.push(Line::from("平台基线：未知"));
        lines.push(Line::from("PCIe/存储：未知"));
    }
    lines
}

fn gpu_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines = if width >= 120 {
        vec![Line::from("编号  名称                  PCI 地址        NUMA  温度  利用率  显存       功耗       P态  状态")]
    } else {
        vec![Line::from(
            "编号  名称             PCI/NUMA       温度  利用率  显存       状态",
        )]
    };
    if state.snapshot.gpus.is_empty() {
        lines.push(Line::from("GPU 清单：0 项"));
    }
    for gpu in &state.snapshot.gpus {
        let temperature = gpu
            .temperature_celsius
            .map_or("--".into(), |v| format!("{v}C"));
        let utilization = gpu
            .utilization_percent
            .map_or("--".into(), |v| format!("{v}%"));
        let memory = match (gpu.memory_used_mib, gpu.memory_total_mib) {
            (Some(used), Some(total)) => format!("{used}/{total}MiB"),
            _ => "--".into(),
        };
        let reset = gpu
            .reset_required
            .map_or("未知", |v| if v { "需reset" } else { "正常" });
        let xid = gpu.xid_codes.as_ref().map_or("未知".into(), |codes| {
            if codes.is_empty() {
                String::from("无Xid")
            } else {
                format!("Xid:{}", join_u32(codes))
            }
        });
        if width >= 120 {
            let power = match gpu.power_draw_watts {
                Some(draw) => gpu
                    .power_limit_watts
                    .map_or(format!("{draw:.0}W"), |limit| {
                        format!("{draw:.0}/{limit:.0}W")
                    }),
                None => "--".into(),
            };
            lines.push(Line::from(format!(
                "GPU{}  {:<20} {:<15} {:<4} {:>4}  {:>5}  {:<10} {:<10} {:<3}  {}",
                gpu.index,
                shorten(&gpu.name, 20),
                opt_ref(&gpu.pci_address),
                opt(gpu.numa_node),
                temperature,
                utilization,
                memory,
                power,
                opt_ref(&gpu.pstate),
                gpu.status.label()
            )));
            lines.push(Line::from(format!("      reset={}  {}", reset, xid)));
        } else {
            lines.push(Line::from(format!(
                "GPU{}  {:<16} {:<15} {:>4}  {:>5}  {:<10} {}",
                gpu.index,
                shorten(&gpu.name, 16),
                format!("{}/{}", opt_ref(&gpu.pci_address), opt(gpu.numa_node)),
                temperature,
                utilization,
                memory,
                gpu.status.label()
            )));
            lines.push(Line::from(format!(
                "      pstate={}  reset={}  {}",
                opt_ref(&gpu.pstate),
                reset,
                xid
            )));
        }
        if let Some(device) = pci_device_for_gpu(state, gpu.pci_address.as_deref()) {
            lines.push(Line::from(format!(
                "      PCIe 协商 {} / 能力 {} · 理论单向上限",
                pcie_link_text(
                    device.current_link_gen,
                    device.current_link_width.as_deref(),
                    device.current_theoretical_bandwidth_mb_s
                ),
                pcie_link_text(
                    device.max_link_gen,
                    device.max_link_width.as_deref(),
                    device.max_theoretical_bandwidth_mb_s
                )
            )));
        }
    }
    if let Some(p2p) = state
        .snapshot
        .platform
        .as_ref()
        .and_then(|platform| platform.p2p.as_ref())
    {
        lines.push(Line::from(""));
        lines.push(Line::from("GPU P2P 驱动能力矩阵"));
        for link in p2p.links.iter().take(32) {
            lines.push(Line::from(format!(
                "GPU{}→GPU{} path={} 读={} 写={} PCIe={} NVLink={} 原子={}",
                link.source_gpu,
                link.target_gpu,
                link.topology_path.as_deref().unwrap_or("未知"),
                link.read.label(),
                link.write.label(),
                link.pcie.label(),
                link.nvlink.label(),
                link.atomics.label()
            )));
        }
        if p2p.links.len() > 32 {
            lines.push(Line::from(format!(
                "P2P 列表已截断：32/{}",
                p2p.links.len()
            )));
        }
        lines.push(Line::from(format!(
            "NVBandwidth 状态：{} · 测试项：device_to_device_memcpy_write_ce",
            p2p.benchmark.status.label()
        )));
        for measurement in p2p.benchmark.measurements.iter().take(32) {
            lines.push(Line::from(format!(
                "GPU{}→GPU{} 实测 {:.2} GB/s",
                measurement.source_gpu, measurement.target_gpu, measurement.gigabytes_per_second
            )));
        }
    }
    lines
}

fn service_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(if width >= 120 {
        "服务                  引擎       来源/PID          端点                         探针(health/models/metrics)  状态"
    } else {
        "服务             引擎       来源       端点/状态"
    })];
    if state.snapshot.services.is_empty() {
        lines.push(Line::from("推理服务清单：0 项"));
    }
    for service in &state.snapshot.services {
        let source = service_source(service);
        let endpoint = service
            .endpoint
            .clone()
            .unwrap_or_else(|| "端点未知".into());
        let probes = format!(
            "{}/{}/{}",
            probe_short(&service.health_probe),
            probe_short(&service.models_probe),
            probe_short(&service.metrics_probe)
        );
        let models = if service.observed_models.is_empty() {
            service.model.clone().unwrap_or_else(|| "模型未知".into())
        } else {
            format!("模型：{}", join_strings(&service.observed_models, 2))
        };
        if width >= 120 {
            lines.push(Line::from(format!(
                "{:<20} {:<9} {:<16} {:<28} {:<24} {}",
                shorten(&service.name, 20),
                service.engine.label(),
                shorten(&source, 16),
                shorten(&endpoint, 28),
                probes,
                service.status.label()
            )));
            lines.push(Line::from(format!("      {}", shorten(&models, 100))));
        } else {
            lines.push(Line::from(format!(
                "{:<15} {:<9} {:<10} {}",
                shorten(&service.name, 15),
                service.engine.label(),
                shorten(&source, 10),
                shorten(&endpoint, 42)
            )));
            lines.push(Line::from(format!(
                "      {}  探针：{}  状态：{}",
                shorten(&models, 30),
                probes,
                service.status.label()
            )));
        }
    }
    lines
}

fn diagnosis_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.snapshot.findings.is_empty() {
        return vec![Line::from("诊断记录：0 项")];
    }
    state
        .snapshot
        .findings
        .iter()
        .flat_map(finding_lines)
        .collect()
}

fn finding_lines(finding: &DiagnosisFinding) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "[{}] {}：{}",
        finding.status.label(),
        finding.object,
        finding.summary
    ))];
    if !finding.evidence.is_empty() {
        lines.push(Line::from(format!(
            "      证据：{}",
            join_strings(&finding.evidence, 3)
        )));
    }
    lines
}

fn report_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("报告导出"),
        Line::from("内容：运行时快照、采集问题"),
        Line::from(format!(
            "schema：suanctl.doctor/v0.1  ·  source：{}",
            state.snapshot.source.label()
        )),
        Line::from(format!(
            "captured_at：{}",
            timestamp(state.snapshot.captured_at)
        )),
        Line::from("快照内容：主机 / GPU / 服务 / 诊断 / 平台基线"),
        Line::from(format!(
            "采集状态：{}  ·  采集问题：{} 条",
            state.collection_status.label(),
            state.collection_issues.len()
        )),
        Line::from(format!(
            "导出格式：{} · 快捷键：m",
            state.report_format.label()
        )),
        Line::from(format!(
            "输出路径：reports/suanctl-<时间>.{} · 快捷键：e",
            state.report_format.extension()
        )),
        Line::from("P2P 测速：NVBandwidth · 快捷键：b"),
        Line::from("写入策略：新建文件"),
    ];
    lines.extend(state.collection_issues.iter().take(3).map(|issue| {
        Line::from(format!(
            "[{}] {}：{}",
            issue.status.label(),
            issue.code,
            shorten(&issue.message, 60)
        ))
    }));
    lines
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let width = area.width.saturating_sub(4).clamp(1, 72);
    let height = area.height.saturating_sub(2).clamp(1, 10);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("快捷键"),
            Line::from("1-5：切换页面   ←/→：翻页"),
            Line::from("r：采集更新  b：P2P 测速  m：导出格式"),
            Line::from("e：报告导出  ?：帮助  q：退出"),
        ])
        .block(Block::default().borders(Borders::ALL).title(" 帮助 "))
        .wrap(Wrap { trim: true }),
        popup,
    );
}

fn draw_operation(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let width = area.width.saturating_sub(8).clamp(1, 76);
    let height = area.height.saturating_sub(4).clamp(1, 9);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let (title, lines) = match &state.operation {
        UiOperationState::Confirm(kind) => (
            " 操作确认 ",
            vec![
                Line::from(format!("操作：{}", kind.label())),
                Line::from(match kind {
                    crate::app::UiOperationKind::P2pBenchmark => {
                        "执行器：NVBandwidth · 负载类型：GPU".to_owned()
                    }
                    crate::app::UiOperationKind::ReportExport => format!(
                        "内容：完整采集结果、{} 条采集问题 · 格式：{}",
                        state.collection_issues.len(),
                        state.report_format.label()
                    ),
                }),
                Line::from("Enter / y：执行    Esc / n：返回"),
            ],
        ),
        UiOperationState::Running(kind) => (
            " 正在执行 ",
            vec![
                Line::from(format!("操作：{}", kind.label())),
                Line::from("任务状态：执行中"),
            ],
        ),
        UiOperationState::Succeeded { kind, message } => (
            " 操作完成 ",
            vec![
                Line::from(format!("{}：完成", kind.label())),
                Line::from(shorten(message, 68)),
                Line::from("Enter / Esc：返回"),
            ],
        ),
        UiOperationState::Failed { kind, message } => (
            " 操作异常 ",
            vec![
                Line::from(format!("{}：异常", kind.label())),
                Line::from(shorten(message, 68)),
                Line::from("Enter / Esc：返回"),
            ],
        ),
        UiOperationState::Idle => return,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn finding_counts(findings: &[DiagnosisFinding]) -> (usize, usize, usize, usize) {
    findings.iter().fold((0, 0, 0, 0), |mut counts, finding| {
        match finding.status {
            HealthStatus::Healthy => counts.0 += 1,
            HealthStatus::Warning => counts.1 += 1,
            HealthStatus::Critical => counts.2 += 1,
            _ => counts.3 += 1,
        }
        counts
    })
}

fn service_source(service: &crate::domain::ServiceSnapshot) -> String {
    service
        .discovery
        .sources
        .first()
        .map(|source| {
            if let Some(container) = &source.container {
                format!("容器/{:?}", container.runtime).to_lowercase()
            } else if let Some(pid) = source.pid.or(service.pid) {
                format!("进程 PID {pid}")
            } else {
                "已发现".into()
            }
        })
        .unwrap_or_else(|| {
            service
                .pid
                .map_or("来源未知".into(), |pid| format!("进程 PID {pid}"))
        })
}

fn probe_short(probe: &ProbeResult) -> &'static str {
    match probe.status {
        ProbeStatus::Succeeded => "OK",
        ProbeStatus::Failed => "失败",
        ProbeStatus::Unavailable => "不可用",
        ProbeStatus::NotAttempted => "未探测",
    }
}

fn timestamp(value: u64) -> String {
    if value == 0 {
        "未知".into()
    } else {
        value.to_string()
    }
}

fn pci_device_for_gpu<'a>(
    state: &'a AppState,
    pci_address: Option<&str>,
) -> Option<&'a crate::domain::PciDeviceSnapshot> {
    let address = normalized_bdf(pci_address?);
    state
        .snapshot
        .platform
        .as_ref()?
        .pci_devices
        .iter()
        .find(|device| normalized_bdf(&device.bdf) == address)
}

fn normalized_bdf(value: &str) -> String {
    let mut pieces = value.splitn(3, ':');
    let domain = pieces.next().unwrap_or(value);
    let bus = pieces.next();
    let device = pieces.next();
    match (bus, device) {
        (Some(bus), Some(device)) => {
            let domain = if domain.len() > 4 {
                &domain[domain.len() - 4..]
            } else {
                domain
            };
            format!("{domain}:{bus}:{device}").to_ascii_lowercase()
        }
        _ => value.to_ascii_lowercase(),
    }
}

fn pcie_link_text(generation: Option<u8>, width: Option<&str>, mb_s: Option<u64>) -> String {
    let generation = generation.map_or("Gen?".to_owned(), |value| format!("Gen{value}"));
    let width = pcie_width_text(width);
    let bandwidth = mb_s.map_or("带宽未知".to_owned(), |value| {
        format!("{:.2} GB/s", value as f64 / 1000.0)
    });
    format!("{generation} {width} {bandwidth}")
}

fn pcie_width_text(width: Option<&str>) -> String {
    width.map_or("x?".to_owned(), |value| {
        if value.starts_with(['x', 'X']) {
            value.to_owned()
        } else {
            format!("x{value}")
        }
    })
}
fn opt<T: ToString>(value: Option<T>) -> String {
    value.map_or("--".into(), |v| v.to_string())
}
fn opt_ref<T: ToString>(value: &Option<T>) -> String {
    value.as_ref().map_or("--".into(), ToString::to_string)
}
fn join_u32(values: &[u32]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
fn join_strings(values: &[String], max: usize) -> String {
    values
        .iter()
        .take(max)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}
fn shorten(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.into()
    } else {
        value
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

trait PresenceLabel {
    fn label(self) -> &'static str;
}
impl PresenceLabel for crate::domain::PresenceStatus {
    fn label(self) -> &'static str {
        match self {
            crate::domain::PresenceStatus::Present => "存在",
            crate::domain::PresenceStatus::Absent => "不存在",
            crate::domain::PresenceStatus::Unknown => "未知",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::AppState, collectors::demo};
    use ratatui::{backend::TestBackend, Terminal};

    fn render(width: u16, height: u16, page: UiPage, demo_mode: bool) -> String {
        let mut snapshot = demo::snapshot();
        if !demo_mode {
            snapshot.source = DataSource::Runtime;
            snapshot.findings.clear();
        }
        let mut state = AppState::new(snapshot);
        state.page = page;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| draw(frame, &state)).expect("render");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_all_pages_at_responsive_sizes_and_tiny_terminal() {
        for (width, height) in [(80, 24), (120, 30), (160, 40), (2, 2)] {
            for page in UiPage::ALL {
                let _ = render(width, height, page, false);
            }
        }
    }

    #[test]
    fn runtime_has_no_stale_demo_copy_and_demo_has_explicit_warning() {
        let runtime = render(120, 30, UiPage::Overview, false);
        assert!(!runtime.contains("不调用 nvidia-smi"));
        assert!(!runtime.contains("后续实现真实采集"));
        let demo = render(120, 30, UiPage::Overview, true);
        assert!(demo.contains("演 示 数 据"));
    }

    #[test]
    fn all_five_pages_keep_chinese_primary_labels() {
        for page in UiPage::ALL {
            let text = render(120, 30, page, true);
            assert!(page
                .label()
                .chars()
                .any(|character| text.contains(character)));
        }
    }

    #[test]
    fn pcie_ui_text_keeps_theoretical_rate_explicit() {
        assert_eq!(
            pcie_link_text(Some(4), Some("x16"), Some(31_508)),
            "Gen4 x16 31.51 GB/s"
        );
        assert_eq!(
            pcie_link_text(Some(5), Some("16"), Some(63_015)),
            "Gen5 x16 63.02 GB/s"
        );
        assert_eq!(normalized_bdf("00000000:3B:00.0"), "0000:3b:00.0");
    }
}
