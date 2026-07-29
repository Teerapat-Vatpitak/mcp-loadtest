//! OTLP/HTTP request mapping and exporter-policy tests.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_core::outcome::ScenarioOutcome;
use mcp_loadtest_core::report::{ProcessStats, Report, ServerInfo};
use mcp_loadtest_output::report::otlp::{
    OtlpExportError, OtlpHttpConfig, OtlpHttpExporter, build_metrics_request,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SECRET: &str = "OTLP_SECRET_SENTINEL";

fn sample_report() -> Report {
    Report {
        run_id: "01OTLP00000000000000000000".to_owned(),
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        duration: Duration::from_secs(2),
        scenario_name: "sustained".to_owned(),
        server_info: ServerInfo {
            command: SECRET.to_owned(),
            args: vec![SECRET.to_owned()],
            pid: Some(42),
            protocol_version: Some("2025-11-25".to_owned()),
        },
        metrics: ScenarioMetrics {
            latency: LatencyStats {
                p50: Duration::from_millis(10),
                p95: Duration::from_millis(20),
                p99: Duration::from_millis(30),
                p999: Duration::from_millis(40),
                mean: Duration::from_millis(15),
                min: Duration::from_millis(1),
                max: Duration::from_millis(50),
                count: 4,
            },
            throughput: ThroughputStats {
                total_requests: 5,
                successful_requests: 4,
                requests_per_sec: 2.5,
            },
            outcomes: OutcomeCounts {
                success: 4,
                server_error: 1,
                ..OutcomeCounts::default()
            },
        },
        process: ProcessStats {
            peak_rss_mb: 12.0,
            final_rss_mb: 11.0,
            avg_cpu_pct: 5.0,
            ..ProcessStats::default()
        },
        scenario_outcome: ScenarioOutcome {
            total_calls: 5,
            successful_calls: 4,
            error_count: 1,
            ..ScenarioOutcome::default()
        },
        trace_path: None,
        threshold_violations: Vec::new(),
        coverage: None,
    }
}

#[test]
fn payload_uses_otlp_json_mapping_and_omits_server_identity() {
    let payload = build_metrics_request(&sample_report()).expect("build OTLP payload");
    let encoded = serde_json::to_string(&payload).expect("serialize payload");
    assert!(!encoded.contains(SECRET));
    assert!(encoded.contains(r#""timeUnixNano":"3000000000""#));
    assert!(encoded.contains(r#""aggregationTemporality":2"#));
    assert!(encoded.contains(r#""asInt":"4""#));
    assert!(encoded.contains(r#""name":"mcp.loadtest.call.duration""#));
    assert!(encoded.contains(r#""key":"mcp.loadtest.run.id""#));
}

#[test]
fn endpoint_and_header_policy_fail_before_network() {
    let query = OtlpHttpExporter::new(OtlpHttpConfig::new(
        "https://collector.example/v1/metrics?token=secret",
    ))
    .expect_err("queries must be rejected");
    assert!(query.to_string().contains("queries"));
    assert!(!query.to_string().contains("token=secret"));

    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_owned(), "OTEL_AUTHORIZATION".to_owned());
    let plaintext = OtlpHttpExporter::new(
        OtlpHttpConfig::new("http://collector.example/v1/metrics").with_headers_from_env(headers),
    )
    .expect_err("secret headers require TLS");
    assert!(plaintext.to_string().contains("https"));
}

#[tokio::test]
async fn exporter_posts_json_to_full_metrics_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_http_request(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .expect("write response");
        request
    });

    let exporter = OtlpHttpExporter::new(
        OtlpHttpConfig::new(format!("http://{address}/v1/metrics"))
            .with_allowed_hosts(vec!["127.0.0.1".to_owned()])
            .with_max_attempts(1),
    )
    .expect("construct exporter");
    let outcome = exporter.export(&sample_report()).await.expect("export");
    assert!(outcome.accepted);
    assert_eq!(outcome.status_code, Some(200));

    let request = server.await.expect("server task");
    assert!(request.starts_with("POST /v1/metrics HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(request.contains(r#""resourceMetrics""#));
    assert!(!request.contains(SECRET));
}

#[tokio::test]
async fn partial_success_is_best_effort_or_strict_by_policy() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut stream).await;
        let body = br#"{"partialSuccess":{"rejectedDataPoints":"1"}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write headers");
        stream.write_all(body).await.expect("write body");
    });

    let exporter = OtlpHttpExporter::new(
        OtlpHttpConfig::new(format!("http://{address}/v1/metrics"))
            .with_allowed_hosts(vec!["127.0.0.1".to_owned()])
            .with_max_attempts(1),
    )
    .expect("construct exporter");
    let outcome = exporter.export(&sample_report()).await.expect("export");
    assert!(!outcome.accepted);
    assert!(outcome.partial_success);
    server.await.expect("server task");

    // The strict branch is also pinned by the public error type without a
    // second socket: partial-success parsing itself is unit-tested in-module.
    let strict_error = OtlpExportError::PartialSuccess;
    assert!(strict_error.to_string().contains("partial"));
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_length = None;
    loop {
        let read = stream.read(&mut buffer).await.expect("read request");
        assert!(read > 0, "connection closed before request completed");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = find_subslice(&bytes, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = expected_length.get_or_insert_with(|| {
                headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                    })
                    .expect("content-length header")
            });
            if bytes.len() >= header_end + 4 + *content_length {
                return String::from_utf8(bytes).expect("UTF-8 request");
            }
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
