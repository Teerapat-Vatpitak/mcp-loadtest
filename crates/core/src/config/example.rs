//! The canonical example TOML printed by `mcp-loadtest example-config`. Kept
//! in its own file because it's a ~70-line string literal — out of the way of
//! the schema definitions in `config.rs`.

/// Returns a fully-populated TOML string with every field shown and inline
/// comments explaining each one. Used by the `mcp-loadtest example-config`
/// subcommand. Round-trips through [`super::Config::from_toml_str`].
pub fn example_config() -> String {
    // Hand-written so we get inline comments, which serde-emitted TOML can't
    // express. Kept in sync with [`super::Config`] field-by-field.
    r#"# mcp-loadtest example config — `mcp-loadtest run --config <path>`.
# See DESIGN.md §7 for the full schema.

[server]
# Command to spawn the MCP server under test.
command = "python"
# CLI args passed to `command`. Empty list = none.
args = ["-m", "my_mcp"]
# Transport: "stdio", "http", "sse", or "ws".
transport = "stdio"
# Time budget for the `initialize` round-trip. humantime: "10s", "500ms", "1m".
startup_timeout = "10s"
# Working directory for the child. Defaults to the parent's CWD.
# working_dir = "/path/to/project"
# allowed_hosts = ["api.example.com"]   # SSRF guard: exact-match allowlist for http/sse/ws.
# Empty/unset = allow any public host. Private/loopback/link-local IP literals are always
# blocked unless the literal is listed here (escape hatch, e.g. "127.0.0.1" for local tests).

# Extra environment variables merged into the child's env. Inline table.
[server.env]
LOG_LEVEL = "warn"

[scenario]
# Scenario kind: one of "sustained", "deadlock_probe", "cold_start",
# "spike", "ramp", "soak", "race_check", "fuzzer", or "pattern".
type = "sustained"
# How long the scenario drives traffic.
duration = "60s"
# Number of concurrent worker tasks.
concurrent = 50

# Tool calls — array of tables, weighted random selection. Each entry is one
# tool the scenario will call; weights bias the random pick.
[[scenario.tool_call]]
name = "get_market_data"
args = { ticker = "AAPL" }
weight = 1.0

[[scenario.tool_call]]
name = "analyze_options"
args = { ticker = "SPY", expiry = "2026-06-19" }
weight = 0.3

[thresholds]
# Latency budgets — humantime strings ("100ms", "1s"). Omit a field to skip
# that check.
p50_latency = "100ms"
p95_latency = "300ms"
p99_latency = "500ms"
p999_latency = "1s"
# Max acceptable error rate as a fraction in [0.0, 1.0]. 0.01 = 1%.
error_rate = 0.01
# Per-call hang threshold (also used by hang_detect).
hang_timeout = "5s"
# Max RSS growth (MB) tolerated during the run.
memory_growth_mb = 50.0
# Opt-in: max least-squares RSS slope (MB/s) over the sampled timeseries.
# Catches slow steady leaks that stay under the absolute bar above.
# rss_leak_mb_per_sec = 0.5

# Per-tool latency SLOs (M7). Each entry pins a p99 latency budget for one
# tool name; the budget is evaluated against the per-tool latency snapshot
# at end-of-run. Omit to skip per-tool checks.
# [[thresholds.tool_slos]]
# tool = "get_market_data"
# p99_latency = "300ms"

[output]
# Where per-run dirs are created. Each run gets its own timestamped subdir.
report_dir = "./runs"
# Output formats to emit. Any subset of "terminal", "markdown", "json", "html".
formats = ["terminal", "markdown", "json"]
"#
    .to_string()
}
