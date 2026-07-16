//! `--explain` support (DESIGN §21.4).
//!
//! Every subcommand answers `--explain` with a static, human- and
//! LLM-readable description of *what the subcommand does* and *which knobs
//! tune it*, so an agent can plan the right invocation without a
//! trial-and-error loop.
//!
//! Why a **pre-clap** `std::env::args()` scan rather than a clap flag the
//! dispatcher reads: several subcommands have *required* args (`run
//! --config`, `cross --server`). A subcommand-local `--explain` would never
//! be reached — clap rejects the parse for the missing required arg before
//! any handler runs. So `main` calls [`maybe_handle_explain`] **before**
//! `Cli::parse()`. A `#[arg(long, global = true)] explain` is still
//! registered on the clap struct purely so `--help` advertises it and the
//! normal parse path doesn't choke on a stray `--explain`.
//!
//! See [ADR 0014](../../../docs/adr/0014-error-hints-explain-doctor.md).

/// If `--explain` appears anywhere in the process args, print the matching
/// subcommand's explanation to stdout and return `true` (the caller should
/// then exit `0`). Otherwise return `false` and let normal parsing proceed.
///
/// The "subcommand" is the first non-flag argument (the first arg after
/// `argv[0]` that does not start with `-`). An unknown or missing subcommand
/// prints a general overview. Always a clean (exit-0) path — `--explain`
/// never fails.
#[must_use]
pub fn maybe_handle_explain() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--explain") {
        return false;
    }
    let subcommand = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str);
    print!("{}", explanation_for(subcommand));
    true
}

/// The static explanation text for `subcommand` (or the general overview
/// when it is unknown / absent). Pure — split out so it is unit-testable
/// without touching `std::env::args()` or stdout.
fn explanation_for(subcommand: Option<&str>) -> &'static str {
    match subcommand {
        Some("deadlock-probe") => DEADLOCK_PROBE,
        Some("run") => RUN,
        Some("compare") => COMPARE,
        Some("replay") => REPLAY,
        Some("cross") => CROSS,
        Some("serve") => SERVE,
        Some("example-config") => EXAMPLE_CONFIG,
        Some("list-scenarios") => LIST_SCENARIOS,
        Some("doctor") => DOCTOR,
        _ => GENERAL,
    }
}

/// Verbatim from DESIGN.md §21.4 — kept byte-for-byte so the documented
/// contract and the runtime output never drift (snapshot-stable per §21.9).
const DEADLOCK_PROBE: &str = "\
Algorithm:
  1. Spawn server process.
  2. Send `initialize`. Wait up to startup_timeout (default 10s).
  3. Send `notifications/initialized`.
  4. Send `tools/list`. Wait up to 1s.
  5. Issue N sequential `tools/call` probes against the same session.
     (The barrier-released concurrent burst of DESIGN §15.2 needs the
     multi-session pool — M8+ backlog. The lazy-init bug class hangs
     regardless of concurrency, so the probe still catches it.)
  6. Each call wrapped in hang_detect(hang_threshold, grace_period):
     - response within hang_threshold → SUCCESS
     - response between threshold and grace_period → SLOW (warning)
     - no response after grace_period → DEADLOCK (critical)
  7. Bail on the first DEADLOCK — the session is wedged — and report.

Defaults: this subcommand probes quickly (N=5, hang_threshold=2s,
  grace_period=5s); `run` with `[scenario] type = \"deadlock_probe\"` is the
  thorough CI form (N=20, hang_threshold=5s, grace_period=10s).

Tunable knobs: --concurrent, --hang-threshold, --grace-period.
See DESIGN.md §15.2 for the spec source (and its \"Shipped reality\" note).
";

const RUN: &str = "\
What it does:
  Loads a TOML config (`[server]` + `[scenario]` + `[thresholds]`), builds
  the requested scenario, drives the workload against the server, writes
  per-run artifacts to `runs/<id>/`, and exits non-zero if any
  `[thresholds]` entry is breached (the CI gating contract).

Algorithm:
  1. Parse --config (TOML). Reject malformed / invalid configs up front.
  2. Build the `[scenario].type` scenario (sustained / ramp / soak / spike /
     pattern / race_check / fuzzer / deadlock_probe / cold_start).
  3. Spawn / connect the server (stdio child, or http / sse / ws — outbound
     network targets pass the SSRF host-allowlist first; see ADR 0012).
  4. Run `initialize` + `tools/list`, then execute the scenario.
  5. Aggregate metrics, render the configured `[output].formats`.
  6. Compare against `[thresholds]`; non-zero exit on any violation.

Tunable knobs: --config (everything else lives in the TOML),
  --capture-stderr (capture the server's stderr to
  runs/<id>/server-stderr.log), --tee-stderr (capture AND mirror it live),
  --trace (record every JSON-RPC frame to an mcp-trace/1 JSONL file,
  replayable via `replay`; see ADR 0021).
See DESIGN.md §15 for the per-scenario specs.
";

const COMPARE: &str = "\
What it does:
  Diffs two `metrics.json` reports (a baseline and a current run) and flags
  regressions, for use as a CI gate between runs.

Algorithm:
  1. Load <baseline.json> and <current.json>.
  2. Compute deltas: p99 latency growth (%), error-rate growth (pp),
     deadlock-count change.
  3. Flag a regression when a delta exceeds its threshold.
  4. Render the diff (`markdown` for humans, `json` for CI) and exit
     non-zero if any regression flag fired.

Tunable knobs: --format, --max-p99-regression-pct,
  --max-error-rate-regression-pp, --allow-deadlock-increase.
See DESIGN.md §15.4 for the gating contract.
";

const REPLAY: &str = "\
What it does:
  Replays a trace recorded with `run --trace <file>` against a server and
  diffs every response against the recording — catches behavior drift
  between two builds or implementations of the same MCP server (ADR 0021).

Algorithm:
  1. Parse the mcp-trace/1 JSONL file (header line + one frame per line).
  2. Spawn / connect the target (--server for stdio, --url for http/sse/ws)
     WITHOUT a handshake — the trace carries the recorded handshake frames.
  3. Re-send every recorded client frame in order: requests get fresh
     sequential JSON-RPC ids; notifications are re-sent as-is.
  4. Diff each response against the recorded one (canonical JSON, ids
     ignored). Transport errors and per-request timeouts count as diverged.
  5. Print a summary and exit non-zero if any frame diverged.

Tunable knobs: --server, --url, --transport (stdio|http|sse|ws),
  --allow-host (SSRF allowlist for URL transports, repeatable; ADR 0012).
";

const CROSS: &str = "\
What it does:
  Runs the *same* workload against N servers and prints a side-by-side
  comparison — e.g. an old vs. new implementation of the same MCP server.

Algorithm:
  1. For each --server: parse the command, spawn it, run the chosen
     scenario (`sustained` or `deadlock_probe`) for --duration.
  2. Collect each server's metrics into its own runs/<id>/ dir.
  3. Render a side-by-side table to stdout (the comparison itself is not
     written to disk; the per-server artifacts are).

Tunable knobs: --server (repeat once per server), --tool, --args,
  --duration, --scenario, --output-dir.
";

const SERVE: &str = "\
What it does:
  Exposes mcp-loadtest itself AS an MCP server over stdio, so an MCP-aware
  agent (Claude Code, Cursor, …) can drive load tests by calling tools
  (`deadlock_probe`, `sustained_load`, `compare_runs`, `report_summary`,
  `list_recent_runs`) — no human-in-the-loop to spawn a child and parse
  stdout.

Algorithm:
  1. Speak the MCP protocol over stdio (the default and only serve mode).
  2. On each tool call, run the corresponding load test and return
     structured JSON.

Tunable knobs: --mcp (stdio; HTTP/SSE serve modes are deferred).
See DESIGN.md §21.2 for the tool catalogue.
";

const EXAMPLE_CONFIG: &str = "\
What it does:
  Prints a fully-commented, known-good TOML config to stdout. Pipe it to a
  file and edit it: `mcp-loadtest example-config > bench.toml`.

Tunable knobs: none — it is a static template.
";

const LIST_SCENARIOS: &str = "\
What it does:
  Lists the built-in scenario kinds with a one-line description of each, so
  you know what to put in `[scenario].type` of a run config.

Tunable knobs: none.
";

const DOCTOR: &str = "\
What it does:
  Diagnoses common setup problems and prints a ✅/❌ checklist with a
  one-line fix per ❌. Exits non-zero if any check fails, so an agent can
  chain it into a fix-it loop.

Checks:
  1. Python interpreter on PATH (for fixture-based tests).
  2. (with --server) the MCP server completes `initialize`; on failure the
     captured stderr is shown.
  3. Stale `runs/` accumulation (too many dirs or too large).
  4. Windows MSVC-vs-GNU toolchain mismatch (no-op off Windows).

Tunable knobs: --server (run the initialize smoke against this command),
  --runs-dir (which directory to scan for stale runs; default ./runs).
See DESIGN.md §21.6.
";

const GENERAL: &str = "\
mcp-loadtest — load tester for MCP servers.

Pass --explain after any subcommand for a description of that subcommand:

  run              run a workload from a TOML config (the main entry point)
  deadlock-probe   probe for the Vibe-Trading-bug-class deadlock
  compare          diff two metrics.json reports (a CI regression gate)
  replay           re-send a recorded trace and diff the responses
  cross            run one workload against N servers, side-by-side
  serve            expose mcp-loadtest itself as an MCP server (--mcp)
  example-config   print a known-good TOML config template
  list-scenarios   list the built-in scenario kinds
  doctor           diagnose common setup problems (✅/❌ checklist)

Example: `mcp-loadtest deadlock-probe --explain`
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlock_probe_text_has_design_21_4_landmarks() {
        let t = explanation_for(Some("deadlock-probe"));
        // Landmarks copied verbatim from DESIGN §21.4 — drift here means the
        // documented contract and the runtime output diverged.
        assert!(t.contains("sequential `tools/call` probes"));
        assert!(t.contains("hang_threshold"));
        assert!(t.contains("grace_period"));
        assert!(t.contains("DEADLOCK (critical)"));
        assert!(t.contains("Bail on the first DEADLOCK"));
        assert!(t.contains("See DESIGN.md §15.2 for the spec source"));
    }

    #[test]
    fn run_text_describes_algorithm_and_knobs() {
        let t = explanation_for(Some("run"));
        assert!(t.contains("Algorithm:"));
        assert!(t.contains("--capture-stderr"));
        assert!(t.contains("--tee-stderr"));
        assert!(t.contains("--trace"));
    }

    #[test]
    fn replay_text_describes_diff_and_exit_contract() {
        let t = explanation_for(Some("replay"));
        assert!(t.contains("mcp-trace/1"));
        assert!(t.contains("ids"));
        assert!(t.contains("exit non-zero"));
        assert!(t.contains("--allow-host"));
    }

    #[test]
    fn known_subcommands_each_have_distinct_text() {
        for sc in [
            "run",
            "deadlock-probe",
            "compare",
            "replay",
            "cross",
            "serve",
            "example-config",
            "list-scenarios",
            "doctor",
        ] {
            let t = explanation_for(Some(sc));
            assert!(!t.is_empty(), "{sc} explanation must not be empty");
            assert_ne!(t, GENERAL, "{sc} must have its own text, not the overview");
        }
    }

    #[test]
    fn unknown_or_missing_subcommand_is_the_general_overview() {
        assert_eq!(explanation_for(None), GENERAL);
        assert_eq!(explanation_for(Some("nonsense")), GENERAL);
        // The overview lists every real subcommand.
        assert!(GENERAL.contains("deadlock-probe"));
        assert!(GENERAL.contains("doctor"));
    }
}
