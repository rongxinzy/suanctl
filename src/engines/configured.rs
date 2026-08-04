//! 调用方显式提供的服务端点。
//!
//! 该 provider 是纯函数式校验：不读取环境变量，不读取配置文件，也不自动
//! 携带任何 secret。configured endpoint 只表示用户明确声明了目标。

use reqwest::Url;

use crate::domain::{
    DiscoveryConfidence, DiscoveryProvenance, DiscoverySourceKind, EndpointKind, EngineKind,
    HealthStatus, ProbeResult, ServiceSnapshot,
};

use super::discovery::{endpoint, source, DiscoveryProvider, DiscoveryResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredEndpoint {
    pub name: String,
    pub engine: EngineKind,
    pub endpoint: String,
    pub model: Option<String>,
}

impl ConfiguredEndpoint {
    pub fn new(name: impl Into<String>, engine: EngineKind, endpoint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            engine,
            endpoint: endpoint.into(),
            model: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredProvider {
    pub targets: Vec<ConfiguredEndpoint>,
}

impl ConfiguredProvider {
    pub fn new(targets: Vec<ConfiguredEndpoint>) -> Self {
        Self { targets }
    }
}

impl DiscoveryProvider for ConfiguredProvider {
    fn discover(&self) -> DiscoveryResult {
        validate_configured(&self.targets)
    }
}

pub fn validate_configured(targets: &[ConfiguredEndpoint]) -> DiscoveryResult {
    let mut result = DiscoveryResult::default();
    for target in targets {
        let Ok(url) = validate_url(&target.endpoint) else {
            result.issues.push(issue(
                "invalid_endpoint",
                format!("配置端点被拒绝：{}", invalid_reason(&target.endpoint)),
            ));
            continue;
        };
        let port = url.port().or_else(|| url.port_or_known_default());
        let provenance = DiscoveryProvenance {
            sources: vec![source(
                DiscoverySourceKind::Configured,
                "调用方显式配置端点",
                ["configured endpoint"],
                DiscoveryConfidence::High,
            )],
            endpoints: vec![endpoint(
                &target.endpoint,
                EndpointKind::Configured,
                "调用方显式配置",
            )],
        };
        result.services.push(ServiceSnapshot {
            name: target.name.clone(),
            engine: target.engine,
            model: target.model.clone(),
            pid: None,
            port,
            status: HealthStatus::Unknown,
            process_present: None,
            endpoint: Some(target.endpoint.clone()),
            endpoint_reachable: None,
            health_probe: ProbeResult::not_attempted(),
            models_probe: ProbeResult::not_attempted(),
            metrics_probe: ProbeResult::not_attempted(),
            observed_models: Vec::new(),
            observed_metrics: Vec::new(),
            gpu_indices: Vec::new(),
            last_error: None,
            discovery: provenance,
        });
    }
    result
}

fn validate_url(value: &str) -> Result<Url, ()> {
    if value.len() > 2048 {
        return Err(());
    }
    let url = Url::parse(value).map_err(|_| ())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(());
    }
    Ok(url)
}

fn invalid_reason(value: &str) -> &'static str {
    if value.contains('@') {
        "不允许 userinfo"
    } else if value.starts_with("file:") || value.starts_with("unix:") {
        "只允许 HTTP(S)"
    } else {
        "只允许无凭据的 HTTP(S) URL"
    }
}

fn issue(code: &'static str, message: impl Into<String>) -> crate::domain::CollectionIssue {
    crate::domain::CollectionIssue {
        collector: "service_configured".to_owned(),
        code: code.to_owned(),
        status: HealthStatus::Warning,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{DiscoverySourceKind, EndpointKind, EngineKind};
    use crate::engines::discovery::DiscoveryProvider;

    use super::{validate_configured, ConfiguredEndpoint, ConfiguredProvider};

    #[test]
    fn accepts_explicit_http_and_https_without_reading_environment() {
        let mut target = ConfiguredEndpoint::new(
            "vllm-configured",
            EngineKind::Vllm,
            "https://127.0.0.1:8443/v1",
        );
        target.model = Some("model-id".to_owned());
        let result = ConfiguredProvider::new(vec![target]).discover();
        assert_eq!(result.services.len(), 1);
        assert_eq!(result.services[0].engine, EngineKind::Vllm);
        assert_eq!(
            result.services[0].discovery.sources[0].kind,
            DiscoverySourceKind::Configured
        );
        assert_eq!(
            result.services[0].discovery.endpoints[0].kind,
            EndpointKind::Configured
        );
    }

    #[test]
    fn rejects_userinfo_non_http_query_and_fragment() {
        let targets = [
            ConfiguredEndpoint::new(
                "secret",
                EngineKind::Vllm,
                "http://user:secret@127.0.0.1:8000",
            ),
            ConfiguredEndpoint::new("file", EngineKind::Vllm, "file:///tmp/service"),
            ConfiguredEndpoint::new(
                "query",
                EngineKind::Vllm,
                "http://127.0.0.1:8000?token=secret",
            ),
            ConfiguredEndpoint::new("fragment", EngineKind::Vllm, "http://127.0.0.1:8000#secret"),
        ];
        let result = validate_configured(&targets);
        assert!(result.services.is_empty());
        assert_eq!(result.issues.len(), targets.len());
        let serialized = serde_json::to_string(&result).expect("safe issue JSON");
        assert!(!serialized.contains("secret"));
    }
}
