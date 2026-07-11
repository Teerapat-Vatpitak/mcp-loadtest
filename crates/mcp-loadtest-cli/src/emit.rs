//! Report emission helper — fans a `Report` out to terminal/markdown/json/html
//! sinks based on the format list from a config or subcommand default.

use std::path::Path;

use anyhow::{Context, Result};

use mcp_loadtest::report::html::HtmlReporter;
use mcp_loadtest::report::json::JsonReporter;
use mcp_loadtest::report::markdown::MarkdownReporter;
use mcp_loadtest::report::terminal::TerminalReporter;
use mcp_loadtest::report::{Report, Reporter};

/// Render `report` in each requested format. Terminal output goes to stdout;
/// other formats are written under `output_dir/<run_id>/`.
pub(crate) fn emit_reports(report: &Report, formats: &[String], output_dir: &Path) -> Result<()> {
    let run_dir = output_dir.join(&report.run_id);
    // run_dir is created by Run::execute; create_dir_all is idempotent.
    std::fs::create_dir_all(&run_dir).ok();

    for fmt in formats {
        match fmt.as_str() {
            "terminal" => {
                print!("{}", TerminalReporter.render(report)?);
            }
            "markdown" => {
                let s = MarkdownReporter.render(report)?;
                let path = run_dir.join("report.md");
                std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
            }
            "json" => {
                let s = JsonReporter.render(report)?;
                let path = run_dir.join("metrics.json");
                std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
            }
            "html" => {
                let s = HtmlReporter.render(report)?;
                let path = run_dir.join("report.html");
                std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
            }
            other => eprintln!("warning: unknown output format `{other}` (ignored)"),
        }
    }
    Ok(())
}
