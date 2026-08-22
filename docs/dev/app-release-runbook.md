# Application Release Runbook

This runbook documents the complete workflow for releasing new versions of XEarthLayer, including the golden path and troubleshooting for common issues.

## Overview

Releases are automated via GitHub Actions when a version tag (`v*`) is pushed. The workflow:
1. Runs `make verify` (format, lint, test)
2. Builds the release binary once
3. Packages for multiple platforms (Linux tarball, Debian, RPM, AUR) — **stable
   releases only**; pre-release tags ship the Linux tarball alone
4. Creates a GitHub Release with all assets — a **pre-release** (not marked "Latest")
   for unstable tags (`-dev.N` / `-alpha.N` / `-beta.N` / `-rc.N`), a normal release
   for stable `X.Y.Z` tags
5. For **stable** releases only: when the `release/*` PR is merged to `main`,
   `website-sync.yml` reads `version.json` and notifies the website

XEarthLayer runs two release channels in parallel — a stable line on `main` and an
unstable line on `develop/0.4.7`. The **Stable Release** golden path below is the common
case; **Unstable / Preview Release**, **Hotfix Release**, and **Promoting
`develop/0.4.7` to Stable** follow it.

## Branch Model & Release Channels

XEarthLayer ships on two parallel channels, each anchored to a long-lived branch:

| Channel | Branch | Version | Example | GitHub Release | Assets | version.json / website |
|---------|--------|---------|---------|----------------|--------|------------------------|
| **Stable** | `main` | `X.Y.Z` | `0.4.6` | Latest | tarball + `.deb` + `.rpm` + AUR | ✅ updated |
| **Unstable** | `develop/0.4.7` | `X.Y.Z-dev.N` | `0.4.7-dev.3` | Pre-release (`--latest=false`) | tarball only | ❌ skipped |

As the unstable line matures toward release, the pre-release identifier progresses
`-dev.N` → `-alpha.N` → `-beta.N` → `-rc.N`, and finally drops the suffix when the branch
is promoted to a stable `X.Y.Z` on `main`. All four pre-release forms are treated
identically by the release workflow (published as GitHub pre-releases, Linux tarball
only).

```
main (stable, v0.4.x)
  ├── hotfix/*   ──PR──▶ main ──tag──▶ vX.Y.(Z+1)     # stable patch
  ├── feature/* ──PR──▶ main ──tag──▶ vX.Y.Z          # stable release
  │                       │
  │                       └──────────── merge forward ───────────┐
  │                                                               ▼
  └── develop/0.4.7 (unstable, long-lived) ◀──────────────────────┘
        ├── feature/* ──PR──▶ develop/0.4.7
        └── ──tag──▶ v0.4.7-dev.N · -alpha.N · -beta.N · -rc.N     # preview
                          │
        release-ready:    └─ release/0.4.7 ──PR──▶ main ──tag──▶ v0.4.7   # promote
```

**The one-way rule.** Fixes land on `main` first, then forward-merge into
`develop/0.4.7`. The develop branch is **never** merged directly back into `main`;
promotion happens through a `release/*` branch cut from develop (see **Promoting
`develop/0.4.7` to Stable**). This keeps `main` releasable at any moment without dragging
unfinished 0.4.7 work into a stable release.

> **Version collision when develop targets the next patch.** `develop/0.4.7` reserves
> `0.4.7` for its own promotion. Because that is the *immediate* next number, an urgent
> **Hotfix Release** cut from `main` cannot also be `0.4.7`. If a hotfix must ship before
> the develop line promotes, give it the next patch (`0.4.7` → the hotfix becomes the
> stable release and develop re-targets `0.4.8`), or fold the fix into the develop line
> and promote once. Reserve a develop line for a further-out minor (e.g. `develop/0.5.0`)
> when you expect stable patches to keep flowing in parallel.

## Prerequisites

- Write access to the repository
- `gh` CLI authenticated (`gh auth status`)
- Clean working tree on the branch you're releasing from — `main` for a stable release or
  hotfix, `develop/0.4.7` for an unstable preview

## Stable Release (Golden Path)

> This is the standard path: a stable `X.Y.Z` release cut from `main`. For an unstable
> preview from `develop/0.4.7`, see **Unstable / Preview Release**; for an urgent fix to
> the stable line, see **Hotfix Release**.

### Step 1: Prepare the Release

```bash
# Ensure you're on main and up to date
git checkout main
git pull origin main

# Verify everything passes
make pre-commit
```

### Step 2: Update Version, Changelog, and version.json

```bash
# Update workspace version in Cargo.toml
# Edit: [workspace.package] version = "X.Y.Z"

# Update CHANGELOG.md
# - Add new version header: ## [X.Y.Z] - YYYY-MM-DD
# - Document all changes under Added/Changed/Fixed/Removed
# - Update comparison links at bottom

# Update version.json
# - version, tag, release_date
# - Asset filenames (replace old version with X.Y.Z)
# - download_base_url
```

> **⚠ RPM filename drift:** The RPM asset name embeds the Fedora base image
> version that the CI workflow uses (e.g. `fc43`, `fc44`). When CI's Fedora
> base image upgrades, the actual RPM that gets published won't match what
> `version.json` declares — the website then resolves a 404 for the RPM
> download URL.
>
> Catch this **after** the release workflow publishes (Step 5) but
> **before** merging the release PR (Step 6). Compare:
>
> ```bash
> gh release view vX.Y.Z --json assets --jq '.assets[].name'
> ```
>
> against the filenames in your `version.json`. If the RPM filename differs,
> update `version.json` on the release branch and push the fix before
> merging — that way the corrected `version.json` lands on main in the same
> merge that the website-sync workflow consumes.

### Step 3: Create Release Branch and PR

> The branch **must** be named `release/X.Y.Z`. `website-sync.yml` only fires when the
> merge commit message contains `release/`, so any other prefix silently skips the
> website update.

```bash
# Create release branch
git checkout -b release/X.Y.Z

# Commit changes
git add Cargo.toml Cargo.lock CHANGELOG.md version.json
git commit -m "Release vX.Y.Z"

# Push and create PR
git push -u origin release/X.Y.Z
gh pr create --base main --title "Release vX.Y.Z" --body "Release vX.Y.Z"
```

### Step 4: Wait for CI, Then Create Tag

**IMPORTANT**: Create the tag BEFORE merging the PR to avoid version.json push conflicts.

```bash
# After CI passes on the PR, create and push the tag
git tag vX.Y.Z
git push origin vX.Y.Z
```

### Step 5: Monitor Release Workflow

```bash
# Watch the release workflow
gh run watch --repo samsoir/xearthlayer

# Expected jobs (all should succeed):
# ✓ Verify (~3-4 min)
# ✓ Build Release Binary (~4 min)
# ✓ Prepare AUR Package (~5 sec)
# ✓ Build RPM Package (~7-8 min)
# ✓ Package Linux Binary (~10 sec)
# ✓ Package Debian Package (~1 min)
# ✓ Publish Release (~15 sec)
```

### Step 6: Reconcile Asset Filenames, Then Merge PR

**CRITICAL**: Only merge the PR AFTER the release workflow completes successfully.

```bash
# Verify release was published, capture asset names
gh release view vX.Y.Z --json assets --jq '.assets[].name'
```

**Compare the published asset names against `version.json` filenames.** The
RPM in particular can drift when CI's Fedora base image upgrades (e.g.
`fc43` → `fc44`). If anything is mismatched:

```bash
# On the release branch, fix version.json to match the actual filenames
git checkout release/X.Y.Z
# edit version.json
git add version.json
git commit -m "Release vX.Y.Z: align version.json asset filenames with built artifacts"
git push
# wait for CI green on the new commit
```

This must happen **before merge** so the corrected `version.json` lands on
main with the merge SHA the website-sync workflow consumes. If you discover
the drift after merge, you'll need a follow-up PR.

```bash
# Merge the PR
gh pr merge --merge --delete-branch
```

### Step 7: Verify Website Updated

```bash
# version.json is now on main (merged with the release PR)
# Verify it shows the correct version:
gh api repos/samsoir/xearthlayer/contents/version.json --jq '.content' | base64 -d | jq .version

# Verify website shows new version (may take 1-2 minutes for CDN)
curl -s https://xearthlayer.app | grep -o 'v[0-9]\+\.[0-9]\+\.[0-9]\+'
```

## CHANGELOG Convention

`CHANGELOG.md` keeps a single `## [Unreleased]` section at the top ([Keep a
Changelog](https://keepachangelog.com/) format), under the usual
Added / Changed / Fixed / Removed groups.

**The changelog is compiled when a release is cut, not per-PR.** A feature PR
leaves `CHANGELOG.md` alone; an empty `## [Unreleased]` section on a feature
branch is correct and should not be flagged in review. At release-cut time the
entries are written in one pass from the PRs merged since the last release
tag:

```bash
git log v<last>..HEAD --merges --oneline    # the PRs to describe
```

Two reasons this beats writing entries as they land: every feature PR would
otherwise conflict on the same few lines, and — more importantly — the
changelog describes *the product's* history rather than the repository's. One
user-visible fix that took two PRs to land (a follow-up carrying commits that
missed the first merge, say) is **one** changelog entry, and that is only
visible in hindsight.

- Entries accumulate under `## [Unreleased]` across the development cycle,
  including across multiple previews.
- Cutting a **preview** (`-alpha.N`, etc.) does **not** move entries out of
  Unreleased — previews are snapshots of in-progress work.
- Only a **stable release** (a normal `X.Y.Z`, a hotfix, or a `develop`
  promotion) moves the accumulated entries under a dated
  `## [X.Y.Z] - YYYY-MM-DD` heading and starts a fresh, empty `## [Unreleased]`.

Write entries from the reader's side — what changed for someone running the
software, and why it matters — not a restatement of the diff.

## Unstable / Preview Release

Preview releases are cut from `develop/0.4.7` and published as GitHub **pre-releases**.
They deliberately skip the stable end-user surfaces: no `version.json` change, no website
notification, no package library, and no `.deb`/`.rpm`/AUR artifacts. Only the Linux
tarball is published, so testers grab the binary directly from the release. The stable
line on `main` stays authoritative for everything user-facing.

### What differs from a stable release

| | Stable | Preview |
|---|--------|---------|
| Branch | `main` | `develop/0.4.7` |
| Version | `X.Y.Z` | `X.Y.Z-dev.N` (`-alpha`/`-beta`/`-rc`) |
| Assets | tarball + `.deb` + `.rpm` + AUR | tarball only |
| GitHub Release | Latest | Pre-release, `--latest=false` |
| `version.json` / website | updated | untouched |
| Release PR / merge | yes (`release/*` → `main`) | none — lives on `develop` |

The release workflow auto-detects the pre-release suffix (`-dev`/`-alpha`/`-beta`/`-rc`)
in the tag and: (a) marks the GitHub Release `--prerelease --latest=false`, and (b) skips
the `.deb`/`.rpm`/AUR jobs. `website-sync.yml` never fires because it only triggers on a
`release/*` merge to `main`, which a preview never performs.

### When the identifier is bumped

`develop/0.4.7` always carries the **next** unreleased identifier, not the last
released one. The bump happens immediately *after* a preview ships, not as part
of cutting the next one, so a binary built from `develop` never falsely claims
to be a tag that testers already hold.

Practical consequence: at cut time the version in `Cargo.toml` is normally
already correct. Verify it rather than bumping it, or the identifier advances
twice and an increment is silently skipped.

### Identifier progression

Increment the trailing number within a stage, then advance the stage as the
line matures. Never reuse an identifier — a tag is immutable once testers have
it.

```
0.4.7-alpha.1 → alpha.2 → … → beta.1 → beta.2 → … → rc.1 → … → 0.4.7
```

`-dev.N` is for throwaway internal snapshots; use `-alpha.N` onwards for
anything handed to testers.

### Release notes

The workflow prefers a hand-written notes file for the exact tag:

```
.github/release-notes/v0.4.7-alpha.1.md
```

If that file exists it becomes the release body verbatim. Otherwise the
workflow falls back to extracting the matching `## [VERSION]` section from
`CHANGELOG.md`, and failing that to the bare string `Release <tag>`.

**Previews should always ship a notes file.** A preview's audience needs
tester-facing guidance — what to look at, what noise to ignore, how to report,
which log line carries the evidence — and none of that belongs in the product
changelog. `## [Unreleased]` will never match a preview version anyway, so
without a notes file a preview publishes as a one-line body.

Stable releases normally have no notes file and fall through to the CHANGELOG
extraction, unchanged.

### Steps

```bash
# 1. On develop, ensure it's current (stable fixes forward-merged in — see Hotfix)
git checkout develop/0.4.7
git pull origin develop/0.4.7

# 2. Confirm the pre-release identifier in Cargo.toml is the one you are cutting.
#    develop carries the NEXT identifier, bumped when the previous preview
#    shipped -- so this is usually already correct. Bump only if it is not,
#    then sync the lockfile.
#    [workspace.package] version = "0.4.7-alpha.N"
cargo update -w
```

> **Do not use `make bump-version` for a preview.** It also rewrites
> `pkg/rpm/xearthlayer.spec`, and RPM forbids `-` in a `Version:` field (it
> separates Version from Release), so it would commit an invalid spec. The RPM
> job is skipped for previews so nothing fails at the time — it fails later, on
> the next stable release built from the committed spec. Edit `Cargo.toml`
> directly. (`build-rpm` rewrites the version from the tag at build time, so the
> spec's committed value is only ever a latent hazard, never the source of
> truth.)

```bash
# 3. Compile CHANGELOG "Unreleased" from PRs merged since the last release tag
#    (see CHANGELOG Convention above). Do NOT date a section for a preview.

# 4. Write the tester-facing notes for this exact tag
#    .github/release-notes/v0.4.7-alpha.N.md

# 5. Verify, commit, and push on develop
make pre-commit
git add Cargo.toml Cargo.lock CHANGELOG.md .github/release-notes/
git commit -m "chore(release): 0.4.7-alpha.N"
git push origin develop/0.4.7

# 6. Tag from develop and push — this triggers the release workflow
git tag v0.4.7-alpha.N
git push origin v0.4.7-alpha.N
```

There is no release PR and no merge step: the preview lives entirely on `develop/0.4.7`.

Confirm the binary agrees with the tag before announcing it — nothing in CI
cross-checks them, and the version testers paste into bug reports comes from
`Cargo.toml`, not from the tag:

```bash
xearthlayer --version   # must match the tag, e.g. 0.4.7-alpha.1
```

### Verify

```bash
# Confirm it is a pre-release and NOT marked latest
gh release view v0.4.7-dev.N --json isPrerelease,isLatest
# Expect: {"isPrerelease": true, "isLatest": false}

# Confirm the stable release is still "Latest"
gh release list --limit 5

# Confirm only the tarball was attached (no .deb/.rpm/AUR)
gh release view v0.4.7-dev.N --json assets --jq '.assets[].name'

# Confirm the notes file was used, not the one-line fallback
gh release view v0.4.7-alpha.N --json body --jq '.body' | head -5
```

## Hotfix Release

A hotfix ships an urgent patch to the **stable** line while `develop/0.4.7` is in flight.
Mechanically it is a normal stable patch release, plus a mandatory forward-merge into
develop so the fix is not lost when the next release ships.

### Release the patch on `main`

Cut the release branch from `main` and run the **Stable Release** golden path with a
`vX.Y.(Z+1)` version:

```bash
git checkout main && git pull origin main
git checkout -b release/X.Y.(Z+1)
# fix + tests (TDD), bump Cargo.toml to X.Y.(Z+1), update CHANGELOG.md and version.json
make pre-commit
```

Then follow the golden path from Step 3: PR `release/X.Y.(Z+1)` → `main`, wait for CI, tag
`vX.Y.(Z+1)` before merge, let the workflow publish, reconcile asset filenames, merge,
verify website.

> The release must flow through a `release/*` branch — `website-sync.yml` only fires when
> the merge commit message contains `release/`. Develop the fix on a `hotfix/*` topic
> branch first if you like, but land it via a `release/*` PR.

### Forward-merge into develop (required)

Once the fix is merged to `main`, carry it into the unstable line:

```bash
git checkout develop/0.4.7 && git pull origin develop/0.4.7
git merge --no-ff main          # bring the stable fix forward
# Cargo.toml will conflict on `version`: keep develop's 0.4.7-dev.N string, take the
# fix itself. Re-run cargo update -w if the lockfile needs it.
make pre-commit
git push origin develop/0.4.7
```

> **Why forward-merge, never back-merge.** `main` must stay releasable at any moment.
> Merging `develop` → `main` would drag unfinished 0.4.7 work into a stable release;
> merging `main` → `develop` only adds already-shipped, already-tested fixes to the
> unstable line. Fixes therefore always travel `main` → `develop`, never the reverse.

## Promoting `develop/0.4.7` to Stable

When the unstable line is feature-complete and has progressed through `-rc.N`, promote it
to a stable release. Promotion goes through a `release/*` branch (not a direct
`develop` → `main` merge) so `website-sync.yml` — which keys on `release/` in the merge
commit message — fires correctly.

```bash
# 1. Ensure every stable fix is forward-merged and develop is green
git checkout develop/0.4.7 && git pull origin develop/0.4.7
make pre-commit

# 2. Cut the release branch from develop
git checkout -b release/0.4.7

# 3. Drop the pre-release suffix and finalize the stable surfaces:
#    - Cargo.toml: [workspace.package] version = "0.4.7"   (then cargo update -w)
#    - CHANGELOG.md: move "Unreleased" entries under ## [0.4.7] - YYYY-MM-DD
#    - version.json: author 0.4.7 metadata + asset filenames (now in play)
cargo update -w
make pre-commit
git add Cargo.toml Cargo.lock CHANGELOG.md version.json
git commit -m "Release v0.4.7"

# 4. Push and open the promotion PR (base main, head release/0.4.7)
git push -u origin release/0.4.7
gh pr create --base main --title "Release v0.4.7" --body "Release v0.4.7"
```

From here follow the **Stable Release** golden path from Step 4 (tag `v0.4.7` before
merging, run the release workflow, reconcile assets, merge, verify website). After
promotion, open the next unstable branch (e.g. `develop/0.5.0`) from `main` for the
following cycle.

## Release Workflow Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Release Workflow Pipeline                         │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  Tag Push (vX.Y.Z)                                                      │
│        │                                                                │
│        ▼                                                                │
│  ┌─────────────┐                                                        │
│  │   Verify    │ ◄── Gate: format, lint, test                          │
│  └─────────────┘                                                        │
│        │                                                                │
│        ▼                                                                │
│  ┌─────────────────┐                                                    │
│  │  Build Binary   │ ◄── Single build, reused by packaging jobs        │
│  └─────────────────┘                                                    │
│        │                                                                │
│        ├──────────────┬───────────────┬──────────────┐                  │
│        ▼              ▼               ▼              ▼                  │
│  ┌──────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐             │
│  │  Linux   │  │  Debian   │  │    RPM    │  │    AUR    │             │
│  │ Tarball  │  │  Package  │  │  Package  │  │  Package  │             │
│  └──────────┘  └───────────┘  └───────────┘  └───────────┘             │
│        │              │               │              │                  │
│        └──────────────┴───────────────┴──────────────┘                  │
│                              │                                          │
│                              ▼                                          │
│                     ┌────────────────┐                                  │
│                     │ Publish Release│ ◄── Upload assets, update        │
│                     │                │     version.json, notify website │
│                     └────────────────┘                                  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Troubleshooting

### Issue: "GH013: Cannot create ref due to creations being restricted"

**Symptoms:**
```
! [remote rejected] vX.Y.Z -> vX.Y.Z (refusing to create ref due to creations being restricted)
error: failed to push some refs
```

**Possible Causes:**
1. Repository rulesets blocking tag creation
2. GitHub internal state from a failed workflow with a draft release
3. Tag already exists (unlikely if you're getting this specific error)

**Diagnosis:**
```bash
# Check if tag exists
git ls-remote --tags origin | grep vX.Y.Z

# Check for existing releases
gh release view vX.Y.Z 2>&1

# Check repository rulesets
gh api repos/samsoir/xearthlayer/rulesets
```

**Solutions:**

1. **If a draft release exists for this tag:**
   ```bash
   gh release delete vX.Y.Z --yes
   git push origin vX.Y.Z
   ```

2. **If rulesets are blocking:**
   - Go to Settings → Rules → Rulesets
   - Add yourself as a bypass actor, or temporarily disable

3. **If the specific version is mysteriously blocked (GitHub internal state):**
   - Skip to the next version number
   - Update `Cargo.toml` and `CHANGELOG.md` to new version
   - Add a note in CHANGELOG: `> **Note**: vX.Y.Z was skipped due to a release infrastructure issue.`
   - Create tag with new version

**Prevention:**
- Always use draft releases when manually creating releases
- Delete draft releases before retrying with the same tag

---

### Issue: version.json Not Updated

**Symptoms:**
After merging the release PR, `version.json` on main still shows the old version.

**Cause:**
`version.json` was not updated on the release branch before the PR was created.

**Note:** As of v0.4.0, `version.json` is updated on the release branch alongside
`Cargo.toml` and `CHANGELOG.md`. It merges to main with the release PR — no
post-release automation needed.

**Solution:**
Update `version.json` on main via a PR:

```bash
git checkout main && git pull
git checkout -b chore/version-json-X.Y.Z
# Edit version.json with correct version, release_date, and asset filenames
git add version.json
git commit -m "chore: update version.json to X.Y.Z"
git push -u origin chore/version-json-X.Y.Z
gh pr create --title "chore: update version.json to X.Y.Z"
```

**Prevention:**
- Include `version.json` in Step 2 of the release process (alongside Cargo.toml and CHANGELOG.md)
- Asset filenames follow a predictable pattern: `xearthlayer[-gpu]-vX.Y.Z-...`

---

### Issue: Website Not Updating

**Symptoms:**
- xearthlayer.app still shows old version after release
- Website workflow ran but pulled old version

**Possible Causes:**
1. `version.json` not updated (see above)
2. Website sync triggered before `version.json` was pushed
3. CDN cache delay

**Diagnosis:**
```bash
# Check version.json in repo (via API, bypasses CDN cache)
gh api repos/samsoir/xearthlayer/contents/version.json --jq '.content' | base64 -d | jq .version

# Check website workflow runs
gh run list --repo samsoir/xearthlayer-website --limit 5

# Check what version the website repo has
gh api repos/samsoir/xearthlayer-website/contents/data/release.json --jq '.content' | base64 -d | jq .version
```

**Solutions:**

1. **If version.json is correct but website has old version:**
   ```bash
   # Re-trigger website sync
   gh api repos/samsoir/xearthlayer-website/dispatches \
     -X POST \
     -f event_type=app-version-updated \
     -f 'client_payload[version]=X.Y.Z'

   # Watch the workflow
   gh run list --repo samsoir/xearthlayer-website --limit 1
   gh run watch --repo samsoir/xearthlayer-website
   ```

2. **If CDN is serving stale content:**
   - Wait 2-5 minutes for GitHub Pages CDN to propagate
   - Try accessing with cache-busting: `https://xearthlayer.app/?v=random`

---

### Issue: Release Workflow Failed

**Diagnosis:**
```bash
# View failed workflow
gh run list --workflow=release.yml --limit 5
gh run view <run-id> --log-failed
```

**Common Failures:**

1. **Verify step failed (tests/lint):**
   - Fix the issue locally
   - Delete the tag: `git push origin :refs/tags/vX.Y.Z`
   - Delete any draft release: `gh release delete vX.Y.Z --yes`
   - Push fixed code and new tag

2. **Package build failed:**
   - Check logs for specific error
   - Usually dependency issues in CI environment
   - The workflow is idempotent, so you can retry

3. **Upload failed:**
   - Network issues or GitHub API limits
   - Re-run the workflow: `gh run rerun <run-id>`

---

### Issue: RPM Download Returns 404 from Website

**Symptoms:**
- Website's RPM download link 404s while DEB / tarball / AUR work fine
- `version.json` declares `xearthlayer-X.Y.Z-1.fcNN.x86_64.rpm` but the
  published asset is named with a different `fcNN`

**Cause:**
The release workflow's RPM job runs in a Fedora container, and the asset
filename embeds whichever `fcNN` that container resolves to at build time.
When Fedora releases a new version, the container upgrades automatically
(typically once or twice a year), and the RPM filename advances. If
`version.json` was authored against the previous Fedora version, the
declared filename diverges from the published artifact.

**Fix (post-release):**

```bash
# Find the actual RPM filename
gh release view vX.Y.Z --json assets --jq '.assets[] | select(.name | endswith(".rpm")) | .name'

# Update version.json on main via a fix-up PR
git checkout main && git pull
git checkout -b fix/version-json-rpm-fcNN
# edit version.json -> assets.rpm.filename
git add version.json
git commit -m "fix(version.json): align RPM filename with built fcNN"
git push -u origin fix/version-json-rpm-fcNN
gh pr create --title "fix(version.json): align RPM filename with built fcNN"
```

**Prevention:**
Reconcile asset filenames against `version.json` *after the release workflow
publishes* but *before merging the release PR* (Step 6). Catching it on the
release branch means the corrected `version.json` lands with the same merge
SHA the website-sync workflow consumes.

---

### Issue: Skipped Version Number

If you had to skip a version (e.g., v0.2.11 was blocked), document it properly:

```markdown
## [0.2.12] - 2026-01-10

> **Note**: v0.2.11 was skipped due to a release infrastructure issue.

### Added
- ...
```

Update the comparison links at the bottom of CHANGELOG.md:
```markdown
[0.2.12]: https://github.com/samsoir/xearthlayer/compare/v0.2.10...v0.2.12
```

## Quick Reference

### Release Checklist

- [ ] Working tree clean, on `main`, up to date
- [ ] `make pre-commit` passes
- [ ] Version updated in `Cargo.toml`
- [ ] CHANGELOG.md updated with all changes
- [ ] `version.json` updated with new version, date, and asset filenames
- [ ] Release branch created and PR opened
- [ ] CI passes on PR
- [ ] Tag created and pushed (BEFORE merging PR)
- [ ] Release workflow completes successfully
- [ ] **Asset filenames reconciled against `version.json`** (RPM `fcNN` drift) — fix on release branch before merge if mismatched
- [ ] PR merged (AFTER workflow completes) — `version.json` lands on main with merge
- [ ] Website shows new version

### Key Commands

```bash
# Create and push tag
git tag vX.Y.Z && git push origin vX.Y.Z

# Watch release workflow
gh run watch

# Check release
gh release view vX.Y.Z

# Verify version.json
gh api repos/samsoir/xearthlayer/contents/version.json --jq '.content' | base64 -d

# Trigger website sync
gh api repos/samsoir/xearthlayer-website/dispatches \
  -X POST -f event_type=app-version-updated

# Delete tag (if needed to retry)
git push origin :refs/tags/vX.Y.Z
git tag -d vX.Y.Z

# Delete release (if needed to retry)
gh release delete vX.Y.Z --yes
```

## Workflow Files

| File | Purpose |
|------|---------|
| `.github/workflows/release.yml` | Main release workflow. Detects pre-release tags (`-dev`/`-alpha`/`-beta`/`-rc`) → GitHub pre-release, tarball only; stable tags → full packaging |
| `.github/workflows/website-sync.yml` | Website notification (triggers on `release/*` merge to `main` — stable only) |
| `.github/workflows/ci.yml` | PR/push CI checks (runs on `main` and `develop/**`, push and PR) |
| `version.json` | Current version metadata for website |
| `CHANGELOG.md` | Release notes history |

## See Also

- [GitHub Releases Publishing](github-releases-publishing.md) - Package publishing workflow
- [CHANGELOG.md](../../CHANGELOG.md) - Version history
