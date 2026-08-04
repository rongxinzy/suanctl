//! 三种推理引擎的统一只读 HTTP 适配边界。
//!
//! 适配器只接收 `GET` transport，因此类型层面没有推理、管理或容器执行
//! 能力。生产路径使用 [`HttpProbeClient`]，测试路径可以注入完全离线的
//! fake transport。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::{
    EngineKind, HealthStatus, ObservedMetric, ProbeResult, ProbeStatus, ServiceSnapshot,
};

use super::http::{
    parse_openai_model_ids, parse_prometheus_scalars, HttpProbeClient, HttpProbeResponse,
    ProbePath, ProbeTarget, ProbeTransport, PrometheusScalar, DEFAULT_MAX_RESPONSE_BYTES,
};
use super::llama_cpp::LlamaCppAdapter;
use super::sglang::SglangAdapter;
use super::vllm::VllmAdapter;

const DEFAULT_SERVICE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_OBSERVED_MODELS: usize = 32;
const MAX_OBSERVED_METRICS: usize = 32;
const MAX_ERROR_BYTES: usize = 256;

/// 一次引擎探针的结果。发现本身不会生成此对象，只有实际执行 GET 后才会
/// 更新 probe/status 字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineProbeSnapshot {
    pub engine: EngineKind,
    pub adapter: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub endpoint_reachable: Option<bool>,
    pub health_probe: ProbeResult,
    pub models_probe: ProbeResult,
    pub metrics_probe: ProbeResult,
    #[serde(default)]
    pub observed_models: Vec<String>,
    #[serde(default)]
    pub observed_metrics: Vec<ObservedMetric>,
    pub status: HealthStatus,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl EngineProbeSnapshot {
    fn not_attempted(
        engine: EngineKind,
        adapter: &'static str,
        capabilities: &'static [&'static str],
        endpoint: Option<String>,
    ) -> Self {
        Self {
            engine,
            adapter: adapter.to_owned(),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            endpoint,
            endpoint_reachable: None,
            health_probe: ProbeResult::not_attempted(),
            models_probe: ProbeResult::not_attempted(),
            metrics_probe: ProbeResult::not_attempted(),
            observed_models: Vec::new(),
            observed_metrics: Vec::new(),
            status: HealthStatus::Unknown,
            last_error: None,
        }
    }

    fn unavailable(
        engine: EngineKind,
        adapter: &'static str,
        capabilities: &'static [&'static str],
        endpoint: Option<String>,
        message: String,
    ) -> Self {
        let message = safe_error(&message);
        let result = ProbeResult {
            status: ProbeStatus::Unavailable,
            http_status: None,
            message: Some(message.clone()),
        };
        Self {
            engine,
            adapter: adapter.to_owned(),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            endpoint,
            endpoint_reachable: Some(false),
            health_probe: result.clone(),
            models_probe: result.clone(),
            metrics_probe: result,
            observed_models: Vec::new(),
            observed_metrics: Vec::new(),
            status: HealthStatus::Unavailable,
            last_error: Some(message),
        }
    }

    fn apply_to_service(&self, service: &mut ServiceSnapshot) {
        service.endpoint_reachable = self.endpoint_reachable;
        service.health_probe = self.health_probe.clone();
        service.models_probe = self.models_probe.clone();
        service.metrics_probe = self.metrics_probe.clone();
        service.observed_models = self.observed_models.clone();
        service.observed_metrics = self.observed_metrics.clone();
        service.status = self.status;
        service.last_error = self.last_error.clone();

        // configured model 优先；只有原来没有模型且 HTTP 明确返回唯一模型时
        // 才补齐 model。观测列表始终保留在 EngineProbeSnapshot 中。
        if service.model.is_none() && self.observed_models.len() == 1 {
            service.model = self.observed_models.first().cloned();
        }
    }
}

/// 每个引擎保留独立名称和 capability；核心编排共享，但路由不会按字符串猜测。
pub trait EngineAdapter: Send + Sync {
    fn engine(&self) -> EngineKind;
    fn adapter_name(&self) -> &'static str;
    fn capabilities(&self) -> &'static [&'static str];

    fn probe_with_transport(
        &self,
        target: &ProbeTarget,
        transport: &dyn ProbeTransport,
    ) -> EngineProbeSnapshot {
        run_probe(
            self.engine(),
            self.adapter_name(),
            self.capabilities(),
            target,
            transport,
        )
    }

    /// 生产入口：构造现有的 `HttpProbeClient`，不允许测试依赖真实端口。
    fn probe_target(&self, target: &ProbeTarget) -> EngineProbeSnapshot {
        match HttpProbeClient::new(target) {
            Ok(client) => self.probe_with_transport(target, &client),
            Err(message) => EngineProbeSnapshot::unavailable(
                self.engine(),
                self.adapter_name(),
                self.capabilities(),
                Some(target.base_url.clone()),
                message,
            ),
        }
    }

    /// 对发现得到的服务执行探针并更新已有服务字段。
    fn probe_service(&self, service: &mut ServiceSnapshot) -> EngineProbeSnapshot {
        let Some(endpoint) = service.endpoint.clone() else {
            let result = EngineProbeSnapshot::not_attempted(
                self.engine(),
                self.adapter_name(),
                self.capabilities(),
                None,
            );
            result.apply_to_service(service);
            return result;
        };

        let target = ProbeTarget::with_timeouts(
            endpoint,
            service.model.clone(),
            DEFAULT_SERVICE_TIMEOUT,
            DEFAULT_SERVICE_TIMEOUT,
            DEFAULT_MAX_RESPONSE_BYTES,
        );
        let result = self.probe_target(&target);
        result.apply_to_service(service);
        result
    }

    /// 测试和上层编排入口：服务端点来自发现结果，但 transport 可完全注入。
    fn probe_service_with_transport(
        &self,
        service: &mut ServiceSnapshot,
        transport: &dyn ProbeTransport,
    ) -> EngineProbeSnapshot {
        let Some(endpoint) = service.endpoint.clone() else {
            let result = EngineProbeSnapshot::not_attempted(
                self.engine(),
                self.adapter_name(),
                self.capabilities(),
                None,
            );
            result.apply_to_service(service);
            return result;
        };
        let target = ProbeTarget::with_timeouts(
            endpoint,
            service.model.clone(),
            DEFAULT_SERVICE_TIMEOUT,
            DEFAULT_SERVICE_TIMEOUT,
            DEFAULT_MAX_RESPONSE_BYTES,
        );
        let result = self.probe_with_transport(&target, transport);
        result.apply_to_service(service);
        result
    }
}

/// 严格按 `EngineKind` 选择 adapter。`Unknown` 不会执行任何请求。
pub fn adapter_for(engine: EngineKind) -> Option<Box<dyn EngineAdapter>> {
    match engine {
        EngineKind::LlamaCpp => Some(Box::new(LlamaCppAdapter)),
        EngineKind::Vllm => Some(Box::new(VllmAdapter)),
        EngineKind::Sglang => Some(Box::new(SglangAdapter)),
        EngineKind::Unknown => None,
    }
}

/// 便于上层对单个已发现服务执行严格路由的辅助函数。
pub fn probe_service_with_transport(
    service: &mut ServiceSnapshot,
    transport: &dyn ProbeTransport,
) -> Option<EngineProbeSnapshot> {
    let adapter = adapter_for(service.engine)?;
    Some(adapter.probe_service_with_transport(service, transport))
}

fn run_probe(
    engine: EngineKind,
    adapter: &'static str,
    capabilities: &'static [&'static str],
    target: &ProbeTarget,
    transport: &dyn ProbeTransport,
) -> EngineProbeSnapshot {
    let health_response = transport.get(ProbePath::Health);
    let models_response = transport.get(ProbePath::Models);
    let metrics_response = transport.get(ProbePath::Metrics);

    let (models_probe, observed_models) = parse_models_response(models_response);
    let (metrics_probe, observed_metrics) = parse_metrics_response(metrics_response);
    let health_probe = health_response.result;
    let endpoint_reachable = [
        health_probe.http_status,
        models_probe.http_status,
        metrics_probe.http_status,
    ]
    .into_iter()
    .flatten()
    .next()
    .map(|_| true)
    .or(Some(false));

    let status = summarize_status(
        &health_probe,
        &models_probe,
        &metrics_probe,
        endpoint_reachable,
    );
    let last_error = first_error(&health_probe, &models_probe, &metrics_probe);

    EngineProbeSnapshot {
        engine,
        adapter: adapter.to_owned(),
        capabilities: capabilities
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        endpoint: Some(target.base_url.clone()),
        endpoint_reachable,
        health_probe,
        models_probe,
        metrics_probe,
        observed_models,
        observed_metrics,
        status,
        last_error,
    }
}

fn parse_models_response(response: HttpProbeResponse) -> (ProbeResult, Vec<String>) {
    if response.result.status != ProbeStatus::Succeeded {
        return (response.result, Vec::new());
    }
    let Some(body) = response.body.as_deref() else {
        return (
            failed_result(response.result.http_status, "模型列表响应体为空"),
            Vec::new(),
        );
    };
    match parse_openai_model_ids(body) {
        Ok(ids) => {
            let mut observed = Vec::new();
            for id in ids {
                let id = id.trim();
                if id.is_empty() || observed.iter().any(|item| item == id) {
                    continue;
                }
                observed.push(id.to_owned());
                if observed.len() >= MAX_OBSERVED_MODELS {
                    break;
                }
            }
            (response.result, observed)
        }
        Err(message) => (
            failed_result(response.result.http_status, &message),
            Vec::new(),
        ),
    }
}

fn parse_metrics_response(response: HttpProbeResponse) -> (ProbeResult, Vec<ObservedMetric>) {
    if response.result.status != ProbeStatus::Succeeded {
        return (response.result, Vec::new());
    }
    let body = response.body.as_deref().unwrap_or_default();
    let observed = parse_allowed_metrics(parse_prometheus_scalars(body));
    (response.result, observed)
}

fn parse_allowed_metrics(samples: Vec<PrometheusScalar>) -> Vec<ObservedMetric> {
    samples
        .into_iter()
        .filter(|sample| metric_is_allowed(&sample.name))
        .take(MAX_OBSERVED_METRICS)
        .map(|sample| ObservedMetric {
            name: sample.name,
            value: sample.value.to_string(),
        })
        .collect()
}

/// 只保存跨引擎都有限、可解释的 scalar；带 label 的 Prometheus 样本已经由
/// HTTP 层去掉 label，不把无限 label map 放入服务快照。
fn metric_is_allowed(name: &str) -> bool {
    matches!(
        name,
        "requests_running"
            | "requests_waiting"
            | "num_requests_running"
            | "num_requests_waiting"
            | "vllm:num_requests_running"
            | "vllm:num_requests_waiting"
            | "sglang:num_running_reqs"
            | "sglang:num_queue_reqs"
            | "llamacpp_requests_active"
            | "llamacpp_prompt_tokens_total"
            | "llamacpp_tokens_predicted_total"
            | "process_resident_memory_bytes"
            | "gpu_cache_usage_perc"
            | "kv_cache_usage_perc"
            | "time_to_first_token_seconds"
            | "request_latency_seconds"
    )
}

fn failed_result(http_status: Option<u16>, message: &str) -> ProbeResult {
    ProbeResult {
        status: ProbeStatus::Failed,
        http_status,
        message: Some(safe_error(message)),
    }
}

fn first_error(results: &ProbeResult, rest: &ProbeResult, last: &ProbeResult) -> Option<String> {
    [results, rest, last]
        .into_iter()
        .find_map(|result| match result.status {
            ProbeStatus::Failed | ProbeStatus::Unavailable => {
                result.message.as_deref().map(safe_error)
            }
            ProbeStatus::NotAttempted | ProbeStatus::Succeeded => None,
        })
}

fn safe_error(message: &str) -> String {
    message.chars().take(MAX_ERROR_BYTES).collect()
}

fn summarize_status(
    health: &ProbeResult,
    models: &ProbeResult,
    metrics: &ProbeResult,
    endpoint_reachable: Option<bool>,
) -> HealthStatus {
    if health.status == ProbeStatus::NotAttempted {
        return HealthStatus::Unknown;
    }

    match health.http_status {
        Some(status @ 200..=299) if health.status == ProbeStatus::Succeeded => {
            if models.status == ProbeStatus::Succeeded && metrics.status == ProbeStatus::Succeeded {
                HealthStatus::Healthy
            } else {
                // health 成功但附属端点缺失/失败，不能把 404 当作空模型。
                HealthStatus::Warning
            }
        }
        Some(500..=599) => HealthStatus::Critical,
        Some(_) => HealthStatus::Warning,
        None if endpoint_reachable == Some(true) => HealthStatus::Warning,
        None => HealthStatus::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{adapter_for, probe_service_with_transport, EngineProbeSnapshot};
    use crate::domain::{EngineKind, HealthStatus, ProbeResult, ProbeStatus, ServiceSnapshot};
    use crate::engines::http::{HttpProbeResponse, ProbePath, ProbeTarget, ProbeTransport};

    #[derive(Debug)]
    struct FakeTransport {
        responses: Vec<HttpProbeResponse>,
        calls: RefCell<Vec<ProbePath>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<HttpProbeResponse>) -> Self {
            Self {
                responses,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<ProbePath> {
            self.calls.borrow().clone()
        }
    }

    impl ProbeTransport for FakeTransport {
        fn get(&self, path: ProbePath) -> HttpProbeResponse {
            self.calls.borrow_mut().push(path);
            self.responses
                .iter()
                .find(|response| response.path == path)
                .cloned()
                .unwrap_or_else(|| response(path, None, ProbeStatus::Unavailable, None))
        }
    }

    fn response(
        path: ProbePath,
        http_status: Option<u16>,
        status: ProbeStatus,
        body: Option<&str>,
    ) -> HttpProbeResponse {
        HttpProbeResponse {
            path,
            result: ProbeResult {
                status,
                http_status,
                message: (status != ProbeStatus::Succeeded)
                    .then(|| "fake transport unavailable".to_owned()),
            },
            body: body.map(str::to_owned),
        }
    }

    fn unavailable_response(path: ProbePath, message: &str) -> HttpProbeResponse {
        HttpProbeResponse {
            path,
            result: ProbeResult {
                status: ProbeStatus::Unavailable,
                http_status: None,
                message: Some(message.to_owned()),
            },
            body: None,
        }
    }

    fn service(engine: EngineKind, endpoint: Option<&str>, model: Option<&str>) -> ServiceSnapshot {
        ServiceSnapshot {
            name: "test-service".to_owned(),
            engine,
            model: model.map(str::to_owned),
            pid: None,
            port: None,
            status: HealthStatus::Unknown,
            process_present: None,
            endpoint: endpoint.map(str::to_owned),
            endpoint_reachable: None,
            health_probe: ProbeResult::not_attempted(),
            models_probe: ProbeResult::not_attempted(),
            metrics_probe: ProbeResult::not_attempted(),
            observed_models: Vec::new(),
            observed_metrics: Vec::new(),
            gpu_indices: Vec::new(),
            last_error: None,
            discovery: Default::default(),
        }
    }

    fn successful_transport() -> FakeTransport {
        FakeTransport::new(vec![
            response(
                ProbePath::Health,
                Some(200),
                ProbeStatus::Succeeded,
                Some("ok"),
            ),
            response(
                ProbePath::Models,
                Some(200),
                ProbeStatus::Succeeded,
                Some(r#"{"data":[{"id":"served-model"}]}"#),
            ),
            response(
                ProbePath::Metrics,
                Some(200),
                ProbeStatus::Succeeded,
                Some("requests_running 2\nvllm:num_requests_waiting 1"),
            ),
        ])
    }

    #[test]
    fn strict_engine_routes_keep_distinct_adapter_names_and_fixed_get_paths() {
        for (engine, expected_name) in [
            (EngineKind::LlamaCpp, "llama.cpp-http"),
            (EngineKind::Vllm, "vllm-http"),
            (EngineKind::Sglang, "sglang-http"),
        ] {
            let adapter = adapter_for(engine).expect("known engine adapter");
            assert_eq!(adapter.engine(), engine);
            assert_eq!(adapter.adapter_name(), expected_name);
            let target = ProbeTarget::default();
            let transport = successful_transport();
            let result = adapter.probe_with_transport(&target, &transport);
            assert_eq!(result.adapter, expected_name);
            assert_eq!(
                transport.calls(),
                vec![ProbePath::Health, ProbePath::Models, ProbePath::Metrics]
            );
        }
    }

    #[test]
    fn successful_health_models_and_metrics_are_healthy_and_observed() {
        let adapter = adapter_for(EngineKind::Vllm).expect("adapter");
        let mut service = service(EngineKind::Vllm, Some("http://fake.local"), None);
        let transport = successful_transport();
        let result = adapter.probe_service_with_transport(&mut service, &transport);

        assert_eq!(result.status, HealthStatus::Healthy);
        assert_eq!(result.endpoint_reachable, Some(true));
        assert_eq!(result.observed_models, ["served-model"]);
        assert_eq!(result.observed_metrics.len(), 2);
        assert_eq!(service.model.as_deref(), Some("served-model"));
        assert_eq!(service.status, HealthStatus::Healthy);
        assert_eq!(service.health_probe.http_status, Some(200));
    }

    #[test]
    fn configured_model_is_never_overwritten_by_observed_model() {
        let adapter = adapter_for(EngineKind::Sglang).expect("adapter");
        let mut service = service(
            EngineKind::Sglang,
            Some("http://fake.local"),
            Some("configured-model"),
        );
        let transport = successful_transport();
        let result = adapter.probe_service_with_transport(&mut service, &transport);

        assert_eq!(result.observed_models, ["served-model"]);
        assert_eq!(service.model.as_deref(), Some("configured-model"));
    }

    #[test]
    fn missing_models_and_unhealthy_health_are_explicit_not_empty_success() {
        let adapter = adapter_for(EngineKind::LlamaCpp).expect("adapter");
        let target = ProbeTarget::default();
        let transport = FakeTransport::new(vec![
            response(
                ProbePath::Health,
                Some(503),
                ProbeStatus::Failed,
                Some("busy"),
            ),
            response(
                ProbePath::Models,
                Some(404),
                ProbeStatus::Failed,
                Some("not found"),
            ),
            response(
                ProbePath::Metrics,
                Some(200),
                ProbeStatus::Succeeded,
                Some("requests_running 0"),
            ),
        ]);
        let result = adapter.probe_with_transport(&target, &transport);

        assert_eq!(result.status, HealthStatus::Critical);
        assert_eq!(result.endpoint_reachable, Some(true));
        assert_eq!(result.models_probe.status, ProbeStatus::Failed);
        assert_eq!(result.models_probe.http_status, Some(404));
        assert!(result.observed_models.is_empty());
    }

    #[test]
    fn transport_timeout_is_unavailable_and_does_not_claim_reachability() {
        let adapter = adapter_for(EngineKind::Vllm).expect("adapter");
        let target = ProbeTarget::default();
        let transport = FakeTransport::new(vec![
            unavailable_response(ProbePath::Health, "fake transport timeout"),
            unavailable_response(ProbePath::Models, "fake transport timeout"),
            unavailable_response(ProbePath::Metrics, "fake transport timeout"),
        ]);
        let result = adapter.probe_with_transport(&target, &transport);

        assert_eq!(result.status, HealthStatus::Unavailable);
        assert_eq!(result.endpoint_reachable, Some(false));
        assert_eq!(result.health_probe.status, ProbeStatus::Unavailable);
        assert!(result
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("timeout")));
        assert_eq!(
            transport.calls(),
            vec![ProbePath::Health, ProbePath::Models, ProbePath::Metrics]
        );
    }

    #[test]
    fn endpoint_none_is_unknown_and_performs_no_get() {
        let mut service = service(EngineKind::LlamaCpp, None, None);
        let transport = successful_transport();
        let result = probe_service_with_transport(&mut service, &transport).expect("adapter");

        assert_eq!(result.status, HealthStatus::Unknown);
        assert_eq!(result.endpoint_reachable, None);
        assert_eq!(service.health_probe.status, ProbeStatus::NotAttempted);
        assert!(transport.calls().is_empty());
    }

    #[test]
    fn unknown_engine_is_not_routed_or_probed() {
        let mut service = service(EngineKind::Unknown, Some("http://fake.local"), None);
        let transport = successful_transport();
        assert!(probe_service_with_transport(&mut service, &transport).is_none());
        assert!(transport.calls().is_empty());
        assert_eq!(service.status, HealthStatus::Unknown);
    }

    #[test]
    fn malformed_models_are_failed_while_endpoint_remains_reachable() {
        let adapter = adapter_for(EngineKind::Vllm).expect("adapter");
        let target = ProbeTarget::default();
        let transport = FakeTransport::new(vec![
            response(
                ProbePath::Health,
                Some(200),
                ProbeStatus::Succeeded,
                Some("ok"),
            ),
            response(
                ProbePath::Models,
                Some(200),
                ProbeStatus::Succeeded,
                Some("not-json"),
            ),
            response(ProbePath::Metrics, Some(404), ProbeStatus::Failed, None),
        ]);
        let result = adapter.probe_with_transport(&target, &transport);

        assert_eq!(result.endpoint_reachable, Some(true));
        assert_eq!(result.models_probe.status, ProbeStatus::Failed);
        assert_eq!(result.models_probe.http_status, Some(200));
        assert_eq!(result.status, HealthStatus::Warning);
    }

    #[test]
    fn live_target_validation_failure_is_bounded_and_has_no_credentials() {
        let adapter = adapter_for(EngineKind::LlamaCpp).expect("adapter");
        let target = ProbeTarget::new(
            "http://user:secret@fake.local",
            None,
            std::time::Duration::from_millis(1),
        );
        let result: EngineProbeSnapshot = adapter.probe_target(&target);

        assert_eq!(result.status, HealthStatus::Unavailable);
        assert_eq!(result.endpoint_reachable, Some(false));
        assert!(!result
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("secret"));
    }
}
