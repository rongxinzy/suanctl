//! 推理端点压测：对 OpenAI 兼容的 /v1/chat/completions 发并发负载。
//!
//! 这是显式操作（`suanctl bench`），不属于只读采集路径：会向目标端点
//! 发送真实推理请求。端点来源优先级：CLI --endpoint > 配置文件
//! [[endpoints]] > 自动发现（进程/容器 + 探针确认可达）。

use std::{
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Instant,
};

use serde::Serialize;

use crate::domain::ServiceSnapshot;
use crate::engines::{
    adapter_for, merge_results, ConfiguredEndpoint, ConfiguredProvider, ContainerProvider,
    DiscoveryProvider, HostProcessProvider,
};

#[derive(Debug)]
pub enum BenchError {
    /// 端点/参数问题。
    Invalid(String),
    /// 网络或 HTTP 层失败。
    Request(String),
}

impl fmt::Display for BenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Request(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BenchError {}

#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// 端点 base URL（如 http://127.0.0.1:8080，可带或不带 /v1）。
    pub endpoint: String,
    /// 模型 id；None 时查询 /v1/models 取第一个。
    pub model: Option<String>,
    /// 总请求数。
    pub prompts: usize,
    /// 并发数。
    pub concurrency: usize,
    /// 每请求最大生成 token 数。
    pub max_tokens: u32,
    /// 单请求超时（秒）。
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub endpoint: String,
    pub model: String,
    pub prompts: usize,
    pub concurrency: usize,
    pub max_tokens: u32,
    pub succeeded: usize,
    pub failed: usize,
    pub wall_seconds: f64,
    pub completion_tokens_total: u64,
    /// 总吞吐（completion tokens / 总耗时）；仅当所有成功响应都带 usage 时有值。
    pub tokens_per_second: Option<f64>,
    /// 单请求吞吐均值（剔除不带 usage 的响应）。
    pub per_request_tokens_per_second: Option<f64>,
    pub latency_avg_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_max_ms: f64,
    /// 前几个失败原因（截断）。
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
struct RequestOutcome {
    latency_ms: f64,
    completion_tokens: Option<u64>,
}

/// 归一化端点：补 http:// scheme、去尾部斜杠与 /v1 后缀。
pub fn normalize_base_url(input: &str) -> Result<String, BenchError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(BenchError::Invalid("端点不能为空".to_owned()));
    }
    let with_scheme = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_owned()
    } else {
        format!("http://{input}")
    };
    let url = reqwest::Url::parse(&with_scheme)
        .map_err(|error| BenchError::Invalid(format!("端点 URL 无效：{input}（{error}）")))?;
    if url.host_str().is_none() {
        return Err(BenchError::Invalid(format!("端点缺少主机名：{input}")));
    }
    let mut normalized = with_scheme.trim_end_matches('/').to_owned();
    if let Some(stripped) = normalized.strip_suffix("/v1") {
        normalized = stripped.to_owned();
    }
    Ok(normalized)
}

/// 发现并探活可压测的推理端点：显式配置的在前，自动发现（进程/容器）在后。
/// 只返回带端点且探针可达的服务。
pub fn discover_candidates(configured: Vec<ConfiguredEndpoint>) -> Vec<ServiceSnapshot> {
    let mut providers: Vec<Box<dyn DiscoveryProvider>> = vec![
        Box::new(HostProcessProvider::default()),
        Box::new(ContainerProvider::default()),
    ];
    if !configured.is_empty() {
        // 配置端点排在最前（优先级最高）。
        providers.insert(0, Box::new(ConfiguredProvider::new(configured)));
    }
    let mut services = merge_results(providers.iter().map(|provider| provider.discover())).services;
    for service in &mut services {
        if service.endpoint.is_some() && service.engine != crate::domain::EngineKind::Unknown {
            if let Some(adapter) = adapter_for(service.engine) {
                adapter.probe_service(service);
            }
        }
    }
    services
        .into_iter()
        .filter(|service| service.endpoint.is_some() && service.endpoint_reachable == Some(true))
        .collect()
}

/// 解析模型 id：显式指定优先；否则查询 /v1/models 取第一个。
fn resolve_model(
    client: &reqwest::blocking::Client,
    base: &str,
    model: Option<&str>,
) -> Result<String, BenchError> {
    if let Some(model) = model {
        if !model.trim().is_empty() {
            return Ok(model.trim().to_owned());
        }
    }
    let url = format!("{base}/v1/models");
    let response = client
        .get(&url)
        .send()
        .map_err(|error| BenchError::Request(format!("查询 {url} 失败：{error}")))?;
    let status = response.status();
    let body = response
        .text()
        .map_err(|error| BenchError::Request(format!("读取 {url} 响应失败：{error}")))?;
    if !status.is_success() {
        return Err(BenchError::Request(format!(
            "查询模型列表失败：HTTP {status}：{}",
            truncate(&body, 200)
        )));
    }
    let ids = crate::engines::parse_openai_model_ids(&body)
        .map_err(|error| BenchError::Request(format!("解析模型列表失败：{error}")))?;
    ids.into_iter()
        .next()
        .ok_or_else(|| BenchError::Invalid(format!("{url} 未返回任何模型，请用 --model 显式指定")))
}

/// 执行压测：1 次热身（失败即中止），随后 concurrency 个线程发满 prompts 个请求。
pub fn run_bench(config: &BenchConfig) -> Result<BenchReport, BenchError> {
    if config.prompts == 0 {
        return Err(BenchError::Invalid("请求数必须 ≥ 1".to_owned()));
    }
    if config.concurrency == 0 {
        return Err(BenchError::Invalid("并发数必须 ≥ 1".to_owned()));
    }
    let base = normalize_base_url(&config.endpoint)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .build()
        .map_err(|error| BenchError::Request(format!("构造 HTTP 客户端失败：{error}")))?;
    let model = resolve_model(&client, &base, config.model.as_deref())?;
    let url = format!("{base}/v1/chat/completions");

    // 热身请求：验证端点真的会推理，失败直接报错不进入正式压测。
    single_request(&client, &url, &model, config.max_tokens)
        .map_err(|error| BenchError::Request(format!("热身请求失败：{error}")))?;

    let next = AtomicUsize::new(0);
    let outcomes: Mutex<Vec<Result<RequestOutcome, String>>> = Mutex::new(Vec::new());
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..config.concurrency.min(config.prompts) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= config.prompts {
                    break;
                }
                let outcome = single_request(&client, &url, &model, config.max_tokens);
                outcomes.lock().expect("outcomes lock").push(outcome);
            });
        }
    });
    let wall_seconds = start.elapsed().as_secs_f64();

    let outcomes = outcomes.into_inner().expect("outcomes lock");
    let mut latencies: Vec<f64> = Vec::new();
    let mut tokens_total = 0_u64;
    let mut all_tokens_known = true;
    let mut per_request_tps: Vec<f64> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for outcome in &outcomes {
        match outcome {
            Ok(ok) => {
                latencies.push(ok.latency_ms);
                match ok.completion_tokens {
                    Some(tokens) => {
                        tokens_total += tokens;
                        if ok.latency_ms > 0.0 {
                            per_request_tps.push(tokens as f64 / (ok.latency_ms / 1000.0));
                        }
                    }
                    None => all_tokens_known = false,
                }
            }
            Err(error) => {
                if errors.len() < 3 {
                    errors.push(error.clone());
                }
            }
        }
    }
    let succeeded = latencies.len();
    if succeeded == 0 {
        return Err(BenchError::Request(format!(
            "全部 {} 个请求失败：{}",
            outcomes.len(),
            errors.join("；")
        )));
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let latency_avg = latencies.iter().sum::<f64>() / succeeded as f64;
    Ok(BenchReport {
        endpoint: base,
        model,
        prompts: config.prompts,
        concurrency: config.concurrency,
        max_tokens: config.max_tokens,
        succeeded,
        failed: outcomes.len() - succeeded,
        wall_seconds,
        completion_tokens_total: tokens_total,
        tokens_per_second: if all_tokens_known && wall_seconds > 0.0 {
            Some(tokens_total as f64 / wall_seconds)
        } else {
            None
        },
        per_request_tokens_per_second: if per_request_tps.is_empty() {
            None
        } else {
            Some(per_request_tps.iter().sum::<f64>() / per_request_tps.len() as f64)
        },
        latency_avg_ms: latency_avg,
        latency_p50_ms: percentile(&latencies, 50.0),
        latency_p95_ms: percentile(&latencies, 95.0),
        latency_max_ms: latencies.last().copied().unwrap_or(0.0),
        errors,
    })
}

/// 单次 chat completion 请求：返回延迟与 completion token 数（usage 缺失时为 None）。
fn single_request(
    client: &reqwest::blocking::Client,
    url: &str,
    model: &str,
    max_tokens: u32,
) -> Result<RequestOutcome, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "请简要说明 GPU 服务器巡检需要关注哪些指标。"}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": false,
    });
    let start = Instant::now();
    let response = client
        .post(url)
        .json(&body)
        .send()
        .map_err(|error| format!("请求失败：{error}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().unwrap_or_default();
        return Err(format!("HTTP {status}：{}", truncate(&text, 200)));
    }
    let parsed: serde_json::Value = response
        .json()
        .map_err(|error| format!("响应不是合法 JSON：{error}"))?;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(RequestOutcome {
        latency_ms,
        completion_tokens: completion_tokens(&parsed),
    })
}

/// 从 chat completion 响应提取 completion token 数（OpenAI 与 llama.cpp 同形）。
pub fn completion_tokens(body: &serde_json::Value) -> Option<u64> {
    body.pointer("/usage/completion_tokens")?.as_u64()
}

/// nearest-rank 百分位；输入必须已排序且非空。
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.max(1).min(sorted.len()) - 1]
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() > max {
        let mut out: String = value.chars().take(max).collect();
        out.push('…');
        out
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn normalize_base_url_handles_schemes_and_suffixes() {
        assert_eq!(
            normalize_base_url("127.0.0.1:8080").expect("url"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_base_url("http://host:8000/").expect("url"),
            "http://host:8000"
        );
        assert_eq!(
            normalize_base_url("http://host:8000/v1").expect("url"),
            "http://host:8000"
        );
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("http://").is_err());
    }

    #[test]
    fn completion_tokens_reads_usage() {
        let body = serde_json::json!({"usage": {"completion_tokens": 42, "prompt_tokens": 5}});
        assert_eq!(completion_tokens(&body), Some(42));
        assert_eq!(completion_tokens(&serde_json::json!({})), None);
    }

    #[test]
    fn percentile_nearest_rank() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&values, 50.0), 5.0);
        assert_eq!(percentile(&values, 95.0), 10.0);
        assert_eq!(percentile(&[], 95.0), 0.0);
    }

    /// 最小 HTTP/1.1 mock：按请求体计数，返回带 usage 的固定响应。
    fn mock_server(response_body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = vec![0_u8; 65536];
                let Ok(_) = stream.read(&mut buffer) else {
                    continue;
                };
                let body = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(body.as_bytes());
            }
        });
        format!("http://{address}")
    }

    #[test]
    fn run_bench_against_mock_server() {
        let endpoint = mock_server(
            r#"{"id":"x","choices":[{"message":{"content":"ok"}}],"usage":{"completion_tokens":8,"prompt_tokens":10}}"#,
        );
        let report = run_bench(&BenchConfig {
            endpoint,
            model: Some("test-model".to_owned()),
            prompts: 6,
            concurrency: 2,
            max_tokens: 8,
            timeout_secs: 10,
        })
        .expect("bench");
        assert_eq!(report.succeeded, 6);
        assert_eq!(report.failed, 0);
        assert_eq!(report.model, "test-model");
        assert_eq!(report.completion_tokens_total, 48);
        assert!(report.tokens_per_second.is_some());
        assert!(report.latency_p95_ms >= report.latency_p50_ms);
        assert!(report.latency_max_ms >= report.latency_p95_ms);
    }

    #[test]
    fn resolve_model_falls_back_to_models_listing() {
        let endpoint = mock_server(r#"{"data":[{"id":"mock-model"}]}"#);
        let client = reqwest::blocking::Client::new();
        let model = resolve_model(&client, &endpoint, None).expect("model");
        assert_eq!(model, "mock-model");
    }

    #[test]
    fn run_bench_fails_fast_when_warmup_fails() {
        // 绑定后立即 drop listener：端口不可连接。
        let address = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr")
        };
        let result = run_bench(&BenchConfig {
            endpoint: format!("http://{address}"),
            model: Some("m".to_owned()),
            prompts: 2,
            concurrency: 1,
            max_tokens: 4,
            timeout_secs: 2,
        });
        let error = result.expect_err("should fail");
        assert!(error.to_string().contains("热身请求失败"));
    }
}
