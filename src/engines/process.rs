//! `/proc` 中推理服务进程的只读发现。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::domain::{
    DiscoveryConfidence, DiscoveryProvenance, DiscoverySourceKind, EndpointKind, EngineKind,
    HealthStatus, ProbeResult, ServiceSnapshot,
};

use super::discovery::{endpoint, source, DiscoveryProvider, DiscoveryResult};

const DEFAULT_MAX_PIDS: usize = 4096;
const DEFAULT_MAX_CMDLINE_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_TOTAL_CMDLINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARGS: usize = 128;
const MAX_TOKEN_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessLimits {
    pub max_pids: usize,
    pub max_cmdline_bytes: usize,
    pub max_total_cmdline_bytes: usize,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            max_pids: DEFAULT_MAX_PIDS,
            max_cmdline_bytes: DEFAULT_MAX_CMDLINE_BYTES,
            max_total_cmdline_bytes: DEFAULT_MAX_TOTAL_CMDLINE_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostProcessProvider {
    pub proc_root: PathBuf,
    pub limits: ProcessLimits,
}

impl HostProcessProvider {
    pub fn new(proc_root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: proc_root.into(),
            limits: ProcessLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: ProcessLimits) -> Self {
        self.limits = limits;
        self
    }
}

impl Default for HostProcessProvider {
    fn default() -> Self {
        Self::new("/proc")
    }
}

impl DiscoveryProvider for HostProcessProvider {
    fn discover(&self) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();
        let mut pids = Vec::new();
        let entries = match fs::read_dir(&self.proc_root) {
            Ok(entries) => entries,
            Err(error) => {
                result.issues.push(issue(
                    "proc_unavailable",
                    HealthStatus::Unavailable,
                    format!("无法读取 proc 根目录：{error}"),
                ));
                return result;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.issues.push(issue(
                        "proc_entry_unreadable",
                        HealthStatus::Warning,
                        format!("读取 proc 条目失败：{error}"),
                    ));
                    continue;
                }
            };
            let name = entry.file_name();
            if let Some(pid) = name.to_str().and_then(|value| value.parse::<u32>().ok()) {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        if pids.len() > self.limits.max_pids {
            result.issues.push(issue(
                "pid_limit",
                HealthStatus::Warning,
                format!("PID 数量超过上限，已限制为 {} 项", self.limits.max_pids),
            ));
            pids.truncate(self.limits.max_pids);
        }

        let mut remaining = self.limits.max_total_cmdline_bytes;
        for pid in pids {
            if remaining == 0 {
                result.issues.push(issue(
                    "cmdline_total_limit",
                    HealthStatus::Warning,
                    "cmdline 总读取量达到上限，后续 PID 未继续读取",
                ));
                break;
            }
            let limit = self.limits.max_cmdline_bytes.min(remaining);
            let path = self.proc_root.join(pid.to_string()).join("cmdline");
            let (bytes, truncated) = match read_limited(&path, limit) {
                Ok(value) => value,
                Err(error) => {
                    result.issues.push(issue(
                        "cmdline_unreadable",
                        HealthStatus::Warning,
                        format!("PID {pid} cmdline 不可读取：{}", classify_io_error(&error)),
                    ));
                    continue;
                }
            };
            remaining = remaining.saturating_sub(bytes.len());
            if truncated {
                result.issues.push(issue(
                    "cmdline_truncated",
                    HealthStatus::Warning,
                    format!("PID {pid} cmdline 超过单项读取上限"),
                ));
            }
            let args = parse_cmdline(&bytes);
            if args.is_empty() {
                continue;
            }
            let Some(detected) = detect_engine(&args, None, None) else {
                continue;
            };
            result
                .services
                .push(service_from_process(pid, &args, detected));
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EngineMatch {
    pub engine: EngineKind,
    pub confidence: DiscoveryConfidence,
    pub reason: &'static str,
}

/// 只返回可靠上下文匹配，避免把普通的 `--llama` 文本误报成引擎。
pub(crate) fn detect_engine(
    args: &[String],
    image: Option<&str>,
    name: Option<&str>,
) -> Option<EngineMatch> {
    let lower: Vec<String> = args.iter().map(|arg| arg.to_ascii_lowercase()).collect();
    let executable = lower
        .first()
        .map(|value| basename(value))
        .unwrap_or_default();
    let context = format!(
        "{} {}",
        image.unwrap_or_default().to_ascii_lowercase(),
        name.unwrap_or_default().to_ascii_lowercase()
    );

    if matches!(executable, "llama-server" | "llama_server")
        || (executable == "server"
            && has_model_argument(&lower)
            && (has_gguf_model(&lower) || context.contains("llama.cpp")))
    {
        return Some(EngineMatch {
            engine: EngineKind::LlamaCpp,
            confidence: DiscoveryConfidence::High,
            reason: "llama.cpp: llama-server/server 与模型参数上下文匹配",
        });
    }

    if (executable == "vllm" && has_subcommand(&lower, "serve"))
        || has_python_module(&lower, "vllm.entrypoints")
        || (context.contains("vllm") && executable == "api_server")
    {
        return Some(EngineMatch {
            engine: EngineKind::Vllm,
            confidence: DiscoveryConfidence::High,
            reason: "vLLM: vllm serve 或 vllm.entrypoints 模块匹配",
        });
    }

    if executable == "sglang.launch_server"
        || has_python_module(&lower, "sglang.launch_server")
        || (context.contains("sglang") && executable == "launch_server")
    {
        return Some(EngineMatch {
            engine: EngineKind::Sglang,
            confidence: DiscoveryConfidence::High,
            reason: "SGLang: sglang.launch_server 模块匹配",
        });
    }

    None
}

fn service_from_process(pid: u32, args: &[String], detected: EngineMatch) -> ServiceSnapshot {
    let executable = args
        .first()
        .map(|value| basename(value).to_owned())
        .unwrap_or_else(|| "process".to_owned());
    let endpoint_data = extract_endpoint(args);
    let mut provenance = DiscoveryProvenance {
        sources: vec![source(
            DiscoverySourceKind::HostProcess,
            detected.reason,
            [format!("PID {pid}"), format!("可执行文件 {executable}")],
            detected.confidence,
        )],
        endpoints: Vec::new(),
    };
    if let Some(endpoint_data) = &endpoint_data {
        provenance.endpoints.push(endpoint(
            &endpoint_data.url,
            endpoint_data.kind,
            endpoint_data.derivation,
        ));
    }

    ServiceSnapshot {
        name: format!("{}-{pid}", detected.engine.label()),
        engine: detected.engine,
        model: None,
        pid: Some(pid),
        port: endpoint_data.as_ref().map(|data| data.port),
        status: HealthStatus::Unknown,
        process_present: Some(true),
        endpoint: endpoint_data.as_ref().map(|data| data.url.clone()),
        endpoint_reachable: None,
        health_probe: ProbeResult::not_attempted(),
        models_probe: ProbeResult::not_attempted(),
        metrics_probe: ProbeResult::not_attempted(),
        observed_models: Vec::new(),
        observed_metrics: Vec::new(),
        gpu_indices: Vec::new(),
        last_error: None,
        discovery: provenance,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndpointData {
    pub url: String,
    pub port: u16,
    pub kind: EndpointKind,
    pub derivation: &'static str,
}

pub(crate) fn extract_endpoint(args: &[String]) -> Option<EndpointData> {
    let host = extract_flag(args, &["--host"]);
    let port = extract_flag(args, &["--port", "--http-port", "--api-port", "-p"])
        .or_else(|| short_port(args));
    let port = port?.parse::<u16>().ok().filter(|port| *port != 0)?;
    let host = host.filter(|value| valid_host(value));
    let (host, kind, derivation) = match host {
        Some(host) => (host, EndpointKind::Observed, "进程显式 --host/--port"),
        None => (
            "127.0.0.1".to_owned(),
            EndpointKind::Inferred,
            "进程显式端口，主机地址按安全默认值推导",
        ),
    };
    Some(EndpointData {
        url: format_endpoint(&host, port),
        port,
        kind,
        derivation,
    })
}

fn extract_flag(args: &[String], names: &[&str]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name {
                return args
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                    .cloned();
            }
            if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn short_port(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|arg| arg.strip_prefix("-p").filter(|value| !value.is_empty()))
        .map(str::to_owned)
}

fn valid_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    !host.is_empty()
        && host.len() <= 255
        && !host.contains(['/', '\\', '@', ' ', '\t', '\n'])
        && !host.starts_with('-')
}

pub(crate) fn format_endpoint(host: &str, port: u16) -> String {
    let host = host.trim_matches(['[', ']']);
    if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .take(MAX_ARGS)
        .map(|part| String::from_utf8_lossy(&part[..part.len().min(MAX_TOKEN_BYTES)]).into_owned())
        .collect()
}

/// 对命令行做保守脱敏，可供调用方在需要有限展示时使用。发现 provider
/// 默认只存 executable 和固定匹配规则，因此不会把原始参数写入快照。
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(args.len().min(MAX_ARGS));
    let mut redact_next = false;
    let mut redact_authorization_tail = 0_u8;
    for arg in args.iter().take(MAX_ARGS) {
        let lower = arg.to_ascii_lowercase();
        if redact_authorization_tail > 0 {
            redacted.push("<redacted>".to_owned());
            redact_authorization_tail -= 1;
            continue;
        }
        let sensitive_flag = is_sensitive_flag(&lower);
        if redact_next {
            redacted.push("<redacted>".to_owned());
            redact_next = false;
            continue;
        }
        if sensitive_flag && !arg.contains('=') {
            redacted.push(cap_token(arg));
            redact_next = true;
        } else if sensitive_flag {
            let key = arg.split('=').next().unwrap_or(arg);
            redacted.push(format!("{key}=<redacted>"));
        } else if lower == "authorization:" {
            redacted.push("Authorization: <redacted>".to_owned());
            redact_authorization_tail = 2;
        } else if lower.starts_with("authorization:") {
            redacted.push("Authorization: <redacted>".to_owned());
        } else {
            redacted.push(cap_token(arg));
        }
    }
    redacted
}

fn is_sensitive_flag(value: &str) -> bool {
    let key = value.split('=').next().unwrap_or(value);
    matches!(
        key,
        "--api-key"
            | "--apikey"
            | "--token"
            | "--password"
            | "--passwd"
            | "--secret"
            | "--authorization"
            | "-h"
    ) || key.ends_with("-token")
        || key.ends_with("_token")
        || key.ends_with("-password")
        || key.ends_with("_password")
}

fn cap_token(value: &str) -> String {
    value.chars().take(MAX_TOKEN_BYTES).collect()
}

fn has_model_argument(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--model" | "-m"))
}

fn has_gguf_model(args: &[String]) -> bool {
    args.windows(2).any(|window| {
        matches!(window[0].as_str(), "--model" | "-m")
            && window[1].to_ascii_lowercase().ends_with(".gguf")
    })
}

fn has_subcommand(args: &[String], subcommand: &str) -> bool {
    args.windows(2)
        .any(|window| window[0] == "vllm" && window[1] == subcommand)
}

fn has_python_module(args: &[String], module_prefix: &str) -> bool {
    args.windows(2).any(|window| {
        window[0] == "-m"
            && (window[1] == module_prefix || window[1].starts_with(&format!("{module_prefix}.")))
    })
}

fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

fn read_limited(path: &Path, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut reader = file.take(limit.saturating_add(1) as u64);
    reader.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, truncated))
}

fn classify_io_error(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "权限拒绝",
        io::ErrorKind::NotFound => "进程已退出",
        _ => "读取失败",
    }
}

fn issue(
    code: &'static str,
    status: HealthStatus,
    message: impl Into<String>,
) -> crate::domain::CollectionIssue {
    crate::domain::CollectionIssue {
        collector: "service_process".to_owned(),
        code: code.to_owned(),
        status,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde::Deserialize;

    use super::{extract_endpoint, redact_args, HostProcessProvider, ProcessLimits};
    use crate::domain::{DiscoverySourceKind, EndpointKind, EngineKind};
    use crate::engines::discovery::DiscoveryProvider;

    fn fixture_root() -> PathBuf {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let fixture_id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "suanctl-process-fixture-{}-{fixture_id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("fixture root");
        root
    }

    fn write_cmdline(root: &Path, pid: u32, args: &[&str]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).expect("pid dir");
        fs::write(dir.join("cmdline"), args.join("\0") + "\0").expect("cmdline");
    }

    #[derive(Debug, Deserialize)]
    struct FixtureProcess {
        pid: u32,
        args: Vec<String>,
    }

    #[test]
    fn discovers_three_engines_and_explicit_endpoint_kinds() {
        let root = fixture_root();
        let fixtures: Vec<FixtureProcess> =
            serde_json::from_str(include_str!("fixtures/host_cmdlines.json"))
                .expect("host process fixture");
        for fixture in fixtures {
            let args: Vec<&str> = fixture.args.iter().map(String::as_str).collect();
            write_cmdline(&root, fixture.pid, &args);
        }
        let result = HostProcessProvider::new(&root).discover();
        assert_eq!(result.services.len(), 3);
        assert_eq!(result.services[0].engine, EngineKind::LlamaCpp);
        assert_eq!(
            result.services[0].discovery.sources[0].kind,
            DiscoverySourceKind::HostProcess
        );
        assert_eq!(
            result.services[0].discovery.endpoints[0].kind,
            EndpointKind::Observed
        );
        assert_eq!(result.services[1].engine, EngineKind::Vllm);
        assert_eq!(result.services[2].engine, EngineKind::Sglang);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_ordinary_text_and_does_not_infer_default_port() {
        let root = fixture_root();
        write_cmdline(
            &root,
            201,
            &["python", "worker.py", "--llama", "--port", "8080"],
        );
        write_cmdline(&root, 202, &["server", "--model", "model.bin"]);
        let result = HostProcessProvider::new(&root).discover();
        assert!(result.services.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_port_without_host_is_inferred_and_ipv6_is_bracketed() {
        let inferred = extract_endpoint(&[
            "llama-server".to_owned(),
            "--port".to_owned(),
            "9000".to_owned(),
        ])
        .expect("inferred endpoint");
        assert_eq!(inferred.kind, EndpointKind::Inferred);
        assert_eq!(inferred.url, "http://127.0.0.1:9000");
        let ipv6 = extract_endpoint(&[
            "llama-server".to_owned(),
            "--host".to_owned(),
            "::1".to_owned(),
            "--port".to_owned(),
            "9001".to_owned(),
        ])
        .expect("ipv6 endpoint");
        assert_eq!(ipv6.url, "http://[::1]:9001");
    }

    #[test]
    fn redaction_removes_secrets_from_finite_evidence() {
        let safe = redact_args(&[
            "vllm".to_owned(),
            "serve".to_owned(),
            "--api-key".to_owned(),
            "secret-token".to_owned(),
            "--token=another-secret".to_owned(),
            "Authorization:".to_owned(),
            "Bearer".to_owned(),
            "abc".to_owned(),
        ]);
        let joined = safe.join(" ");
        assert!(!joined.contains("secret-token"));
        assert!(!joined.contains("another-secret"));
        assert!(!joined.contains("Bearer"));
        assert!(!joined.contains("abc"));
        assert!(joined.contains("<redacted>"));
    }

    #[test]
    fn process_limits_bound_pid_and_cmdline_reading() {
        let root = fixture_root();
        write_cmdline(
            &root,
            301,
            &["llama-server", "--model", "model.gguf", "--port", "8000"],
        );
        write_cmdline(
            &root,
            302,
            &["llama-server", "--model", "model.gguf", "--port", "8001"],
        );
        let result = HostProcessProvider::new(&root)
            .with_limits(ProcessLimits {
                max_pids: 1,
                max_cmdline_bytes: 8,
                max_total_cmdline_bytes: 8,
            })
            .discover();
        assert!(result.services.len() <= 1);
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "pid_limit" || issue.code == "cmdline_truncated"));
        let _ = fs::remove_dir_all(root);
    }
}
