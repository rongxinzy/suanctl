//! 三种推理引擎共用的只读 HTTP 探针。
//!
//! 探针只会发送 `GET /health`、`GET /v1/models` 和 `GET /metrics`。它不跟随
//! 重定向，不携带调用方的环境变量或认证信息，也不会发送推理请求。

use std::io::Read;
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::{ProbeResult, ProbeStatus};

pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_MAX_METRIC_SAMPLES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbePath {
    Health,
    Models,
    Metrics,
}

impl ProbePath {
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Health => "/health",
            Self::Models => "/v1/models",
            Self::Metrics => "/metrics",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeTarget {
    pub base_url: String,
    #[serde(default)]
    pub expected_model: Option<String>,
    #[serde(default = "default_timeout", with = "duration_millis")]
    pub connect_timeout: Duration,
    #[serde(default = "default_timeout", with = "duration_millis")]
    pub request_timeout: Duration,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
}

impl ProbeTarget {
    /// 兼容 v0.1 占位契约：一个 timeout 同时作为连接和请求超时。
    pub fn new(
        base_url: impl Into<String>,
        expected_model: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self::with_timeouts(
            base_url,
            expected_model,
            timeout,
            timeout,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    pub fn with_timeouts(
        base_url: impl Into<String>,
        expected_model: Option<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
        max_response_bytes: usize,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            expected_model,
            connect_timeout,
            request_timeout,
            max_response_bytes: max_response_bytes.max(1),
        }
    }

    pub fn validate(&self) -> Result<Url, String> {
        let url = Url::parse(&self.base_url).map_err(|_| "endpoint URL 无法解析".to_owned())?;
        match url.scheme() {
            "http" | "https" => {}
            _ => return Err("endpoint 只允许 http 或 https".to_owned()),
        }
        if url.host_str().is_none() {
            return Err("endpoint 缺少主机名".to_owned());
        }
        if url.username() != "" || url.password().is_some() {
            return Err("endpoint 不允许在 URL 中携带用户名或密码".to_owned());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("endpoint 不允许携带 query 或 fragment".to_owned());
        }
        Ok(url)
    }
}

impl Default for ProbeTarget {
    fn default() -> Self {
        Self::new("http://127.0.0.1", None, DEFAULT_TIMEOUT)
    }
}

fn default_timeout() -> Duration {
    DEFAULT_TIMEOUT
}

const fn default_max_response_bytes() -> usize {
    DEFAULT_MAX_RESPONSE_BYTES
}

mod duration_millis {
    use super::*;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_millis().min(u64::MAX as u128) as u64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpProbeResponse {
    pub path: ProbePath,
    pub result: ProbeResult,
    /// 只在本次探针内使用；不会被服务发现结果持久化。
    pub body: Option<String>,
}

#[derive(Debug)]
pub struct HttpProbeClient {
    client: Client,
    base_url: Url,
    max_response_bytes: usize,
}

/// 只读 HTTP 传输边界。适配器只能请求固定的 `ProbePath`，没有 POST、任意
/// URL 或推理入口；生产代码使用下方的 `HttpProbeClient`，测试可注入 fake。
pub trait ProbeTransport {
    fn get(&self, path: ProbePath) -> HttpProbeResponse;
}

impl HttpProbeClient {
    pub fn new(target: &ProbeTarget) -> Result<Self, String> {
        let base_url = target.validate()?;
        let client = Client::builder()
            .connect_timeout(target.connect_timeout)
            .timeout(target.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法创建只读 HTTP 客户端".to_owned())?;
        Ok(Self {
            client,
            base_url,
            max_response_bytes: target.max_response_bytes.max(1),
        })
    }

    pub fn get(&self, path: ProbePath) -> HttpProbeResponse {
        let url = append_path(&self.base_url, path.suffix());
        let response = match self.client.get(url).send() {
            Ok(response) => response,
            Err(_) => {
                return HttpProbeResponse {
                    path,
                    result: ProbeResult {
                        status: ProbeStatus::Unavailable,
                        http_status: None,
                        message: Some("HTTP endpoint 不可达或请求超时".to_owned()),
                    },
                    body: None,
                }
            }
        };

        let status = response.status().as_u16();
        match read_limited_body(response, self.max_response_bytes) {
            Ok(body) => HttpProbeResponse {
                path,
                result: ProbeResult {
                    status: if (200..300).contains(&status) {
                        ProbeStatus::Succeeded
                    } else {
                        ProbeStatus::Failed
                    },
                    http_status: Some(status),
                    message: None,
                },
                body: Some(body),
            },
            Err(message) => HttpProbeResponse {
                path,
                result: ProbeResult {
                    status: ProbeStatus::Failed,
                    http_status: Some(status),
                    message: Some(message),
                },
                body: None,
            },
        }
    }
}

impl ProbeTransport for HttpProbeClient {
    fn get(&self, path: ProbePath) -> HttpProbeResponse {
        HttpProbeClient::get(self, path)
    }
}

fn append_path(base_url: &Url, suffix: &str) -> Url {
    let mut url = base_url.clone();
    // ProbePath 定义的是服务根路径。即使用户把 OpenAI base URL 配成
    // `http://host/v1`，也不能产生 `/v1/v1/models` 或 `/v1/health`。
    url.set_path(suffix);
    url.set_query(None);
    url.set_fragment(None);
    url
}

fn read_limited_body(mut response: Response, limit: usize) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("HTTP 响应体超过大小限制".to_owned());
    }

    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut limited = response.by_ref().take(limit as u64 + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|_| "读取 HTTP 响应体失败".to_owned())?;
    if bytes.len() > limit {
        return Err("HTTP 响应体超过大小限制".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "HTTP 响应体不是有效 UTF-8".to_owned())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrometheusScalar {
    pub name: String,
    pub value: f64,
}

pub fn parse_openai_model_ids(body: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "模型列表不是有效 JSON".to_owned())?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "模型列表缺少 data 数组".to_owned())?;
    let mut ids = Vec::new();
    for item in data.iter().take(512) {
        if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

/// 解析 Prometheus exposition 中的简单 scalar；忽略 histogram、注释和非法行。
pub fn parse_prometheus_scalars(body: &str) -> Vec<PrometheusScalar> {
    let mut samples = Vec::new();
    for line in body.lines() {
        if samples.len() >= DEFAULT_MAX_METRIC_SAMPLES {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let name_with_labels = match fields.next() {
            Some(name) => name,
            None => continue,
        };
        let value = match fields.next().and_then(|raw| raw.parse::<f64>().ok()) {
            Some(value) if value.is_finite() => value,
            _ => continue,
        };
        let name = name_with_labels
            .split_once('{')
            .map_or(name_with_labels, |(name, _)| name);
        if name.is_empty() {
            continue;
        }
        samples.push(PrometheusScalar {
            name: name.to_owned(),
            value,
        });
    }
    samples
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use super::{
        append_path, parse_openai_model_ids, parse_prometheus_scalars, HttpProbeClient, ProbePath,
        ProbeTarget,
    };
    use crate::domain::ProbeStatus;

    fn serve_once(listener: TcpListener, response: &'static str) {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(response.as_bytes());
        }
    }

    fn listener() -> Option<TcpListener> {
        TcpListener::bind("127.0.0.1:0").ok()
    }

    fn base_url(listener: &TcpListener) -> String {
        format!(
            "http://{}",
            listener.local_addr().expect("listener address")
        )
    }

    #[test]
    fn parses_limited_openai_models_and_prometheus_scalars() {
        let ids = parse_openai_model_ids(r#"{"data":[{"id":"qwen"},{"id":"llama"}]}"#)
            .expect("model ids");
        assert_eq!(ids, ["qwen", "llama"]);

        let metrics = parse_prometheus_scalars(
            "# HELP requests requests\nrequests_running{model=\"qwen\"} 3\nlatency 1.25 123\ninvalid nope",
        );
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].name, "requests_running");
        assert_eq!(metrics[1].value, 1.25);
    }

    #[test]
    fn probe_paths_replace_an_openai_base_path_instead_of_duplicating_it() {
        let base = reqwest::Url::parse("http://127.0.0.1:8000/v1").expect("base URL");
        assert_eq!(
            append_path(&base, ProbePath::Models.suffix()).as_str(),
            "http://127.0.0.1:8000/v1/models"
        );
        assert_eq!(
            append_path(&base, ProbePath::Health.suffix()).as_str(),
            "http://127.0.0.1:8000/health"
        );
    }

    #[test]
    fn only_allowed_get_path_is_built_and_200_is_success() {
        let Some(listener) = listener() else {
            return;
        };
        let url = base_url(&listener);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let mut request = [0_u8; 1024];
            let count = stream.read(&mut request).expect("request bytes");
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("GET /health HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("response");
        });

        let target = ProbeTarget::with_timeouts(
            url,
            None,
            Duration::from_millis(200),
            Duration::from_millis(200),
            64,
        );
        let response = HttpProbeClient::new(&target)
            .expect("client")
            .get(ProbePath::Health);
        handle.join().expect("server");
        assert_eq!(response.result.status, ProbeStatus::Succeeded);
        assert_eq!(response.result.http_status, Some(200));
    }

    #[test]
    fn preserves_404_and_503_as_reachable_failed_responses() {
        for code in [404_u16, 503_u16] {
            let Some(listener) = listener() else {
                return;
            };
            let url = base_url(&listener);
            let response = Box::leak(
                format!("HTTP/1.1 {code} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .into_boxed_str(),
            );
            let handle = thread::spawn(move || serve_once(listener, response));
            let target = ProbeTarget::with_timeouts(
                url,
                None,
                Duration::from_millis(200),
                Duration::from_millis(200),
                64,
            );
            let result = HttpProbeClient::new(&target)
                .expect("client")
                .get(ProbePath::Health)
                .result;
            handle.join().expect("server");
            assert_eq!(result.http_status, Some(code));
            assert_eq!(result.status, ProbeStatus::Failed);
        }
    }

    #[test]
    fn response_body_limit_is_enforced() {
        let Some(listener) = listener() else {
            return;
        };
        let url = base_url(&listener);
        let handle = thread::spawn(move || {
            serve_once(
                listener,
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345",
            )
        });
        let target = ProbeTarget::with_timeouts(
            url,
            None,
            Duration::from_millis(200),
            Duration::from_millis(200),
            4,
        );
        let result = HttpProbeClient::new(&target)
            .expect("client")
            .get(ProbePath::Metrics)
            .result;
        handle.join().expect("server");
        assert_eq!(result.status, ProbeStatus::Failed);
        assert!(result
            .message
            .as_deref()
            .is_some_and(|message| message.contains("大小限制")));
    }

    #[test]
    fn timeout_is_unavailable_without_public_network() {
        let Some(listener) = listener() else {
            return;
        };
        let url = base_url(&listener);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request");
            let mut request = [0_u8; 128];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(80));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let target = ProbeTarget::with_timeouts(
            url,
            None,
            Duration::from_millis(200),
            Duration::from_millis(10),
            64,
        );
        let result = HttpProbeClient::new(&target)
            .expect("client")
            .get(ProbePath::Health)
            .result;
        let _ = handle.join();
        assert_eq!(result.status, ProbeStatus::Unavailable);
        assert_eq!(result.http_status, None);
    }

    #[allow(dead_code)]
    fn _stream_type_is_used(_: TcpStream) {}
}
