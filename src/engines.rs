//! 推理服务发现与只读探针。
//!
//! `engines` 模块只负责把服务来源、容器元数据和 HTTP 探针结果表达成
//! 可组合的领域对象。它不启动、停止、进入或修改任何服务。

pub mod adapter;
pub mod configured;
pub mod container;
pub mod discovery;
pub mod http;
pub mod llama_cpp;
pub mod process;
pub mod sglang;
pub mod vllm;

pub use configured::{validate_configured, ConfiguredEndpoint, ConfiguredProvider};
pub use container::{ContainerProvider, ContainerRuntimeCommand, ContainerRuntimeKind};
pub use discovery::{merge_results, DiscoveryProvider, DiscoveryResult};
pub use process::{HostProcessProvider, ProcessLimits};

pub use crate::domain::ObservedMetric;
pub use adapter::{adapter_for, probe_service_with_transport, EngineAdapter, EngineProbeSnapshot};
pub use http::{
    parse_openai_model_ids, parse_prometheus_scalars, HttpProbeClient, HttpProbeResponse,
    ProbePath, ProbeTarget, ProbeTransport, PrometheusScalar, DEFAULT_MAX_RESPONSE_BYTES,
};
pub use llama_cpp::LlamaCppAdapter;
pub use sglang::SglangAdapter;
pub use vllm::VllmAdapter;
