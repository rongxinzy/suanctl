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
        gcu::ChainGpuCollector,
        logs::{LinuxLogCollector, LogsCollector},
        p2p::{ChainP2pCollector, NvidiaP2pCollector, P2pCollection},
        pcie::{enrich_p2p_upstream, LinuxPcieCollector},
        runtime::{runtime_status, RuntimeCollection, RuntimeCollector},
        GpuCollector,
    },
    config::SuanctlConfig,
    domain::{DoctorReport, HealthStatus, LogSnapshot, P2pBenchmarkSnapshot, P2pSnapshot},
    storage::{EvidenceReport, EvidenceWriter, ReportFormat},
    store::Store,
    tui,
};

#[derive(Debug, Parser)]
#[command(name = "suanctl", version, verbatim_doc_comment)]
/// 智算服务器诊断与监控工具
///
/// 常用流程：
///   suanctl tui            中文终端界面（按 ? 查看按键帮助）
///   suanctl doctor         本机诊断能力自检
///   suanctl report --format markdown --output report.md   导出证据报告
///   suanctl net set        交互式配置网卡 IP（netplan）
///
/// 子命令详细说明与示例：suanctl help <子命令>
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
    ///
    /// 页面：概览 / GPU / 服务 / 平台 / 远程（←→ 或数字键切换）。
    /// 按键：q 退出，? 帮助，r 刷新，b 运行 P2P 基准，e 导出报告，
    /// m 切换报告格式，/ 过滤，s 远程主机选择，j/k 滚动。
    #[command(verbatim_doc_comment)]
    Tui {
        /// 显式使用演示数据，不读取真实主机
        #[arg(long)]
        demo: bool,
    },
    /// 输出本机诊断能力状态
    ///
    /// 检查采集能力（GPU/平台/日志/存储）、可用工具与权限，
    /// 用于部署后自检：哪些能力可用、哪些因缺工具/权限降级。
    ///
    /// 示例：
    ///   suanctl doctor
    ///   suanctl doctor --json
    ///   suanctl doctor --p2p-benchmark   # 附带 NVBandwidth 实测
    #[command(verbatim_doc_comment)]
    Doctor {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
        /// 显式运行 NVBandwidth GPU-to-GPU 负载
        #[arg(long)]
        p2p_benchmark: bool,
    },
    /// 导出一次完整证据报告
    ///
    /// 汇聚主机/GPU/服务/平台/日志/诊断结果输出到文件；
    /// 输出文件已存在时拒绝覆盖，除非加 --force。
    ///
    /// 示例：
    ///   suanctl report --format markdown --output suanctl-report.md
    ///   suanctl report --format json --output report.json --p2p-benchmark
    #[command(verbatim_doc_comment)]
    Report {
        /// 报告格式：json / jsonl / markdown
        #[arg(long, value_enum)]
        format: CliReportFormat,
        /// 输出文件路径；已存在时拒绝覆盖（除非 --force）
        #[arg(long)]
        output: std::path::PathBuf,
        /// 允许覆盖已存在的输出文件
        #[arg(long)]
        force: bool,
        /// 显式运行 NVBandwidth，并将实测 P2P 速率写入报告
        #[arg(long)]
        p2p_benchmark: bool,
    },
    /// 查看 GPU P2P 能力、拓扑及可选实测速率
    ///
    /// 能力矩阵与拓扑路径来自驱动查询（只读）；--benchmark 会跑
    /// 内置 CUDA Samples p2pBandwidthLatencyTest（未内置时回退 NVBandwidth）。
    ///
    /// 示例：
    ///   suanctl p2p
    ///   suanctl p2p --benchmark
    #[command(verbatim_doc_comment)]
    P2p {
        /// 显式运行 NVBandwidth GPU-to-GPU 负载
        #[arg(long)]
        benchmark: bool,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 采集系统日志并检测异常模式（dmesg / journalctl / syslog）
    ///
    /// 内置 Xid / NVRM / PCIe AER / ECC / NVLink 等异常模式；
    /// 自定义模式见 suanctl.toml 的日志模式配置。
    ///
    /// 示例：
    ///   suanctl logs
    ///   suanctl logs --json
    #[command(verbatim_doc_comment)]
    Logs {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 校验并展示配置文件
    ///
    /// 展示最终生效配置（远程主机、日志模式、插件等）；
    /// 配置错误时给出具体字段错误。
    ///
    /// 示例：suanctl config --config suanctl.toml
    #[command(verbatim_doc_comment)]
    Config {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 采集并保存一次快照到本地库（SurrealDB）
    ///
    /// 保存后用 history / log-events 查询；库位置见全局 --data-dir。
    #[command(verbatim_doc_comment)]
    Save,
    /// 扫描远程设备（~/.ssh/config 免密主机；先检测权限后降级）
    ///
    /// 无 sudo 权限时自动降级为只读采集，不会因此失败。
    ///
    /// 示例：
    ///   suanctl remote
    ///   suanctl remote --host gpu-node-1
    #[command(verbatim_doc_comment)]
    Remote {
        /// 只扫描指定别名
        #[arg(long)]
        host: Option<String>,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查询本地历史快照
    ///
    /// 示例：
    ///   suanctl history --since 7d --status warning
    ///   suanctl history --search OOM
    ///   suanctl history --show snapshots:xxx --json
    #[command(verbatim_doc_comment)]
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
    ///
    /// 构建时检测到 nccl.h + libnccl 才会编入二进制，否则命令提示不可用。
    ///
    /// 示例：suanctl nccl --gpus 8 --bytes 268435456 --iterations 50
    #[command(verbatim_doc_comment)]
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
    ///
    /// 通过 ~/.ssh/config 别名把同版本二进制部署到对端；
    /// 不带 COMMAND 时仅部署并显示状态，带 COMMAND 时在远程执行该子命令。
    ///
    /// 示例：
    ///   suanctl agent gpu-node-1
    ///   suanctl agent gpu-node-1 doctor --json
    ///   suanctl agent gpu-node-1 report --format markdown --output /tmp/r.md
    #[command(verbatim_doc_comment)]
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
    ///
    /// 示例：suanctl identity --json
    #[command(verbatim_doc_comment)]
    Identity {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 查询日志异常事件（跨快照）
    ///
    /// 事件在 save 时从日志异常展开入库。
    ///
    /// 示例：
    ///   suanctl log-events --pattern xid --limit 50
    ///   suanctl log-events --stats
    #[command(verbatim_doc_comment)]
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
    /// 对推理服务端点做并发压测（OpenAI 兼容 /v1/chat/completions）
    ///
    /// 会向目标端点发送真实推理请求（显式操作，非只读采集）。
    /// 端点来源优先级：--endpoint > 配置文件 [[endpoints]] > 自动发现
    /// （进程/容器 + 探针确认可达）。
    ///
    /// 示例：
    ///   suanctl bench --list                          # 只列出可压测端点
    ///   suanctl bench                                 # 自动发现第一个可达端点
    ///   suanctl bench --endpoint http://127.0.0.1:8080 --prompts 32 --concurrency 8
    ///   suanctl bench --endpoint 127.0.0.1:8080 --model qwen --json
    #[command(verbatim_doc_comment)]
    Bench {
        /// 目标端点（如 http://127.0.0.1:8080）；缺省从配置/自动发现选择
        #[arg(long)]
        endpoint: Option<String>,
        /// 模型 id；缺省查询 /v1/models 取第一个
        #[arg(long)]
        model: Option<String>,
        /// 总请求数（默认 16）
        #[arg(long, default_value_t = 16)]
        prompts: usize,
        /// 并发数（默认 4）
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// 每请求最大生成 token 数（默认 128）
        #[arg(long, default_value_t = 128)]
        max_tokens: u32,
        /// 单请求超时秒数（默认 120）
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// 只列出可压测端点，不执行压测
        #[arg(long)]
        list: bool,
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 快速配置网络（netplan）：交互式设置静态 IP / DHCP，免手写 YAML
    ///
    /// net show 只读查看；net set 生成 /etc/netplan/60-suanctl-<iface>.yaml
    /// 并执行 netplan apply（需 root），写入前自动备份、失败自动回滚。
    #[command(verbatim_doc_comment)]
    Net {
        #[command(subcommand)]
        action: NetAction,
    },
}

#[derive(Debug, Subcommand)]
enum NetAction {
    /// 列出网卡、当前地址与 netplan 配置文件（只读）
    #[command(verbatim_doc_comment)]
    Show {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },
    /// 配置网卡 IP：缺省参数时逐项交互询问
    ///
    /// 交互流程：选网卡 → DHCP/静态 → 地址/网关/DNS → 预览 → 确认。
    /// 应用前备份 /etc/netplan 到 ~/.suanctl/netplan-backup/<时间戳>/；
    /// netplan apply 失败自动回滚。SSH 会话内修改当前网卡可能断连。
    ///
    /// 示例：
    ///   suanctl net set                              # 交互式
    ///   suanctl net set eno1 --dhcp --yes
    ///   suanctl net set eno1 --address 192.168.1.10/24 --gateway 192.168.1.1 --dns 114.114.114.114
    ///   suanctl net set eno1 --address 192.168.1.10/24 --dry-run   # 只预览 YAML
    #[command(verbatim_doc_comment)]
    Set {
        /// 网卡名（如 eno1）；缺省交互选择
        #[arg(value_name = "IFACE")]
        iface: Option<String>,
        /// 使用 DHCP
        #[arg(long, conflicts_with = "address")]
        dhcp: bool,
        /// 静态地址 CIDR（如 192.168.1.10/24）
        #[arg(long)]
        address: Option<String>,
        /// 默认网关（如 192.168.1.1）
        #[arg(long)]
        gateway: Option<String>,
        /// DNS，逗号分隔（如 114.114.114.114,8.8.8.8）
        #[arg(long)]
        dns: Option<String>,
        /// 跳过确认直接应用（仍需 root）
        #[arg(long)]
        yes: bool,
        /// 只打印将生成的 netplan YAML，不写入不应用
        #[arg(long)]
        dry_run: bool,
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
            let collector = ChainP2pCollector::new();
            let mut collection = if benchmark {
                collector.collect_with_benchmark()
            } else {
                collector.collect_topology()
            };
            // 用 GPU 清单 × PCIe 树补齐每条链路的上行汇聚点；任一采集失败时保持"未知"。
            if let Ok(gpus) = ChainGpuCollector::new().collect_gpus() {
                let pcie = LinuxPcieCollector::new().collect_snapshot();
                enrich_p2p_upstream(&gpus, &pcie.devices, &mut collection.snapshot);
            }
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
        Command::Net { action } => {
            run_net(action)?;
        }
        Command::Bench {
            endpoint,
            model,
            prompts,
            concurrency,
            max_tokens,
            timeout,
            list,
            json,
        } => {
            run_bench_command(
                &config,
                endpoint,
                model,
                prompts,
                concurrency,
                max_tokens,
                timeout,
                list,
                json,
            )?;
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
            "GPU{} -> GPU{}  路径={}  读={} 写={} PCIe={} NVLink={} 原子={} 汇聚={}",
            link.source_gpu,
            link.target_gpu,
            link.topology_path.as_deref().unwrap_or("未知"),
            link.read.label(),
            link.write.label(),
            link.pcie.label(),
            link.nvlink.label(),
            link.atomics.label(),
            link.upstream_meeting_bdf.as_deref().unwrap_or("未知")
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

fn run_net(action: NetAction) -> Result<(), Box<dyn Error>> {
    match action {
        NetAction::Show { json } => run_net_show(json),
        NetAction::Set {
            iface,
            dhcp,
            address,
            gateway,
            dns,
            yes,
            dry_run,
        } => run_net_set(iface, dhcp, address, gateway, dns, yes, dry_run),
    }
}

fn run_net_show(json: bool) -> Result<(), Box<dyn Error>> {
    let interfaces = suanctl::net::list_interfaces()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&interfaces)?);
        return Ok(());
    }
    println!("网卡清单：");
    for iface in &interfaces {
        let mac = iface.mac.as_deref().unwrap_or("MAC 未知");
        let addresses = if iface.addresses.is_empty() {
            "（无地址）".to_owned()
        } else {
            iface.addresses.join(", ")
        };
        println!(
            "  {:<10} {:<7} {}  {}",
            iface.name,
            iface.state.to_uppercase(),
            mac,
            addresses
        );
    }
    let netplan_dir = std::path::Path::new("/etc/netplan");
    let configs: Vec<String> = std::fs::read_dir(netplan_dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".yaml"))
                .collect()
        })
        .unwrap_or_default();
    if configs.is_empty() {
        println!("netplan 配置：未发现 /etc/netplan/*.yaml");
    } else {
        println!("netplan 配置：{}", configs.join(", "));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_net_set(
    iface: Option<String>,
    dhcp: bool,
    address: Option<String>,
    gateway: Option<String>,
    dns: Option<String>,
    yes: bool,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    use std::io::IsTerminal;
    use suanctl::net;
    let interfaces = net::list_interfaces()?;
    if interfaces.is_empty() {
        return Err("未发现可用网卡（/sys/class/net 为空）".into());
    }
    let interactive = std::io::stdin().is_terminal();

    // 1. 网卡：参数优先，否则交互选择。
    let iface = match iface {
        Some(name) => {
            if interfaces.iter().all(|item| item.name != name) {
                let known: Vec<&str> = interfaces.iter().map(|item| item.name.as_str()).collect();
                return Err(format!("网卡 {name} 不存在；可用：{}", known.join(", ")).into());
            }
            name
        }
        None => {
            if !interactive {
                return Err("非交互环境必须指定 IFACE 参数（或配合 --dry-run 预览）".into());
            }
            println!("可用网卡：");
            for (index, item) in interfaces.iter().enumerate() {
                let addresses = if item.addresses.is_empty() {
                    "（无地址）".to_owned()
                } else {
                    item.addresses.join(", ")
                };
                println!(
                    "  {}. {:<10} {:<7} {}",
                    index + 1,
                    item.name,
                    item.state.to_uppercase(),
                    addresses
                );
            }
            let choice = prompt("请选择网卡编号")?;
            let index: usize = choice
                .trim()
                .parse()
                .ok()
                .filter(|index: &usize| *index >= 1 && *index <= interfaces.len())
                .ok_or("编号无效")?;
            interfaces[index - 1].name.clone()
        }
    };
    if let Some(current) = interfaces.iter().find(|item| item.name == iface) {
        println!(
            "当前 {iface}：状态 {}，地址 {}",
            current.state.to_uppercase(),
            if current.addresses.is_empty() {
                "（无）".to_owned()
            } else {
                current.addresses.join(", ")
            }
        );
    }

    // 2. 模式：--dhcp / --address 优先，否则交互询问。
    let mode = if dhcp {
        net::NetplanMode::Dhcp
    } else if let Some(address) = address {
        net::NetplanMode::Static(net::StaticConfig {
            address: net::validate_cidr(&address)?,
            gateway: gateway.map(|value| net::validate_ip(&value)).transpose()?,
            dns: parse_dns_list(dns.as_deref())?,
        })
    } else if interactive {
        let choice = prompt("DHCP 自动获取还是静态地址？(d/s)")?;
        if choice.trim().eq_ignore_ascii_case("d") {
            net::NetplanMode::Dhcp
        } else {
            let address = loop {
                let input = prompt("静态地址 CIDR（如 192.168.1.10/24）")?;
                match net::validate_cidr(&input) {
                    Ok(value) => break value,
                    Err(error) => println!("{error}，请重输"),
                }
            };
            let gateway = loop {
                let input = prompt("默认网关（可留空）")?;
                if input.trim().is_empty() {
                    break None;
                }
                match net::validate_ip(&input) {
                    Ok(value) => break Some(value),
                    Err(error) => println!("{error}，请重输"),
                }
            };
            let dns = loop {
                let input = prompt("DNS，逗号分隔（可留空）")?;
                if input.trim().is_empty() {
                    break Vec::new();
                }
                match parse_dns_list(Some(&input)) {
                    Ok(value) => break value,
                    Err(error) => println!("{error}，请重输"),
                }
            };
            net::NetplanMode::Static(net::StaticConfig {
                address,
                gateway,
                dns,
            })
        }
    } else {
        return Err("非交互环境需要 --dhcp 或 --address 指定配置方式".into());
    };

    // 3. 预览与冲突提示。
    let yaml = net::render_netplan(&iface, &mode);
    println!("\n将生成 /etc/netplan/60-suanctl-{iface}.yaml：\n\n{yaml}");
    let netplan_dir = std::path::Path::new("/etc/netplan");
    let conflicts = net::find_conflicting_files(netplan_dir, &iface);
    if !conflicts.is_empty() {
        println!("注意：以下既有配置也包含 {iface}，netplan 合并时数值以文件名靠后者为准：");
        for path in &conflicts {
            println!("  - {}", path.display());
        }
    }
    if dry_run {
        println!("（dry-run：未写入、未应用）");
        return Ok(());
    }

    // 4. 确认并应用。
    if !net::is_root() {
        return Err("写入 /etc/netplan 与执行 netplan apply 需要 root，请用 sudo 运行".into());
    }
    if !yes {
        if !interactive {
            return Err("非交互环境需要 --yes 确认应用".into());
        }
        if std::env::var_os("SSH_CONNECTION").is_some() {
            println!("警告：检测到 SSH 会话，修改 {iface} 可能中断当前连接。");
        }
        let confirm = prompt("确认应用以上配置？(y/N)")?;
        if !confirm.trim().eq_ignore_ascii_case("y") {
            println!("已取消。");
            return Ok(());
        }
    }
    let backup_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".suanctl")
        .join("netplan-backup")
        .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    let (file, backup) = net::apply_netplan(netplan_dir, &backup_dir, &iface, &yaml)?;
    println!(
        "已写入 {}（备份在 {}）并执行 netplan apply。",
        file.display(),
        backup.display()
    );
    Ok(())
}

fn parse_dns_list(value: Option<&str>) -> Result<Vec<String>, Box<dyn Error>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| suanctl::net::validate_ip(item).map_err(|error| error.into()))
        .collect()
}

fn prompt(question: &str) -> Result<String, Box<dyn Error>> {
    use std::io::Write;
    print!("{question}：");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

#[allow(clippy::too_many_arguments)]
fn run_bench_command(
    config: &SuanctlConfig,
    endpoint: Option<String>,
    model: Option<String>,
    prompts: usize,
    concurrency: usize,
    max_tokens: u32,
    timeout: u64,
    list: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    use suanctl::bench;

    // 显式指定端点：直接压测。
    if let Some(endpoint) = endpoint {
        let report = bench::run_bench(&bench::BenchConfig {
            endpoint,
            model,
            prompts,
            concurrency,
            max_tokens,
            timeout_secs: timeout,
        })?;
        print_bench_report(&report, json);
        return Ok(());
    }

    // 否则：配置文件端点（优先）+ 自动发现，探活后取候选。
    let configured = config.to_configured_endpoints().unwrap_or_default();
    let candidates = bench::discover_candidates(configured);
    if candidates.is_empty() {
        if list {
            println!("未发现可压测的推理端点（可 --endpoint 显式指定，或配置 [[endpoints]]）。");
            return Ok(());
        }
        return Err(
            "未发现可压测的推理端点。请用 --endpoint 显式指定，或在 suanctl.toml 配置 [[endpoints]]。"
                .into(),
        );
    }
    if list {
        if json {
            println!("{}", serde_json::to_string_pretty(&candidates)?);
        } else {
            println!("可压测端点（探针可达）：");
            for service in &candidates {
                println!(
                    "  {:<16} {:<10} {}  模型：{}",
                    service.name,
                    service.engine.label(),
                    service.endpoint.as_deref().unwrap_or(""),
                    service.model.as_deref().unwrap_or("未知")
                );
            }
        }
        return Ok(());
    }
    let chosen = &candidates[0];
    if candidates.len() > 1 {
        eprintln!(
            "发现 {} 个可达端点，使用第一个：{}（{}）；其余可用 --endpoint 指定。",
            candidates.len(),
            chosen.name,
            chosen.endpoint.as_deref().unwrap_or("")
        );
    }
    let report = bench::run_bench(&bench::BenchConfig {
        endpoint: chosen.endpoint.clone().expect("候选必有端点"),
        model: model.or_else(|| chosen.model.clone()),
        prompts,
        concurrency,
        max_tokens,
        timeout_secs: timeout,
    })?;
    print_bench_report(&report, json);
    Ok(())
}

fn print_bench_report(report: &suanctl::bench::BenchReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).expect("序列化压测报告")
        );
        return;
    }
    println!("压测结果：{}（模型 {}）", report.endpoint, report.model);
    println!(
        "  请求：总数 {} · 并发 {} · 成功 {} · 失败 {}",
        report.prompts, report.concurrency, report.succeeded, report.failed
    );
    println!("  总耗时：{:.2}s", report.wall_seconds);
    match report.tokens_per_second {
        Some(tps) => println!(
            "  吞吐：{:.1} tok/s（共 {} completion tokens）",
            tps, report.completion_tokens_total
        ),
        None => println!(
            "  吞吐：响应未带 usage，总吞吐未知；单请求均值 {}",
            report
                .per_request_tokens_per_second
                .map(|value| format!("{value:.1} tok/s"))
                .unwrap_or_else(|| "未知".to_owned())
        ),
    }
    println!(
        "  延迟 ms：avg {:.0} · p50 {:.0} · p95 {:.0} · max {:.0}",
        report.latency_avg_ms, report.latency_p50_ms, report.latency_p95_ms, report.latency_max_ms
    );
    if !report.errors.is_empty() {
        println!("  失败样例：{}", report.errors.join("；"));
    }
}
