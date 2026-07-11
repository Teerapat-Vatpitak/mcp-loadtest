//! doctor check (d): Windows MSVC-vs-GNU toolchain mismatch.
//!
//! On Windows, a `*-pc-windows-gnu` binary linked against an MSVC-default
//! rustup (or vice-versa) is a classic, confusing setup failure (link
//! errors, missing CRT). This check compares the ABI *this* binary was built
//! with against `rustc`'s default host triple and flags only a **confident**
//! mismatch. Any uncertainty (rustc absent, host line unparseable) → a
//! "could not determine" pass: a diagnostic must never fail on its own
//! inability to run. On non-Windows it is a trivial n/a pass.

use super::CheckResult;

const NAME: &str = "windows toolchain (MSVC/GNU)";

#[cfg(not(windows))]
pub(super) async fn check() -> CheckResult {
    CheckResult::pass(NAME, "n/a (non-Windows)")
}

#[cfg(windows)]
const FIX: &str = "align the toolchain, e.g. `rustup default stable-x86_64-pc-windows-msvc`";

/// The ABI this binary was compiled for, as the substring that appears in a
/// rustc host triple (`msvc` / `gnu`), or `None` if it is neither (no
/// confident comparison possible).
#[cfg(windows)]
fn self_env() -> Option<&'static str> {
    if cfg!(target_env = "msvc") {
        Some("msvc")
    } else if cfg!(target_env = "gnu") {
        Some("gnu")
    } else {
        None
    }
}

/// Parse the `host: <triple>` line out of `rustc -vV` output.
#[cfg(windows)]
fn host_triple(rustc_vv: &str) -> Option<String> {
    rustc_vv
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|h| h.trim().to_string())
}

/// Classify a (self-abi, rustc-host) pair. Pulled out pure so the decision
/// table is unit-testable without invoking `rustc`.
#[cfg(windows)]
fn classify(self_abi: Option<&str>, host: Option<&str>) -> CheckResult {
    match (self_abi, host) {
        (Some(abi), Some(host)) if host.contains("msvc") || host.contains("gnu") => {
            let host_abi = if host.contains("msvc") { "msvc" } else { "gnu" };
            if abi == host_abi {
                CheckResult::pass(NAME, format!("binary `{abi}` matches rustc host `{host}`"))
            } else {
                CheckResult::fail(
                    NAME,
                    format!(
                        "this binary is `{abi}` but rustc default host is `{host}` \
                         (`{host_abi}`)"
                    ),
                    FIX,
                )
            }
        }
        // Anything ambiguous: don't fail on what we couldn't determine.
        _ => CheckResult::pass(NAME, "could not determine (skipped)"),
    }
}

#[cfg(windows)]
pub(super) async fn check() -> CheckResult {
    let host = match tokio::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).into_owned();
            host_triple(&s)
        }
        _ => None,
    };
    classify(self_env(), host.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_runs_and_is_well_formed() {
        // Off Windows this is an n/a pass; on Windows it's pass-or-fail but
        // must always carry a non-empty detail.
        let r = check().await;
        assert_eq!(r.name, NAME);
        assert!(!r.detail.is_empty());
        #[cfg(not(windows))]
        assert!(r.ok && r.detail.contains("non-Windows"));
    }

    #[cfg(windows)]
    #[test]
    fn classify_matching_abi_passes() {
        let r = classify(Some("msvc"), Some("x86_64-pc-windows-msvc"));
        assert!(r.ok);
    }

    #[cfg(windows)]
    #[test]
    fn classify_confident_mismatch_fails_with_fix() {
        let r = classify(Some("gnu"), Some("x86_64-pc-windows-msvc"));
        assert!(!r.ok);
        assert!(r.fix.is_some());
        assert!(r.detail.contains("gnu") && r.detail.contains("msvc"));
    }

    #[cfg(windows)]
    #[test]
    fn classify_uncertain_is_a_pass_not_a_failure() {
        // rustc host unknown, or self ABI neither msvc nor gnu.
        assert!(classify(Some("msvc"), None).ok);
        assert!(classify(None, Some("x86_64-pc-windows-msvc")).ok);
        assert!(classify(Some("msvc"), Some("some-weird-triple")).ok);
    }

    #[cfg(windows)]
    #[test]
    fn host_triple_parses_the_host_line() {
        let vv = "rustc 1.88.0\nbinary: rustc\nhost: x86_64-pc-windows-msvc\nrelease: 1.88.0\n";
        assert_eq!(host_triple(vv).as_deref(), Some("x86_64-pc-windows-msvc"));
        assert_eq!(host_triple("no host line here"), None);
    }
}
