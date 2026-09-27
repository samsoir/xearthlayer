# CI/CD Pipeline and Merge Strategy

How XEarthLayer builds, tests and releases itself on GitHub Actions.

This document describes the **system**. For the step-by-step procedure to cut a
release, see the [Application Release Runbook](app-release-runbook.md). For publishing
*scenery packages* (a separate pipeline), see
[GitHub Releases Publishing](github-releases-publishing.md).

## Branch Model

Two long-lived branches, one per release channel:

| Branch | Channel | Version format | Purpose |
|--------|---------|----------------|---------|
| `main` | Stable | `X.Y.Z` | Production releases — what most users run |
| `develop/X.Y.Z` | Unstable | `X.Y.Z-dev.N` | The next release, including breaking changes |

Work branches are named for the *kind* of change; the base branch selects the
*channel*:

- `feature/<name>` — new functionality
- `bugfix/<issue>-<description>` — bug fixes
- `hotfix/<description>` — urgent fix to stable (branch from `main`)
- `chore/<description>` — tooling, docs, maintenance
- `release/<version>` — version bump and changelog, immediately before a tag

**Fixes flow one way.** Land fixes on `main`, then forward-merge `main` into the
unstable branch. The unstable branch is never merged back into `main` until the whole
line is promoted as a stable release. This keeps `main` releasable at any moment.

## Workflows

| Workflow | Triggers | Purpose |
|----------|----------|---------|
| `ci.yml` | push to `main`, `develop/**`, `feature/**`, `bugfix/**`, `hotfix/**`, `chore/**`; PRs to `main` and `develop/**` | Verification on every change |
| `release-test.yml` | push and PRs on `release/*` | Same verification for release-prep branches |
| `release.yml` | push of a `v*` tag; manual dispatch | Build, package, publish a GitHub Release |
| `website-sync.yml` | push to `main` touching `version.json` | Notify the website repo of a new release |
| `close-issues-on-develop-merge.yml` | a merged PR into `develop/**` | Close the issues the PR body says it closes |

`ci.yml` and `release-test.yml` each run two jobs — `Verify` (Linux) and
`Verify (macOS)`.

### Issue closing on develop merges

GitHub honours closing keywords only when a commit reaches the repository's
default branch. Every PR here targets a `develop/*` integration branch, so
without help an issue whose work is finished and merged stays open until the
release merge to `main`, weeks or months later. A milestone then reports a
fraction of the work actually done. No setting changes this.

`close-issues-on-develop-merge.yml` supplies what GitHub will not. On a merged
PR into `develop/**` it reads closing keywords from the **PR body**, then
comments on and closes each issue named. So the PR body is load-bearing: write
`Closes #123` there for the issue to close on merge to `develop`. Anything
weaker, such as `Related to #123`, deliberately does not close.

Consequences worth knowing:

- **The milestone tracks development, not shipping.** An issue closes when its
  work lands on `develop/*`. Whether it has shipped is answered by the release
  tag. This is the intended trade: the alternative leaves the milestone
  useless for the entire development cycle.
- **It is idempotent.** An already-closed issue is skipped, so a re-merge or a
  reverted-then-remerged PR does not comment twice.
- **A PR number is never closed.** The issues API also serves pull requests, so
  the workflow skips anything with a `pull_request` field. `Closes #274` where
  274 is a PR is a no-op.
- **Cross-repository syntax does not work.** `Fixes owner/repo#14` is ignored;
  the workflow only closes issues in this repository.
- **It uses `pull_request_target`.** A `pull_request` event raised from a fork
  receives a read-only token, so contributor PRs could never close anything.
  `pull_request_target` runs in the base repository's context with a writable
  token. That trigger is dangerous when a workflow checks out and runs the PR's
  code; this one never checks anything out, and the only PR-controlled values
  it touches are the title and body, passed through the environment rather than
  interpolated into the script.

## The Release Job Graph

`release.yml` is a DAG with two independent entry points: `verify` (Linux) and
`verify-macos` (macOS) both start directly off the tag push and run in parallel —
neither gates the other. `publish` fans in from every packaging job, so a failure in
any of them prevents the release from being created.

```
┌──────────┐                                    ┌────────────┐
│  verify  │ (ubuntu; also emits release_tag/    │verify-macos│ (macos-15)
│          │  version/prerelease)                │            │
└────┬─────┘                                    └─────┬──────┘
     │                                                 │
     ├──────────────┬──────────────┐                   │
     │              │              │                   │
┌────▼───────┐┌──────▼─────┐┌──────▼───────┐     ┌──────▼───────┐
│build-binary││ build-rpm  ││ prepare-aur  │     │package-macos │
│  (ubuntu)  ││stable only ││ stable only  │     │ (macos-15)   │
└──┬──────┬──┘└──────┬─────┘└──────┬───────┘     └──────┬───────┘
   │      │          │             │                    │
┌──▼───┐┌─▼─────────┐│             │                    │
│pkg-  ││pkg-deb    ││             │                    │
│linux ││stable only││             │                    │
└──┬───┘└─────┬──────┘│             │                    │
   │          │        │             │                    │
   └──────────┴────────┴─────────────┴────────────────────┘
                              │
                        ┌─────▼─────┐
                        │  publish  │
                        └───────────┘
```

`package-macos` needs **both** `verify` and `verify-macos` (`needs: [verify,
verify-macos]`) — it is the only packaging job with two independent parents, since it
sits at the point where the Linux and macOS verification paths converge. Every other
packaging job needs only `verify`, directly or transitively through `build-binary`.
The diagram omits the direct `verify → package-macos` edge for readability; the `needs:`
list above is the source of truth.

`publish` uses `!cancelled()` plus explicit per-dependency result checks rather than
relying on `needs:` alone:

```yaml
needs: [verify, package-linux, package-macos, package-deb, build-rpm, prepare-aur]
if: >-
  ${{ !cancelled()
  && needs.verify.result == 'success'
  && needs.package-linux.result == 'success'
  && needs.package-macos.result == 'success'
  && needs.package-deb.result != 'failure'
  && needs.build-rpm.result != 'failure'
  && needs.prepare-aur.result != 'failure' }}
```

A **skipped** dependency would otherwise skip `publish` too, and the stable-only jobs
are skipped on every pre-release tag. Jobs that always run are checked with
`== 'success'`; jobs that may legitimately be skipped are checked with `!= 'failure'`.

**When adding a packaging job, add it to both `needs:` and this `if:`.** Adding it to
`needs:` alone does not gate the release.

## Platform Support Tiers

Linux and macOS are both **Tier 1**: a failure is meant to block a PR merge, and does
abort release publication.

The two halves are enforced by different mechanisms, and only one is live today:

| Gate | Linux (`Verify`) | macOS (`Verify (macOS)`) |
|------|------------------|--------------------------|
| Aborts release publication | ✅ live — `publish` requires `package-linux` | ✅ live — `publish` requires `package-macos` |
| Blocks a PR merge | ✅ live on `main` (required context) | ⏳ pending branch protection |

Release publication is gated for both: `publish` names each packaging job in its
`needs:` *and* its `if:` expression (see the job graph above). Merge blocking, by
contrast, is **branch protection** — a repository setting, not a workflow. `main`
currently requires only the `Verify` context, and `develop/0.5.0` is unprotected, so a
red `Verify (macOS)` is visible but not merge-blocking. Adding it as a required check
is deliberately deferred until the macOS port merges: protecting the branch while that
job is red would block every merge, including the port itself. See
[Status Checks and Branch Protection](#status-checks-and-branch-protection).

| Platform | Arch | CI runner | Notes |
|----------|------|-----------|-------|
| Linux | x86_64 | `ubuntu-latest` | Primary development platform |
| macOS | arm64 (Apple Silicon) | `macos-15` | Intel is untested and not built |

macOS is Apple Silicon only. GitHub does offer native Intel runners
(`macos-15-intel`), but no maintainer flight-tests XEarthLayer on an Intel Mac, and a
blocking gate on unvalidated hardware is worse than no gate.

The macOS runner is pinned to `macos-15`, not `macos-latest`. That label has moved
across both OS versions and CPU architectures; with a pinned `fuse3` fork and an
ISPC-backed encoder, a silent bump would surface as an unexplained failure on an
unrelated PR.

**macOS jobs install no dependencies.** The pinned `fuse3` fork is pure Rust — no
`build.rs`, no `pkg-config`, no `cc` — and `xearthlayer/build.rs` already emits `-lc++`
on macOS. macFUSE is required only at runtime.

## Release Channels

| Channel | Tag | GitHub Release | Assets |
|---------|-----|----------------|--------|
| Stable | `vX.Y.Z` | Latest | Linux tarball, macOS tarball, `.deb`, `.rpm`, AUR |
| Pre-release | `vX.Y.Z-dev.N`, `-alpha.N`, `-beta.N`, `-rc.N` | Pre-release, never "Latest" | Linux tarball, macOS tarball |

`verify` classifies the tag and emits a `prerelease` output; `package-deb`,
`build-rpm` and `prepare-aur` are gated on it.

**Why those three are stable-only — do not "fix" this.** It is not policy, it is a
format constraint. RPM uses `-` to delimit `Version` from `Release`, so
`Version: 0.5.0-dev.1` is malformed. Arch's `pkgver` forbids hyphens outright. Neither
can represent a pre-release version at all. Tarballs have no such constraint, which is
why both ship on every channel.

## Status Checks and Branch Protection

`main` requires the status check context **`Verify`** — an exact string match.

This constrains the pipeline's shape. GitHub Actions names matrix jobs
`Verify (ubuntu-latest)`, `Verify (macos-15)`. Converting the `verify` job to a matrix
would mean the required context `Verify` never reports again, and **merges to `main`
would hang forever** waiting for a check that no longer exists.

That is why macOS is a separate, explicitly-named job rather than a matrix dimension.
Never rename or matrix-ify `verify`.

Required contexts:

| Branch | Required checks |
|--------|-----------------|
| `main` | `Verify` |
| `develop/0.5.0` | `Verify`, `Verify (macOS)` — planned, pending the macOS port merge |

## What CI Cannot Verify

Some behaviour is structurally untestable on hosted runners. Know the gaps.

**macFUSE mounts.** macFUSE is a kernel extension. Loading a third-party kext on Apple
Silicon requires booting to Recovery, downgrading to Reduced Security, enabling user
management of kexts, and rebooting. Hosted runners permit none of that.

As of this writing, `xearthlayer/tests/macfuse_smoke.rs` does not yet exist on this
branch; it ships with the macOS port (PR #202, still open against `develop/0.5.0`).
Until that merges, `make verify-macos`'s final step has no test to run — the platform
and macFUSE guards ahead of it still work today.

Once present, the test is designed to be `#[ignore]`d for exactly this reason: CI
**compiles** it — which catches API and type breakage under `-Dwarnings` — but never
**runs** it, because macFUSE is a kext and hosted runners cannot load one.

**Live FUSE mounts on Linux** are equally uncovered: `make verify` runs
`cargo test` without `--ignored`, so `#[ignore]`d tests are skipped on both platforms.

The human gates that close these holes:

| Command | Covers | When |
|---------|--------|------|
| `make verify` | fmt, clippy, unit and integration tests | Every commit; both CI platforms |
| `make verify-macos` | `make verify` plus live macFUSE mount tests | Before promoting a release to stable, on real Apple Silicon |
| `make integration-tests` | `#[ignore]`d integration tests | Manually, on real hardware |

## Version Propagation

Five files carry the version. `make bump-version VERSION=<semver>` is the single tool
that updates them, and it writes **two different forms**:

| File | Form | Example for `0.5.0-dev.1` |
|------|------|---------------------------|
| `Cargo.toml` | Full semver | `0.5.0-dev.1` |
| `Cargo.lock` | Full semver | `0.5.0-dev.1` |
| `version.json` | Full semver | `0.5.0-dev.1` |
| `pkg/rpm/xearthlayer.spec` | Base version | `0.5.0` |
| `pkg/arch/PKGBUILD` | Base version | `0.5.0` |

The base form exists for the hyphen constraint described under Release Channels.

`version.json`'s RPM asset filename also embeds the Fedora release tag the CI
container resolves to (e.g. `fc44`). `bump-version` cannot know that tag in advance, so
it reads the current one back out of the existing `version.json` and carries it
forward into the new filename. If it cannot extract a valid `fcNN` tag — a missing or
malformed `.assets.rpm.filename` — it **aborts with an error** rather than silently
writing a corrupted filename. See the runbook's RPM 404 troubleshooting entry for the
scenario (a Fedora base-image upgrade) this guards against.

`version.json` `release_date` is **not** automated. Excluding it preserves a useful
invariant: running `make bump-version VERSION=<current>` on a synchronised tree
produces no diff, so the target doubles as a drift detector.

```bash
make bump-version VERSION=$(grep '^version' Cargo.toml | cut -d'"' -f2)
git diff --stat   # empty == all layers agree
```

On `develop`, the preview-release path (see the runbook's **Unstable / Preview
Release**) deliberately hand-edits only `Cargo.toml`, so `version.json` intentionally
lags behind it between preview bumps — the drift check is therefore meaningful on
`main` and on `release/*` branches, not on `develop` itself.

**Why drift used to go unnoticed.** `release.yml` rewrites the RPM `Version:` at build
time and generates the AUR `PKGBUILD` from a heredoc, ignoring `pkg/arch/PKGBUILD`
entirely. Both files are inert in CI while remaining live in the Makefile path — so
`pkg/rpm/xearthlayer.spec` sat at `0.2.5` and `pkg/arch/PKGBUILD` at `0.2.0` while CI
published correct artifacts, and `make pkg-rpm` quietly produced an RPM labelled
`0.2.5`. Both files have since been swept to `0.5.0`; `make bump-version` and its
idempotence check above exist to keep it from recurring.

## Secrets and External Repositories

| Secret | Used by | Notes |
|--------|---------|-------|
| `GITHUB_TOKEN` | `release.yml` | Automatic; creates the release and uploads assets |
| `AUR_SSH_PRIVATE_KEY` | `release.yml` | Optional. Absent, the AUR files are attached to the release for manual submission |
| `WEBSITE_DISPATCH_TOKEN` | `website-sync.yml` | 90-day PAT. **Expiry is silent** — the website then lags by up to 24h until the daily fallback cron runs |

### The `version.json` contract

`version.json` on `main` is the interface to `samsoir/xearthlayer-website`. The website's
`sync-version.yml` fetches it, extracts specific keys with `jq`, and rewrites its own
`data/release.json`.

It reads exactly `deb`, `rpm` and `tarball` and ignores everything else — `aur` is
already carried in `version.json` and silently dropped. Adding a key is therefore safe,
but a new asset will not appear on the website until that workflow is taught to read it.

Website updates fire on a push to `main` that changes `version.json`, gated on the
commit message indicating a `release/*` branch merge — so merging a release PR is what
triggers the site, not the release workflow itself.
