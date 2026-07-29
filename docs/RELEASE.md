# Release runbook — `v0.2.0` candidate

`v0.2.0` is the next intended public release. `v0.1.0` remains an immutable
historical release; this runbook never moves or replaces that tag or its
assets. The candidate is not considered released until its exact commit, tag,
workflow, checksums, and GitHub Release have all been verified.

Changing repository visibility, pushing a release tag, creating a GitHub
Release, and publishing to crates.io are external maintainer actions. Preparing
code and running local checks does not authorize any of them.

## Non-negotiable release rules

1. Every gate below must pass on one clean commit.
2. Public visibility, tag push, and release creation each require an explicit
   maintainer confirmation.
3. GitHub **immutable releases must be enabled before the tag is pushed**.
   The release workflow queries the repository setting and refuses publication
   unless it is enabled. Once published, never delete, move, overwrite, or
   reuse that locked tag or its assets. If a tagged build is wrong, fix it and
   issue a new version such as `v0.1.1`.
4. Do not publish to crates.io as part of this runbook. Registry publication is
   append-only and needs its own explicit decision and dry-run evidence.
5. Release-facing documentation stored in the immutable tag must be true both
   before and after publication: source text never proves external
   availability, and users must verify the exact tag and GitHub Release.

## Gate A — freeze and verify the candidate locally

Run from the exact commit proposed for release.

### A1. Identity and clean tree

```bash
git status --short
git fetch origin --tags
test -z "$(git tag --list v0.2.0)"
test "$(git ls-remote --tags origin refs/tags/v0.2.0)" = ""
test "$(awk -F'"' '/^version = "/ { print $2; exit }' Cargo.toml)" = "0.2.0"
grep -F "## [0.2.0]" CHANGELOG.md
```

Expected:

- the worktree is clean;
- neither a local nor remote `v0.2.0` tag exists;
- every workspace package resolves to `0.2.0`;
- the changelog identifies `0.2.0` without claiming that external artifacts
  already exist.

### A2. Engineering gates

```bash
bash scripts/ci-checks.sh
bash scripts/test-action-args.sh
```

On Windows, run the equivalent main gate with:

```powershell
pwsh scripts/ci-checks.ps1
```

Record the commit SHA and retain the complete logs. The checks must include
formatting, `clippy -D warnings`, locked builds, all-feature tests, rustdoc,
supply-chain policy, and the adversarial Action-input contract. Invalid JSON,
non-string elements, embedded NULs, command substitutions, workflow-command
text, and shell metacharacters must all fail closed or remain literal.

### A3. Repeat/stress evidence

Run the repository's repeat-test script using the release profile documented by
the script. Both implementations disable nextest retries and retain every
attempt log plus JUnit:

```bash
bash scripts/repeat-tests.sh
```

```powershell
pwsh scripts/repeat-tests.ps1
```

The release evidence must contain:

- the exact command and commit SHA;
- JUnit XML;
- stdout/stderr for every attempt, including successful attempts;
- `DISPOSITION.txt`, which marks a fully green no-retry cohort or leaves any
  failed attempts explicitly unresolved until a human records and fixes their
  root cause.

An unexplained intermittent failure is a failed release gate. Re-running until
green without retaining the failing attempt is not acceptable.

### A4. Protocol conformance

Run the repository tests that exercise official MCP reference/conformance
artifacts, not only the self-authored Python fixtures. This candidate is pinned
to specification commit
`5f5440bb26a62e2cf3440b92da5a667efa03b267` (the official final
`2026-07-28` tag) and conformance commit
`49103de6ed70804e940637bf3e9e29e4a3f54e64`; record both in the evidence.
Run the platform-appropriate command:

```bash
bash crates/protocol/tests/run-official-conformance.sh
```

```powershell
pwsh crates/protocol/tests/run-official-conformance.ps1
```

The runners build `crates/protocol/examples/conformance_client.rs` with the
lockfile, fetch and verify the pinned specification revision, fetch the pinned
conformance revision, and exercise the `2026-07-28` request-metadata,
`tools_call`, standard-header, custom-header, and invalid-tool-header cases.
The conformance revision is the reviewed official harness commit at
verification. Final-spec reconciliation is an independent evidence leg: the
runner verifies the final tag/commit and the vendored schema contract recorded
by ADR 0023.
Do not mark the gate complete unless the runner exits successfully and its
complete log plus `SPEC_COMMIT.txt`, `FINAL_SCHEMA_RECONCILIATION.txt`,
`UPSTREAM_STATUS.txt`,
`OFFICIAL_CLIENT_SCENARIOS.txt`, and `SCOPE.tsv` are retained. The runner
checks that the final tag resolves to the reviewed final commit. It also
compares the scope manifest with the official client-scenario inventory and
fails if a scenario is unnamed or is not executed. The reviewed scope contains
32 client-role scenarios covering request metadata, headers, tools, remote
authentication, MRTR/request state, and schema references. Server-role cases
remain outside this client-adapter claim.

The retained evidence must show:

- each advertised protocol revision and transport;
- the implementation's full `2026-07-28` client-role behavior against the
  pinned final specification and reviewed official harness;
- skipped or unsupported cases with an explicit scope statement;
- zero unexpected failures.

The dated `2026-07-28` final specification is pinned at
`5f5440bb26a62e2cf3440b92da5a667efa03b267`. Evidence captured before this
candidate is stale and cannot satisfy the gate: rerun the harness against the
exact release commit and retain a fresh passing result plus
`FINAL_SCHEMA_RECONCILIATION.txt`. The runner compares its reviewed harness pin
with upstream `refs/heads/main` and fails if they differ; review and repin
deliberately rather than silently testing stale code.
If the final artifacts disagree with the implementation, remove that revision
from the release claim and keep the candidate unreleased.

### A5. Packaging dry-runs

Workspace packages depend on sibling `0.2.0` packages that are not in the
registry yet, so do not claim a verified registry package merely because the
source tree builds. Inspect the intended contents of all eight packages
without resolving unpublished registry dependencies, then dry-run the two
independent base crates:

```bash
for package in \
  mcp-loadtest-auth mcp-loadtest-core mcp-loadtest-protocol \
  mcp-loadtest-distributed mcp-loadtest-engine mcp-loadtest-output \
  mcp-loadtest mcp-loadtest-cli
do
  cargo package -p "$package" --list
done
cargo package -p mcp-loadtest-auth --locked --no-verify
cargo package -p mcp-loadtest-core --locked --no-verify
```

Keep the generated package lists and warnings in the release evidence. A
`cargo package` dry-run for a crate with unpublished sibling dependencies is
expected to fail until the internal crates are published in dependency order.
The successful base-crate dry-runs therefore do not prove that a registry
consumer can resolve the full workspace, and they do not authorize a crates.io
publish.

## Gate B — maintainer approves public visibility

Hard confirmation required.

- [ ] The maintainer reviewed the Gate A evidence.
- [ ] No secret, private fixture, credential, or proprietary test artifact is
      present in the repository or commit history intended to become public.
- [ ] README, CHANGELOG, security policy, licenses, Action contract, and
      remote-auth limitations match the candidate.
- [ ] Remote-auth wording covers environment-backed static headers plus OAuth
      authorization-code and client-credentials flows. Protected-resource
      discovery, pre-registration, CIMD/DCR, PKCE, refresh, and bounded step-up
      are documented and tested. Authenticated endpoints require HTTPS, URL
      userinfo is forbidden, and query strings must not carry secrets.
- [ ] The maintainer explicitly approved changing repository visibility.
- [ ] The repository is confirmed public only after that approval.

Stop here if any item is incomplete.

## Gate C — maintainer enables immutable releases and approves the tag workflow

Pushing `v0.2.0` triggers `.github/workflows/release.yml`, which can create the
GitHub Release. Treat the tag push as the release action, not as a harmless
preparatory step.

The workflow first verifies that GitHub immutable releases is enabled. It then
repeats the main engineering gate, invokes the complete reusable Action
contract (cross-platform argv tests plus composite end-to-end checks), runs
five no-retry stress attempts, and runs pinned protocol conformance under
read-only permissions. Four platform jobs then upload internal build
artifacts. Only one final job receives `contents: write`; it verifies all four
archives and four checksums, creates a draft Release, confirms all eight
assets, and only then publishes the draft. No individual matrix job can
publish a partial or mutable Release.

First confirm in GitHub repository settings that immutable releases is
enabled. Configure the Actions secret `IMMUTABLE_RELEASES_AUDIT_TOKEN` with a
fine-grained, read-only credential scoped to this repository and
**Administration: read**; the ordinary `GITHUB_TOKEN` cannot read this setting.
The workflow uses that credential in preflight and again immediately before
publishing the verified draft. A missing credential, disabled setting, or
read failure blocks publication. Then obtain one explicit maintainer
confirmation that names **both** the tag push and the automatically-triggered
GitHub Release. Only after that confirmation may the maintainer run:

```bash
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD)" = "<approved-candidate-commit>"
git tag -a v0.2.0 -m "mcp-loadtest v0.2.0"
test "$(git cat-file -t v0.2.0)" = "tag"
test "$(git rev-parse 'v0.2.0^{commit}')" = "$(git rev-parse HEAD)"
git push origin refs/tags/v0.2.0
```

If the workflow failed before creating any Release, rerun against the same
existing tag with `workflow_dispatch`; do not recreate or move the tag:

```bash
gh workflow run release.yml -f tag=v0.2.0
```

The publisher refuses to overwrite an existing draft or published Release. If
a failed upload left a draft, inspect its assets and logs first; cleanup or a
rerun is a separate explicit maintainer action. Never replace assets on an
already-published Release.

## Gate D — verify the published GitHub release

The release is not complete merely because the workflow is green.

```bash
gh release view v0.2.0 --json tagName,isDraft,isPrerelease,assets,url
```

Verify:

- [ ] the release points at the approved immutable commit;
- [ ] all expected platform archives and `.sha256` files exist;
- [ ] every downloaded archive matches its sidecar checksum;
- [ ] a clean machine can run the binary and `mcp-loadtest doctor`;
- [ ] `cargo install --git
      https://github.com/Teerapat-Vatpitak/mcp-loadtest --tag v0.2.0 --locked
      mcp-loadtest-cli` succeeds;
- [ ] a disposable consumer workflow pinned to
      `Teerapat-Vatpitak/mcp-loadtest@v0.2.0` passes the Action contract tests;
- [ ] release notes describe remote authentication and unsupported protocol
      cases without overstating them.
- [ ] authenticated remote examples use HTTPS/WSS and do not place
      credentials in URL userinfo or query strings.

Release-facing documentation in the tag is deliberately state-neutral and
must not need a truth-correction after publication. A later main-branch update
may add the actual release date and URL, but it does not alter the immutable
tag and must not be required to make tagged documentation accurate.

## Failure handling

- **Before a tag push:** fix the candidate, repeat Gate A, and request approval
  again.
- **After a tag push but before a usable Release:** leave the tag immutable,
  document the failed release, fix forward as `v0.1.1`, and repeat all gates.
- **After release:** publish a patch version. Never silently replace assets or
  rewrite history to make the old version appear correct.
- **Security incident:** follow `SECURITY.md`; if artifacts must be withdrawn,
  document what was removed and why. The tag and changelog remain historical
  evidence.

## Deferred: crates.io

crates.io publication is not a checkbox in this runbook. It permanently claims
names and versions and cannot be undone by deleting a GitHub Release. If the
maintainer later chooses that channel, first record the decision, verify all
workspace dependency versions from a clean tag checkout, perform package
dry-runs, and request a separate explicit confirmation immediately before each
`cargo publish`.
