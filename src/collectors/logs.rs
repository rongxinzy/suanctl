//! 系统日志只读采集器。
//!
//! 能力提炼自 llama-test-matrix 的 blackbox 日志跟随模块：多源采集
//! （`dmesg`、`journalctl`、`/var/log` 常见文件）并检测 NVIDIA/PCIe/内核
//! 异常模式（Xid、NVRM、PCIe AER、ECC 等）。适配 suanctl 的一次性快照
//! 哲学：不常驻跟随、不保存完整日志流，只保留尾部行与异常匹配，作为证据
//! 进入诊断与报告。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use regex::Regex;

use super::command::{CommandRequest, CommandRunner, ProcessCommandRunner};
use crate::domain::{
    CollectionIssue, HealthStatus, LocalProbeStatus, LogPatternMatch, LogSnapshot,
    LogSourceSnapshot,
};

/// 每个日志源最多保留的尾部行数。
pub const MAX_TAIL_LINES: usize = 200;
/// 单行日志最多保留的字符数（证据白名单，避免把无界日志塞进报告）。
pub const MAX_LINE_CHARS: usize = 300;
/// 每个异常模式最多保留的样例行数。
pub const MAX_EXAMPLES_PER_PATTERN: usize = 5;

pub trait LogsCollector {
    fn collect_logs(&self) -> LogSnapshot;
}

/// 显式占位实现：供受控装配路径（如纯构造测试）使用，不读取系统日志。
pub struct UnavailableLogsCollector;

impl LogsCollector for UnavailableLogsCollector {
    fn collect_logs(&self) -> LogSnapshot {
        LogSnapshot {
            sources: Vec::new(),
            matches: Vec::new(),
            issues: Vec::new(),
            status: HealthStatus::Unavailable,
        }
    }
}

/// 用户通过配置追加的异常日志模式。正则由 `config.rs` 校验后传入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPattern {
    pub name: String,
    pub regex: String,
    pub severity: HealthStatus,
}

/// 异常日志模式：名称、正则、严重级别。正则来自 llama-test-matrix blackbox
/// 默认触发正则的提炼（去掉过于宽泛的 `ERR!?`，避免误报）。
struct LogPattern {
    id: String,
    regex: Regex,
    severity: HealthStatus,
}

fn patterns() -> Vec<LogPattern> {
    [
        ("xid", r"NVRM: Xid|Xid\s*\(", HealthStatus::Critical),
        // NVRM 行很多是正常的（模块加载、GSP firmware loaded），只有携带错误语义的
        // 才算异常；Rust regex 不支持负向断言，用枚举错误关键词。
        (
            "nvrm",
            r"(?i)NVRM:.*(xid|error|fail|fault|fallen)",
            HealthStatus::Critical,
        ),
        (
            "gpu_fallen_off",
            r"GPU has fallen off|fallen off the bus",
            HealthStatus::Critical,
        ),
        (
            "pcie_bus_error",
            r"PCIe Bus Error|AER:|pcieport",
            HealthStatus::Warning,
        ),
        (
            "nvlink_error",
            r"NVLink.*(?:error|Error)",
            HealthStatus::Warning,
        ),
        ("ecc", r"\bECC\b|uncorrectable", HealthStatus::Warning),
        (
            "rm_init",
            r"RmInit|Failed to initialize NVML|Unknown Error|(?i)GSP.*(error|fail|fault)",
            HealthStatus::Warning,
        ),
    ]
    .into_iter()
    .map(|(id, pattern, severity)| LogPattern {
        id: id.to_owned(),
        regex: Regex::new(pattern).expect("日志异常模式必须是合法正则"),
        severity,
    })
    .collect()
}

/// 合并内置模式与配置追加模式；非法追加正则被跳过。
fn merged_patterns(extra: &[ConfiguredPattern]) -> Vec<LogPattern> {
    let mut all = patterns();
    all.extend(extra.iter().filter_map(|pattern| {
        Some(LogPattern {
            id: pattern.name.clone(),
            regex: Regex::new(&pattern.regex).ok()?,
            severity: pattern.severity,
        })
    }));
    all
}

/// 文件型日志源：路径 + 稳定名称。
const FILE_SOURCES: [(&str, &str); 3] = [
    ("/var/log/kern.log", "kern.log"),
    ("/var/log/messages", "messages"),
    ("/var/log/syslog", "syslog"),
];

pub struct LinuxLogCollector<R = ProcessCommandRunner> {
    runner: R,
    file_sources: Vec<PathBuf>,
    extra_patterns: Vec<ConfiguredPattern>,
}

impl LinuxLogCollector<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            file_sources: FILE_SOURCES
                .iter()
                .map(|(path, _)| PathBuf::from(path))
                .collect(),
            extra_patterns: Vec::new(),
        }
    }
}

impl Default for LinuxLogCollector<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: CommandRunner> LinuxLogCollector<R> {
    /// 受控构造：用于纯逻辑测试，不采集真实系统文件。
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            file_sources: Vec::new(),
            extra_patterns: Vec::new(),
        }
    }

    pub fn with_file_sources(mut self, paths: Vec<PathBuf>) -> Self {
        self.file_sources = paths;
        self
    }

    /// 追加配置文件中的异常日志模式。
    pub fn with_extra_patterns(mut self, patterns: Vec<ConfiguredPattern>) -> Self {
        self.extra_patterns = patterns;
        self
    }
}

impl<R: CommandRunner> LogsCollector for LinuxLogCollector<R> {
    fn collect_logs(&self) -> LogSnapshot {
        let patterns = merged_patterns(&self.extra_patterns);
        let mut state = CollectionState::default();

        collect_command_source(
            &self.runner,
            "dmesg",
            "dmesg",
            &["-T"],
            &patterns,
            &mut state,
        );

        collect_command_source(
            &self.runner,
            "journalctl_kernel",
            "journalctl",
            &["-k", "-n", "500", "--no-pager", "-o", "short-iso"],
            &patterns,
            &mut state,
        );

        collect_command_source(
            &self.runner,
            "journalctl_nvidia",
            "journalctl",
            &[
                "-n",
                "300",
                "--no-pager",
                "-o",
                "short-iso",
                "-u",
                "nvidia-persistenced",
                "-u",
                "nvidia-fabricmanager",
                "-u",
                "nvidia-dcgm",
            ],
            &patterns,
            &mut state,
        );

        for path in &self.file_sources {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            collect_file_source(Path::new(path), &name, &patterns, &mut state);
        }

        let matches_vec = state.matches.into_sorted();
        let status = overall_status(&state.sources, &matches_vec);
        LogSnapshot {
            sources: state.sources,
            matches: matches_vec,
            issues: state.issues,
            status,
        }
    }
}

/// 一次日志采集的运行状态：来源、问题与异常匹配聚合。
#[derive(Default)]
struct CollectionState {
    sources: Vec<LogSourceSnapshot>,
    issues: Vec<CollectionIssue>,
    matches: Matches,
}

/// 运行中收集的异常匹配，按模式聚合。
#[derive(Default)]
struct Matches {
    by_pattern: BTreeMap<String, LogPatternMatch>,
}

impl Matches {
    fn record(&mut self, pattern: &LogPattern, source: &str, line: &str) {
        let entry = self
            .by_pattern
            .entry(pattern.id.clone())
            .or_insert_with(|| LogPatternMatch {
                pattern: pattern.id.clone(),
                severity: pattern.severity,
                count: 0,
                sources: Vec::new(),
                examples: Vec::new(),
            });
        entry.count += 1;
        if !entry.sources.iter().any(|s| s == source) {
            entry.sources.push(source.to_owned());
        }
        if entry.examples.len() < MAX_EXAMPLES_PER_PATTERN {
            entry.examples.push(truncate_line(line));
        }
    }

    fn into_sorted(self) -> Vec<LogPatternMatch> {
        let mut matches: Vec<_> = self.by_pattern.into_values().collect();
        // 严重级别高的在前（Critical 优先展示）。
        matches.sort_by_key(|m| std::cmp::Reverse(status_rank(m.severity)));
        matches
    }
}

fn collect_command_source(
    runner: &dyn CommandRunner,
    name: &str,
    program: &str,
    args: &[&str],
    patterns: &[LogPattern],
    state: &mut CollectionState,
) {
    let command = format!("{} {}", program, args.join(" "));
    let request = CommandRequest::new(program, args.iter().copied());
    let output = match runner.run(&request) {
        Ok(output) => output,
        Err(error) => {
            // 工具缺失（如无 journald 的系统）是正常环境差异，不算采集问题。
            state.sources.push(source_snapshot(
                name,
                LocalProbeStatus::Unavailable,
                Some(&command),
                None,
                Vec::new(),
                false,
                0,
            ));
            debug_assert!(error.collector == "command");
            return;
        }
    };
    if !output.success {
        state.issues.push(CollectionIssue {
            collector: "logs".to_owned(),
            code: "log_cmd_failed".to_owned(),
            status: HealthStatus::Unavailable,
            message: format!(
                "{name} 采集命令执行失败：rc={:?} {}",
                output.exit_code,
                output.stderr.trim()
            ),
        });
        state.sources.push(source_snapshot(
            name,
            LocalProbeStatus::Failed,
            Some(&command),
            None,
            Vec::new(),
            false,
            0,
        ));
        return;
    }

    let lines: Vec<String> = output.stdout.lines().map(truncate_line).collect();
    let match_count = scan_lines(&lines, name, patterns, &mut state.matches);
    let truncated = lines.len() > MAX_TAIL_LINES;
    let tail: Vec<String> = lines
        .iter()
        .rev()
        .take(MAX_TAIL_LINES)
        .rev()
        .cloned()
        .collect();
    state.sources.push(source_snapshot(
        name,
        LocalProbeStatus::Succeeded,
        Some(&command),
        None,
        tail,
        truncated,
        match_count,
    ));
}

fn collect_file_source(
    path: &Path,
    name: &str,
    patterns: &[LogPattern],
    state: &mut CollectionState,
) {
    if !path.is_file() {
        state.sources.push(source_snapshot(
            name,
            LocalProbeStatus::Unavailable,
            None,
            Some(path.to_string_lossy().into_owned()),
            Vec::new(),
            false,
            0,
        ));
        return;
    }
    match tail_file_lines(path, MAX_TAIL_LINES) {
        Ok((lines, truncated)) => {
            let match_count = scan_lines(&lines, name, patterns, &mut state.matches);
            state.sources.push(source_snapshot(
                name,
                LocalProbeStatus::Succeeded,
                None,
                Some(path.to_string_lossy().into_owned()),
                lines,
                truncated,
                match_count,
            ));
        }
        Err(error) => {
            state.issues.push(CollectionIssue {
                collector: "logs".to_owned(),
                code: "log_file_unreadable".to_owned(),
                status: HealthStatus::Unavailable,
                message: format!("无法读取 {}：{error}", path.display()),
            });
            state.sources.push(source_snapshot(
                name,
                LocalProbeStatus::Failed,
                None,
                Some(path.to_string_lossy().into_owned()),
                Vec::new(),
                false,
                0,
            ));
        }
    }
}

fn source_snapshot(
    name: &str,
    probe_status: LocalProbeStatus,
    command: Option<&str>,
    path: Option<String>,
    lines_tail: Vec<String>,
    truncated: bool,
    match_count: usize,
) -> LogSourceSnapshot {
    LogSourceSnapshot {
        name: name.to_owned(),
        probe_status,
        command: command.map(str::to_owned),
        path,
        lines_tail,
        truncated,
        match_count,
    }
}

/// 对日志行逐一匹配异常模式，返回该来源的命中行数。
fn scan_lines(
    lines: &[String],
    source: &str,
    patterns: &[LogPattern],
    matches: &mut Matches,
) -> usize {
    let mut count = 0;
    for line in lines {
        for pattern in patterns {
            if pattern.regex.is_match(line) {
                matches.record(pattern, source, line);
                count += 1;
            }
        }
    }
    count
}

fn overall_status(sources: &[LogSourceSnapshot], matches: &[LogPatternMatch]) -> HealthStatus {
    if let Some(worst) = matches
        .iter()
        .map(|m| m.severity)
        .max_by_key(|status| status_rank(*status))
    {
        return worst;
    }
    if sources
        .iter()
        .any(|source| source.probe_status == LocalProbeStatus::Succeeded)
    {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unavailable
    }
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

fn truncate_line(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.chars().count() > MAX_LINE_CHARS {
        let mut result: String = trimmed.chars().take(MAX_LINE_CHARS).collect();
        result.push('…');
        result
    } else {
        trimmed.to_owned()
    }
}

/// 从文件尾部读取最多 max_lines 行，返回 (行, 是否截断)。
/// 单行不受限时会截断，避免无界内存与无界报告内容。
fn tail_file_lines(path: &Path, max_lines: usize) -> std::io::Result<(Vec<String>, bool)> {
    let mut file = File::open(path)?;
    let length = file.seek(SeekFrom::End(0))?;
    if length == 0 {
        return Ok((Vec::new(), false));
    }

    let mut lines = Vec::new();
    let mut position = length;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut pending = String::new();

    while position > 0 && lines.len() < max_lines {
        let chunk_len = position.min(buffer.len() as u64) as usize;
        position -= chunk_len as u64;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut buffer[..chunk_len])?;
        let text = String::from_utf8_lossy(&buffer[..chunk_len]);
        pending = format!("{text}{pending}");
        while lines.len() < max_lines {
            match pending.rfind('\n') {
                Some(index) => {
                    let line = pending[index + 1..].to_string();
                    if !line.is_empty() {
                        lines.push(truncate_line(&line));
                    }
                    pending.truncate(index);
                }
                None => break,
            }
        }
        // 防御：残留 pending 是未完成的行（文件开头或超长单行）。
        // 无换行且超长时才截断，避免无界内存；正常多行内容不受影响。
        if !pending.contains('\n') && pending.len() > MAX_LINE_CHARS * 4 {
            let keep = pending.len() - MAX_LINE_CHARS * 4;
            pending.drain(..keep);
        }
    }

    let truncated = if lines.len() >= max_lines {
        position > 0 || pending.contains('\n')
    } else {
        false
    };
    if lines.len() < max_lines && !pending.is_empty() {
        lines.push(truncate_line(&pending));
    }
    lines.reverse();
    Ok((lines, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::command::{CommandOutput, CommandRequest, CommandRunner};
    use crate::collectors::CollectorError;

    struct FakeRunner {
        responses: std::collections::HashMap<String, CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            match self.responses.get(&request.program) {
                Some(output) => Ok(output.clone()),
                None => Err(CollectorError::unavailable(
                    "command",
                    format!("missing {} in fake runner", request.program),
                )),
            }
        }
    }

    fn output(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            exit_code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn detects_xid_as_critical() {
        let runner = FakeRunner {
            responses: std::collections::HashMap::from([
                (
                    "dmesg".to_owned(),
                    output("NVRM: Xid (PCI:0000:03:00): 31, pid=123, name=python\n"),
                ),
                ("journalctl".to_owned(), output("kernel: boot ok\n")),
            ]),
        };
        let snapshot = LinuxLogCollector::with_runner(runner).collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Critical);
        assert!(snapshot
            .matches
            .iter()
            .any(|m| m.pattern == "xid" && m.count == 1));
        assert!(snapshot
            .matches
            .iter()
            .any(|m| m.pattern == "nvrm" && m.count == 1));
    }

    #[test]
    fn benign_nvrm_module_load_is_not_an_anomaly() {
        // 真机 new8F6 的 dmesg：NVRM 模块加载行是正常日志，不得命中 nvrm 模式。
        let runner = FakeRunner {
            responses: std::collections::HashMap::from([
                (
                    "dmesg".to_owned(),
                    output(
                        "NVRM: loading NVIDIA UNIX Open Kernel Module for x86_64  595.58.03\nNVRM: GSP firmware loaded successfully\n",
                    ),
                ),
                ("journalctl".to_owned(), output("kernel: boot ok\n")),
            ]),
        };
        let snapshot = LinuxLogCollector::with_runner(runner).collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert!(!snapshot.matches.iter().any(|m| m.pattern == "nvrm"));
    }

    #[test]
    fn clean_logs_are_healthy() {
        let runner = FakeRunner {
            responses: std::collections::HashMap::from([
                ("dmesg".to_owned(), output("Linux version 6.8.0\n")),
                ("journalctl".to_owned(), output("systemd: Started.\n")),
            ]),
        };
        let snapshot = LinuxLogCollector::with_runner(runner).collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert!(snapshot.matches.is_empty());
    }

    #[test]
    fn missing_tools_are_unavailable_not_fatal() {
        let runner = FakeRunner {
            responses: std::collections::HashMap::new(),
        };
        let snapshot = LinuxLogCollector::with_runner(runner).collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Unavailable);
        assert!(snapshot
            .sources
            .iter()
            .all(|s| s.probe_status == LocalProbeStatus::Unavailable));
    }

    #[test]
    fn truncates_long_lines() {
        let long = "x".repeat(1000);
        assert!(truncate_line(&long).chars().count() <= MAX_LINE_CHARS + 1);
    }

    #[test]
    fn custom_patterns_from_config_are_detected() {
        let runner = FakeRunner {
            responses: std::collections::HashMap::from([(
                "dmesg".to_owned(),
                output("app demo-failure-123 in module x\n"),
            )]),
        };
        let snapshot = LinuxLogCollector::with_runner(runner)
            .with_extra_patterns(vec![ConfiguredPattern {
                name: "demo_error".to_owned(),
                regex: "demo-failure-123".to_owned(),
                severity: HealthStatus::Critical,
            }])
            .collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Critical);
        assert!(snapshot
            .matches
            .iter()
            .any(|m| m.pattern == "demo_error" && m.count == 1));
    }

    #[test]
    fn invalid_extra_pattern_is_skipped_not_fatal() {
        let runner = FakeRunner {
            responses: std::collections::HashMap::from([(
                "dmesg".to_owned(),
                output("kernel ok\n"),
            )]),
        };
        let snapshot = LinuxLogCollector::with_runner(runner)
            .with_extra_patterns(vec![ConfiguredPattern {
                name: "broken".to_owned(),
                regex: "(".to_owned(),
                severity: HealthStatus::Warning,
            }])
            .collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert!(snapshot.matches.is_empty());
    }

    #[test]
    fn file_source_detects_all_builtin_patterns_from_fixture() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/collectors/fixtures/syslog_multiple_errors.log");
        let runner = FakeRunner {
            responses: std::collections::HashMap::new(),
        };
        let snapshot = LinuxLogCollector::with_runner(runner)
            .with_file_sources(vec![fixture.clone()])
            .collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Critical);
        assert_eq!(snapshot.sources.len(), 4, "3 个命令源 + 1 个文件源");
        let file_source = snapshot
            .sources
            .iter()
            .find(|source| source.path.as_deref() == Some(fixture.to_str().unwrap()))
            .expect("应包含文件源");
        assert_eq!(file_source.probe_status, LocalProbeStatus::Succeeded);

        let patterns: Vec<&str> = snapshot
            .matches
            .iter()
            .map(|m| m.pattern.as_str())
            .collect();
        for expected in [
            "xid",
            "nvrm",
            "gpu_fallen_off",
            "pcie_bus_error",
            "nvlink_error",
            "ecc",
            "rm_init",
        ] {
            assert!(
                patterns.contains(&expected),
                "缺少模式 {expected}: {patterns:?}"
            );
        }
        // 命中统计：Xid 1 行；NVRM 3 行（Xid/fallen off/RmInit 行；
        // "GSP firmware loaded successfully" 是正常日志，不计入）。
        let xid = snapshot
            .matches
            .iter()
            .find(|m| m.pattern == "xid")
            .unwrap();
        let nvrm = snapshot
            .matches
            .iter()
            .find(|m| m.pattern == "nvrm")
            .unwrap();
        let pcie = snapshot
            .matches
            .iter()
            .find(|m| m.pattern == "pcie_bus_error")
            .unwrap();
        let rm_init = snapshot
            .matches
            .iter()
            .find(|m| m.pattern == "rm_init")
            .unwrap();
        assert_eq!(xid.count, 1);
        assert_eq!(nvrm.count, 3);
        assert_eq!(pcie.count, 2);
        assert_eq!(rm_init.count, 1);
    }

    #[test]
    fn real_rx_box_logs_are_clean_and_healthy() {
        // 真实设备 rx-box（172.18.5.123）的 dmesg/syslog/kern.log 尾部：无 GPU 异常。
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/collectors/fixtures/real-rx-box");
        let runner = FakeRunner {
            responses: std::collections::HashMap::new(),
        };
        let snapshot = LinuxLogCollector::with_runner(runner)
            .with_file_sources(vec![
                fixture_dir.join("rx_dmesg_tail.log"),
                fixture_dir.join("rx_syslog_tail.log"),
                fixture_dir.join("rx_kernlog_tail.log"),
            ])
            .collect_logs();
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert!(
            snapshot.matches.is_empty(),
            "真实日志不应误报异常：{:?}",
            snapshot.matches
        );
        let succeeded: Vec<_> = snapshot
            .sources
            .iter()
            .filter(|s| s.probe_status == LocalProbeStatus::Succeeded)
            .collect();
        assert_eq!(succeeded.len(), 3, "三个文件源均应成功读取");
        assert!(succeeded.iter().all(|s| !s.lines_tail.is_empty()));
    }

    #[test]
    fn tail_file_reads_from_end() {
        let dir =
            std::env::temp_dir().join(format!("suanctl-logs-test-many-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tail-test.log");
        let content: String = (0..500).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&path, content).unwrap();

        let (lines, truncated) = tail_file_lines(&path, MAX_TAIL_LINES).unwrap();
        assert_eq!(lines.len(), MAX_TAIL_LINES);
        assert!(truncated);
        assert_eq!(lines[0], "line-300");
        assert_eq!(lines[lines.len() - 1], "line-499");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_file_handles_giant_single_line() {
        let dir =
            std::env::temp_dir().join(format!("suanctl-logs-test-giant-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("giant.log");
        std::fs::write(&path, "y".repeat(1024 * 1024)).unwrap();

        let (lines, truncated) = tail_file_lines(&path, MAX_TAIL_LINES).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!truncated);
        assert!(lines[0].chars().count() <= MAX_LINE_CHARS + 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
