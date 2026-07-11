## Summary

<!-- 1-3 bullets describing what changed and why. -->

## Test plan

<!-- Tick what's been verified locally. -->
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo nextest run --workspace --all-features` (or `cargo test`) — all green
- [ ] `bash scripts/ci-checks.sh` (or `pwsh scripts/ci-checks.ps1`) — all green
- [ ] CHANGELOG.md `[Unreleased]` updated if user-visible
- [ ] New / changed public API has rustdoc

## Notes

<!-- Linked issues, breaking changes, follow-ups, anything reviewers should know. -->

---
By contributing you agree your changes are dual-licensed under MIT and Apache-2.0,
matching the project. See [CONTRIBUTING.md](../CONTRIBUTING.md).
