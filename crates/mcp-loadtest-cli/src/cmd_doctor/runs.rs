//! doctor check (c): stale `runs/` accumulation.
//!
//! Every run writes a `runs/<ulid>/` directory; nothing prunes them. Over a
//! long CI life this silently fills the disk. Flag it when there are too
//! many run directories *or* the tree is too large. Best-effort and
//! **bounded**: one level of run dirs plus immediate files in their one-level
//! artifact subdirectories (for example `server-stderr/`). We never recurse
//! arbitrarily deep.

use std::path::Path;

use super::CheckResult;

const NAME: &str = "runs/ accumulation";
const FIX: &str = "prune old run directories (e.g. delete or archive entries under runs/)";
/// Flag once this many run dirs accumulate.
const MAX_RUN_DIRS: usize = 50;
/// Flag once the tree exceeds this many bytes (500 MiB).
const MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Scan `runs_dir` for stale accumulation. A missing directory means "no
/// runs yet" — a clean pass, not a failure.
pub(super) async fn check(runs_dir: &Path) -> CheckResult {
    let mut entries = match tokio::fs::read_dir(runs_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::pass(NAME, "no runs yet (directory absent)");
        }
        Err(e) => {
            // Can't read it — report, but don't fail the whole doctor over a
            // diagnostic we couldn't perform.
            return CheckResult::pass(NAME, format!("skipped (cannot read {runs_dir:?}: {e})"));
        }
    };

    let mut dir_count = 0usize;
    let mut total_bytes = 0u64;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        dir_count += 1;
        total_bytes = total_bytes.saturating_add(dir_size_bounded(&entry.path()).await);
    }

    let too_many = dir_count > MAX_RUN_DIRS;
    let too_big = total_bytes > MAX_TOTAL_BYTES;
    if too_many || too_big {
        CheckResult::fail(
            NAME,
            format!(
                "{dir_count} run dir(s), ~{} MiB total (threshold: {MAX_RUN_DIRS} dirs / \
                 {} MiB)",
                total_bytes / (1024 * 1024),
                MAX_TOTAL_BYTES / (1024 * 1024)
            ),
            FIX,
        )
    } else {
        CheckResult::pass(
            NAME,
            format!(
                "{dir_count} run dir(s), ~{} MiB total",
                total_bytes / (1024 * 1024)
            ),
        )
    }
}

/// Sum immediate run files and immediate files in one artifact-directory
/// level. This includes per-session `server-stderr/*.log` evidence while
/// remaining bounded against arbitrary nested trees.
async fn dir_size_bounded(dir: &Path) -> u64 {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut size = 0u64;
    while let Ok(Some(e)) = rd.next_entry().await {
        let Ok(meta) = e.metadata().await else {
            continue;
        };
        if meta.is_file() {
            size = size.saturating_add(meta.len());
            continue;
        }
        if !meta.is_dir() {
            continue;
        }

        let Ok(mut nested) = tokio::fs::read_dir(e.path()).await else {
            continue;
        };
        while let Ok(Some(nested_entry)) = nested.next_entry().await {
            if let Ok(nested_meta) = nested_entry.metadata().await
                && nested_meta.is_file()
            {
                size = size.saturating_add(nested_meta.len());
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_dir_is_a_clean_pass() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let r = check(&missing).await;
        assert!(r.ok);
        assert!(r.detail.contains("no runs yet"));
    }

    #[tokio::test]
    async fn empty_runs_dir_passes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = check(tmp.path()).await;
        assert!(r.ok);
        assert!(r.detail.contains("0 run dir"));
    }

    #[tokio::test]
    async fn too_many_run_dirs_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..(MAX_RUN_DIRS + 5) {
            tokio::fs::create_dir(tmp.path().join(format!("run-{i:03}")))
                .await
                .expect("mkdir fake run");
        }
        let r = check(tmp.path()).await;
        assert!(!r.ok, "more than {MAX_RUN_DIRS} run dirs must fail");
        assert!(r.fix.is_some());
        assert!(r.detail.contains("threshold"));
    }

    #[tokio::test]
    async fn loose_files_are_not_counted_as_run_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(tmp.path().join("README"), b"not a run dir")
            .await
            .expect("write loose file");
        let r = check(tmp.path()).await;
        assert!(r.ok);
        assert!(r.detail.contains("0 run dir"));
    }

    #[tokio::test]
    async fn bounded_size_includes_per_session_stderr_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run = tmp.path().join("run");
        let stderr = run.join("server-stderr");
        tokio::fs::create_dir_all(&stderr)
            .await
            .expect("create stderr artifact directory");
        tokio::fs::write(run.join("metrics.json"), vec![0u8; 3])
            .await
            .expect("write root artifact");
        tokio::fs::write(stderr.join("session-000001.log"), vec![0u8; 5])
            .await
            .expect("write worker artifact");
        let ignored = stderr.join("deeper");
        tokio::fs::create_dir_all(&ignored)
            .await
            .expect("create deeper directory");
        tokio::fs::write(ignored.join("ignored.log"), vec![0u8; 7])
            .await
            .expect("write deeper artifact");

        assert_eq!(dir_size_bounded(&run).await, 8);
    }
}
