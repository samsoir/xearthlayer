# Application Release Runbook

This runbook documents the complete workflow for releasing new versions of XEarthLayer, including the golden path and troubleshooting for common issues.

## Overview

Releases are automated via GitHub Actions when a version tag is pushed. The workflow:
1. Runs `make verify` (format, lint, test)
2. Builds the release binary once
3. Packages for multiple platforms (Linux tarball, Debian, RPM, AUR)
4. Creates a GitHub Release with all assets
5. Updates `version.json` for website sync
6. Notifies the website to update download links

## Prerequisites

- Write access to the repository
- `gh` CLI authenticated (`gh auth status`)
- Clean working tree on `main` branch

## Golden Path: Releasing a New Version

### Step 1: Prepare the Release

```bash
# Ensure you're on main and up to date
git checkout main
git pull origin main

# Verify everything passes
make pre-commit
```

### Step 2: Update Version and Changelog

```bash
# Update workspace version in Cargo.toml
# Edit: [workspace.package] version = "X.Y.Z"

# Update CHANGELOG.md
# - Add new version header: ## [X.Y.Z] - YYYY-MM-DD
# - Document all changes under Added/Changed/Fixed/Removed
# - Update comparison links at bottom
```

### Step 3: Create Release Branch and PR

```bash
# Create release branch
git checkout -b release/X.Y.Z

# Commit changes
git add Cargo.toml Cargo.lock CHANGELOG.md
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

### Step 6: Merge PR After Release Completes

**CRITICAL**: Only merge the PR AFTER the release workflow completes successfully.

```bash
# Verify release was published
gh release view vX.Y.Z

# Merge the PR
gh pr merge --merge --delete-branch
```

### Step 7: Verify Website Updated

```bash
# Check version.json was updated
gh api repos/samsoir/xearthlayer/contents/version.json --jq '.content' | base64 -d | jq .version

# If still showing old version, update manually (see Troubleshooting)

# Verify website shows new version (may take 1-2 minutes for CDN)
curl -s https://xearthlayer.app | grep -o 'v[0-9]\+\.[0-9]\+\.[0-9]\+'
```

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

### Issue: version.json Push Rejected

**Symptoms:**
In the release workflow logs:
```
! [rejected] HEAD -> main (fetch first)
error: failed to push some refs
Push failed - version.json may need manual update
```

**Cause:**
The release workflow runs on a detached HEAD (from the tag). If the main branch was updated (e.g., PR merged) before the workflow pushes `version.json`, the push is rejected due to diverged history.

**Solution:**
Update `version.json` manually:

```bash
# Ensure you're on main and up to date
git checkout main
git pull

# Update version.json
cat > version.json << 'EOF'
{
  "version": "X.Y.Z",
  "tag": "vX.Y.Z",
  "release_date": "YYYY-MM-DD",
  "homepage": "https://xearthlayer.app",
  "assets": {
    "deb": {
      "filename": "xearthlayer_X.Y.Z-1_amd64.deb",
      "description": "Debian/Ubuntu package"
    },
    "rpm": {
      "filename": "xearthlayer-X.Y.Z-1.fc43.x86_64.rpm",
      "description": "Fedora/RHEL package"
    },
    "tarball": {
      "filename": "xearthlayer-vX.Y.Z-x86_64-linux.tar.gz",
      "description": "Linux binary tarball"
    },
    "aur": {
      "filename": "aur-package.zip",
      "description": "Arch Linux AUR package"
    }
  },
  "download_base_url": "https://github.com/samsoir/xearthlayer/releases/download/vX.Y.Z"
}
EOF

# Commit and push
git add version.json
git commit -m "chore: update version.json to X.Y.Z"
git push
```

**Prevention:**
- Merge the release PR **AFTER** the release workflow completes
- The workflow updates `version.json` as part of the publish step

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
- [ ] Release branch created and PR opened
- [ ] CI passes on PR
- [ ] Tag created and pushed (BEFORE merging PR)
- [ ] Release workflow completes successfully
- [ ] PR merged (AFTER workflow completes)
- [ ] `version.json` updated (auto or manual)
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
| `.github/workflows/release.yml` | Main release workflow |
| `.github/workflows/ci.yml` | PR/push CI checks |
| `version.json` | Current version metadata for website |
| `CHANGELOG.md` | Release notes history |

## See Also

- [GitHub Releases Publishing](github-releases-publishing.md) - Package publishing workflow
- [CHANGELOG.md](../../CHANGELOG.md) - Version history
