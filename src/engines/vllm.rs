//! vLLM 的只读 HTTP adapter。

use super::adapter::EngineAdapter;
use crate::domain::EngineKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct VllmAdapter;

impl EngineAdapter for VllmAdapter {
    fn engine(&self) -> EngineKind {
        EngineKind::Vllm
    }

    fn adapter_name(&self) -> &'static str {
        "vllm-http"
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &["health", "openai_models", "prometheus_metrics", "v1"]
    }
}
