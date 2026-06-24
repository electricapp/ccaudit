# electricapp — GitHub Workflow & Supply-Chain Standards

**Status:** canonical org standard · **Scope:** every `electricapp` Rust repo
**Reference implementations:** `ccaudit` (CI + release + governance), `hasp` (security), `pgbattery` (breadth), `aethergraph` (correctness tooling), `ane-bridge-rs` (FFI/sanitizers)

This is the single source of truth for `.github/` across all electricapp
repositories. It is both a **rationale document** and an **implementation
spec**: the canonical snippets below are meant to be copy-pasted, with only the
repo-specific bits (binary name, targets, features) changed.

> **Why standardize.** Six repos drifted into six different CI dialects: some
> pin actions to commit SHAs, two float on mutable tags; one has no Dependabot
> at all; one has no concurrency control; least-privilege `permissions` and
> `actionlint` exist in only half. The org ships [`hasp`](https://github.com/electricapp/hasp)
> — *"verifies pins, audits workflows"* — so our own workflows passing hasp's
> paranoid mode is table stakes, not aspiration.

---

## 0. Current-state matrix (2026-06)

| Repo | branch | action pins | dependabot | least-priv perms | concurrency | actionlint job | deny.toml | governance | release hardening |
|---|---|---|---|---|---|---|---|---|---|
| `ccaudit` | main | yes (SHA) | yes (5 ecosystems) | yes (full) | yes | yes | yes | yes (gold) | yes (gold) |
| `pgbattery` | main | yes (SHA) | yes (groups+docker) | partial | yes (+main nuance) | no | yes | no | partial (minisign) |
| `hasp` | main | yes (SHA) | yes | yes (`{}`) | yes | no (self-scan) | no | no | yes (good) |
| `power-monitor` | main | yes (SHA) | yes | no | yes | no | no | no | no |
| `aethergraph` | main | **no (floating)** | yes (weekly) | no | yes | partial (config-only) | yes | no | no |
| `ane-bridge-rs` | **master** | **no (floating)** | **no (none)** | no | **no (none)** | no | no | no | no |

The right-most "best" cell in each column is the target for every repo.

---

## 1. Core principles

1. **Pin everything to an immutable ref.** Every `uses:` is a full 40-char
   commit SHA with a trailing `# vX.Y.Z` comment. Every downloaded tool
   (actionlint, cosign) is fetched by version **and** verified against a
   pinned `sha256`. Mutable tags (`@v4`, `@stable`, `@main`) are forbidden in
   `uses:` — they are a supply-chain hole and an action that flips under you
   breaks `main` non-deterministically.
2. **Least privilege by default.** Top-level `permissions:` is `{}` (deny-all)
   or `contents: read`. Each job re-grants only the scopes it needs. Every
   `checkout` sets `persist-credentials: false` unless it must push.
3. **Every job is bounded.** `timeout-minutes` on every job; `concurrency`
   with `cancel-in-progress` on every workflow.
4. **The lockfile is the unit of trust.** `Cargo.lock` is committed; supply-chain
   gates run against the *committed* lock on a schedule, not just at release.
5. **Reproducible & attestable releases.** Anything we ship is signed (Sigstore
   keyless), attested (SLSA provenance), and published via OIDC Trusted
   Publishing — **no long-lived registry secrets**.
6. **Governance is code.** Branch protection, environments, and merge policy
   live in a committed `settings.json` reconciled by a script — not clicked
   into the UI where they silently drift.
7. **Warnings are errors, everywhere.** `RUSTFLAGS: -D warnings`,
   `RUSTDOCFLAGS: -D warnings`, `clippy -- -D warnings`, `fmt --check`.
8. **Lint the pipeline itself.** `actionlint` (which also shellchecks inline
   `run:` blocks) runs on every repo.

---

## 2. The standard file set

Every Rust repo MUST contain:

```
.github/
  dependabot.yml              # §7  — cargo + github-actions (+ docker/npm/pip as needed)
  settings.json               # §10 — branch protection, environments, merge policy
  apply-settings.sh           # §10 — idempotent reconcile + drift assertion
  actionlint.yaml             # §11 — only if self-hosted runner labels are used
  workflows/
    ci.yml                    # §5  — fmt, clippy, test, doc, msrv, deny, audit, typos, machete, actionlint
    supply-chain.yml          # §8  — scheduled re-audit of the committed lockfile
    release.yml               # §9  — tag-triggered, signed, attested  (repos that ship artifacts)
deny.toml                     # §8  — license/bans/advisories/sources policy
rust-toolchain.toml           # §3  — optional; pins the toolchain for local + CI parity
```

Optional, per repo's risk profile (see §12 menu): `fuzz.yml`, `miri`/`loom`
jobs, sanitizer lanes, `security.yml` (self-scan), `action-diff.yml`,
`nightly-bench.yml`, SBOM generation.

---

## 3. Toolchain & MSRV policy

**Problem today:** toolchains are scattered — `1.94` (hasp), `1.95`
(ccaudit/aethergraph/ane), `1.96` (pgbattery/power-monitor), and bare `stable`
(aethergraph/ane). MSRV is declared in `Cargo.toml` for some, enforced in CI
for some, neither for others.

**Standard:**

- Declare `rust-version = "X.Y"` in `Cargo.toml` (the MSRV). This is the
  contract.
- CI runs the **main lanes on a pinned stable** (an `env: RUST_TOOLCHAIN`
  string, bumped by a human, not floating `stable`) **and** a dedicated
  **`msrv` job** that builds with exactly the declared `rust-version`. The two
  diverging is the signal that you bumped MSRV without meaning to.
- Two equivalent install idioms are allowed; pick one per repo and be
  consistent:
  - **`dtolnay/rust-toolchain`** (SHA-pinned `@v1`), Rust channel chosen via
    the `toolchain:` input. Comment the action's git tag `# v1`. **The bare `v1`
    ref has no default channel — you MUST pass a `toolchain:` input, or the
    action fails fast with `'toolchain' is a required input`.** dtolnay also
    publishes channel/version tags (`@stable`/`@nightly`/`@1.96`) that embed a
    default toolchain; when you SHA-pin `@v1` you lose that default, so the
    explicit `toolchain:` input is mandatory. (pgbattery hit exactly this when
    its `# 1.96`/`# nightly` channel-tag pins were converged to `@v1` without
    adding inputs — every Rust job broke until the inputs were restored.)
  - **inline `rustup`** (`rustup toolchain install "$RUST_TOOLCHAIN" --profile
    minimal --component clippy,rustfmt`). No external action; fully explicit.
    Used by ccaudit/hasp.
- `cargo-fuzz`, `miri`, sanitizers require **nightly** — install it only in the
  job that needs it, never as the repo default.

---

## 4. Triggers, concurrency, permissions (the workflow header)

Canonical header for `ci.yml`:

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:
  workflow_dispatch:

# Deny-all by default; each job re-grants what it needs.
permissions: {}

# A new push to a PR/branch cancels the prior in-progress run.
concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: "-D warnings"
  RUST_TOOLCHAIN: "1.96.0"   # bump by hand; the msrv job guards the floor
```

Decisions baked in above, with rationale:

- **`paths-ignore` vs required checks.** Do **not** add `paths-ignore: ['**/*.md']`
  if the job is a *required* status check. A workflow skipped by a path filter
  never reports its checks, so a docs-only PR hangs forever on *"Expected —
  waiting for status"* (ccaudit learned this; its `ci.yml` documents it). Two
  valid resolutions: (a) no path filter, CI is cheap enough to always run
  (ccaudit's choice), or (b) path filter **plus** a sentinel "always-green"
  job carrying the required-check name. Pick (a) unless CI is slow.
- **Cancel-in-progress on `main`?** Default `true` everywhere. pgbattery uses
  the stricter `cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}` so
  every post-merge commit on `main` keeps its signal — adopt this **only** if
  you care about per-commit `main` history (e.g. bisecting CI failures).
- **`persist-credentials: false`** on every `actions/checkout` that doesn't push.

Per-job least privilege:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    permissions:
      contents: read          # re-granted explicitly even though it's the common case
    steps:
      - uses: actions/checkout@<SHA>  # v6.0.3
        with:
          persist-credentials: false
```

---

## 5. The required CI job set

Every repo's `ci.yml` provides these checks (names matter — they become the
required-status-check contexts in `settings.json`). Combine into fewer jobs if
runner minutes matter, but keep the coverage.

| Check | Command | Notes |
|---|---|---|
| **fmt** | `cargo fmt --all -- --check` | cheap; Linux runner even for macOS crates |
| **clippy** | `cargo clippy --all-targets --all-features -- -D warnings` | all targets incl. tests/benches |
| **feature matrix** | `clippy --no-default-features [--features X]` per feature | catches feature-gating bugs (ccaudit pattern) |
| **test** | `cargo test --all-features` (`--release` if zero-alloc/optimizer-dependent tests) | bump `PROPTEST_CASES` for proptest repos (hasp: 2000) |
| **doc** | `cargo doc --no-deps --all-features` with `RUSTDOCFLAGS: -D warnings` | catches broken intra-doc links; add `broken-intra-doc-links`, `private-intra-doc-links`, `invalid-codeblock-attributes` (power-monitor pattern) |
| **msrv** | `cargo build --all-targets` on declared `rust-version` | the MSRV contract |
| **deny** | `EmbarkStudios/cargo-deny-action@<SHA>` or `cargo deny check` | §8 |
| **audit** | `rustsec/audit-check@<SHA>` (needs `checks: write`) or `cargo audit` | §8 |
| **typos** | `crate-ci/typos@<SHA>` | docs + identifiers |
| **unused-deps** | `cargo machete` (via `taiki-e/install-action@<SHA>`) | flags dead deps |
| **actionlint** | SHA-pinned download + `sha256` verify, then `./actionlint` | lints workflows + inline shell |

**`cargo machete` (unused-deps) is mandatory in every repo's CI — no exceptions.**
Unused dependencies inflate build time and the supply-chain surface and
accumulate silently; the `unused-deps` job runs on every PR in all repos and is
a required status check. Resolving a finding:

- **Genuinely unused** crate -> remove it from `Cargo.toml`.
- **False positive** — a crate used only via a macro, `derive`, re-export, or
  feature that machete can't see (commonly `serde`, `serde_json`, `thiserror`,
  `tokio`) -> do NOT delete it; add it to that crate's `Cargo.toml`:

  ```toml
  [package.metadata.cargo-machete]
  ignored = ["serde", "thiserror"]
  ```

Never make the job non-blocking or drop it to "resolve" a finding.

**Caching** — pick one and use it consistently:

- `Swatinem/rust-cache@<SHA>` with a per-job `key:` (aethergraph/power-monitor).
  Preferred — it understands cargo and keys on the lockfile automatically.
- Manual `actions/cache@<SHA>` over `~/.cargo/registry`, `~/.cargo/git`,
  `target/`, keyed on `hashFiles('**/Cargo.lock')` with a `restore-keys:`
  fallback (ccaudit/pgbattery). Use when you need precise control.

**Platform lanes** — run the OS that actually exercises the code:

- Pure-Linux crates: `ubuntu-latest` only.
- Crates with `cfg(target_os = "macos")` FFI (ccaudit's `getattrlistbulk`,
  power-monitor's IOReport, ane-bridge's ANE): a dedicated **`macos-latest`
  lane** (arm64) that compiles + clippies + smoke-tests the gated code, so
  regressions surface in CI, not at release. ccaudit's `macos` job documents
  exactly this.
- Feature/HW combos (aethergraph's `rdma`, `gpudirect`, `xdp_bpf`): a `matrix`
  with `fail-fast: false` and `if:`-gated steps; truly HW-bound tests go to a
  `workflow_dispatch`-gated self-hosted lane.

---

## 6. Action pinning — the convention

```yaml
# every uses: is a full commit SHA + the semver tag it resolves to.
- uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10 # v6.0.3
```

- Dependabot's `github-actions` ecosystem bumps **both** the SHA and the
  trailing comment (it parses the comment as the version). This is the only
  reason the comment is load-bearing — keep it accurate.
- For tools we `curl` (actionlint, cosign): pin the **version string** and
  verify a pinned **sha256** before executing (ccaudit/aethergraph/hasp all do
  this). Never `curl | sh` an unpinned URL.
- `dtolnay/rust-toolchain`: pin the `v1` SHA, comment `# v1`, and **always pass
  a `toolchain:` input** — the `v1` ref carries no implied channel and errors
  without it. Its `@stable`/`@nightly`/`@1.96` tags embed a default channel, but
  those are mutable tags; prefer the SHA-pinned `v1` + explicit `toolchain:`.

### Canonical pin table (known-good SHAs already in use across the org)

Use these exact pins; let Dependabot move them forward in lockstep.

| Action | Pin | Tag |
|---|---|---|
| `actions/checkout` | `df4cb1c069e1874edd31b4311f1884172cec0e10` | v6.0.3 |
| `actions/cache` | `27d5ce7f107fe9357f9df03efb73ab90386fccae` | v5.0.5 |
| `actions/upload-artifact` | `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` | v7.0.1 |
| `actions/download-artifact` | `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c` | v8.0.1 |
| `actions/setup-node` | `48b55a011bda9f5d6aeb4c2d9c7362e8dae4041e` | v6.4.0 |
| `actions/setup-python` | `a309ff8b426b58ec0e2a45f0f869d46889d02405` | v6.2.0 |
| `dtolnay/rust-toolchain` | `e97e2d8cc328f1b50210efc529dca0028893a2d9` | v1 |
| `Swatinem/rust-cache` | `c19371144df3bb44fab255c43d04cbc2ab54d1c4` | v2.9.1 |
| `EmbarkStudios/cargo-deny-action` | `bb137d7af7e4fb67e5f82a49c4fce4fad40782fe` | v2.0.20 |
| `rustsec/audit-check` | `69366f33c96575abad1ee0dba8212993eecbe998` | v2.0.0 |
| `crate-ci/typos` | `37bb98842b0d8c4ffebdb75301a13db0267cef89` | v1.47.2 |
| `taiki-e/install-action` | `15449e3094499af05d8d964a1c884208e4b8b595` | v2.81.11 |
| `docker/setup-buildx-action` | `d7f5e7f509e45cec5c76c4d5afdd7de93d0b3df5` | v4.1.0 |
| `docker/build-push-action` | `f9f3042f7e2789586610d6e8b85c8f03e5195baf` | v7.2.0 |
| `rust-lang/crates-io-auth-action` | `bbd81622f20ce9e2dd9622e3218b975523e45bbe` | v1.0.4 |
| `pypa/gh-action-pypi-publish` | `cef221092ed1bacb1cc03d23a2d87d1d172e277b` | v1.14.0 |
| `actions/attest-build-provenance` | `a2bbfa25375fe432b6a289bc6b6cd05ecd0c4c32` | v4.1.0 |

> **Needs resolution** (currently floating in aethergraph/ane-bridge-rs — pin
> to current SHA via `gh api repos/<owner>/<repo>/git/ref/tags/<tag>`):
> `astral-sh/setup-uv@v7`, `oven-sh/setup-bun@v2`. Two different dtolnay SHAs
> are in the wild (`e0818162…` in pgbattery, `e97e2d8c…` in power-monitor) —
> converge on one; the table picks `e97e2d8c…` (commented `# v1`).

---

## 7. Dependabot standard

**Problem today:** ane-bridge-rs has none; cadence varies (daily vs weekly);
limits vary (5/10/99); some group, some don't; labels are inconsistent.

**Standard** (`.github/dependabot.yml`):

- **Daily** schedule (security-relevant; PRs are grouped so volume stays low).
- One `cargo` entry at `/`, one `github-actions` entry at `/`, plus a `docker`,
  `npm`, or `pip` entry **per directory that actually has that manifest**
  (ccaudit declares 5 because it ships npm + PyPI wrappers).
- **Group** updates so you get one PR per ecosystem per day, not N. Separate
  the `github-actions` group from `cargo` (different review posture).
- Consistent labels: `dependencies` + an ecosystem tag (`rust`, `actions`,
  `docker`, `npm`, `python`).
- `open-pull-requests-limit: 99` (grouping keeps actual PR count tiny; the high
  limit just avoids Dependabot silently dropping a needed update).
- Use `ignore` with a comment when a major bump is deliberately deferred
  (ccaudit defers `bincode` 3.0 — on-disk format migration).

Canonical baseline (extend with docker/npm/pip per repo):

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: "/"
    schedule:
      interval: daily
    open-pull-requests-limit: 99
    commit-message:
      prefix: "deps(rust)"
    labels: ["dependencies", "rust"]
    groups:
      cargo:
        patterns: ["*"]
        update-types: ["patch", "minor", "major"]

  - package-ecosystem: github-actions
    directory: "/"
    schedule:
      interval: daily
    open-pull-requests-limit: 99
    commit-message:
      prefix: "deps(actions)"
    labels: ["dependencies", "actions"]
    groups:
      actions:
        patterns: ["*"]
```

Pair Dependabot with repo-level **Dependabot security updates** and
**vulnerability alerts**, enabled via `apply-settings.sh` (§10).

---

## 8. Supply-chain: `deny.toml` + scheduled re-audit

**Problem today:** hasp, power-monitor, ane-bridge-rs have **no `deny.toml`**.
Only ccaudit runs a *scheduled* re-audit; everyone else only checks at PR/release
time, so an advisory disclosed against an already-shipped dep goes unnoticed
until the next push.

**Standard:**

1. Commit a `deny.toml` (license allowlist, banned crates, advisory policy,
   source allowlist) — copy pgbattery/aethergraph/ccaudit's and trim.
2. CI runs `cargo deny check` + `cargo audit` (the `deny`/`audit` jobs in §5).
3. Add **`supply-chain.yml`** — a scheduled (weekly, off-peak-minute cron) +
   `workflow_dispatch` job that re-audits the **committed lockfile** against the
   live advisory DB. This catches *newly disclosed* advisories with no version
   bump to trigger CI. ccaudit's is the template:

```yaml
name: Supply chain
on:
  schedule:
    - cron: "17 7 * * 1"   # Mon 07:17 UTC — offset off the busy top-of-hour
  workflow_dispatch:
permissions:
  contents: read
concurrency:
  group: ${{ github.workflow }}
  cancel-in-progress: true
jobs:
  audit:
    runs-on: ubuntu-latest
    timeout-minutes: 15
    steps:
      - uses: actions/checkout@<SHA>  # v6.0.3
        with: { persist-credentials: false }
      - name: Install Rust
        run: rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --no-self-update
      - name: Install cargo-audit + cargo-deny (version-pinned)
        run: |
          cargo install cargo-audit@0.21.2 --locked
          cargo install cargo-deny@0.19.5 --locked
      - run: cargo audit --deny warnings
      - run: cargo audit --deny yanked
      - run: cargo deny check --config deny.toml
```

> When `cargo audit` ignores an advisory, mirror the exact RUSTSEC id in
> `deny.toml`'s `[advisories.ignore]` **and** as a `cargo audit --ignore` flag
> (cargo-audit has no shared config) — aethergraph documents this duplication.

---

## 9. Release hardening (repos that ship artifacts)

`ccaudit`'s `release.yml` is the gold standard; `hasp`'s is a strong subset.
Apply the tier that matches what the repo distributes.

**Tier 0 — every release.yml:**
- Trigger only on `push: tags: ['v*']`; `permissions: {}` top-level, per-job grants.
- Build matrix of real targets (`fail-fast: false`); cross-compile linux-arm64
  with `gcc-aarch64-linux-gnu`; static musl where it buys portability (hasp).
- Verify the tag is **annotated** (reject lightweight); verify
  `Cargo.toml` version == tag (ccaudit's `publish-crates` guard).
- Generate a `.sha256` per artifact; `--generate-notes` on the GitHub release.

**Tier 1 — signing & provenance (anything users install):**
- **Sigstore keyless** signing via `cosign sign-blob` (cosign fetched by version
  + sha256, never unpinned). Verify the signature in the same step.
- **SLSA build provenance** via `actions/attest-build-provenance@<SHA>` (needs
  `id-token: write` + `attestations: write`), or `gh attestation create`.
- **SBOM**: `cargo sbom --output-format spdx_json_2_3` attached to the release
  (hasp).

**Tier 2 — registry publishing without secrets:**
- **OIDC Trusted Publishing** — no long-lived tokens:
  - crates.io: `rust-lang/crates-io-auth-action@<SHA>` -> short-lived token.
  - npm: `npm ≥ 11.5.1` + `npm publish --provenance` (token exchanged from OIDC).
  - PyPI: `pypa/gh-action-pypi-publish@<SHA>`.
- Gate each publish behind a **deployment environment** (`environment:
  crates-publish` etc.) with a reviewer + wait timer + tag policy (§10).

**Tier 3 — reproducibility & integrity (ccaudit's extras, recommended):**
- `SOURCE_DATE_EPOCH=0` + `RUSTFLAGS: --remap-path-prefix=$workspace=.` for
  path-independent, time-independent builds (hasp).
- Build-twice-and-compare reproducibility check.
- **Block network egress** (`iptables`) for the compile step after
  `cargo fetch --locked` — proves the build pulls nothing at compile time.
- Binary sanity gates: size bounds, reject embedded `/home/runner` / `.debug_info`
  paths, **dynamic-dep allowlist** via `ldd` (reject anything outside libc/libm/…).
- Cross-job checksum verification before publish.

**Signing-key note:** prefer keyless Sigstore (no key to manage). pgbattery's
minisign scaffold requires a `MINISIGN_SECRET_KEY` repo secret and an embedded
public key — migrate it to cosign keyless unless the `upgrade` self-updater
specifically needs minisign verification offline.

---

## 10. Governance as code (branch protection, environments, merge policy)

**Problem today:** only ccaudit encodes this. Everyone else's branch protection
(if any) is clicked into the UI and drifts.

**Standard:** copy ccaudit's `.github/settings.json` + `.github/apply-settings.sh`
into every repo and edit the `REPO` slug + the required-check `contexts` to match
that repo's job names. The script is **idempotent**: it applies, then re-fetches
and asserts `actual == desired`, exiting non-zero on any drift — so it doubles as
a CI/cron audit.

`settings.json` encodes:

- **Branch protection on the default branch:** required status checks
  (`contexts` = the job names from §5), `strict` (up-to-date before merge),
  `required_linear_history`, no force-push, no deletion,
  `required_conversation_resolution`.
- **Deployment environments** (one per publish target): `wait_timer`, required
  `reviewer_logins`, and `tag_patterns` (`v*.*.*`) so only release tags deploy.
- **Merge policy:** squash-only, `delete_branch_on_merge`, `allow_auto_merge`.
- **Security & analysis:** secret scanning + push protection; the script also
  flips on `vulnerability-alerts` + `automated-security-fixes`.

```jsonc
{
  "branch_protection": {
    "main": {
      "required_status_checks_contexts": ["<job names from §5>"],
      "required_status_checks_strict": true,
      "enforce_admins": false,
      "required_linear_history": true,
      "allow_force_pushes": false,
      "allow_deletions": false,
      "required_conversation_resolution": true
    }
  },
  "environments": [
    { "name": "crates-publish", "wait_timer": 10, "reviewer_logins": ["electricapp"], "tag_patterns": ["v*.*.*"] }
  ],
  "repo": {
    "allow_squash_merge": true, "allow_merge_commit": false, "allow_rebase_merge": false,
    "delete_branch_on_merge": true, "allow_auto_merge": true
  },
  "security_and_analysis": { "secret_scanning": "enabled", "secret_scanning_push_protection": "enabled" }
}
```

**Branch naming:** standardize on `main`. `ane-bridge-rs` is on `master` —
rename to `main` (`gh api -X POST repos/electricapp/ane-bridge-rs/branches/master/rename -f new_name=main`)
and update its workflow `branches:` filters, or keep `master` and document it as
a deliberate exception. (Recommendation: rename for consistency.)

---

## 11. `actionlint` config

When a workflow targets a self-hosted runner with a custom label, actionlint
flags the label as unknown unless declared (aethergraph's `rdma` lane):

```yaml
# .github/actionlint.yaml
self-hosted-runner:
  labels:
    - <your-self-hosted-label>
```

Repos with no self-hosted runners don't need this file — but they still need the
**actionlint job** in `ci.yml` (§5).

---

## 12. Advanced / optional menu (apply by risk profile)

These are not required everywhere, but they are the org's proven techniques —
adopt the ones that fit each repo's failure modes.

| Technique | Source repo | When to use |
|---|---|---|
| **feature-matrix clippy** | ccaudit | any repo with cargo features |
| **fuzz-clippy** (lint an out-of-workspace `fuzz/` crate every PR) | pgbattery | repos with a separate fuzz crate that drifts |
| **`cargo-fuzz` short-budget job** (60s/target) + scheduled deep soak | aethergraph, ane | parsers, decoders, untrusted input |
| **miri** (`-Zmiri-strict-provenance`) | aethergraph | `unsafe`, raw pointers, custom allocators |
| **loom** (exhaustive interleavings) | aethergraph, ane | lock-free / hand-rolled sync |
| **macOS heap guards** (`MallocGuardEdges`/`Scribble`/`CheckHeap`) | ane | FFI / C interop on macOS, no nightly needed |
| **ThreadSanitizer + `leaks`** | ane | C/Obj-C FFI, data-race & leak hunting |
| **clang-format + clang-tidy** (`--Werror`) | ane | repos with a C/Obj-C component |
| **self-scan / dogfood** (run the tool on its own repo, must pass clean) | hasp | any repo that *is* a code/security tool |
| **`action-diff`** (post upstream changelog on Dependabot action-bump PRs) | hasp | reduces blind-merging of action bumps |
| **`ratelimit.yml`** (`workflow_dispatch` GH API budget check) | hasp | repos that hit the GitHub API in tests/CI |
| **`nightly-bench` + criterion summary** | aethergraph | perf-sensitive libs; track regressions |
| **coverage** (`cargo-llvm-cov`) | ane | libraries wanting a coverage gate/badge |
| **`$GITHUB_STEP_SUMMARY` rollups** | aethergraph | multi-job matrices — one glanceable table |
| **per-commit `main` signal** (no cancel on main) | pgbattery | repos where CI bisection matters |

---

## 13. Per-repo implementation checklist

Ordered by blast radius. DONE = already compliant, TODO = action needed.

### `ane-bridge-rs` (most drift)
- TODO: **Add `dependabot.yml`** (cargo + github-actions). *Currently none.*
- TODO: **Pin all actions to SHAs** (`@v4`/`@v5`/`@stable` -> table pins).
- TODO: **Add `concurrency` + `permissions: {}` + `timeout-minutes`** to every workflow.
- TODO: `persist-credentials: false` on checkout.
- TODO: Add `deny.toml` + `deny`/`audit` jobs + `supply-chain.yml`.
- TODO: Add `actionlint` job; add `settings.json` + `apply-settings.sh`.
- TODO: Decide `master` -> `main`.
- DONE: Keep its excellent sanitizer/heap-guard/loom/TSAN lanes (§12 — these are a *model* for other FFI repos).

### `aethergraph`
- TODO: **Pin all actions to SHAs** (`@v6`/`@stable`/`@nightly`, `setup-uv@v7`, `setup-bun@v2`).
- TODO: Add top-level `permissions:` + per-job least privilege + `persist-credentials: false`.
- TODO: Add `timeout-minutes` to the jobs missing it; add an **actionlint job** (config already present).
- TODO: Add `settings.json` + `apply-settings.sh`.
- DONE: Dependabot present (consider daily+grouped to match standard); deny.toml present; miri/loom/fuzz exemplary.

### `power-monitor`
- TODO: Add top-level `permissions:` + per-job grants + `persist-credentials: false`.
- TODO: Add `deny.toml` + `deny`/`audit` jobs + `supply-chain.yml`.
- TODO: Add `actionlint` job; add `settings.json` + `apply-settings.sh`.
- TODO: Add `typos` + `cargo-machete` checks.
- DONE: SHA-pinned; concurrency; dependabot; macOS lane; doc-warnings-as-errors.

### `hasp`
- TODO: Add `deny.toml` + `cargo deny check` (currently audit-only).
- TODO: Add `actionlint` job (ironic gap for a workflow-security tool) — or wire its own `self-scan` to enforce it.
- TODO: Add `settings.json` + `apply-settings.sh`.
- DONE: SHA-pinned; `permissions: {}`; dependabot; self-scan; action-diff; SBOM; signed release.

### `pgbattery`
- TODO: Tighten `permissions:` (top-level deny-all + per-job grants; `persist-credentials: false`).
- TODO: Add `actionlint` job; converge dtolnay to the `@v1` SHA **and add explicit
  `toolchain:` inputs** (`1.96` for the main jobs, `nightly` for the fuzz lane) —
  the old `# 1.96`/`# nightly` were dtolnay channel tags supplying the toolchain
  implicitly, so dropping them without an input breaks every Rust job.
- TODO: Add `settings.json` + `apply-settings.sh`; add `supply-chain.yml` (deny.toml exists).
- TODO: Migrate release signing minisign -> cosign keyless (or document why minisign stays).
- DONE: SHA-pinned; broad CI; grouped dependabot; concurrency main-nuance.

### `ccaudit` (reference — keep as the template)
- DONE: Compliant across the board. Only gap: add a committed `.github/actionlint.yaml`
  if/when it gains a self-hosted runner (none today). Use it as the copy source.

---

## 14. Rollout plan

1. **Land this doc** + promote it to an `electricapp/.github` org repo (org-level
   default `dependabot.yml`, issue templates, and this standard live there;
   individual repos inherit/override).
2. **Sweep 1 — zero-risk, mechanical:** add/normalize `dependabot.yml`
   everywhere; SHA-pin every floating action (aethergraph, ane). These can't
   break a build and immediately close the biggest supply-chain gaps.
3. **Sweep 2 — hardening:** add `permissions`/`concurrency`/`timeout-minutes`/
   `persist-credentials:false` + the `actionlint` job to every workflow. Verify
   each repo's CI still goes green.
4. **Sweep 3 — supply chain:** add `deny.toml` + `supply-chain.yml` to hasp,
   power-monitor, ane.
5. **Sweep 4 — governance:** drop `settings.json` + `apply-settings.sh` into each
   repo, edit the `contexts`, run the script once (it asserts no drift).
6. **Sweep 5 — release parity:** bring each shipping repo up to the §9 tier it
   needs; migrate pgbattery to cosign keyless.
7. Verify with **hasp** itself: `hasp --paranoid` against every repo's
   `.github/workflows` should pass clean.
```
