//! Docker、Podman 与 nerdctl 的只读 `ps` 发现。
//!
//! 这里不连接 runtime socket，不调用 inspect，不 exec，也不启动/停止容器。
//! 所有 runtime 都通过固定参数的外部命令，并复用受限的 CommandRunner。

use std::time::Duration;

use serde_json::Value;

use crate::collectors::command::{CommandRequest, CommandRunner, ProcessCommandRunner};
use crate::domain::{
    ContainerMetadata, ContainerRuntime, DiscoveryProvenance, DiscoverySourceKind, EndpointKind,
    HealthStatus, ProbeResult, PublishedPort, ServiceSnapshot,
};

use super::discovery::{endpoint, source, DiscoveryProvider, DiscoveryResult};
use super::process::{detect_engine, format_endpoint, EngineMatch};

pub type ContainerRuntimeKind = ContainerRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntimeCommand {
    Docker,
    Podman,
    Nerdctl,
    /// 保留 CRI provider 边界；本轮不执行 crictl。
    Crictl,
}

impl ContainerRuntimeCommand {
    const fn executable(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Nerdctl => "nerdctl",
            Self::Crictl => "crictl",
        }
    }

    const fn domain_runtime(self) -> Option<ContainerRuntime> {
        match self {
            Self::Docker => Some(ContainerRuntime::Docker),
            Self::Podman => Some(ContainerRuntime::Podman),
            Self::Nerdctl => Some(ContainerRuntime::Nerdctl),
            Self::Crictl => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContainerProvider<R> {
    pub runner: R,
    pub runtimes: Vec<ContainerRuntimeCommand>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

impl ContainerProvider<ProcessCommandRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessCommandRunner,
            runtimes: vec![
                ContainerRuntimeCommand::Docker,
                ContainerRuntimeCommand::Podman,
                ContainerRuntimeCommand::Nerdctl,
            ],
            timeout: Duration::from_secs(3),
            stdout_limit: 512 * 1024,
            stderr_limit: 64 * 1024,
        }
    }
}

impl Default for ContainerProvider<ProcessCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> ContainerProvider<R> {
    pub fn with_runner(runner: R) -> Self {
        Self {
            runner,
            runtimes: vec![
                ContainerRuntimeCommand::Docker,
                ContainerRuntimeCommand::Podman,
                ContainerRuntimeCommand::Nerdctl,
            ],
            timeout: Duration::from_secs(3),
            stdout_limit: 512 * 1024,
            stderr_limit: 64 * 1024,
        }
    }
}

impl<R: CommandRunner> DiscoveryProvider for ContainerProvider<R> {
    fn discover(&self) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();
        for runtime in &self.runtimes {
            if runtime.domain_runtime().is_none() {
                continue;
            }
            let runtime_result = self.discover_runtime(*runtime);
            result.services.extend(runtime_result.services);
            result.issues.extend(runtime_result.issues);
        }
        result
    }
}

impl<R: CommandRunner> ContainerProvider<R> {
    fn discover_runtime(&self, runtime: ContainerRuntimeCommand) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();
        let request = command_request(runtime, self.timeout, self.stdout_limit, self.stderr_limit);
        let output = match self.runner.run(&request) {
            Ok(output) => output,
            Err(error) if error.code == "spawn_failed" => {
                result.issues.push(issue(
                    "runtime_missing",
                    HealthStatus::Unavailable,
                    format!("{} 命令不可用，未安装或不在 PATH 中", runtime.executable()),
                ));
                return result;
            }
            Err(error) => {
                result.issues.push(issue(
                    "runtime_command_failed",
                    HealthStatus::Unavailable,
                    format!("{} 只读命令执行失败：{}", runtime.executable(), error.code),
                ));
                return result;
            }
        };

        if output.timed_out {
            result.issues.push(issue(
                "runtime_timeout",
                HealthStatus::Unavailable,
                format!("{} ps 超过 3 秒超时", runtime.executable()),
            ));
            return result;
        }
        if output.stdout_truncated || output.stderr_truncated {
            result.issues.push(issue(
                "runtime_output_truncated",
                HealthStatus::Warning,
                format!("{} ps 输出超过安全上限", runtime.executable()),
            ));
        }
        if !output.success {
            result.issues.push(issue(
                "runtime_daemon_unavailable",
                HealthStatus::Unavailable,
                format!("{} daemon 不可达或权限不足", runtime.executable()),
            ));
            return result;
        }

        let rows = match parse_runtime_output(runtime, &output.stdout) {
            Ok(rows) => rows,
            Err(message) => {
                result.issues.push(issue(
                    "runtime_json_invalid",
                    HealthStatus::Warning,
                    format!("{} ps JSON 无法解析：{message}", runtime.executable()),
                ));
                return result;
            }
        };
        for row in rows {
            if let Some(service) = service_from_container(runtime, row) {
                result.services.push(service);
            }
        }
        result
    }
}

fn command_request(
    runtime: ContainerRuntimeCommand,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> CommandRequest {
    let args: Vec<String> = match runtime {
        ContainerRuntimeCommand::Docker | ContainerRuntimeCommand::Nerdctl => vec![
            "ps".to_owned(),
            "--no-trunc".to_owned(),
            "--format".to_owned(),
            "{{json .}}".to_owned(),
        ],
        ContainerRuntimeCommand::Podman => vec![
            "ps".to_owned(),
            "--no-trunc".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ],
        ContainerRuntimeCommand::Crictl => Vec::new(),
    };
    CommandRequest {
        program: runtime.executable().to_owned(),
        args,
        timeout,
        stdout_limit,
        stderr_limit,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerRow {
    id: Option<String>,
    name: Option<String>,
    image: Option<String>,
    command: String,
    ports: Vec<PublishedPort>,
}

pub fn parse_docker_lines(output: &str) -> Result<Vec<ContainerMetadata>, String> {
    parse_json_rows(output, ContainerRuntime::Docker).map(|rows| {
        rows.into_iter()
            .map(|row| metadata_from_row(ContainerRuntime::Docker, row))
            .collect()
    })
}

pub fn parse_podman_json(output: &str) -> Result<Vec<ContainerMetadata>, String> {
    parse_json_rows(output, ContainerRuntime::Podman).map(|rows| {
        rows.into_iter()
            .map(|row| metadata_from_row(ContainerRuntime::Podman, row))
            .collect()
    })
}

pub fn parse_nerdctl_lines(output: &str) -> Result<Vec<ContainerMetadata>, String> {
    parse_json_rows(output, ContainerRuntime::Nerdctl).map(|rows| {
        rows.into_iter()
            .map(|row| metadata_from_row(ContainerRuntime::Nerdctl, row))
            .collect()
    })
}

fn parse_runtime_output(
    runtime: ContainerRuntimeCommand,
    output: &str,
) -> Result<Vec<ContainerRow>, String> {
    parse_json_rows(
        output,
        runtime.domain_runtime().ok_or("CRI provider 尚未执行")?,
    )
}

fn parse_json_rows(output: &str, runtime: ContainerRuntime) -> Result<Vec<ContainerRow>, String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        match value {
            Value::Array(items) => values.extend(items),
            Value::Object(_) => values.push(value),
            _ => return Err("顶层 JSON 不是对象或数组".to_owned()),
        }
    } else {
        for line in trimmed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let value = serde_json::from_str::<Value>(line)
                .map_err(|_| format!("{} 行不是 JSON 对象", runtime_label(runtime)))?;
            if !value.is_object() {
                return Err("行式 JSON 必须是对象".to_owned());
            }
            values.push(value);
        }
    }

    values
        .into_iter()
        .map(parse_row)
        .collect::<Result<Vec<_>, _>>()
}

fn parse_row(value: Value) -> Result<ContainerRow, String> {
    let object = value.as_object().ok_or("容器条目不是 JSON 对象")?;
    let id = string_field(object, &["ID", "Id", "id"]);
    let name = string_field(object, &["Names", "Name", "names", "name"])
        .map(|value| value.trim_start_matches('/').to_owned());
    let image = string_field(object, &["Image", "image"]);
    let command = string_field(object, &["Command", "command"]).unwrap_or_default();
    let ports = parse_ports(object.get("Ports").or_else(|| object.get("ports")));
    Ok(ContainerRow {
        id: id.map(|value| truncate(&value, 12)),
        name: name.map(|value| truncate(&value, 128)),
        image: image.map(|value| truncate(&value, 256)),
        command: truncate(&command, 4096),
        ports,
    })
}

fn metadata_from_row(runtime: ContainerRuntime, row: ContainerRow) -> ContainerMetadata {
    ContainerMetadata {
        runtime,
        container_id: row.id,
        name: row.name,
        image: row.image,
        published_ports: row.ports,
    }
}

fn service_from_container(
    runtime: ContainerRuntimeCommand,
    row: ContainerRow,
) -> Option<ServiceSnapshot> {
    let image = row.image.as_deref();
    let name = row.name.as_deref();
    let command_tokens = shell_tokens(&row.command);
    let detected = detect_container_engine(&command_tokens, image, name)?;
    let metadata = metadata_from_row(runtime.domain_runtime()?, row);
    let endpoint_data = metadata
        .published_ports
        .iter()
        .filter_map(|port| {
            port.host_port.map(|host_port| {
                let observed_host = port
                    .host_ip
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("127.0.0.1");
                let host = connectable_host(observed_host);
                (
                    format_endpoint(host, host_port),
                    host_port,
                    if matches!(observed_host, "0.0.0.0" | "::" | "[::]") {
                        "容器 published port 绑定通配地址，探针端点按 loopback 推导"
                    } else if port.host_ip.is_some() {
                        "容器 published port"
                    } else {
                        "容器 published port，主机地址未提供，按 loopback 展示"
                    },
                )
            })
        })
        .next();
    let mut provenance = DiscoveryProvenance {
        sources: vec![{
            let mut value = source(
                DiscoverySourceKind::Container,
                detected.reason,
                [format!(
                    "runtime {}",
                    runtime_label(runtime.domain_runtime()?)
                )],
                detected.confidence,
            );
            value.container = Some(metadata.clone());
            value
        }],
        endpoints: Vec::new(),
    };
    if let Some((endpoint_value, _, derivation)) = &endpoint_data {
        provenance.endpoints.push(endpoint(
            endpoint_value,
            EndpointKind::Published,
            *derivation,
        ));
    }
    let name = metadata
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-container", detected.engine.label()));
    Some(ServiceSnapshot {
        name,
        engine: detected.engine,
        model: None,
        pid: None,
        port: endpoint_data.as_ref().map(|(_, port, _)| *port),
        status: HealthStatus::Unknown,
        process_present: Some(true),
        endpoint: endpoint_data.map(|(value, _, _)| value),
        endpoint_reachable: None,
        health_probe: ProbeResult::not_attempted(),
        models_probe: ProbeResult::not_attempted(),
        metrics_probe: ProbeResult::not_attempted(),
        observed_models: Vec::new(),
        observed_metrics: Vec::new(),
        gpu_indices: Vec::new(),
        last_error: None,
        discovery: provenance,
    })
}

fn connectable_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        value => value,
    }
}

fn detect_container_engine(
    command_tokens: &[String],
    image: Option<&str>,
    name: Option<&str>,
) -> Option<EngineMatch> {
    if let Some(detected) = detect_engine(command_tokens, image, name) {
        return Some(detected);
    }
    // `ps` 的 Command 可能以 `/bin/sh -c` 包装；只对每个后缀重新应用
    // 精确规则，不对任意文本做 substring 识别。
    for index in 1..command_tokens.len() {
        if let Some(detected) = detect_engine(&command_tokens[index..], image, name) {
            return Some(detected);
        }
    }
    let context = format!(
        "{} {}",
        image.unwrap_or_default().to_ascii_lowercase(),
        name.unwrap_or_default().to_ascii_lowercase()
    );
    if has_marker(&context, &["llama.cpp", "llama-server", "llama_server"]) {
        return Some(EngineMatch {
            engine: crate::domain::EngineKind::LlamaCpp,
            confidence: crate::domain::DiscoveryConfidence::Medium,
            reason: "llama.cpp: 容器 image/name 标识匹配",
        });
    }
    if has_marker(&context, &["/vllm", ":vllm", "-vllm", "vllm/"]) {
        return Some(EngineMatch {
            engine: crate::domain::EngineKind::Vllm,
            confidence: crate::domain::DiscoveryConfidence::Medium,
            reason: "vLLM: 容器 image/name 标识匹配",
        });
    }
    if has_marker(&context, &["/sglang", ":sglang", "-sglang", "sglang/"]) {
        return Some(EngineMatch {
            engine: crate::domain::EngineKind::Sglang,
            confidence: crate::domain::DiscoveryConfidence::Medium,
            reason: "SGLang: 容器 image/name 标识匹配",
        });
    }
    None
}

fn shell_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .take(128)
        .map(|value| value.trim_matches(['\'', '"']).to_owned())
        .collect()
}

fn has_marker(context: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| context.contains(marker))
}

fn string_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match object.get(*name) {
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    })
}

fn parse_ports(value: Option<&Value>) -> Vec<PublishedPort> {
    match value {
        Some(Value::String(value)) => value.split(',').filter_map(parse_port_string).collect(),
        Some(Value::Array(values)) => values
            .iter()
            .flat_map(|value| match value {
                Value::String(value) => parse_port_string(value).into_iter(),
                _ => parse_port_object(value).into_iter(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_port_string(value: &str) -> Option<PublishedPort> {
    let (host_ip, host_port, right) = match value.trim().split_once("->") {
        Some((left, right)) => {
            let (host_ip, host_port) = parse_host_binding(left.trim());
            (host_ip, host_port, right)
        }
        None => (None, None, value.trim()),
    };
    let (container_port, protocol) = parse_port_protocol(right)?;
    Some(PublishedPort {
        host_ip,
        host_port,
        container_port,
        protocol,
    })
}

fn parse_port_object(value: &Value) -> Option<PublishedPort> {
    let object = value.as_object()?;
    let container_port = number_field(object, &["PrivatePort", "private_port", "ContainerPort"])?;
    let host_port = number_field(object, &["PublicPort", "public_port", "HostPort"]);
    let host_ip =
        string_field(object, &["IP", "Ip", "HostIp", "host_ip"]).filter(|value| !value.is_empty());
    let protocol = string_field(object, &["Type", "type", "Protocol", "protocol"]);
    Some(PublishedPort {
        host_ip,
        host_port,
        container_port,
        protocol,
    })
}

fn parse_port_protocol(value: &str) -> Option<(u16, Option<String>)> {
    let (port, protocol) = value
        .split_once('/')
        .map_or((value, None), |(p, proto)| (p, Some(proto.to_owned())));
    Some((port.trim().parse().ok()?, protocol))
}

fn parse_host_binding(value: &str) -> (Option<String>, Option<u16>) {
    let value = value.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Some(end) = value.rfind("]:") {
        if value.starts_with('[') {
            let host = value[1..end].to_owned();
            return (Some(host), value[end + 2..].parse().ok());
        }
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        return (
            if host.is_empty() {
                None
            } else {
                Some(host.trim_matches(['[', ']']).to_owned())
            },
            port.parse().ok(),
        );
    }
    (None, value.parse().ok())
}

fn number_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<u16> {
    names.iter().find_map(|name| match object.get(*name) {
        Some(Value::Number(value)) => value.as_u64().and_then(|value| u16::try_from(value).ok()),
        Some(Value::String(value)) => value.parse().ok(),
        _ => None,
    })
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn runtime_label(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::Podman => "podman",
        ContainerRuntime::Nerdctl => "nerdctl",
        ContainerRuntime::Cri => "cri",
    }
}

fn issue(
    code: &'static str,
    status: HealthStatus,
    message: impl Into<String>,
) -> crate::domain::CollectionIssue {
    crate::domain::CollectionIssue {
        collector: "service_container".to_owned(),
        code: code.to_owned(),
        status,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::collectors::command::{CommandOutput, CommandRequest, CommandRunner};
    use crate::domain::{ContainerRuntime, DiscoverySourceKind, EngineKind};
    use crate::engines::discovery::DiscoveryProvider;

    use super::{
        connectable_host, parse_docker_lines, parse_nerdctl_lines, parse_podman_json,
        ContainerProvider, ContainerRuntimeCommand,
    };

    #[test]
    fn wildcard_published_addresses_use_connectable_loopback_for_probes() {
        assert_eq!(connectable_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connectable_host("::"), "::1");
        assert_eq!(connectable_host("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn parses_docker_json_lines_with_ipv4_and_no_published_port() {
        let rows =
            parse_docker_lines(include_str!("fixtures/docker_ps.jsonl")).expect("docker rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].runtime, ContainerRuntime::Docker);
        assert_eq!(rows[0].container_id.as_deref(), Some("abcdef012345"));
        assert_eq!(
            rows[0].published_ports[0].host_ip.as_deref(),
            Some("127.0.0.1")
        );
        assert!(rows[1].published_ports.is_empty());
    }

    #[test]
    fn parses_podman_array_and_ipv6_published_port() {
        let rows = parse_podman_json(include_str!("fixtures/podman_ps.json")).expect("podman rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runtime, ContainerRuntime::Podman);
        assert_eq!(rows[0].published_ports[0].host_ip.as_deref(), Some("::1"));
        assert_eq!(rows[0].published_ports[0].host_port, Some(3000));
    }

    #[test]
    fn parses_nerdctl_and_discovers_container_without_endpoint() {
        let rows =
            parse_nerdctl_lines(include_str!("fixtures/nerdctl_ps.jsonl")).expect("nerdctl rows");
        assert_eq!(rows[0].runtime, ContainerRuntime::Nerdctl);
        assert!(rows[0].published_ports[0].host_port.is_none());
    }

    #[derive(Clone)]
    struct FakeRunner {
        output: CommandOutput,
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _request: &CommandRequest,
        ) -> Result<CommandOutput, crate::collectors::CollectorError> {
            Ok(self.output.clone())
        }
    }

    struct MissingRunner;

    impl CommandRunner for MissingRunner {
        fn run(
            &self,
            _request: &CommandRequest,
        ) -> Result<CommandOutput, crate::collectors::CollectorError> {
            Err(crate::collectors::CollectorError::new(
                "command",
                "spawn_failed",
                "runtime missing",
            ))
        }
    }

    #[test]
    fn provider_uses_fixed_read_only_runtime_commands_and_keeps_secret_out() {
        let runner = FakeRunner {
            output: CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: include_str!("fixtures/docker_ps.jsonl").to_owned(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            },
        };
        let mut provider = ContainerProvider::with_runner(runner);
        provider.runtimes = vec![ContainerRuntimeCommand::Docker];
        provider.timeout = Duration::from_secs(3);
        let result = provider.discover();
        assert_eq!(result.services.len(), 2);
        assert_eq!(result.services[1].engine, EngineKind::Vllm);
        assert_eq!(
            result.services[1].discovery.sources[0].kind,
            DiscoverySourceKind::Container
        );
        assert!(result.services[1].endpoint.is_none());
        let serialized = serde_json::to_string(&result).expect("safe discovery JSON");
        assert!(!serialized.contains("secret-token"));
        assert!(!serialized.contains("api-key"));
    }

    #[test]
    fn missing_runtime_is_capability_issue_not_global_failure() {
        let runner = FakeRunner {
            output: CommandOutput {
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            },
        };
        let mut provider = ContainerProvider::with_runner(runner);
        provider.runtimes = vec![ContainerRuntimeCommand::Docker];
        let result = provider.discover();
        assert!(result.services.is_empty());
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "runtime_daemon_unavailable"));
    }

    #[test]
    fn missing_runtime_command_is_capability_unavailable() {
        let mut provider = ContainerProvider::with_runner(MissingRunner);
        provider.runtimes = vec![ContainerRuntimeCommand::Nerdctl];
        let result = provider.discover();
        assert!(result.services.is_empty());
        assert!(result
            .issues
            .iter()
            .any(|issue| issue.code == "runtime_missing"));
    }
}
