# Contributing to mcp-loadtest

Thanks for considering a contribution!

## Quick start

```bash
git clone https://github.com/Teerapat-Vatpitak/mcp-loadtest
cd mcp-loadtest
bash scripts/ci-checks.sh    # or pwsh scripts/ci-checks.ps1 on Windows
```

If `ci-checks` passes, you have a working environment.

## Conventions

Project-wide conventions:
- File size discipline (< 300 lines)
- Error handling (thiserror enums, no bare `unwrap()` in lib)
- Async I/O (no blocking calls in async paths)
- Public API stability (CHANGELOG required for any `pub` change)

## Adding things

- **A new scenario** → one `impl Scenario` under [`crates/engine/src/scenario/`](crates/engine/src/scenario/), registered in the scenario builder; config-block schema documented in DESIGN.md §8
- **A new mock fixture** → a stdlib-only Python script under [`crates/engine/tests/fixtures/`](crates/engine/tests/fixtures/), using the `_common.py` framing helpers
- **A new ADR** (architecture decision) → next number under `docs/adr/`, update its README

## Pull requests

- Keep PRs focused on one change. Easier to review, easier to revert.
- Update CHANGELOG.md `[Unreleased]` if your change is user-visible.
- All checks in `scripts/ci-checks.sh` must pass.

## Reporting bugs

- Include the exact command that triggered the bug.
- Include OS, Rust version (`rustc --version`), and crate version.
- If the bug is in a specific MCP server interaction, include the smallest reproducer (or a link to one).

## License

By contributing, you agree your contributions are dual-licensed under MIT and Apache-2.0, matching the project license.
