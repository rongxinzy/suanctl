//! SGLang 的只读 HTTP adapter。

use super::adapter::EngineAdapter;
use crate::domain::EngineKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct SglangAdapter;

impl EngineAdapter for SglangAdapter {
    fn engine(&self) -> EngineKind {
        EngineKind::Sglang
    }

    fn adapter_name(&self) -> &'static str {
        "sglang-http"
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &[
            "health",
            "openai_models",
            "prometheus_metrics",
            "runtime_metrics",
        ]
    }
}
