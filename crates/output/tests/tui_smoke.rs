//! Smoke test for the live TUI dashboard.
//!
//! Renders one frame against `ratatui::backend::TestBackend` using synthetic
//! `ScenarioMetrics`. Asserts the render does not panic and that the latency
//! p99 string ("89.0ms") shows up in the rendered buffer.
//!
//! Owned by M6 Agent Q. Keep this test free of real I/O — the dashboard's
//! crossterm-backed loop is exercised indirectly through `cargo run`, not
//! here.

use std::time::Duration;

use mcp_loadtest_core::metrics::{LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats};
use mcp_loadtest_output::tui::render_frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn synthetic_metrics() -> ScenarioMetrics {
    ScenarioMetrics {
        latency: LatencyStats {
            p50: Duration::from_millis(12),
            p95: Duration::from_millis(45),
            p99: Duration::from_millis(89),
            p999: Duration::from_millis(120),
            mean: Duration::from_millis(23),
            min: Duration::from_millis(1),
            max: Duration::from_millis(150),
            count: 1_234,
        },
        throughput: ThroughputStats {
            total_requests: 1_234,
            successful_requests: 1_200,
            requests_per_sec: 42.5,
        },
        outcomes: OutcomeCounts {
            success: 1_200,
            server_error: 34,
            ..Default::default()
        },
    }
}

/// Flatten a `TestBackend` buffer into one whitespace-separated string so we
/// can assert that expected labels show up somewhere on screen.
fn buffer_to_string(backend: &TestBackend) -> String {
    let buf = backend.buffer();
    let mut out = String::with_capacity((buf.area.width * buf.area.height) as usize);
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn renders_one_frame_against_test_backend() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test backend");
    let snapshot = synthetic_metrics();

    terminal
        .draw(|frame| {
            let area = frame.area();
            render_frame(
                frame.buffer_mut(),
                area,
                &snapshot,
                Duration::from_secs(7),
                "sustained",
                "python -m my_mcp",
            );
        })
        .expect("draw must not fail on TestBackend");

    let rendered = buffer_to_string(terminal.backend());

    // Header markers
    assert!(
        rendered.contains("mcp-loadtest live"),
        "header missing in:\n{rendered}",
    );
    assert!(
        rendered.contains("scenario=sustained"),
        "scenario name missing in:\n{rendered}",
    );
    assert!(
        rendered.contains("python -m my_mcp"),
        "server command missing in:\n{rendered}",
    );

    // Throughput
    assert!(
        rendered.contains("requests=1234"),
        "throughput count missing in:\n{rendered}",
    );
    assert!(rendered.contains("rps=42.5"), "rps missing in:\n{rendered}",);
    assert!(
        rendered.contains("errors=34"),
        "errors derived from total - success missing in:\n{rendered}",
    );

    // Latency table — p99 is the marquee value the dashboard exists to surface.
    assert!(
        rendered.contains("p99"),
        "p99 row label missing in:\n{rendered}",
    );
    assert!(
        rendered.contains("89.0ms"),
        "p99 latency value missing in:\n{rendered}",
    );

    // Footer hints
    assert!(
        rendered.contains("[q]"),
        "quit hint missing in:\n{rendered}",
    );
    assert!(
        rendered.contains("[esc]"),
        "esc hint missing in:\n{rendered}",
    );
}

#[test]
fn empty_metrics_renders_without_panic() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test backend");
    let snapshot = ScenarioMetrics::default();

    terminal
        .draw(|frame| {
            let area = frame.area();
            render_frame(
                frame.buffer_mut(),
                area,
                &snapshot,
                Duration::from_secs(0),
                "cold_start",
                "true",
            );
        })
        .expect("default-snapshot draw must not fail");

    let rendered = buffer_to_string(terminal.backend());
    // count=0 should appear; rps=0.0 should not break the formatter.
    assert!(rendered.contains("requests=0"), "got:\n{rendered}");
}

#[test]
fn long_server_command_gets_truncated() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test backend");
    let snapshot = synthetic_metrics();
    let long_cmd = "python -m really_very_extremely_long_server_command_name --with --many --args";

    terminal
        .draw(|frame| {
            let area = frame.area();
            render_frame(
                frame.buffer_mut(),
                area,
                &snapshot,
                Duration::from_secs(3),
                "sustained",
                long_cmd,
            );
        })
        .expect("long command must not break layout");

    let rendered = buffer_to_string(terminal.backend());
    // The ellipsis from `truncate` signals the clip happened cleanly.
    assert!(
        rendered.contains('…'),
        "expected truncation ellipsis with an over-long command; got:\n{rendered}",
    );
}
