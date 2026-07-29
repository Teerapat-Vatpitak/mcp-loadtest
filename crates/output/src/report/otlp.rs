//! One-shot OTLP/HTTP JSON metrics exporter.
//!
//! A completed load-test report is a bounded batch, so this exporter builds a
//! single OTLP `ExportMetricsServiceRequest` without pulling in the full
//! OpenTelemetry SDK or protobuf runtime. OTLP/HTTP explicitly supports the
//! proto3 JSON mapping with `Content-Type: application/json`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_loadtest_core::config::is_managed_remote_header;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::report::Report;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_ATTEMPTS: u8 = 3;
const MAX_ATTEMPTS: u8 = 10;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Configuration for [`OtlpHttpExporter`].
///
/// Header values are referenced by environment-variable name and resolved
/// only when export begins. Literal secret values are intentionally absent
/// from this API.
#[derive(Debug, Clone)]
pub struct OtlpHttpConfig {
    /// Full OTLP metrics endpoint, normally ending in `/v1/metrics`.
    pub endpoint: String,
    /// Outbound header name to environment-variable name.
    pub headers_from_env: BTreeMap<String, String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Turn backend/network rejection into a hard error instead of a
    /// best-effort [`OtlpExportOutcome`] with `accepted = false`.
    pub fail_on_error: bool,
    /// Exact-match outbound host allowlist. Empty permits any public host.
    pub allowed_hosts: Vec<String>,
    /// Maximum request attempts, including the first request.
    pub max_attempts: u8,
}

impl OtlpHttpConfig {
    /// Construct an exporter config with safe one-shot defaults.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            headers_from_env: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            fail_on_error: false,
            allowed_hosts: Vec::new(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }

    /// Set outbound header environment references.
    #[must_use]
    pub fn with_headers_from_env(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers_from_env = headers;
        self
    }

    /// Set the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set whether final export rejection is a hard error.
    #[must_use]
    pub fn with_fail_on_error(mut self, fail_on_error: bool) -> Self {
        self.fail_on_error = fail_on_error;
        self
    }

    /// Set the exact-match outbound host allowlist.
    #[must_use]
    pub fn with_allowed_hosts(mut self, allowed_hosts: Vec<String>) -> Self {
        self.allowed_hosts = allowed_hosts;
        self
    }

    /// Set the bounded request-attempt count.
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u8) -> Self {
        self.max_attempts = max_attempts;
        self
    }
}

/// Result of an OTLP export attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpExportOutcome {
    /// Whether the collector accepted the whole payload.
    pub accepted: bool,
    /// Whether a successful response reported partial acceptance or a warning.
    pub partial_success: bool,
    /// Number of request attempts made.
    pub attempts: u8,
    /// Final HTTP status, when a response was received.
    pub status_code: Option<u16>,
    /// Sanitized diagnostic category, never a response body or endpoint.
    pub diagnostic: Option<String>,
}

/// Errors raised while configuring, building, or strictly exporting OTLP.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OtlpExportError {
    /// The exporter configuration is invalid.
    #[error("invalid OTLP exporter configuration: {0}")]
    InvalidConfig(String),
    /// A required header environment variable is unavailable.
    #[error("OTLP header environment variable `{0}` is unavailable")]
    MissingEnvironment(String),
    /// A header value could not be represented safely.
    #[error("OTLP header from environment variable `{0}` is not a valid HTTP header value")]
    InvalidHeaderValue(String),
    /// DNS resolution or SSRF policy rejected the endpoint.
    #[error("OTLP endpoint resolution was rejected: {0}")]
    EndpointRejected(String),
    /// A report value cannot be represented in OTLP.
    #[error("OTLP metric payload rejected `{0}`")]
    InvalidMetric(&'static str),
    /// The collector response exceeded the fixed safety bound.
    #[error("OTLP collector response exceeded the 4 MiB safety limit")]
    ResponseTooLarge,
    /// The collector rejected the payload and strict export was requested.
    #[error("OTLP collector rejected the payload with HTTP status {0}")]
    HttpStatus(u16),
    /// The collector reported partial acceptance and strict export was requested.
    #[error("OTLP collector reported partial metric acceptance")]
    PartialSuccess,
    /// Every transport attempt failed and strict export was requested.
    #[error("OTLP export failed after {0} attempt(s)")]
    Transport(u8),
}

/// OTLP/HTTP JSON exporter for one completed [`Report`].
#[derive(Debug, Clone)]
pub struct OtlpHttpExporter {
    config: OtlpHttpConfig,
    endpoint: reqwest::Url,
}

impl OtlpHttpExporter {
    /// Validate `config` and construct an exporter.
    pub fn new(config: OtlpHttpConfig) -> Result<Self, OtlpExportError> {
        validate_config(&config)?;
        let endpoint = reqwest::Url::parse(&config.endpoint).map_err(|_| {
            OtlpExportError::InvalidConfig(
                "endpoint must be a valid absolute HTTP(S) URL".to_owned(),
            )
        })?;
        validate_endpoint(&endpoint, &config)?;
        Ok(Self { config, endpoint })
    }

    /// Export a report as one OTLP metrics request.
    ///
    /// Configuration and payload errors always fail. Backend/network errors
    /// return a sanitized non-accepted outcome unless `fail_on_error` is set.
    pub async fn export(&self, report: &Report) -> Result<OtlpExportOutcome, OtlpExportError> {
        let payload = build_metrics_request(report)?;
        let body =
            serde_json::to_vec(&payload).map_err(|_| OtlpExportError::InvalidMetric("payload"))?;
        let headers = resolve_headers(&self.config.headers_from_env)?;
        let client = self.build_pinned_client().await?;

        for attempt in 1..=self.config.max_attempts {
            let response = client
                .post(self.endpoint.clone())
                .headers(headers.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await;

            let mut response = match response {
                Ok(response) => response,
                Err(_) if attempt < self.config.max_attempts => {
                    tokio::time::sleep(retry_delay(attempt, None)).await;
                    continue;
                }
                Err(_) if self.config.fail_on_error => {
                    return Err(OtlpExportError::Transport(attempt));
                }
                Err(_) => {
                    return Ok(OtlpExportOutcome {
                        accepted: false,
                        partial_success: false,
                        attempts: attempt,
                        status_code: None,
                        diagnostic: Some("transport failure".to_owned()),
                    });
                }
            };

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            let response_body = read_limited_body(&mut response).await?;

            if status.is_success() {
                let partial_success = response_reports_partial_success(&response_body);
                if partial_success && self.config.fail_on_error {
                    return Err(OtlpExportError::PartialSuccess);
                }
                return Ok(OtlpExportOutcome {
                    accepted: !partial_success,
                    partial_success,
                    attempts: attempt,
                    status_code: Some(status.as_u16()),
                    diagnostic: partial_success.then(|| "partial success".to_owned()),
                });
            }

            if is_retryable_status(status.as_u16()) && attempt < self.config.max_attempts {
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
                continue;
            }

            if self.config.fail_on_error {
                return Err(OtlpExportError::HttpStatus(status.as_u16()));
            }
            return Ok(OtlpExportOutcome {
                accepted: false,
                partial_success: false,
                attempts: attempt,
                status_code: Some(status.as_u16()),
                diagnostic: Some("collector rejected payload".to_owned()),
            });
        }

        unreachable!("max_attempts validation guarantees at least one iteration")
    }

    async fn build_pinned_client(&self) -> Result<reqwest::Client, OtlpExportError> {
        let host = self
            .endpoint
            .host_str()
            .expect("endpoint validation requires a host");
        let port = self.endpoint.port_or_known_default().ok_or_else(|| {
            OtlpExportError::InvalidConfig("endpoint port could not be determined".to_owned())
        })?;
        let explicitly_allowed = self
            .config
            .allowed_hosts
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(host));

        if !self.config.allowed_hosts.is_empty() && !explicitly_allowed {
            return Err(OtlpExportError::EndpointRejected(
                "host is not present in allowed_hosts".to_owned(),
            ));
        }

        let addresses = resolve_endpoint(host, port).await?;
        if !explicitly_allowed && addresses.iter().any(|address| is_blocked_ip(address.ip())) {
            return Err(OtlpExportError::EndpointRejected(
                "host resolves to a private, loopback, link-local, or reserved address".to_owned(),
            ));
        }

        reqwest::Client::builder()
            .timeout(self.config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            // A system proxy could resolve the hostname again and bypass the
            // vetted address set, so OTLP traffic is always direct.
            .no_proxy()
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| {
                OtlpExportError::InvalidConfig("HTTP client could not be constructed".to_owned())
            })
    }
}

/// Build the OTLP JSON `ExportMetricsServiceRequest` for a report.
pub fn build_metrics_request(report: &Report) -> Result<Value, OtlpExportError> {
    validate_report_metrics(report)?;
    let start_time = unix_nanos(report.started_at)?;
    let end_time = start_time
        .checked_add(report.duration.as_nanos())
        .ok_or(OtlpExportError::InvalidMetric("run timestamp"))?;
    let start = start_time.to_string();
    let end = end_time.to_string();

    let mut metrics = vec![
        gauge_metric(
            "mcp.loadtest.run.passed",
            "Whether the load-test run passed.",
            "1",
            &end,
            f64::from(u8::from(report.passed())),
        ),
        gauge_metric(
            "mcp.loadtest.run.duration",
            "Full lifecycle duration of the load-test run.",
            "s",
            &end,
            report.duration.as_secs_f64(),
        ),
        gauge_metric(
            "mcp.loadtest.requests.rate",
            "Mean aggregate request throughput.",
            "{request}/s",
            &end,
            report.metrics.throughput.requests_per_sec,
        ),
        latency_summary_metric(report, &start, &end)?,
        gauge_metric(
            "mcp.loadtest.process.rss.peak",
            "Peak resident memory observed for the server process.",
            "By",
            &end,
            report.process.peak_rss_mb * 1_048_576.0,
        ),
        gauge_metric(
            "mcp.loadtest.process.rss.final",
            "Final resident memory observed for the server process.",
            "By",
            &end,
            report.process.final_rss_mb * 1_048_576.0,
        ),
        gauge_metric(
            "mcp.loadtest.process.cpu",
            "Mean CPU usage observed for the server process.",
            "%",
            &end,
            report.process.avg_cpu_pct,
        ),
        gauge_metric(
            "mcp.loadtest.threshold.violations",
            "Configured threshold violations in the completed run.",
            "{violation}",
            &end,
            report.threshold_violations.len() as f64,
        ),
    ];

    metrics.push(outcome_sum_metric(report, &start, &end)?);
    metrics.push(correctness_sum_metric(report, &start, &end)?);

    let protocol_version = report.server_info.protocol_version.as_deref().unwrap_or("");
    Ok(json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": [
                    string_attribute("service.name", "mcp-loadtest"),
                    string_attribute("service.version", env!("CARGO_PKG_VERSION")),
                    string_attribute("mcp.loadtest.run.id", &report.run_id),
                    string_attribute("mcp.loadtest.scenario", &report.scenario_name),
                    string_attribute("mcp.protocol.version", protocol_version)
                ]
            },
            "scopeMetrics": [{
                "scope": {
                    "name": "mcp-loadtest",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "metrics": metrics
            }]
        }]
    }))
}

fn validate_config(config: &OtlpHttpConfig) -> Result<(), OtlpExportError> {
    if config.timeout.is_zero() {
        return Err(OtlpExportError::InvalidConfig(
            "timeout must be greater than zero".to_owned(),
        ));
    }
    if !(1..=MAX_ATTEMPTS).contains(&config.max_attempts) {
        return Err(OtlpExportError::InvalidConfig(format!(
            "max_attempts must be in 1..={MAX_ATTEMPTS}"
        )));
    }
    for host in &config.allowed_hosts {
        if host.is_empty()
            || host.contains("://")
            || host.contains('/')
            || host.contains(':')
            || host.chars().any(char::is_whitespace)
        {
            return Err(OtlpExportError::InvalidConfig(
                "allowed_hosts entries must be bare hostnames or IPv4 literals".to_owned(),
            ));
        }
    }
    for (name, environment) in &config.headers_from_env {
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            OtlpExportError::InvalidConfig(format!("`{name}` is not a valid HTTP header name"))
        })?;
        if is_managed_remote_header(header.as_str()) {
            return Err(OtlpExportError::InvalidConfig(format!(
                "`{name}` is managed by the OTLP HTTP client"
            )));
        }
        if !is_portable_env_name(environment) {
            return Err(OtlpExportError::InvalidConfig(format!(
                "`{environment}` is not a portable environment-variable name"
            )));
        }
    }
    Ok(())
}

fn validate_endpoint(
    endpoint: &reqwest::Url,
    config: &OtlpHttpConfig,
) -> Result<(), OtlpExportError> {
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(OtlpExportError::InvalidConfig(
            "endpoint scheme must be http or https".to_owned(),
        ));
    }
    if endpoint.host_str().is_none_or(str::is_empty) {
        return Err(OtlpExportError::InvalidConfig(
            "endpoint must include a host".to_owned(),
        ));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(OtlpExportError::InvalidConfig(
            "endpoint URL userinfo is forbidden".to_owned(),
        ));
    }
    if endpoint.fragment().is_some() {
        return Err(OtlpExportError::InvalidConfig(
            "endpoint URL fragments are forbidden".to_owned(),
        ));
    }
    if endpoint.query().is_some() {
        return Err(OtlpExportError::InvalidConfig(
            "endpoint URL queries are forbidden; use headers_from_env for authentication"
                .to_owned(),
        ));
    }
    if !config.headers_from_env.is_empty() && endpoint.scheme() != "https" {
        return Err(OtlpExportError::InvalidConfig(
            "headers_from_env requires an https endpoint".to_owned(),
        ));
    }
    Ok(())
}

fn resolve_headers(references: &BTreeMap<String, String>) -> Result<HeaderMap, OtlpExportError> {
    let mut headers = HeaderMap::new();
    for (name, environment) in references {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            OtlpExportError::InvalidConfig(format!("`{name}` is not a valid HTTP header name"))
        })?;
        let value = std::env::var(environment)
            .map_err(|_| OtlpExportError::MissingEnvironment(environment.clone()))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|_| OtlpExportError::InvalidHeaderValue(environment.clone()))?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

async fn resolve_endpoint(host: &str, port: u16) -> Result<Vec<SocketAddr>, OtlpExportError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| {
            OtlpExportError::EndpointRejected("DNS lookup failed for endpoint host".to_owned())
        })?
        .collect();
    if addresses.is_empty() {
        return Err(OtlpExportError::EndpointRejected(
            "DNS lookup returned no addresses".to_owned(),
        ));
    }
    Ok(addresses)
}

fn validate_report_metrics(report: &Report) -> Result<(), OtlpExportError> {
    let latency = &report.metrics.latency;
    for (name, value) in [
        ("run.duration", report.duration.as_secs_f64()),
        ("requests.rate", report.metrics.throughput.requests_per_sec),
        ("latency.p50", latency.p50.as_secs_f64()),
        ("latency.p95", latency.p95.as_secs_f64()),
        ("latency.p99", latency.p99.as_secs_f64()),
        ("latency.p999", latency.p999.as_secs_f64()),
        (
            "latency.sum",
            latency.mean.as_secs_f64() * latency.count as f64,
        ),
        ("process.peak_rss", report.process.peak_rss_mb),
        ("process.final_rss", report.process.final_rss_mb),
        ("process.cpu", report.process.avg_cpu_pct),
    ] {
        if !value.is_finite() {
            return Err(OtlpExportError::InvalidMetric(name));
        }
    }
    Ok(())
}

fn latency_summary_metric(
    report: &Report,
    start: &str,
    end: &str,
) -> Result<Value, OtlpExportError> {
    let latency = &report.metrics.latency;
    let count = signed_count(latency.count, "latency.count")?;
    Ok(json!({
        "name": "mcp.loadtest.call.duration",
        "description": "Observed MCP call latency summary.",
        "unit": "s",
        "summary": {
            "dataPoints": [{
                "startTimeUnixNano": start,
                "timeUnixNano": end,
                "count": count,
                "sum": latency.mean.as_secs_f64() * latency.count as f64,
                "quantileValues": [
                    { "quantile": 0.5, "value": latency.p50.as_secs_f64() },
                    { "quantile": 0.95, "value": latency.p95.as_secs_f64() },
                    { "quantile": 0.99, "value": latency.p99.as_secs_f64() },
                    { "quantile": 0.999, "value": latency.p999.as_secs_f64() }
                ]
            }]
        }
    }))
}

fn outcome_sum_metric(report: &Report, start: &str, end: &str) -> Result<Value, OtlpExportError> {
    let outcomes = &report.metrics.outcomes;
    let mut points = Vec::new();
    for (label, count) in [
        ("success", outcomes.success),
        ("expected_rejection", outcomes.expected_rejection),
        ("hang", outcomes.hang),
        ("deadlock", outcomes.deadlock),
        ("timeout", outcomes.timeout),
        ("server_error", outcomes.server_error),
        ("protocol_error", outcomes.protocol_error),
        ("crash", outcomes.crash),
        ("malformed", outcomes.malformed),
        ("disconnected", outcomes.disconnected),
        ("cancelled", outcomes.cancelled),
    ] {
        points.push(sum_point(
            start,
            end,
            count,
            vec![string_attribute("outcome", label)],
            "requests.count",
        )?);
    }
    Ok(sum_metric(
        "mcp.loadtest.requests",
        "Requests observed by the recorder, partitioned by outcome.",
        "{request}",
        points,
    ))
}

fn correctness_sum_metric(
    report: &Report,
    start: &str,
    end: &str,
) -> Result<Value, OtlpExportError> {
    let outcome = &report.scenario_outcome;
    let mut points = Vec::new();
    for (signal, count) in [
        ("deadlock", u64::from(outcome.deadlock_count)),
        ("hang", u64::from(outcome.hang_count)),
        ("divergence", outcome.divergence_count),
        ("incomplete_worker", outcome.incomplete_worker_count),
        ("teardown_failure", outcome.teardown_failure_count),
    ] {
        points.push(sum_point(
            start,
            end,
            count,
            vec![string_attribute("signal", signal)],
            "correctness.count",
        )?);
    }
    Ok(sum_metric(
        "mcp.loadtest.correctness.events",
        "Scenario-level correctness events.",
        "{event}",
        points,
    ))
}

fn gauge_metric(name: &str, description: &str, unit: &str, end: &str, value: f64) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "gauge": {
            "dataPoints": [{
                "timeUnixNano": end,
                "asDouble": value
            }]
        }
    })
}

fn sum_metric(name: &str, description: &str, unit: &str, points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "description": description,
        "unit": unit,
        "sum": {
            "aggregationTemporality": 2,
            "isMonotonic": true,
            "dataPoints": points
        }
    })
}

fn sum_point(
    start: &str,
    end: &str,
    value: u64,
    attributes: Vec<Value>,
    metric_name: &'static str,
) -> Result<Value, OtlpExportError> {
    Ok(json!({
        "startTimeUnixNano": start,
        "timeUnixNano": end,
        "asInt": signed_count(value, metric_name)?,
        "attributes": attributes
    }))
}

fn signed_count(value: u64, metric_name: &'static str) -> Result<String, OtlpExportError> {
    i64::try_from(value)
        .map(|value| value.to_string())
        .map_err(|_| OtlpExportError::InvalidMetric(metric_name))
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({
        "key": key,
        "value": {
            "stringValue": value
        }
    })
}

fn unix_nanos(time: SystemTime) -> Result<u128, OtlpExportError> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|_| OtlpExportError::InvalidMetric("run timestamp"))
}

async fn read_limited_body(response: &mut reqwest::Response) -> Result<Vec<u8>, OtlpExportError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OtlpExportError::Transport(1))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(OtlpExportError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response_reports_partial_success(body: &[u8]) -> bool {
    if body.is_empty() {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        // A successful status with a malformed OTLP response is not evidence
        // that the collector accepted the complete payload.
        return true;
    };
    let Some(partial) = value.get("partialSuccess") else {
        return false;
    };
    let rejected = partial
        .get("rejectedDataPoints")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0);
    let warning = partial
        .get("errorMessage")
        .and_then(Value::as_str)
        .is_some_and(|message| !message.is_empty());
    rejected > 0 || warning
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .map(|delay| delay.min(MAX_RETRY_AFTER))
}

fn retry_delay(attempt: u8, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay;
    }
    let exponent = u32::from(attempt.saturating_sub(1)).min(8);
    let base_ms = 100_u64.saturating_mul(1_u64 << exponent);
    let jitter_bound = (base_ms / 4).max(1);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % jitter_bound)
        .unwrap_or(0);
    Duration::from_millis(base_ms.saturating_add(jitter))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn is_portable_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => !is_public_ipv4(ipv4),
        IpAddr::V6(ipv6) => !is_public_ipv6(ipv6),
    }
}

fn is_public_ipv4(address: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_ipv6(address: std::net::Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || is_non_public_nat64(address))
}

fn is_non_public_nat64(address: std::net::Ipv6Addr) -> bool {
    let octets = address.octets();
    let well_known_prefix = octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0];
    let local_prefix = octets[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01];
    if !well_known_prefix && !local_prefix {
        return false;
    }
    !is_public_ipv4(std::net::Ipv4Addr::new(
        octets[12], octets[13], octets[14], octets[15],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_status_set_matches_otlp() {
        for status in [429, 502, 503, 504] {
            assert!(is_retryable_status(status));
        }
        for status in [400, 401, 404, 500, 501] {
            assert!(!is_retryable_status(status));
        }
    }

    #[test]
    fn partial_success_parses_integer_strings() {
        assert!(response_reports_partial_success(
            br#"{"partialSuccess":{"rejectedDataPoints":"2"}}"#
        ));
        assert!(response_reports_partial_success(
            br#"{"partialSuccess":{"errorMessage":"collector warning"}}"#
        ));
        assert!(!response_reports_partial_success(br#"{}"#));
    }

    #[test]
    fn blocks_private_and_documentation_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "ff02::1",
            "64:ff9b::7f00:1",
        ] {
            assert!(is_blocked_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
    }
}
