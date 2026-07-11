//! Real-time TUI dashboard loop. Polls `Recorder::snapshot()` every
//! `POLL_INTERVAL` and redraws a 4-row layout: header, throughput, latency,
//! status footer. Pressing `q` or `Esc` cancels the shared run token.
//!
//! Owned by M6 Agent Q. Pure rendering helpers (`render_frame`) are kept
//! separate from the I/O loop so they can be tested against a `TestBackend`.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};
use tokio_util::sync::CancellationToken;

use crate::report::common::fmt_duration;
use mcp_loadtest_core::metrics::{Recorder, ScenarioMetrics};

/// How often to redraw the dashboard. 250 ms hits the documented refresh
/// rate without taxing the CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long the input poller blocks before checking the cancel token again.
/// Kept small so quitting feels instant; the spawn_blocking task is cheap.
const INPUT_POLL_TICK: Duration = Duration::from_millis(100);

/// Live dashboard owning the rendering loop.
///
/// One `Dashboard` is constructed alongside `Run::execute()`; the caller
/// `spawn`s the future returned by `run()` on a Tokio task. Cancellation is
/// bidirectional: the run's cancel token kills the dashboard; pressing `q`
/// inside the dashboard cancels the run.
pub struct Dashboard {
    metrics: Recorder,
    cancel: CancellationToken,
    started_at: Instant,
    scenario_name: String,
    server_command: String,
}

impl Dashboard {
    /// Construct a new dashboard. Clones the recorder so the live loop and
    /// the run share state without locking.
    pub fn new(
        metrics: Recorder,
        cancel: CancellationToken,
        scenario_name: String,
        server_command: String,
    ) -> Self {
        Self {
            metrics,
            cancel,
            started_at: Instant::now(),
            scenario_name,
            server_command,
        }
    }

    /// Run the TUI loop. Returns when the cancel token fires or the user
    /// presses `q` / `Esc`. Always restores the terminal — even on error —
    /// before returning.
    pub async fn run(self) -> io::Result<()> {
        let mut terminal = setup_terminal()?;
        let result = self.event_loop(&mut terminal).await;
        // Best-effort cleanup; if restoration itself fails we still want to
        // surface the original loop error if any.
        let restore = restore_terminal(&mut terminal);
        match (result, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
        }
    }

    async fn event_loop(
        &self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // First tick fires immediately so the user sees a frame even before
        // the first scenario call lands.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Draw current snapshot.
            let snapshot = self.metrics.snapshot();
            let elapsed = self.started_at.elapsed();
            terminal.draw(|frame| {
                let area = frame.area();
                render_frame(
                    frame.buffer_mut(),
                    area,
                    &snapshot,
                    elapsed,
                    &self.scenario_name,
                    &self.server_command,
                );
            })?;

            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Ok(());
                }
                _ = interval.tick() => {
                    // Loop and redraw.
                }
                key = poll_quit_key() => {
                    if key? {
                        self.cancel.cancel();
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Poll for quit keys (`q` or `Esc`) on a blocking thread so the crossterm
/// reader doesn't fight the tokio runtime.
async fn poll_quit_key() -> io::Result<bool> {
    tokio::task::spawn_blocking(|| -> io::Result<bool> {
        if !event::poll(INPUT_POLL_TICK)? {
            return Ok(false);
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => Ok(matches!(
                k.code,
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
            )),
            _ => Ok(false),
        }
    })
    .await
    .unwrap_or_else(|join_err| Err(io::Error::other(join_err)))
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render a single frame into the supplied buffer. Public so tests can drive
/// it against `TestBackend` without bringing up a real terminal.
///
/// Layout: 4 stacked rows.
/// 1. Header — `mcp-loadtest live :: <server> :: scenario=<name> :: elapsed Xs`
/// 2. Throughput — `requests=N rps=X.X errors=N`
/// 3. Latency table — p50 / p95 / p99 / max
/// 4. Footer — `[q] quit  [esc] quit & cancel`
pub fn render_frame(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    snapshot: &ScenarioMetrics,
    elapsed: Duration,
    scenario_name: &str,
    server_command: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(3), // throughput
            Constraint::Min(7),    // latency table
            Constraint::Length(3), // footer
        ])
        .split(area);

    render_header(buffer, chunks[0], scenario_name, server_command, elapsed);
    render_throughput(buffer, chunks[1], snapshot);
    render_latency(buffer, chunks[2], snapshot);
    render_footer(buffer, chunks[3]);
}

fn render_header(
    buffer: &mut ratatui::buffer::Buffer,
    area: Rect,
    scenario_name: &str,
    server_command: &str,
    elapsed: Duration,
) {
    let text = Line::from(vec![
        Span::styled(
            "mcp-loadtest live",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" :: "),
        Span::raw(truncate(server_command, 40)),
        Span::raw(" :: scenario="),
        Span::raw(scenario_name.to_string()),
        Span::raw(" :: elapsed "),
        Span::raw(format!("{:.1}s", elapsed.as_secs_f64())),
    ]);
    let para = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    ratatui::widgets::Widget::render(para, area, buffer);
}

fn render_throughput(buffer: &mut ratatui::buffer::Buffer, area: Rect, snapshot: &ScenarioMetrics) {
    let total = snapshot.throughput.total_requests;
    let success = snapshot.throughput.successful_requests;
    let errors = total.saturating_sub(success);
    let line = Line::from(format!(
        "requests={} rps={:.1} errors={}",
        total, snapshot.throughput.requests_per_sec, errors
    ));
    let para =
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("throughput"));
    ratatui::widgets::Widget::render(para, area, buffer);
}

fn render_latency(buffer: &mut ratatui::buffer::Buffer, area: Rect, snapshot: &ScenarioMetrics) {
    let l = &snapshot.latency;
    let rows = vec![
        Row::new(vec!["p50".to_string(), fmt_duration(l.p50)]),
        Row::new(vec!["p95".to_string(), fmt_duration(l.p95)]),
        Row::new(vec!["p99".to_string(), fmt_duration(l.p99)]),
        Row::new(vec!["max".to_string(), fmt_duration(l.max)]),
        Row::new(vec!["count".to_string(), l.count.to_string()]),
    ];
    let table = Table::new(rows, [Constraint::Length(8), Constraint::Min(10)])
        .block(Block::default().borders(Borders::ALL).title("latency"));
    ratatui::widgets::Widget::render(table, area, buffer);
}

fn render_footer(buffer: &mut ratatui::buffer::Buffer, area: Rect) {
    let line = Line::from("[q] quit  [esc] quit & cancel");
    let para = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    ratatui::widgets::Widget::render(para, area, buffer);
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_loadtest_core::metrics::{
        LatencyStats, OutcomeCounts, ScenarioMetrics, ThroughputStats,
    };

    fn synthetic_snapshot() -> ScenarioMetrics {
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
                ..Default::default()
            },
        }
    }

    #[test]
    fn truncate_clips_long_strings() {
        let long = "a".repeat(50);
        let result = truncate(&long, 10);
        assert!(result.chars().count() <= 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_passes_short_strings() {
        assert_eq!(truncate("hello", 40), "hello");
    }

    #[test]
    fn render_does_not_panic_on_tiny_area() {
        use ratatui::backend::TestBackend;
        // 10x6 is below the comfortable layout but rendering must still not
        // panic — ratatui clips, doesn't crash, when constraints overflow.
        let backend = TestBackend::new(10, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let snapshot = synthetic_snapshot();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_frame(
                    frame.buffer_mut(),
                    area,
                    &snapshot,
                    Duration::from_secs(1),
                    "sustained",
                    "python -m foo",
                );
            })
            .unwrap();
    }
}
