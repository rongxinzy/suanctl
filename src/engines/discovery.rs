//! 推理服务发现的公共领域边界。
//!
//! 发现阶段只收集低敏感元数据和端点来源，不执行 HTTP 请求，也不把“发现”
//! 当成“服务健康”。各 provider 可以独立测试，最后由本模块按明确身份规则
//! 合并候选。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::domain::{
    CollectionIssue, DiscoveryConfidence, DiscoverySource, DiscoverySourceKind, EndpointKind,
    EndpointProvenance, HealthStatus, ServiceSnapshot,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryResult {
    #[serde(default)]
    pub services: Vec<ServiceSnapshot>,
    #[serde(default)]
    pub issues: Vec<CollectionIssue>,
}

pub trait DiscoveryProvider {
    fn discover(&self) -> DiscoveryResult;
}

pub fn source(
    kind: DiscoverySourceKind,
    match_reason: impl Into<String>,
    evidence: impl IntoIterator<Item = impl Into<String>>,
    confidence: DiscoveryConfidence,
) -> DiscoverySource {
    DiscoverySource {
        kind,
        match_reason: match_reason.into(),
        evidence: evidence.into_iter().map(Into::into).collect(),
        confidence,
        pid: None,
        container: None,
    }
}

pub fn endpoint(
    value: impl Into<String>,
    kind: EndpointKind,
    derivation: impl Into<String>,
) -> EndpointProvenance {
    EndpointProvenance {
        endpoint: value.into(),
        kind,
        derivation: derivation.into(),
    }
}

/// 合并多个发现来源。身份规则故意保守：
///
/// * 同一引擎的完全相同端点可以合并；
/// * 同一 host PID 或同一 runtime/container id 可以合并；
/// * 仅凭 name、model 或“看起来像同一个进程”不得合并；
/// * host PID 与 container 候选没有上述稳定证据时保持两个候选。
pub fn merge_results<I>(results: I) -> DiscoveryResult
where
    I: IntoIterator<Item = DiscoveryResult>,
{
    let mut merged = DiscoveryResult::default();
    for result in results {
        merged.issues.extend(result.issues);
        for service in result.services {
            if let Some(index) = merged
                .services
                .iter()
                .position(|existing| can_merge(existing, &service))
            {
                merge_service(&mut merged.services[index], service);
            } else {
                merged.services.push(service);
            }
        }
    }
    merged
}

fn can_merge(left: &ServiceSnapshot, right: &ServiceSnapshot) -> bool {
    if left.engine != right.engine {
        return false;
    }

    if let (Some(left_endpoint), Some(right_endpoint)) = (&left.endpoint, &right.endpoint) {
        return normalize_endpoint(left_endpoint) == normalize_endpoint(right_endpoint);
    }

    if let (Some(left_pid), Some(right_pid)) = (left.pid, right.pid) {
        if left_pid == right_pid
            && has_source(left, DiscoverySourceKind::HostProcess)
            && has_source(right, DiscoverySourceKind::HostProcess)
        {
            return true;
        }
    }

    let left_containers = container_identities(left);
    let right_containers = container_identities(right);
    !left_containers.is_empty()
        && left_containers
            .iter()
            .any(|identity| right_containers.contains(identity))
}

fn merge_service(base: &mut ServiceSnapshot, candidate: ServiceSnapshot) {
    let candidate_endpoint = candidate.endpoint.clone();
    let candidate_kind = candidate
        .discovery
        .endpoints
        .iter()
        .filter(|item| Some(&item.endpoint) == candidate_endpoint.as_ref())
        .map(|item| item.kind)
        .max_by_key(|kind| endpoint_rank(*kind));

    if let (Some(existing), Some(incoming)) = (&base.endpoint, &candidate_endpoint) {
        let existing_kind = base
            .discovery
            .endpoints
            .iter()
            .filter(|item| &item.endpoint == existing)
            .map(|item| item.kind)
            .max_by_key(|kind| endpoint_rank(*kind))
            .unwrap_or(EndpointKind::Inferred);
        if candidate_kind.is_some_and(|kind| endpoint_rank(kind) > endpoint_rank(existing_kind)) {
            base.endpoint = Some(incoming.clone());
            base.port = candidate.port;
            base.endpoint_reachable = candidate.endpoint_reachable;
        }
    } else if base.endpoint.is_none() && candidate_endpoint.is_some() {
        base.endpoint = candidate_endpoint;
        base.port = candidate.port;
        base.endpoint_reachable = candidate.endpoint_reachable;
    }

    if base.pid.is_none() {
        base.pid = candidate.pid;
    }
    if base.process_present.is_none() {
        base.process_present = candidate.process_present;
    }
    if base.model.is_none() {
        base.model = candidate.model;
    }
    if base.name.is_empty() && !candidate.name.is_empty() {
        base.name = candidate.name;
    }
    base.status = worse_status(base.status, candidate.status);
    if base.last_error.is_none() {
        base.last_error = candidate.last_error;
    }

    for source in candidate.discovery.sources {
        if !base.discovery.sources.contains(&source) {
            base.discovery.sources.push(source);
        }
    }
    for observed_endpoint in candidate.discovery.endpoints {
        if !base.discovery.endpoints.contains(&observed_endpoint) {
            base.discovery.endpoints.push(observed_endpoint);
        }
    }
}

fn has_source(service: &ServiceSnapshot, kind: DiscoverySourceKind) -> bool {
    service
        .discovery
        .sources
        .iter()
        .any(|source| source.kind == kind)
}

fn container_identities(
    service: &ServiceSnapshot,
) -> HashSet<(crate::domain::ContainerRuntime, String)> {
    service
        .discovery
        .sources
        .iter()
        .filter_map(|source| {
            source.container.as_ref().and_then(|container| {
                container
                    .container_id
                    .as_ref()
                    .map(|id| (container.runtime, id.trim().to_owned()))
            })
        })
        .filter(|(_, id)| !id.is_empty())
        .collect()
}

fn normalize_endpoint(endpoint: &str) -> String {
    endpoint.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn endpoint_rank(kind: EndpointKind) -> u8 {
    match kind {
        EndpointKind::Configured => 4,
        EndpointKind::Observed => 3,
        EndpointKind::Published => 2,
        EndpointKind::Inferred => 1,
    }
}

fn worse_status(left: HealthStatus, right: HealthStatus) -> HealthStatus {
    fn rank(status: HealthStatus) -> u8 {
        match status {
            HealthStatus::Critical => 5,
            HealthStatus::Warning => 4,
            HealthStatus::Unavailable => 3,
            HealthStatus::Unknown => 2,
            HealthStatus::Healthy => 1,
        }
    }

    if rank(right) > rank(left) {
        right
    } else {
        left
    }
}

#[cfg(test)]
mod tests {
    use super::{endpoint, merge_results, source};
    use crate::domain::{
        DiscoveryConfidence, DiscoveryProvenance, DiscoverySourceKind, EndpointKind, EngineKind,
        HealthStatus, ProbeResult, ServiceSnapshot,
    };

    fn service(
        name: &str,
        endpoint_value: Option<&str>,
        endpoint_kind: Option<EndpointKind>,
        source_kind: DiscoverySourceKind,
    ) -> ServiceSnapshot {
        let mut provenance = DiscoveryProvenance {
            sources: vec![source(
                source_kind,
                "测试匹配规则",
                ["固定测试证据"],
                DiscoveryConfidence::High,
            )],
            endpoints: Vec::new(),
        };
        if let (Some(value), Some(kind)) = (endpoint_value, endpoint_kind) {
            provenance
                .endpoints
                .push(endpoint(value, kind, "测试端点来源"));
        }
        ServiceSnapshot {
            name: name.to_owned(),
            engine: EngineKind::Vllm,
            model: None,
            pid: None,
            port: None,
            status: HealthStatus::Unknown,
            process_present: None,
            endpoint: endpoint_value.map(str::to_owned),
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

    #[test]
    fn same_endpoint_merges_sources_and_keeps_endpoint_provenance() {
        let merged = merge_results([
            super::DiscoveryResult {
                services: vec![service(
                    "host",
                    Some("http://127.0.0.1:8000"),
                    Some(EndpointKind::Observed),
                    DiscoverySourceKind::HostProcess,
                )],
                issues: Vec::new(),
            },
            super::DiscoveryResult {
                services: vec![service(
                    "container",
                    Some("http://127.0.0.1:8000/"),
                    Some(EndpointKind::Published),
                    DiscoverySourceKind::Container,
                )],
                issues: Vec::new(),
            },
        ]);
        assert_eq!(merged.services.len(), 1);
        assert_eq!(merged.services[0].discovery.sources.len(), 2);
        assert_eq!(merged.services[0].discovery.endpoints.len(), 2);
        assert_eq!(
            merged.services[0].endpoint.as_deref(),
            Some("http://127.0.0.1:8000")
        );
    }

    #[test]
    fn host_and_container_without_stable_identity_stay_separate() {
        let merged = merge_results([
            super::DiscoveryResult {
                services: vec![service(
                    "same-name",
                    None,
                    None,
                    DiscoverySourceKind::HostProcess,
                )],
                issues: Vec::new(),
            },
            super::DiscoveryResult {
                services: vec![service(
                    "same-name",
                    None,
                    None,
                    DiscoverySourceKind::Container,
                )],
                issues: Vec::new(),
            },
        ]);
        assert_eq!(merged.services.len(), 2);
    }
}
