//! doctor check (a): a Python 3 interpreter is on PATH.
//!
//! Most fixture-based tests and the `python -m foo` server convention need
//! `python` resolvable. Resolution follows the project convention:
//! `$MCP_LOADTEST_PYTHON` if set, else `"python"`.

use tokio::process::Command;

use super::CheckResult;

const NAME: &str = "python on PATH";
const FIX: &str = "install Python 3 and ensure it's on PATH, or set \
                    MCP_LOADTEST_PYTHON to the interpreter path";

/// Resolve the interpreter the rest of the tool would use.
fn python_bin() -> String {
    std::env::var("MCP_LOADTEST_PYTHON").unwrap_or_else(|_| "python".to_string())
}

/// Run `<python> --version` and classify the result. Parameterised on the
/// binary name so the failure path is testable without mutating the process
/// environment (this crate `forbid`s `unsafe`, so `std::env::set_var` — which
/// is `unsafe` on edition 2024 — is unavailable even in tests).
async fn check_bin(bin: &str) -> CheckResult {
    match Command::new(bin).arg("--version").output().await {
        Ok(out) if out.status.success() => {
            // Python <3.4 wrote the version to stderr; modern ones to
            // stdout. Take whichever is non-empty.
            let v = String::from_utf8_lossy(&out.stdout);
            let v = if v.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr)
            } else {
                v
            };
            CheckResult::pass(NAME, format!("{} ({})", v.trim(), bin))
        }
        Ok(out) => CheckResult::fail(
            NAME,
            format!(
                "`{bin} --version` exited with {}",
                out.status
                    .code()
                    .map_or_else(|| "a signal".to_string(), |c| c.to_string())
            ),
            FIX,
        ),
        Err(e) => CheckResult::fail(NAME, format!("could not run `{bin}`: {e}"), FIX),
    }
}

/// Run the Python check against the conventionally-resolved interpreter.
pub(super) async fn check() -> CheckResult {
    check_bin(&python_bin()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn python_is_present_on_this_machine() {
        let r = check().await;
        assert!(
            r.ok,
            "Python should be on PATH for the dev/CI env: {}",
            r.detail
        );
        assert!(r.detail.to_lowercase().contains("python"));
    }

    #[tokio::test]
    async fn missing_interpreter_fails_with_fix() {
        let r = check_bin("no-such-python-interpreter-zzzz").await;
        assert!(!r.ok);
        assert!(r.fix.is_some());
        assert!(r.detail.contains("could not run"));
    }
}
