# Release runbook — v0.0.1 (manual, operator-executed)

Distribution model is set by [ADR 0015](adr/0015-defer-crates-io-distribution.md):
**no crates.io for v0.0.1** — ship via `cargo install --git` + prebuilt GitHub
Release binaries. Every step here is **reversible** (re-privatise the repo, edit
or delete a Release, move a tag) — the opposite of a crates.io publish.

Going public and creating a Release are **hard-confirm maintainer gates**.
Run these yourself in your own terminal.

This runbook is append-friendly: future versions add a new section.

---

## Precondition — verify before the gates

All of the following must hold on the commit the `v0.0.1` tag points at:

- `v0.0.1` annotated tag exists on origin.
- CI green: `fmt` · `clippy -D warnings` · `build --locked` · full test suite · `doc -D warnings`.
- `cargo deny check` ok · `cargo audit` 0 vuln (allowed warns per ADR 0011).
- `cargo package -p mcp-loadtest` packages + verifies.
- Cargo.toml registry metadata complete (used by the deferred crates.io path; harmless now).
- crates.io names `mcp-loadtest` / `mcp-loadtest-cli` free (only relevant if the Appendix is ever taken).

Quick confirm (safe, read-only):

```bash
git fetch origin --tags
test "$(git rev-parse 'v0.0.1^{commit}')" = "$(git rev-parse HEAD || true)" && echo "HEAD == tag" || echo "HEAD != tag (build the release from the tag)"
cargo package -p mcp-loadtest --locked   # must succeed
```

---

## Gate A — make the repo public _(hard-confirm; reversible)_

Both distribution channels need a public repo. `Cargo.toml`
`repository`/`homepage` already point at the canonical URL.

- [ ] Repo is **public** at `https://github.com/Teerapat-Vatpitak/mcp-loadtest`.
- [ ] `v0.0.1` tag is on origin: `git ls-remote --tags origin v0.0.1` → prints `…refs/tags/v0.0.1`.
- [ ] Push any post-tag docs commits so the public `main` matches reality:
      `git push origin main` (the `v0.0.1` tag does **not** include them —
      intentional).
- [ ] Smoke the first channel immediately:
      `cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest mcp-loadtest-cli`
      then `mcp-loadtest --version && mcp-loadtest doctor`.

Reversibility: the repo can be set private again; nothing is permanent here.

## Gate B — prebuilt binaries + Release (`release.yml`) _(reversible)_

`.github/workflows/release.yml` (hand-rolled matrix — **not** cargo-dist) builds
the `mcp-loadtest` binary for `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`, then
**creates the GitHub Release** (if absent) and attaches each archive + its
`.sha256`. A `v*` tag push runs it automatically; if the tag was pushed before
the workflow existed (or a run needs re-triggering), dispatch it manually:

```bash
gh workflow run release.yml -f tag=v0.0.1
```

(Or: GitHub → Actions → **release** → Run workflow → `tag = v0.0.1`.)

Watch the run:

```bash
gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
```

Do **not** delete-and-repush the `v0.0.1` tag to trigger CI — moving a pushed
tag is destructive and needs explicit sign-off; `workflow_dispatch` exists so
you don't have to.

- [ ] Workflow green; binaries built for all 4 targets.

## Gate C — verify the Release _(reversible)_

The workflow already created the Release with notes + assets — no manual
`gh release create`. Verify:

```bash
gh release view v0.0.1 --json assets -q ".assets[].name"
cargo install --git https://github.com/Teerapat-Vatpitak/mcp-loadtest mcp-loadtest-cli
mcp-loadtest --version
```

- [ ] 4 archives + 4 `.sha256` attached; `cargo install --git` works.
- [ ] A Release can be edited or deleted later — nothing append-only here.

## Gate D — announce _(manual)_

- [ ] HN / lobste.rs / r/rust per DESIGN.md §10. Lead with the deadlock demo;
      install line is the `cargo install --git` one (not `cargo install`).

---

## Caveats & rollback

- **Everything here is reversible** — the whole reason for ADR 0015. Repo →
  private again; Release → edit/delete; binaries → re-upload; tag → (with
  sign-off) move. Contrast crates.io: a version there is permanent.
- **Post-tag commits** (planning/distribution docs) are _not_
  in `v0.0.1`. Intentional — planning/distribution docs must not mutate a
  shipped tag. The tag is the code; `main` is where the docs move forward.
- If a Bash/permission rule blocks a step inside Claude, run it in your own
  terminal; nothing about this release requires it to run inside Claude.

## Appendix — crates.io (deferred, ADR 0015)

Recorded for the future revisit (trigger: 0.x API stabilises, or a real external
library consumer asks). Append-only — only do this when the name/API commitment
is acceptable.

```bash
# scoped token (publish-new + publish-update) at https://crates.io/settings/tokens
cargo login
git switch --detach v0.0.1          # publish from the exact tagged commit
cargo publish -p mcp-loadtest --locked
#   wait ~1–2 min for the index
cargo publish -p mcp-loadtest-cli --locked
git switch main
```

When this is taken, update README/DESIGN install lines back to plain
`cargo install mcp-loadtest-cli` and write the follow-up ADR that closes ADR
0015's revisit question.
