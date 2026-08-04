//! llama.cpp 的只读 HTTP adapter。

use super::adapter::EngineAdapter;
use crate::domain::EngineKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct LlamaCppAdapter;

impl EngineAdapter for LlamaCppAdapter {
    fn engine(&self) -> EngineKind {
        EngineKind::LlamaCpp
    }

    fn adapter_name(&self) -> &'static str {
        "llama.cpp-http"
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &["health", "openai_models", "prometheus_metrics"]
    }
}
