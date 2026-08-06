use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand, ValueEnum};
use suanctl::{
    app::{AppState, UiReportFormat},
    collectors::{
        demo,
        logs::{LinuxLogCollector, LogsCollector},
        p2p::{NvidiaP2pCollector, P2pCollection},
        runtime::{runtime_status, RuntimeCollection, RuntimeCollector},
    },
    config::SuanctlConfig,
    domain::{DoctorReport, HealthStatus, LogSnapshot, P2pBenchmarkSnapshot, P2pSnapshot},
    storage::{EvidenceReport, EvidenceWriter, ReportFormat},
    store::Store,
    tui,
};

#[derive(Debug, Parser)]
#[command(name = "suanctl", version, about = "智算服务器诊断与监控工具")]
struct Cli {
    /// suanctl.toml 配置文件路径；缺省使用内置默认
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// 本地数据目录（SurrealDB 库位置）；缺省 ~/.suanctl/data
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 启动中文终端界面
    Tui {
        /// 显式使用演示数据，不读取真实主机
        #[arg(long)]
        demo: bool,
    },
    /// 输出本机诊断能力状态
    Doctor {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
        /// 显式运行 NVBandwidth GPU-to-GPU 负载
        #[arg(long)]
        p2p_benchmark: bool,
    },
    /// 导出一次完整证据报告
    Report {
        #[arg(long, value_enum)]
        format: CliReportFormat,
        #[arg(long)]
        output: std::path::PathBuf,
        #[arg(long)]
        force: bool,
        /// 显式运行 NVBandwidth，并将实测 P2P 速率写入报告
        #[arg(long)]
        p2p_benchmark: bool,
    },
    /// 查看 GPU P2P 能力、拓扑及可选实测速率
    P2p {
        /// 显式运行 NVBandwidth GPU-to-GPU 负载
        #[arg(long)]
        benchmark: bool,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 采集系统日志并检测异常模式（dmesg / journalctl / syslog）
    Logs {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 校验并展示配置文件
    Config {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 采集并保存一次快照到本地库（SurrealDB）
    Save,
    /// 扫描远程设备（~/.ssh/config 免密主机；先检测权限后降级）
    Remote {
        /// 只扫描指定别名
        #[arg(long)]
        host: Option<String>,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查询本地历史快照
    History {
        /// 最近 N 条（默认 10）
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// 跳过前 N 条（分页）
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// 按采集状态过滤
        #[arg(long, value_enum)]
        status: Option<HistoryStatus>,
        /// 主机名子串匹配
        #[arg(long)]
        host: Option<String>,
        /// 起始时间：2026-08-01 / RFC3339 / 相对（7d、24h、30m）/ Unix 毫秒
        #[arg(long)]
        since: Option<String>,
        /// 结束时间：格式同 --since
        #[arg(long)]
        until: Option<String>,
        /// 日志异常模式名过滤（如 xid、nvrm、pcie_bus_error）
        #[arg(long)]
        pattern: Option<String>,
        /// 日志来源过滤（如 dmesg、kern.log、syslog）
        #[arg(long)]
        log_source: Option<String>,
        /// 日志异常严重级别过滤
        #[arg(long, value_enum)]
        log_status: Option<HistoryStatus>,
        /// GPU 数量下界（含）
        #[arg(long)]
        gpus: Option<usize>,
        /// 在最近快照的日志尾部行中搜索关键字（如 OOM、nvme）
        #[arg(long)]
        search: Option<String>,
        /// 展示指定 id 的完整快照
        #[arg(long)]
        show: Option<String>,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 运行内置 NCCL all_reduce 基准（需构建时编译进 NCCL 支持）
    Nccl {
        /// 参与 GPU 数（缺省全部）
        #[arg(long)]
        gpus: Option<i32>,
        /// 数据量字节（缺省 256MiB）
        #[arg(long)]
        bytes: Option<i64>,
        /// 迭代次数（缺省 50）
        #[arg(long)]
        iterations: Option<i32>,
    },
    /// 部署 suanctl 自身到免密主机并作为 worker 执行本地模式命令
    Agent {
        /// ssh config 主机别名（如 wfk8smaster3）
        #[arg(value_name = "HOST")]
        host: String,
        /// 远程执行的 suanctl 子命令及其参数（缺省仅部署并显示状态）
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// 本端角色：controller（控制端，默认）/ worker（被控端，无决策权）
        #[arg(long, value_enum, default_value_t = AgentRole::Controller)]
        role: AgentRole,
        /// 忽略版本匹配，强制重新上传
        #[arg(long)]
        redeploy: bool,
        /// 部署后可选的 sudo 预检测（仅部署模式）
        #[arg(long)]
        check_sudo: bool,
    },
    /// 输出本机身份信息（hostname / 用户 / 版本 / 内置能力），供 agent 协商使用
    Identity {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查询日志异常事件（跨快照）
    LogEvents {
        /// 按异常模式过滤（如 xid、nvrm、pcie_bus_error）
        #[arg(long)]
        pattern: Option<String>,
        /// 最近 N 条（默认 20）
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 输出异常模式统计（出现次数/命中合计）
        #[arg(long)]
        stats: bool,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliReportFormat {
    Json,
    Jsonl,
    Markdown,
}

/// history --status 的取值（与 HealthStatus 对应）。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum AgentRole {
    /// 控制端：发起命令、持有决策权（默认）。
    Controller,
    /// 被控端：只执行命令，无决策权。
    Worker,
}

impl AgentRole {
    const fn label(self) -> &'static str {
        match self {
            Self::Controller => "控制端",
            Self::Worker => "被控端",
        }
    }
}

/// history --status 的取值（与 HealthStatus 对应）。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum HistoryStatus {
    Healthy,
    Warning,
    Critical,
    Unavailable,
    Unknown,
}

impl From<HistoryStatus> for HealthStatus {
    fn from(value: HistoryStatus) -> Self {
        match value {
            HistoryStatus::Healthy => HealthStatus::Healthy,
            HistoryStatus::Warning => HealthStatus::Warning,
            HistoryStatus::Critical => HealthStatus::Critical,
            HistoryStatus::Unavailable => HealthStatus::Unavailable,
            HistoryStatus::Unknown => HealthStatus::Unknown,
        }
    }
}

impl From<CliReportFormat> for ReportFormat {
    fn from(value: CliReportFormat) -> Self {
        match value {
            CliReportFormat::Json => Self::Json,
            CliReportFormat::Jsonl => Self::Jsonl,
            CliReportFormat::Markdown => Self::Markdown,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let config = match SuanctlConfig::load(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("配置加载失败：{error}");
            std::process::exit(2);
        }
    };
    let config = Arc::new(config);
    let data_dir = cli.data_dir.clone().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".suanctl").join("data"))
            .unwrap_or_else(|| PathBuf::from(".suanctl/data"))
    });

    match cli.command.unwrap_or(Command::Tui { demo: false }) {
        Command::Tui { demo: use_demo } => {
            let collection = if use_demo {
                demo_collection()
            } else {
                RuntimeCollector::new().with_config(&config).collect()
            };
            let mut state = AppState::from_collection(collection);
            if use_demo {
                state.remote_candidates = demo::snapshot()
                    .remote
                    .as_ref()
                    .map(|remote| remote.hosts.iter().map(|host| host.alias.clone()).collect())
                    .unwrap_or_default();
                tui::run_with_actions(
                    &mut state,
                    demo_collection,
                    || {
                        (
                            demo_collection(),
                            Err("演示模式：P2P 实测不可用".to_owned()),
                        )
                    },
                    export_runtime_collection,
                    move |_target: &str| demo_remote_scan(),
                )?;
            } else {
                let config_for_refresh = Arc::clone(&config);
                let config_for_benchmark = Arc::clone(&config);
                let ssh_config = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".ssh").join("config"))
                    .unwrap_or_else(|| PathBuf::from(".ssh/config"));
                let scanner =
                    suanctl::collectors::remote::RemoteScanner::from_ssh_config(&ssh_config);
                state.remote_candidates = scanner
                    .candidates()
                    .iter()
                    .map(|host| host.alias.clone())
                    .collect();
                tui::run_with_actions(
                    &mut state,
                    move || {
                        RuntimeCollector::new()
                            .with_config(&config_for_refresh)
                            .collect()
                    },
                    move || run_tui_p2p_benchmark(&config_for_benchmark),
                    export_runtime_collection,
                    move |target: &str| run_tui_remote_scan(&ssh_config, target),
                )?;
            }
        }
        Command::Doctor {
            json,
            p2p_benchmark,
        } => {
            let report = DoctorReport::from_runtime(collect_runtime(p2p_benchmark, &config));
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("suanctl doctor: {}", report.status.label());
                println!("来源：{}", report.source.label());
                println!("说明：{}", report.message);
            }
        }
        Command::Report {
            format,
            output,
            force,
            p2p_benchmark,
        } => {
            let collection = collect_runtime(p2p_benchmark, &config);
            let report = EvidenceReport::from_runtime(&collection);
            let format = ReportFormat::from(format);
            EvidenceWriter::write(&output, &report, format, force)?;
            println!(
                "报告已写入：{}（格式：{}，状态：{}）",
                output.display(),
                format.label(),
                collection.status.label()
            );
        }
        Command::P2p { benchmark, json } => {
            let collector = NvidiaP2pCollector::new();
            let collection = if benchmark {
                collector.collect_with_benchmark()
            } else {
                collector.collect_topology()
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&collection)?);
            } else {
                print_p2p(&collection);
            }
        }
        Command::Logs { json } => {
            let patterns = config.to_log_patterns().unwrap_or_default();
            let snapshot = LinuxLogCollector::default()
                .with_extra_patterns(patterns)
                .collect_logs();
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print_logs(&snapshot);
            }
        }
        Command::Config { json } => {
            print_config(&config, json);
        }
        Command::Nccl {
            gpus,
            bytes,
            iterations,
        } => {
            run_nccl_benchmark(gpus, bytes, iterations);
        }
        Command::Agent {
            host,
            command,
            role,
            redeploy,
            check_sudo,
        } => {
            run_agent_command(&host, &command, role, redeploy, check_sudo);
        }
        Command::Identity { json } => {
            print_identity(json);
        }
        Command::Save => {
            let collection = collect_runtime(false, &config);
            let store = Store::open(&data_dir).await.map_err(print_store_error)?;
            let id = store
                .save_snapshot(&collection.snapshot, collection.status)
                .await
                .map_err(print_store_error)?;
            println!(
                "快照已保存：{id}（主机：{}，状态：{}）",
                collection.snapshot.host.hostname,
                collection.status.label()
            );
        }
        Command::Remote { host, json } => {
            let ssh_config = std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".ssh").join("config"))
                .unwrap_or_else(|| PathBuf::from(".ssh/config"));
            let scanner = suanctl::collectors::remote::RemoteScanner::from_ssh_config(&ssh_config);
            match host {
                Some(alias) => {
                    let snapshot = scanner.scan_alias(&alias);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snapshot)?);
                    } else {
                        print_remote_scan(&snapshot);
                    }
                }
                None => {
                    // 不自动扫描全部设备：列出候选，由用户指定某台。
                    if scanner.host_count() == 0 {
                        println!("~/.ssh/config 中未找到可用主机（或均为通配条目）。");
                    } else {
                        println!(
                            "~/.ssh/config 候选设备（{0} 台，仅展示不连接）：",
                            scanner.host_count()
                        );
                        for candidate in scanner.candidates() {
                            let target =
                                match (candidate.user.as_deref(), candidate.hostname.as_deref()) {
                                    (Some(user), Some(hostname)) => format!("{user}@{hostname}"),
                                    (_, Some(hostname)) => hostname.to_owned(),
                                    (Some(user), None) => format!("{user}@?"),
                                    (None, None) => "?".to_owned(),
                                };
                            println!("  {0}（{1}）", candidate.alias, target);
                        }
                    }
                    println!("提示：用 --host <别名> 指定要扫描的某台设备（不指定则不扫描）。");
                }
            }
        }
        Command::LogEvents {
            pattern,
            limit,
            stats,
            json,
        } => {
            let store = Store::open(&data_dir).await.map_err(print_store_error)?;
            if stats {
                let list = store
                    .log_event_stats(limit)
                    .await
                    .map_err(print_store_error)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    print_log_event_stats(&list);
                }
            } else {
                let list = store
                    .list_log_events(pattern.as_deref(), limit)
                    .await
                    .map_err(print_store_error)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    print_log_events(&list);
                }
            }
        }
        Command::History {
            limit,
            offset,
            status,
            host,
            since,
            until,
            pattern,
            log_source,
            log_status,
            gpus,
            search,
            show,
            json,
        } => {
            let store = Store::open(&data_dir).await.map_err(print_store_error)?;
            if let Some(keyword) = search {
                // 日志内容搜索优先于列表/回放。
                let hits = store
                    .search_logs(&keyword, limit)
                    .await
                    .map_err(print_store_error)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&hits)?);
                } else {
                    print_log_search_hits(&hits, &keyword);
                }
                return Ok(());
            }
            if let Some(id) = show {
                let record = store.get_snapshot(&id).await.map_err(print_store_error)?;
                match record {
                    Some(record) => {
                        if json {
                            println!("{}", serde_json::to_string_pretty(&record.snapshot)?);
                        } else {
                            println!(
                                "快照 {}：{} · 主机 {} · GPU {} 张",
                                id, record.status, record.hostname, record.gpu_count
                            );
                        }
                    }
                    None => {
                        eprintln!("未找到快照：{id}");
                        std::process::exit(2);
                    }
                }
            } else {
                let since_millis = parse_time_filter(since.as_deref())?;
                let until_millis = parse_time_filter(until.as_deref())?;
                let query = suanctl::store::SnapshotQuery {
                    limit,
                    offset,
                    status: status.map(Into::into),
                    host_contains: host,
                    since_millis,
                    until_millis,
                    pattern,
                    log_source,
                    log_status: log_status.map(Into::into),
                    gpus_min: gpus,
                };
                let list = store
                    .query_snapshots(&query)
                    .await
                    .map_err(print_store_error)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    print_snapshot_list(&list);
                }
            }
        }
    }

    Ok(())
}

fn demo_collection() -> RuntimeCollection {
    RuntimeCollection {
        snapshot: demo::snapshot(),
        issues: Vec::new(),
        status: HealthStatus::Healthy,
    }
}

fn run_tui_p2p_benchmark(config: &SuanctlConfig) -> (RuntimeCollection, Result<String, String>) {
    let collection = collect_runtime(true, config);
    let result = collection
        .snapshot
        .platform
        .as_ref()
        .and_then(|platform| platform.p2p.as_ref())
        .map(|p2p| &p2p.benchmark)
        .map(|benchmark| match benchmark.status {
            suanctl::domain::P2pBenchmarkStatus::Succeeded => {
                Ok("P2P 实测速率已写入当前快照".to_owned())
            }
            _ => Err(benchmark
                .message
                .clone()
                .unwrap_or_else(|| format!("P2P 测速{}", benchmark.status.label()))),
        })
        .unwrap_or_else(|| Err("平台快照状态：不可用".to_owned()));
    (collection, result)
}

fn export_runtime_collection(
    collection: RuntimeCollection,
    format: UiReportFormat,
) -> Result<String, String> {
    let generated_at = now_millis();
    let output =
        PathBuf::from("reports").join(format!("suanctl-{generated_at}.{}", format.extension()));
    fs::create_dir_all("reports").map_err(|error| format!("无法创建 reports 目录：{error}"))?;
    let report = EvidenceReport::from_runtime(&collection);
    let format = match format {
        UiReportFormat::Json => ReportFormat::Json,
        UiReportFormat::Jsonl => ReportFormat::Jsonl,
        UiReportFormat::Markdown => ReportFormat::Markdown,
    };
    EvidenceWriter::write(&output, &report, format, false).map_err(|error| error.to_string())?;
    Ok(output.display().to_string())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn collect_runtime(run_p2p_benchmark: bool, config: &SuanctlConfig) -> RuntimeCollection {
    let mut collection = RuntimeCollector::new().with_config(config).collect();
    if !run_p2p_benchmark {
        return collection;
    }

    let (benchmark, issues) = NvidiaP2pCollector::new().run_benchmark();
    attach_benchmark(&mut collection, benchmark);
    collection.issues.extend(issues);
    collection.status = runtime_status(&collection.snapshot, &collection.issues);
    collection
}

fn attach_benchmark(collection: &mut RuntimeCollection, benchmark: P2pBenchmarkSnapshot) {
    let Some(platform) = collection.snapshot.platform.as_mut() else {
        collection.issues.push(suanctl::domain::CollectionIssue {
            collector: "p2p".to_owned(),
            code: "platform_unavailable".to_owned(),
            status: HealthStatus::Unavailable,
            message: "平台快照不可用，无法把 P2P 实测写入完整诊断报告".to_owned(),
        });
        return;
    };
    platform
        .p2p
        .get_or_insert_with(|| P2pSnapshot {
            gpu_indices: Vec::new(),
            links: Vec::new(),
            benchmark: P2pBenchmarkSnapshot::default(),
            status: HealthStatus::Unknown,
        })
        .benchmark = benchmark;
}

fn print_p2p(collection: &P2pCollection) {
    println!("P2P 拓扑状态：{}", collection.snapshot.status.label());
    println!("GPU：{:?}", collection.snapshot.gpu_indices);
    for link in &collection.snapshot.links {
        println!(
            "GPU{} -> GPU{}  路径={}  读={} 写={} PCIe={} NVLink={} 原子={}",
            link.source_gpu,
            link.target_gpu,
            link.topology_path.as_deref().unwrap_or("未知"),
            link.read.label(),
            link.write.label(),
            link.pcie.label(),
            link.nvlink.label(),
            link.atomics.label()
        );
    }
    let benchmark = &collection.snapshot.benchmark;
    println!(
        "实测：{}（{} / {}）",
        benchmark.status.label(),
        benchmark.tool,
        benchmark.testcase
    );
    for measurement in &benchmark.measurements {
        println!(
            "GPU{} -> GPU{}  {:.2} GB/s",
            measurement.source_gpu, measurement.target_gpu, measurement.gigabytes_per_second
        );
    }
    if benchmark.status == suanctl::domain::P2pBenchmarkStatus::NotRequested {
        println!("提示：运行 suanctl p2p --benchmark 才会执行 GPU 负载并测量速率。");
    }
    for issue in &collection.issues {
        println!(
            "[{}] {}：{}",
            issue.status.label(),
            issue.code,
            issue.message
        );
    }
}

fn print_logs(snapshot: &LogSnapshot) {
    println!("suanctl logs: {}", snapshot.status.label());
    if snapshot.sources.is_empty() {
        println!("没有可用的系统日志来源。");
        return;
    }
    let mut source_text: Vec<String> = Vec::new();
    for source in &snapshot.sources {
        let status = match source.probe_status {
            suanctl::domain::LocalProbeStatus::Succeeded => "可用",
            suanctl::domain::LocalProbeStatus::Failed => "失败",
            suanctl::domain::LocalProbeStatus::NotAttempted => "未尝试",
            suanctl::domain::LocalProbeStatus::Unknown => "未知",
            suanctl::domain::LocalProbeStatus::Unavailable => "不可用",
        };
        source_text.push(format!(
            "{}({}，{} 条异常)",
            source.name, status, source.match_count
        ));
    }
    println!("来源：{}", source_text.join("、"));
    if snapshot.matches.is_empty() {
        println!("未检测到异常模式。");
        return;
    }
    println!("异常模式：");
    for matched in &snapshot.matches {
        println!(
            "  [{}] {}：命中 {} 次（{}）",
            matched.severity.label(),
            matched.pattern,
            matched.count,
            matched.sources.join("、")
        );
        for example in matched.examples.iter().take(3) {
            println!("    - {example}");
        }
    }
    for issue in &snapshot.issues {
        println!(
            "[{}] {}：{}",
            issue.status.label(),
            issue.code,
            issue.message
        );
    }
}

/// 本机身份（供 `suanctl identity` 与 agent 握手交换）。
#[derive(serde::Serialize)]
struct Identity {
    hostname: String,
    user: String,
    version: String,
    os: String,
    capabilities: IdentityCapabilities,
}

#[derive(serde::Serialize)]
struct IdentityCapabilities {
    builtin_p2p: bool,
    builtin_nccl: bool,
}

fn local_identity() -> Identity {
    let hostname = match std::env::var("HOSTNAME") {
        Ok(value) => value,
        Err(_) => std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_else(|_| "未知".to_owned()),
    };
    let hostname = hostname.trim().to_owned();
    let user = std::env::var("USER").unwrap_or_else(|_| "未知".to_owned());
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("PRETTY_NAME="))
                .map(|line| {
                    line.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_owned()
                })
        })
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    Identity {
        hostname,
        user,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os,
        capabilities: IdentityCapabilities {
            builtin_p2p: cfg!(suanctl_builtin_p2p),
            builtin_nccl: cfg!(suanctl_builtin_nccl),
        },
    }
}

fn print_identity(json: bool) {
    let identity = local_identity();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&identity).unwrap_or_default()
        );
    } else {
        println!("主机：{}", identity.hostname);
        println!("用户：{}", identity.user);
        println!("版本：{}", identity.version);
        println!("系统：{}", identity.os);
        println!(
            "能力：内置 P2P 测速 {} / NCCL {}",
            if identity.capabilities.builtin_p2p {
                "有"
            } else {
                "无"
            },
            if identity.capabilities.builtin_nccl {
                "有"
            } else {
                "无"
            }
        );
    }
}

/// agent 握手：本端显式声明角色（--role），交换双方身份，输出协商结果。
/// 被控端（worker）无决策权：只执行命令、不校验角色；角色不依赖 ssh 免密探测。
fn run_handshake(host: &str, role: AgentRole) {
    use suanctl::collectors::agent;

    let local = local_identity();
    println!("=== agent 握手：{} ===", host);
    println!();
    println!("本端（{}）", role.label());
    println!("  主机：{}（用户 {}）", local.hostname, local.user);
    println!("  版本：{} · 系统：{}", local.version, local.os);
    println!(
        "  能力：内置 P2P 测速 {} / NCCL {}",
        if local.capabilities.builtin_p2p {
            "有"
        } else {
            "无"
        },
        if local.capabilities.builtin_nccl {
            "有"
        } else {
            "无"
        }
    );

    let remote = agent::run_agent_captured(host, &["identity".to_owned(), "--json".to_owned()]);
    match remote {
        Ok(json) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
            let remote_role = match role {
                AgentRole::Controller => "被控端（worker）",
                AgentRole::Worker => "控制端（对端如要控制本机，请在对方执行 agent）",
            };
            println!();
            println!("对端（{}）", remote_role);
            if parsed.is_null() {
                println!("  （无法解析远程身份输出：{json}）");
            } else {
                println!(
                    "  主机：{}（用户 {}）",
                    parsed["hostname"].as_str().unwrap_or("?"),
                    parsed["user"].as_str().unwrap_or("?")
                );
                println!(
                    "  版本：{} · 系统：{}",
                    parsed["version"].as_str().unwrap_or("?"),
                    parsed["os"].as_str().unwrap_or("?")
                );
                let caps = &parsed["capabilities"];
                println!(
                    "  能力：内置 P2P 测速 {} / NCCL {}",
                    caps["builtin_p2p"]
                        .as_bool()
                        .unwrap_or(false)
                        .to_string()
                        .replace("true", "有")
                        .replace("false", "无"),
                    caps["builtin_nccl"]
                        .as_bool()
                        .unwrap_or(false)
                        .to_string()
                        .replace("true", "有")
                        .replace("false", "无")
                );
            }
            println!();
            match role {
                AgentRole::Controller => {
                    println!("协商结果：本端为控制端，{host} 为被控端（worker，无决策权）");
                    println!("角色由 --role 显式指定；对端不校验、只执行命令。");
                }
                AgentRole::Worker => {
                    println!("协商结果：本端声明为被控端（worker），仅执行控制端发来的命令");
                    println!("若需反向控制 {host}，请在对方机器上执行：suanctl agent <本机> --role controller");
                }
            }
        }
        Err(message) => {
            eprintln!("无法获取对端身份：{message}");
        }
    }
}

/// 运行内置 NCCL all_reduce 基准（构建时探测到 NCCL 才编译进二进制）。
#[allow(unused_variables)]
fn run_nccl_benchmark(gpus: Option<i32>, bytes: Option<i64>, iterations: Option<i32>) {
    #[cfg(suanctl_builtin_nccl)]
    {
        // 解包内嵌的独立 NCCL 测速可执行并以子进程运行（主二进制零 CUDA/NCCL 依赖）。
        let embedded: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/suanctl_nccl_test_bin"));
        let path = std::env::temp_dir().join(format!(
            "suanctl-nccl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        if let Err(error) = std::fs::write(&path, embedded) {
            eprintln!("解包 NCCL 测速器失败：{error}");
            std::process::exit(2);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        }
        // 参数：-n <gpus> -b <bytes> -i <iters>（缺省参数省略）。
        let mut args: Vec<String> = Vec::new();
        if let Some(gpus) = gpus {
            args.push("-n".to_owned());
            args.push(gpus.to_string());
        }
        if let Some(bytes) = bytes {
            args.push("-b".to_owned());
            args.push(bytes.to_string());
        }
        if let Some(iterations) = iterations {
            args.push("-i".to_owned());
            args.push(iterations.to_string());
        }
        let status = std::process::Command::new(&path)
            .args(&args)
            .status()
            .map_err(|error| {
                let _ = std::fs::remove_file(&path);
                format!("NCCL 测速器启动失败（目标机器缺少 libnccl/libcudart？）：{error}")
            });
        let _ = std::fs::remove_file(&path);
        match status {
            Ok(status) => {
                if !status.success() {
                    std::process::exit(status.code().unwrap_or(1));
                }
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(2);
            }
        }
    }
    #[cfg(not(suanctl_builtin_nccl))]
    {
        eprintln!(
            "NCCL 测速不可用：此构建未检测到 NCCL（需要系统安装 libnccl-dev + libnccl，并在有 nvcc 的环境重新构建 suanctl）"
        );
        std::process::exit(2);
    }
}

/// 部署并（可选）在远程执行 suanctl 子命令（agent / worker 模式）。
fn run_agent_command(
    host: &str,
    command: &[String],
    role: AgentRole,
    redeploy: bool,
    check_sudo: bool,
) {
    use suanctl::collectors::agent;

    let binary = match agent::current_binary() {
        Ok(binary) => binary,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let outcome = agent::ensure_deployed(host, &binary, redeploy);
    match &outcome.state {
        agent::DeployState::Failed(message) => {
            eprintln!("部署失败（{host}）：{message}");
            std::process::exit(2);
        }
        agent::DeployState::AlreadyDeployed => {
            eprintln!(
                "{host}：agent 已部署（版本 {}，{}）",
                outcome.local_version, outcome.remote_path
            );
        }
        agent::DeployState::Uploaded => {
            eprintln!(
                "{host}：agent 上传完成（版本 {} → {}）",
                outcome.local_version, outcome.remote_path
            );
        }
    }

    // 握手：双方交换身份并输出角色协商结果（角色由 --role 显式指定，不做自动判定）。
    if command.first().is_some_and(|first| first == "handshake") {
        run_handshake(host, role);
        return;
    }

    if check_sudo {
        let status = std::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "StrictHostKeyChecking=accept-new",
                host,
                "--",
                "sudo -n true",
            ])
            .status()
            .map(|status| status.code().unwrap_or(1))
            .unwrap_or(1);
        eprintln!(
            "sudo 预检测：{}",
            if status == 0 {
                "可用"
            } else {
                "不可用（部分系统级采集将降级）"
            }
        );
    }

    if command.is_empty() {
        println!("提示：在 {host} 上执行本地模式命令，例如：");
        println!("  suanctl agent {host} doctor --json");
        println!("  suanctl agent {host} p2p --benchmark");
        println!("  suanctl agent {host} nccl");
        println!("  suanctl agent {host} logs");
        println!();
        println!(
            "角色协商：本端显式声明为 {}（--role 指定，对端为被控端）",
            role.label()
        );
        return;
    }

    // 执行远程命令，透传输出与退出码。
    match agent::run_agent(host, command) {
        Ok(exit_code) => {
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Err(message) => {
            eprintln!("远程执行失败（{host}）：{message}");
            std::process::exit(2);
        }
    }
}

fn print_config(config: &SuanctlConfig, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(config).unwrap_or_else(|e| e.to_string())
        );
        return;
    }
    if config.hosts.is_empty() && config.logs.extra_patterns.is_empty() && !config.plugins.enabled {
        println!("suanctl config: 默认配置（未使用配置文件，行为与无配置一致）");
        return;
    }
    println!("suanctl config: 配置文件已生效");
    if !config.hosts.is_empty() {
        println!("远程主机：{} 台", config.hosts.len());
        for host in &config.hosts {
            println!("  - {}（{}）", host.name, host.address);
        }
    }
    if !config.logs.extra_patterns.is_empty() {
        println!("追加日志模式：{} 个", config.logs.extra_patterns.len());
        for pattern in &config.logs.extra_patterns {
            println!(
                "  - {}（{}）：{}",
                pattern.name, pattern.severity, pattern.regex
            );
        }
    }
    if config.plugins.enabled {
        println!(
            "插件采集：已启用（目录：{}）",
            config
                .plugins
                .dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "~/.suanctl/plugins".to_owned())
        );
    }
}

fn print_snapshot_list(list: &[suanctl::store::SnapshotMeta]) {
    if list.is_empty() {
        println!("历史快照：0 条（先运行 suanctl save 保存）");
        return;
    }
    println!("历史快照：{} 条（--show <id> 查看完整内容）", list.len());
    for meta in list {
        let ts = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(meta.captured_at))
            .map(humantime_like)
            .unwrap_or_else(|| format!("ts={}", meta.captured_at));
        println!(
            "  {}  {} · {} · 主机 {} · GPU {} 张",
            meta.id, ts, meta.status, meta.hostname, meta.gpu_count
        );
    }
}

fn humantime_like(instant: std::time::SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Local> = instant.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn print_store_error(error: suanctl::store::StoreError) -> Box<dyn std::error::Error> {
    eprintln!("本地库错误：{}", error.message);
    std::process::exit(2);
}

/// 解析时间过滤参数为 Unix 毫秒。
/// 支持：纯毫秒数字、相对（Nd/Nh/Nm/Ns）、`YYYY-MM-DD`、
/// `YYYY-MM-DD HH:MM:SS`、RFC3339。`None` 表示未指定。
fn parse_time_filter(value: Option<&str>) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // 相对时间：7d / 24h / 30m / 10s
    if let Some(relative) = parse_relative_time(trimmed) {
        return Ok(Some(relative));
    }
    // 纯数字：Unix 毫秒
    if let Ok(millis) = trimmed.parse::<u64>() {
        return Ok(Some(millis));
    }
    // 日期时间格式
    let formats = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d",
    ];
    for format in formats {
        if let Ok(datetime) = chrono::NaiveDateTime::parse_from_str(trimmed, format) {
            let millis = datetime.and_utc().timestamp_millis().max(0) as u64;
            return Ok(Some(millis));
        }
    }
    // 纯日期 %Y-%m-%d 需要特判：NaiveDate
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let millis = date
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 必然合法")
            .and_utc()
            .timestamp_millis()
            .max(0) as u64;
        return Ok(Some(millis));
    }
    Err(format!("无法解析时间参数：{trimmed}（支持 2026-08-01、2026-08-01 10:00:00、RFC3339、7d/24h/30m、毫秒）").into())
}

fn parse_relative_time(value: &str) -> Option<u64> {
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let amount: u64 = number.parse().ok()?;
    let seconds = match unit {
        "d" => amount * 24 * 60 * 60,
        "h" => amount * 60 * 60,
        "m" => amount * 60,
        "s" => amount,
        _ => return None,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(now.saturating_sub(seconds * 1000))
}

fn print_log_events(list: &[suanctl::store::LogEventMeta]) {
    if list.is_empty() {
        println!("日志异常事件：0 条（快照中的日志异常会在 save 时展开为事件）");
        return;
    }
    println!(
        "日志异常事件：{} 条（--pattern <名> 过滤，--stats 查看统计）",
        list.len()
    );
    for event in list {
        let ts = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(event.captured_at))
            .map(humantime_like)
            .unwrap_or_else(|| format!("ts={}", event.captured_at));
        println!(
            "  {}  [{}] {}：命中 {} 行（{}）",
            ts,
            event.severity,
            event.pattern,
            event.count,
            event.sources.join("、")
        );
        for example in event.examples.iter().take(2) {
            println!("      {example}");
        }
    }
}

fn print_log_event_stats(list: &[suanctl::store::LogEventStat]) {
    if list.is_empty() {
        println!("日志异常模式统计：0 项");
        return;
    }
    println!("日志异常模式统计（按出现快照次数降序）");
    for stat in list {
        println!(
            "  {:>4} 次事件 / {:>6} 行命中  {}",
            stat.occurrences, stat.total_hits, stat.pattern
        );
    }
}

fn print_log_search_hits(hits: &[suanctl::store::LogSearchHit], keyword: &str) {
    if hits.is_empty() {
        println!(
            "日志搜索 \"{keyword}\"：0 处命中（最近 {} 条快照的日志尾部）",
            10
        );
        return;
    }
    println!("日志搜索 \"{keyword}\"：{} 处命中", hits.len());
    for hit in hits {
        let ts = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(hit.captured_at))
            .map(humantime_like)
            .unwrap_or_else(|| format!("ts={}", hit.captured_at));
        println!(
            "  {}  {} / {} / {}\n      {}",
            ts, hit.hostname, hit.source, hit.status, hit.line
        );
    }
}

/// 演示模式远程扫描：返回示例快照，不真实连接。
fn demo_remote_scan() -> (suanctl::domain::RemoteScanSnapshot, Result<String, String>) {
    let snapshot = demo_collection().snapshot;
    let remote = snapshot
        .remote
        .unwrap_or_else(|| suanctl::domain::RemoteScanSnapshot {
            scanned_at: 0,
            hosts: Vec::new(),
            issues: Vec::new(),
            status: suanctl::domain::HealthStatus::Unavailable,
        });
    (remote, Ok("演示模式：远程扫描为示例数据".to_owned()))
}

/// TUI 非演示模式远程扫描：用户指定某台别名才扫描该台（不扫描全部）。
fn run_tui_remote_scan(
    ssh_config: &std::path::Path,
    target: &str,
) -> (suanctl::domain::RemoteScanSnapshot, Result<String, String>) {
    let scanner = suanctl::collectors::remote::RemoteScanner::from_ssh_config(ssh_config);
    let snapshot = scanner.scan_alias(target);
    let host = snapshot.hosts.first();
    let message = match host {
        Some(host) if host.reachable => format!(
            "{}（{}）扫描完成：{}",
            host.alias,
            host.hostname.as_deref().unwrap_or("?"),
            if host.sudo_available {
                "sudo 可用"
            } else {
                "无 sudo，已降级跳过系统级项"
            }
        ),
        Some(host) => format!("{} 不可达（ssh 免密连接失败）", host.alias),
        None => format!("未找到目标：{target}"),
    };
    (snapshot, Ok(message))
}

fn print_remote_scan(snapshot: &suanctl::domain::RemoteScanSnapshot) {
    if snapshot.hosts.is_empty() {
        println!("远程设备：0 台（~/.ssh/config 中未找到可用主机，或均为通配条目）");
        return;
    }
    println!(
        "远程设备扫描：{} 台（--host <别名> 单台扫描）",
        snapshot.hosts.len()
    );
    for host in &snapshot.hosts {
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
        let degraded = if host.degraded {
            " [已降级：跳过系统级采集项]"
        } else {
            ""
        };
        println!(
            "  {}（{}） {} / {}{}",
            host.alias,
            host.hostname.as_deref().unwrap_or("?"),
            reachable,
            sudo,
            degraded
        );
        if let Some(info) = &host.host_info {
            println!("    系统：{}", shorten_line(info, 100));
        }
        if let Some(gpu) = &host.gpu_summary {
            println!("    GPU：{}", shorten_line(gpu, 100));
        }
        if !host.kernel_log_tail.is_empty() {
            println!("    内核日志尾部（{} 行）：", host.kernel_log_tail.len());
            for line in host.kernel_log_tail.iter().take(3) {
                println!("      {}", shorten_line(line, 100));
            }
        }
        for issue in &host.issues {
            println!(
                "    [{}] {}",
                issue.status.label(),
                shorten_line(&issue.message, 90)
            );
        }
    }
}

fn shorten_line(value: &str, max: usize) -> String {
    if value.chars().count() > max {
        let mut result: String = value.chars().take(max).collect();
        result.push('…');
        result
    } else {
        value.to_owned()
    }
}
