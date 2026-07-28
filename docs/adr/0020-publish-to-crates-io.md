# 20. Publish to crates.io (supersedes 0015)

Date: 2026-07-07
Status: Proposed, not approved or executed

**Release-truth annotation (2026-07-28):** no crate has been published under
this decision. [`docs/RELEASE.md`](../RELEASE.md) explicitly excludes
crates.io from the `v0.1.0` distribution and defers it to a separate,
irreversible maintainer decision. The floating `v1` force-move procedure in
the original proposal is retired: exact release tags are immutable, and the
release documentation recommends exact version or commit pins.

Supersedes [0015](0015-defer-crates-io-distribution.md) once executed: the
git-install and GitHub-Release-binary channels stay, crates.io is added as the
primary channel instead of a deferred option.

## Context

ADR 0015 deferred crates.io because publishing is append-only and irreversible,
and set an explicit revisit trigger: "after the 0.x API stabilises _or_ the
first external library consumer requests a registry dependency". It also
recorded an accepted risk: the `mcp-loadtest` / `mcp-loadtest-cli` names stay
unclaimed (verified free 2026-05-18) and could be squatted.

Both halves of that trade have now moved:

- **The API has stabilised in practice.** The public surface has survived
  sustained development (443 tests) with additive-only changes — no breaking
  change has been needed since the surface settled. The "may still move in
  0.x" fear that motivated 0015 has not materialised.
- **The squat risk grows with MCP's popularity.** The names are still free,
  but every month of deferral is another month in which anyone can take
  `mcp-loadtest` on crates.io permanently. 0015 explicitly declined a
  placeholder publish; the remaining way to retire the risk is the real one.
- The deferral's ongoing costs (no `cargo add mcp-loadtest`, no docs.rs, no
  crates.io discoverability, source-builds for `--git` installs) are exactly
  the adoption blockers Phase 2 exists to remove.

## Decision

Publish **both crates** to crates.io at the **next release** (the next `v0.x`
tag), in dependency order:

1. `mcp-loadtest` (library) first — the CLI's path dependency also carries
   `version = "0.0.1"`, so the CLI cannot resolve until the lib is indexed.
2. `mcp-loadtest-cli` second, after the lib version is visible in the index.

The publish itself is a **maintainer action** (hard-confirm decision point).
Tooling prepares metadata and dry-runs only; the exact command list is in
Consequences below.

### Yank policy

crates.io versions can never be deleted, only yanked (hidden from new
resolutions; existing `Cargo.lock`s still fetch them). Policy:

- **Yank when** a published version has a security vulnerability, leaks a
  secret, fails to build for new consumers, or was published by mistake
  (wrong content for its tag).
- **Never yank** merely for bugs or regressions — publish a fixed patch
  version instead and note it in CHANGELOG.
- Every yank gets a CHANGELOG entry naming the version, the reason, and the
  replacement version. Command: `cargo yank --version X.Y.Z <crate>`
  (`--undo` to reverse).
- Yank the CLI and lib together when the defect is in the lib (the CLI is a
  thin wrapper and would otherwise keep resolving the bad lib).

### GitHub Action versioning scheme (historical proposal; not in force)

The original proposal considered a floating `v1` tag. It was not executed and
is no longer the release procedure. Documentation uses immutable exact
versions such as `@v0.1.0` (or a full commit SHA). Any future moving-major-tag
policy requires a new explicit decision rather than reusing this proposal.

## Alternatives considered

| Option                             | Why rejected                                                                                                          |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| **Keep deferring (status quo)**    | 0015's revisit triggers are met; the squat risk and discoverability/docs.rs costs now outweigh the flexibility bought. |
| **Placeholder/reservation publish**| Same append-only commitment as a real publish, with none of the value; already rejected in 0015.                       |
| **Publish the CLI only**           | Impossible as ordered work — the CLI's registry dependency on `mcp-loadtest` requires the lib to be published first — and it leaves library consumers with no registry path. |
| **Wait for 1.0**                   | 0.x on crates.io is normal and semver-meaningful (0.x minor = breaking); waiting keeps the squat window open indefinitely. |

## Consequences

- **Append-only commitment**: the names and every published version are
  permanent. First publish must be correct — hence dry-runs in CI-checked
  metadata and the user-executed final step.
- Downstream crates get `cargo add mcp-loadtest`; users get
  `cargo install mcp-loadtest-cli` and (with T2.2's metadata)
  `cargo binstall mcp-loadtest-cli`.
- docs.rs builds the lib with `--all-features`
  (`[package.metadata.docs.rs] all-features = true`, verified present).
- The `readme = "../../README.md"` paths mean the repo-root README is
  packaged into both crates; README edits become part of the published
  artifact at each release.
- CHANGELOG discipline tightens: a version is frozen at publish; fixes ship
  as new patch versions, never edits.

### Exact publish procedure (user-executed)

```bash
# 0. One-time: log in with a crates.io token (https://crates.io/settings/tokens,
#    scope: publish-new + publish-update)
cargo login

# 1. From the release tag's checkout, verify packaging one last time
cargo publish --dry-run -p mcp-loadtest

# 2. Publish the library
cargo publish -p mcp-loadtest

# 3. Wait until the version is live (usually < 1 min):
#    https://crates.io/crates/mcp-loadtest

# 4. Publish the CLI (its dry-run only resolves after step 2 is indexed)
cargo publish --dry-run -p mcp-loadtest-cli
cargo publish -p mcp-loadtest-cli

# 5. Post-publish
#    - check the docs.rs build: https://docs.rs/mcp-loadtest
#    - add crates.io + docs.rs badges and `cargo install mcp-loadtest-cli`
#      to README's install section
#    - document the immutable exact Action tag; do not move or reuse tags
```

## Open questions

- Whether to add `mcp-loadtest-cli` as a `cargo install` alias crate named
  plain `mcp-loadtest` (binary name and crate name currently differ). Default:
  no — one crate per artifact; the README documents the install name.
