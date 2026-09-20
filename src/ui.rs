use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap},
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

    let hint = if state.filter_mode() {
        format!(
            " 过滤输入：{}（输入字符实时过滤，Backspace 删除，Esc/Enter 退出）",
            state.filter().unwrap_or("")
        )
    } else {
        match state.filter() {
            Some(filter) => format!(
                " 过滤：{filter}（/ 重新编辑，Esc 清除）   ↑↓/PgUp/PgDn 滚动   r 刷新   e 导出   q 退出"
            ),
            None => " 1-7/←→ 切页   ↑↓/PgUp/PgDn 滚动   / 过滤   r 刷新   b P2P测速   s 远程扫描   m 导出格式   e 导出   ? 帮助   q 退出".to_owned(),
        }
    };
    frame.render_widget(
        Paragraph::new(hint)
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
    let scroll = state.current_scroll();
    match state.page {
        UiPage::Overview => draw_overview(frame, state, area),
        UiPage::Gpu => {
            let (header, widths, rows) = gpu_table(state, area.width);
            let title = match state
                .snapshot
                .gpus
                .first()
                .and_then(|gpu| gpu.smi_tool.as_deref())
            {
                Some(tool) => format!(" GPU ({tool}) "),
                None => " GPU ".to_owned(),
            };
            draw_table(frame, &title, &header, &widths, rows, scroll, area);
        }
        UiPage::Services => {
            let (header, widths, rows) = service_table(state, area.width);
            draw_table(frame, " 服务 ", &header, &widths, rows, scroll, area);
        }
        UiPage::Diagnosis => {
            let lines = filter_lines(diagnosis_lines(state), state.filter());
            draw_lines(frame, " 诊断 ", lines, scroll, area);
        }
        UiPage::Reports => draw_lines(frame, " 报告 ", report_lines(state), scroll, area),
        UiPage::Logs => {
            let lines = filter_lines(log_lines(state, area.width), state.filter());
            draw_lines(frame, " 日志 ", lines, scroll, area);
        }
        UiPage::Remote => draw_lines(
            frame,
            " 远程 ",
            remote_lines(state, area.width),
            scroll,
            area,
        ),
    }
}

/// 通用 Table 渲染：表头高亮 + 边框 + 独立滚动（TableState 偏移）。
fn draw_table(
    frame: &mut Frame<'_>,
    title: &str,
    header: &[&str],
    widths: &[Constraint],
    rows: Vec<Row<'static>>,
    scroll: u16,
    area: Rect,
) {
    let header_cells = header.iter().map(|text| {
        Cell::from(*text).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });
    let table = Table::new(rows, widths)
        .header(Row::new(header_cells))
        .block(Block::default().borders(Borders::ALL).title(title))
        .column_spacing(1);
    let mut state = TableState::default();
    *state.offset_mut() = scroll as usize;
    frame.render_stateful_widget(table, area, &mut state);
}

/// GPU 页表格数据（宽屏全列，窄屏精简列）。
fn gpu_table(
    state: &AppState,
    area_width: u16,
) -> (Vec<&'static str>, Vec<Constraint>, Vec<Row<'static>>) {
    if state.snapshot.gpus.is_empty() {
        return (
            vec!["编号", "名称", "状态"],
            vec![
                Constraint::Length(8),
                Constraint::Length(24),
                Constraint::Length(10),
            ],
            vec![Row::new(vec![
                Cell::from("—"),
                Cell::from("GPU 清单：0 项"),
                Cell::from("—"),
            ])],
        );
    }
    if area_width >= 128 {
        let pci_devices: &[crate::domain::PciDeviceSnapshot] = state
            .snapshot
            .platform
            .as_ref()
            .map_or(&[], |platform| platform.pci_devices.as_slice());
        let header = vec![
            "编号",
            "名称",
            "PCI 地址",
            "链路",
            "NUMA",
            "温度",
            "利用率",
            "显存",
            "功耗",
            crate::vendors::power_state_header(&state.snapshot.gpus),
            "状态",
            "备注",
        ];
        let widths = vec![
            Constraint::Length(6),
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
        ];
        let rows = state
            .snapshot
            .gpus
            .iter()
            .map(|gpu| {
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
                let power = match gpu.power_draw_watts {
                    Some(draw) => gpu
                        .power_limit_watts
                        .map_or(format!("{draw:.0}W"), |limit| {
                            format!("{draw:.0}/{limit:.0}W")
                        }),
                    None => "--".into(),
                };
                let link = crate::collectors::pcie::gpu_pcie_device(gpu, pci_devices)
                    .and_then(crate::collectors::pcie::link_brief_compact)
                    .unwrap_or_else(|| "--".into());
                let mut cells = vec![
                    Cell::from(format!("GPU{}", gpu.index)),
                    Cell::from(shorten(&gpu.name, 18)),
                    Cell::from(opt_ref(&gpu.pci_address)),
                    Cell::from(link),
                    Cell::from(opt(gpu.numa_node)),
                    Cell::from(temperature),
                    Cell::from(utilization),
                    Cell::from(memory),
                    Cell::from(power),
                    Cell::from(opt_ref(&gpu.pstate)),
                    Cell::from(gpu.status.label()),
                ];
                // 备注列的语义由厂商画像决定（NVIDIA: reset/Xid；Enflame: ECC/错误计数……）。
                let remark = crate::vendors::profile_for(gpu.vendor.as_deref()).gpu_remark(gpu);
                cells.push(Cell::from(remark));
                Row::new(cells)
            })
            .collect();
        (header, widths, rows)
    } else {
        let header = vec!["编号", "名称", "PCI/NUMA", "温度", "利用率", "显存", "状态"];
        let widths = vec![
            Constraint::Length(6),
            Constraint::Length(18),
            Constraint::Length(20),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(8),
        ];
        let rows = state
            .snapshot
            .gpus
            .iter()
            .map(|gpu| {
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
                Row::new(vec![
                    Cell::from(format!("GPU{}", gpu.index)),
                    Cell::from(shorten(&gpu.name, 18)),
                    Cell::from(format!(
                        "{}/{}",
                        opt_ref(&gpu.pci_address),
                        opt(gpu.numa_node)
                    )),
                    Cell::from(temperature),
                    Cell::from(utilization),
                    Cell::from(memory),
                    Cell::from(gpu.status.label()),
                ])
            })
            .collect();
        (header, widths, rows)
    }
}

/// 服务页表格数据（宽屏全列，窄屏精简列）。
fn service_table(
    state: &AppState,
    area_width: u16,
) -> (Vec<&'static str>, Vec<Constraint>, Vec<Row<'static>>) {
    if state.snapshot.services.is_empty() {
        return (
            vec!["服务", "状态"],
            vec![Constraint::Length(28), Constraint::Length(10)],
            vec![Row::new(vec![
                Cell::from("推理服务清单：0 项"),
                Cell::from("—"),
            ])],
        );
    }
    let rows = state
        .snapshot
        .services
        .iter()
        .map(|service| {
            let source = service_source(service);
            let endpoint = service
                .endpoint
                .clone()
                .unwrap_or_else(|| "端点未知".into());
            let models = if service.observed_models.is_empty() {
                service.model.clone().unwrap_or_else(|| "模型未知".into())
            } else {
                format!("模型：{}", join_strings(&service.observed_models, 2))
            };
            if area_width >= 120 {
                let probes = format!(
                    "{}/{}/{}",
                    probe_short(&service.health_probe),
                    probe_short(&service.models_probe),
                    probe_short(&service.metrics_probe)
                );
                Row::new(vec![
                    Cell::from(shorten(&service.name, 20)),
                    Cell::from(service.engine.label()),
                    Cell::from(shorten(&source, 18)),
                    Cell::from(shorten(&endpoint, 30)),
                    Cell::from(shorten(&models, 26)),
                    Cell::from(probes),
                    Cell::from(service.status.label()),
                ])
            } else {
                Row::new(vec![
                    Cell::from(shorten(&service.name, 18)),
                    Cell::from(service.engine.label()),
                    Cell::from(shorten(&endpoint, 30)),
                    Cell::from(service.status.label()),
                ])
            }
        })
        .collect();
    if area_width >= 120 {
        (
            vec![
                "服务",
                "引擎",
                "来源",
                "端点",
                "模型",
                "探针(health/models/metrics)",
                "状态",
            ],
            vec![
                Constraint::Length(20),
                Constraint::Length(9),
                Constraint::Length(18),
                Constraint::Length(30),
                Constraint::Length(26),
                Constraint::Length(24),
                Constraint::Length(8),
            ],
            rows,
        )
    } else {
        (
            vec!["服务", "引擎", "端点", "状态"],
            vec![
                Constraint::Length(18),
                Constraint::Length(9),
                Constraint::Length(30),
                Constraint::Length(8),
            ],
            rows,
        )
    }
}

/// 按关键字过滤行列表；空关键字或不处于过滤模式时原样返回。
fn filter_lines(lines: Vec<Line<'static>>, filter: Option<&str>) -> Vec<Line<'static>> {
    match filter.map(str::trim).filter(|value| !value.is_empty()) {
        None => lines,
        Some(keyword) => lines
            .into_iter()
            .filter(|line| line.to_string().contains(keyword))
            .collect(),
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
            0,
            columns[0],
        );
        draw_lines(
            frame,
            " 平台与健康 ",
            overview_platform_lines(state),
            0,
            columns[1],
        );
    } else {
        draw_lines(
            frame,
            " 总览 ",
            [overview_host_lines(state), overview_platform_lines(state)].concat(),
            0,
            area,
        );
    }
}

fn draw_lines(
    frame: &mut Frame<'_>,
    title: &str,
    lines: Vec<Line<'static>>,
    scroll: u16,
    area: Rect,
) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((scroll, 0))
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
        // 厂商段（NVIDIA 驱动/CUDA 或 Enflame 驱动/ECC/错误计数……）由厂商画像提供。
        for (item, value, status) in
            crate::vendors::platform_facts(Some(platform), &state.snapshot.gpus)
        {
            if status == HealthStatus::Unknown {
                lines.push(Line::from(format!("{item}：{value}")));
            } else {
                lines.push(Line::from(format!("{item}：{value}（{}）", status.label())));
            }
        }
        lines.push(Line::from(format!(
            "PCIe 设备：{}",
            platform.pci_devices.len()
        )));
        if let Some(summary) = &platform.acs_summary {
            lines.push(Line::from(format!(
                "PCIe ACS：支持 {} · 开启 {} · 全部关闭 {}",
                summary.supported, summary.enabled, summary.disabled
            )));
        }
        let topology = crate::collectors::pcie::accelerator_topology_lines(
            &platform.pci_devices,
            &state.snapshot.gpus,
            32,
        );
        if !topology.is_empty() {
            lines.push(Line::from("PCIe 拓扑（加速器子树）："));
            for line in topology {
                lines.push(Line::from(line));
            }
        }
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
        if !platform.plugins.is_empty() {
            lines.push(Line::from(format!("插件：{} 个", platform.plugins.len())));
        }
    } else {
        lines.push(Line::from("平台基线：未知"));
        lines.push(Line::from("PCIe/存储：未知"));
    }
    if let Some(logs) = &state.snapshot.logs {
        lines.push(Line::from(format!(
            "系统日志：{}  ·  异常模式：{} 条",
            logs.status.label(),
            logs.matches.len()
        )));
    } else {
        lines.push(Line::from("系统日志：未知"));
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
    if let Some(suggestion) = &finding.suggestion {
        lines.push(Line::from(format!("      建议：{suggestion}")));
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
        Line::from("快照内容：主机 / GPU / 服务 / 诊断 / 平台基线 / 系统日志 / P2P"),
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

fn remote_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("远程设备（按 s 指定目标扫描 · 不自动扫描全部）"),
        Line::from(""),
    ];
    // 候选设备（只读展示，不连接）
    if state.remote_candidates.is_empty() {
        lines.push(Line::from(
            "候选设备：~/.ssh/config 未找到可用主机（或均为通配条目）",
        ));
    } else if state.remote_target_mode() {
        // 选择模式：光标 + 快捷数字
        let selected = state.remote_selection();
        lines.push(Line::from(format!(
            "候选设备（{} 台）：↑/↓ 或数字选择，Enter 扫描选中设备，Esc 取消",
            state.remote_candidates.len()
        )));
        for (index, alias) in state.remote_candidates.iter().enumerate() {
            let marker = if index == selected { "▶" } else { " " };
            let number = if index < 9 {
                format!("{}", index + 1)
            } else {
                " ".to_owned()
            };
            lines.push(Line::from(format!("{marker} {number} {alias}")));
        }
    } else {
        lines.push(Line::from(format!(
            "候选设备（{} 台，仅展示不连接）：",
            state.remote_candidates.len()
        )));
        for alias in &state.remote_candidates {
            lines.push(Line::from(format!("  {alias}")));
        }
    }
    lines.push(Line::from(""));
    // 操作提示
    if state.remote_target_mode() {
        let selected = state
            .remote_candidates
            .get(state.remote_selection())
            .map_or("？", |alias| alias.as_str());
        lines.push(Line::from(format!(
            "扫描目标：{selected}（Enter 确认 · Esc 取消）"
        )));
    } else {
        lines.push(Line::from(
            "按 s 选择要扫描的设备（↑/↓ 或数字 1-9，回车后扫描该台）",
        ));
    }
    lines.push(Line::from(""));
    // 扫描结果
    let Some(remote) = state.snapshot.remote.as_ref() else {
        return lines;
    };
    if remote.hosts.is_empty() {
        lines.push(Line::from("远程设备：无扫描结果（未指定目标或目标不存在）"));
        return lines;
    }
    lines.push(Line::from(format!("扫描结果（{}）：", remote.hosts.len())));
    for host in &remote.hosts {
        let reachable = if host.reachable {
            "可达"
        } else {
            "不可达"
        };
        let sudo = if host.sudo_available {
            "sudo 可用"
        } else if host.reachable {
            "无 sudo"
        } else {
            "-"
        };
        let degraded = if host.degraded { " [已降级]" } else { "" };
        lines.push(Line::from(format!(
            "{}（{}） {} / {}{}",
            host.alias,
            host.hostname.as_deref().unwrap_or("?"),
            reachable,
            sudo,
            degraded
        )));
        if let Some(info) = &host.host_info {
            lines.push(Line::from(format!("    {}", shorten(info, width as usize))));
        }
        if let Some(gpu) = &host.gpu_summary {
            lines.push(Line::from(format!(
                "    GPU: {}",
                shorten(gpu, width as usize)
            )));
        }
        for issue in host.issues.iter().take(2) {
            lines.push(Line::from(format!(
                "    [{}] {}",
                issue.status.label(),
                shorten(&issue.message, width as usize)
            )));
        }
        lines.push(Line::from(""));
    }
    lines.extend(remote.issues.iter().take(3).map(|issue| {
        Line::from(format!(
            "[{}] {}：{}",
            issue.status.label(),
            issue.code,
            shorten(&issue.message, 60)
        ))
    }));
    lines
}

fn log_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let Some(logs) = state.snapshot.logs.as_ref() else {
        return vec![Line::from("日志采集：未启用或不可用")];
    };
    let mut lines = vec![
        Line::from(format!("系统日志状态：{}", logs.status.label())),
        Line::from(format!(
            "来源数：{} · 异常模式数：{}",
            logs.sources.len(),
            logs.matches.len()
        )),
        Line::from(""),
    ];
    if logs.sources.is_empty() {
        lines.push(Line::from("无可用日志来源。"));
    } else {
        lines.push(Line::from("-- 日志来源 --"));
        for source in &logs.sources {
            let probe = match source.probe_status {
                crate::domain::LocalProbeStatus::Succeeded => "可用",
                crate::domain::LocalProbeStatus::Failed => "失败",
                crate::domain::LocalProbeStatus::NotAttempted => "未尝试",
                crate::domain::LocalProbeStatus::Unknown => "未知",
                crate::domain::LocalProbeStatus::Unavailable => "不可用",
            };
            lines.push(Line::from(format!(
                "{} [{}] 异常 {} 条{}",
                source.name,
                probe,
                source.match_count,
                if source.truncated {
                    "（尾部截断）"
                } else {
                    ""
                }
            )));
            for tail in source.lines_tail.iter().take(3) {
                lines.push(Line::from(format!(
                    "    {}",
                    shorten(tail, width.saturating_sub(6) as usize)
                )));
            }
        }
        lines.push(Line::from(""));
    }
    if logs.matches.is_empty() {
        lines.push(Line::from("未检测到异常模式。"));
    } else {
        lines.push(Line::from("-- 异常模式 --"));
        for matched in &logs.matches {
            lines.push(Line::from(format!(
                "[{}] {}：{} 次（{}）",
                matched.severity.label(),
                matched.pattern,
                matched.count,
                matched.sources.join("、")
            )));
            for example in matched.examples.iter().take(2) {
                lines.push(Line::from(format!(
                    "  {}",
                    shorten(example, width as usize)
                )));
            }
        }
        lines.push(Line::from(""));
    }
    lines.extend(logs.issues.iter().take(5).map(|issue| {
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
            Line::from("1-7：切换页面   ←/→：翻页"),
            Line::from("↑↓/PgUp/PgDn：滚动   Home/End：顶部/底部"),
            Line::from("/：过滤（日志/诊断页）   r：采集更新"),
            Line::from("b：P2P 测速  s：远程设备扫描（远程页）"),
            Line::from("m：导出格式  e：报告导出  ?：帮助  q：退出"),
            Line::from("页面：总览 / GPU / 服务 / 诊断 / 报告 / 日志 / 远程"),
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
                    crate::app::UiOperationKind::RemoteScan => {
                        "范围：~/.ssh/config 免密主机 · 先检测权限后降级采集".to_owned()
                    }
                    // 刷新不经过确认弹窗，此处不会渲染。
                    crate::app::UiOperationKind::Refresh => unreachable!(),
                }),
                Line::from("Enter / y：执行    Esc / n：返回"),
            ],
        ),
        UiOperationState::Running(kind) => (
            " 正在执行 ",
            vec![
                Line::from(format!("操作：{}", kind.label())),
                Line::from("任务状态：执行中"),
                Line::from("Esc：取消（后台任务继续，完成时丢弃结果）"),
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

fn opt<T: ToString>(value: Option<T>) -> String {
    value.map_or("--".into(), |v| v.to_string())
}
fn opt_ref<T: ToString>(value: &Option<T>) -> String {
    value.as_ref().map_or("--".into(), ToString::to_string)
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
    fn remote_selection_mode_renders_cursor_and_candidates() {
        let snapshot = demo::snapshot();
        let mut state = AppState::new(snapshot);
        state.page = UiPage::Remote;
        state.remote_candidates = vec!["host-a".to_owned(), "wfk8smaster3".to_owned()];
        state.handle_key(crossterm::event::KeyCode::Char('s').into());
        state.handle_key(crossterm::event::KeyCode::Down.into());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &state))
            .expect("render selection mode");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // 光标 ▶ 与选中项同行（buffer 中宽字符后有空列，逐行匹配）
        let cursor_line = text
            .lines()
            .find(|line| line.contains('▶'))
            .expect("应渲染光标");
        assert!(
            cursor_line.contains("wfk8smaster3"),
            "光标应标在选中项：{cursor_line}"
        );
        // buffer 中 CJK 字符列间有空格，比较时去除
        let compact: String = text.chars().filter(|character| *character != ' ').collect();
        assert!(
            compact.contains("扫描目标") && compact.contains("Enter扫描选中设备"),
            "应显示当前目标与操作提示：{text}"
        );
    }

    #[test]
    fn gpu_and_services_pages_render_table_widget() {
        // GPU / 服务页应使用 Table 组件：有边框字符与表头（不再是纯文本行）。
        for page in [UiPage::Gpu, UiPage::Services] {
            let text = render(120, 30, page, true);
            assert!(
                text.contains('┌') && text.contains('┐'),
                "{page:?} 应有表格边框：{text}"
            );
            let compact: String = text.chars().filter(|c| *c != ' ').collect();
            assert!(
                compact.contains("编号") || compact.contains("服务"),
                "{page:?} 应有表头：{text}"
            );
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
}
