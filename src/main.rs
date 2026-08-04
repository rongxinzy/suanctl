use std::{
    error::Error,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand, ValueEnum};
use suanctl::{
    app::{AppState, UiReportFormat},
    collectors::{
        demo,
        p2p::{NvidiaP2pCollector, P2pCollection},
        runtime::{runtime_status, RuntimeCollection, RuntimeCollector},
    },
    domain::{DoctorReport, HealthStatus, P2pBenchmarkSnapshot, P2pSnapshot},
    storage::{EvidenceReport, EvidenceWriter, ReportFormat},
    tui,
};

#[derive(Debug, Parser)]
#[command(name = "suanctl", version, about = "智算服务器诊断与监控工具")]
struct Cli {
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
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliReportFormat {
    Json,
    Jsonl,
    Markdown,
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

    match cli.command.unwrap_or(Command::Tui { demo: false }) {
        Command::Tui { demo: use_demo } => {
            let collection = if use_demo {
                demo_collection()
            } else {
                RuntimeCollector::new().collect()
            };
            let mut state = AppState::from_collection(collection);
            if use_demo {
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
                )?;
            } else {
                tui::run_with_actions(
                    &mut state,
                    || RuntimeCollector::new().collect(),
                    run_tui_p2p_benchmark,
                    export_runtime_collection,
                )?;
            }
        }
        Command::Doctor {
            json,
            p2p_benchmark,
        } => {
            let report = DoctorReport::from_runtime(collect_runtime(p2p_benchmark));
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
            let collection = collect_runtime(p2p_benchmark);
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

fn run_tui_p2p_benchmark() -> (RuntimeCollection, Result<String, String>) {
    let collection = collect_runtime(true);
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

fn collect_runtime(run_p2p_benchmark: bool) -> RuntimeCollection {
    let mut collection = RuntimeCollector::new().collect();
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
