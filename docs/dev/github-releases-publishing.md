# Publishing Packages via GitHub Releases

This runbook documents the complete workflow for publishing XEarthLayer scenery packages using GitHub Releases as the hosting platform.

## Overview

GitHub Releases provides free hosting for package archives with these benefits:
- No hosting costs for public repositories
- Reliable CDN-backed downloads
- Version tagging integrated with git
- 2 GB per-file limit (use archive splitting for larger packages)

## Prerequisites

- XEarthLayer CLI built and available
- Package repository initialized (`xearthlayer publish init`)
- GitHub CLI (`gh`) installed and authenticated
- Ortho4XP source tiles ready

## Workflow Summary

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Publishing Workflow                             │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  1. Process Tiles ──► 2. Set Version ──► 3. Build Archives          │
│                                                │                    │
│                                                ▼                    │
│  6. Release ◄── 5. Configure URLs ◄── 4. Upload to GitHub           │
│       │                                                             │
│       ▼                                                             │
│  7. Push Library Index                                              │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Complete Step-by-Step Guide

### Step 1: Prepare the Environment

```bash
# Navigate to package repository
cd /path/to/xearthlayer-packages

# Verify repository is initialized
ls -la .xearthlayer-repo

# Check current packages
xearthlayer publish list
```

### Step 2: Configure Part Size (Optional)

For GitHub Releases, 1 GB parts are recommended (under 2 GB limit with margin):

```bash
# Edit .xearthlayer-repo to add/update config
cat >> .xearthlayer-repo << 'EOF'

[config]
part_size = 1G
EOF
```

Valid suffixes: `K` (KB), `M` (MB), `G` (GB), or plain bytes.

### Step 3: Process Ortho Tiles

```bash
xearthlayer publish add \
  --source /path/to/Ortho4XP/Tiles \
  --region na \
  --type ortho
```

**Expected output:**
```
Processing Ortho4XP tiles output...
  Source: /path/to/Ortho4XP/Tiles
  Region: NA
  Type:   ortho
  Version: 1.0.0

Scanning...
Found 1950 tiles

Processing into package...
Processing Summary
------------------
Tiles processed: 1950
DSF files:       1950
TER files:       576376
Mask files:      190354
DDS skipped:     27599

Package created successfully!
  Location: ./packages/zzXEL_na_ortho
```

**Note:** DDS files are intentionally skipped - XEarthLayer generates these on-demand.

### Step 4: Process Overlay Tiles

```bash
xearthlayer publish add \
  --source /path/to/yOrtho4XP_Overlays \
  --region na \
  --type overlay
```

**Expected output:**
```
Processing Ortho4XP overlays output...
Found 1804 tiles

Processing Summary
------------------
Tiles processed: 1804
DSF files:       1804
TER files:       0
Mask files:      0
DDS skipped:     0

Package created successfully!
  Location: ./packages/yzXEL_na_overlay
```

### Step 5: Set Package Version

For new packages, version defaults to 1.0.0. To set a specific version:

```bash
# Set explicit version
xearthlayer publish version --region na --type ortho --set 0.2.0
xearthlayer publish version --region na --type overlay --set 0.2.0

# Or bump existing version
xearthlayer publish version --region na --type ortho --bump minor
xearthlayer publish version --region na --type overlay --bump minor
```

### Step 6: Build Archives

```bash
# Build ortho archive (large - may take several minutes)
xearthlayer publish build --region na --type ortho

# Build overlay archive
xearthlayer publish build --region na --type overlay
```

**Expected output (ortho):**
```
Building archive for NA ortho...
  Part size: 1 GB

Archive created successfully!

  Archive: zzXEL_na_ortho-0.2.0.tar.gz
  Parts:   36
  Size:    35.9 GB

Archive parts:
  zzXEL_na_ortho-0.2.0.tar.gz.aa (1 GB)
  zzXEL_na_ortho-0.2.0.tar.gz.ab (1 GB)
  ...
  zzXEL_na_ortho-0.2.0.tar.gz.bj (965.5 MB)
```

Archives are stored in `dist/<region>/<type>/`.

### Step 7: Create GitHub Release

```bash
# Create the release (this also creates the git tag)
gh release create na-v0.2.0 \
  --repo owner/repo-name \
  --title "North America v0.2.0" \
  --notes "## North America Regional Package v0.2.0

### Coverage
- Hawaii, Alaska, Canada, CONUS, Caribbean, Mexico

### Package Details
| Package | Tiles | Size | Parts |
|---------|-------|------|-------|
| Ortho | 1,950 | 35.9 GB | 36 |
| Overlay | 1,804 | 1.8 GB | 2 |"
```

### Step 8: Upload Archive Parts

```bash
# Upload overlay parts (smaller, faster)
gh release upload na-v0.2.0 \
  --repo owner/repo-name \
  dist/na/overlay/yzXEL_na_overlay-0.2.0.tar.gz.*

# Upload ortho parts (larger, takes longer)
gh release upload na-v0.2.0 \
  --repo owner/repo-name \
  dist/na/ortho/zzXEL_na_ortho-0.2.0.tar.gz.*
```

**Tip:** For very large uploads, you can run these in parallel or use `--clobber` to retry failed uploads.

### Step 9: Verify Upload

```bash
# Count uploaded assets
gh release view na-v0.2.0 \
  --repo owner/repo-name \
  --json assets --jq '.assets | length'

# Expected: 38 (36 ortho + 2 overlay)
```

### Step 10: Configure URLs

```bash
# Configure ortho URLs
xearthlayer publish urls \
  --region na \
  --type ortho \
  --base-url https://github.com/owner/repo-name/releases/download/na-v0.2.0/

# Configure overlay URLs
xearthlayer publish urls \
  --region na \
  --type overlay \
  --base-url https://github.com/owner/repo-name/releases/download/na-v0.2.0/
```

### Step 11: Upload Metadata Files

```bash
# Copy metadata files with proper names
cp packages/zzXEL_na_ortho/xearthlayer_scenery_package.txt \
   /tmp/zzXEL_na_ortho-metadata.txt
cp packages/yzXEL_na_overlay/xearthlayer_scenery_package.txt \
   /tmp/yzXEL_na_overlay-metadata.txt

# Upload to release
gh release upload na-v0.2.0 \
  --repo owner/repo-name \
  /tmp/zzXEL_na_ortho-metadata.txt \
  /tmp/yzXEL_na_overlay-metadata.txt
```

### Step 12: Release to Library Index

```bash
# Release ortho to library
xearthlayer publish release \
  --region na \
  --type ortho \
  --metadata-url https://github.com/owner/repo-name/releases/download/na-v0.2.0/zzXEL_na_ortho-metadata.txt

# Release overlay to library
xearthlayer publish release \
  --region na \
  --type overlay \
  --metadata-url https://github.com/owner/repo-name/releases/download/na-v0.2.0/yzXEL_na_overlay-metadata.txt
```

### Step 13: Commit and Push Library Index

```bash
git add xearthlayer_package_library.txt
git commit -m "Release NA v0.2.0 to library index

- Ortho: 1,950 tiles, 36 parts, 35.9 GB
- Overlay: 1,804 DSFs, 2 parts, 1.8 GB"
git push origin main
```

### Step 14: Verify Release

```bash
# Test metadata URL accessibility
curl -sI "https://github.com/owner/repo-name/releases/download/na-v0.2.0/zzXEL_na_ortho-metadata.txt" | head -3
# Expected: HTTP/2 302 (redirect to CDN)

# Verify library index
curl -sL "https://raw.githubusercontent.com/owner/repo-name/main/xearthlayer_package_library.txt" | head -15
```

## Troubleshooting

### Issue: "tag_name was used by an immutable release"

**Cause:** GitHub's immutable releases feature permanently blocks reuse of tags that were previously associated with a published (non-draft) release.

**Solution:** Use a different version number:
```bash
# Bump to next patch version
xearthlayer publish version --region na --type ortho --set 0.2.1
xearthlayer publish version --region na --type overlay --set 0.2.1

# Rebuild archives
xearthlayer publish build --region na --type ortho
xearthlayer publish build --region na --type overlay

# Create release with new tag
gh release create na-v0.2.1 ...
```

**Prevention:** Create releases as drafts first, verify everything, then publish:
```bash
gh release create na-v0.2.0 --draft ...
# Upload and verify...
gh release edit na-v0.2.0 --draft=false
```

### Issue: Upload fails partway through

**Cause:** Network issues or GitHub API limits during large uploads.

**Solution:** Retry individual failed parts:
```bash
# Check what's uploaded
gh release view na-v0.2.0 --json assets --jq '.assets[].name' | sort

# Upload missing parts individually
gh release upload na-v0.2.0 dist/na/ortho/zzXEL_na_ortho-0.2.0.tar.gz.bj
```

### Issue: "Cannot create ref due to creations being restricted"

**Cause:** Repository rulesets blocking tag creation.

**Solution:**
1. Go to Settings → Rules → Rulesets
2. Add your account as a bypass actor
3. Or temporarily disable the rule

### Issue: URLs return 404

**Cause:** Release is still in draft mode (draft releases don't serve assets via download URLs).

**Solution:** Publish the release:
```bash
gh release edit na-v0.2.0 --repo owner/repo-name --draft=false
```

## File Locations Reference

| File | Location | Purpose |
|------|----------|---------|
| Repository config | `.xearthlayer-repo` | Part size, repo settings |
| Package metadata | `packages/<name>/xearthlayer_scenery_package.txt` | Package info, checksums, URLs |
| Library index | `xearthlayer_package_library.txt` | Master list of all packages |
| Archive parts | `dist/<region>/<type>/*.tar.gz.*` | Distributable archives |

## URL Patterns

For GitHub Releases:

| Asset Type | URL Pattern |
|------------|-------------|
| Archive part | `https://github.com/{owner}/{repo}/releases/download/{tag}/{filename}` |
| Metadata | `https://github.com/{owner}/{repo}/releases/download/{tag}/{package}-metadata.txt` |
| Library index | `https://raw.githubusercontent.com/{owner}/{repo}/main/xearthlayer_package_library.txt` |

## Quick Reference Commands

```bash
# Full workflow for a new region
xearthlayer publish add --source /tiles --region xx --type ortho
xearthlayer publish add --source /overlays --region xx --type overlay
xearthlayer publish version --region xx --type ortho --set 1.0.0
xearthlayer publish version --region xx --type overlay --set 1.0.0
xearthlayer publish build --region xx --type ortho
xearthlayer publish build --region xx --type overlay

gh release create xx-v1.0.0 --repo owner/repo --title "Region XX v1.0.0" --notes "..."
gh release upload xx-v1.0.0 --repo owner/repo dist/xx/ortho/*.tar.gz.*
gh release upload xx-v1.0.0 --repo owner/repo dist/xx/overlay/*.tar.gz.*

xearthlayer publish urls --region xx --type ortho --base-url https://github.com/owner/repo/releases/download/xx-v1.0.0/
xearthlayer publish urls --region xx --type overlay --base-url https://github.com/owner/repo/releases/download/xx-v1.0.0/

cp packages/zzXEL_xx_ortho/xearthlayer_scenery_package.txt /tmp/zzXEL_xx_ortho-metadata.txt
cp packages/yzXEL_xx_overlay/xearthlayer_scenery_package.txt /tmp/yzXEL_xx_overlay-metadata.txt
gh release upload xx-v1.0.0 --repo owner/repo /tmp/*-metadata.txt

xearthlayer publish release --region xx --type ortho --metadata-url https://github.com/owner/repo/releases/download/xx-v1.0.0/zzXEL_xx_ortho-metadata.txt
xearthlayer publish release --region xx --type overlay --metadata-url https://github.com/owner/repo/releases/download/xx-v1.0.0/yzXEL_xx_overlay-metadata.txt

git add xearthlayer_package_library.txt && git commit -m "Release XX v1.0.0" && git push
```

## Version Update Workflow

When updating an existing region:

```bash
# 1. Add new/updated tiles (replaces existing package content)
xearthlayer publish add --source /new-tiles --region na --type ortho

# 2. Bump version
xearthlayer publish version --region na --type ortho --bump minor  # or --set x.y.z

# 3. Rebuild archives
xearthlayer publish build --region na --type ortho

# 4. Create new release and upload
gh release create na-v0.3.0 ...
gh release upload na-v0.3.0 dist/na/ortho/*.tar.gz.*

# 5. Configure URLs, upload metadata, release to library
# (same steps as initial release)
```

## Estimated Times

| Operation | ~1,000 tiles | ~2,000 tiles |
|-----------|--------------|--------------|
| Process tiles | 1-2 min | 2-4 min |
| Build archive | 5-10 min | 15-30 min |
| Upload (good connection) | 30-60 min | 1-2 hours |

## Generating Coverage Maps

After publishing packages, generate visual coverage maps for your repository README:

### Static PNG Map

```bash
# Light theme (OpenStreetMap tiles)
xearthlayer publish coverage --output coverage.png

# Dark theme (CartoDB Dark Matter tiles) - recommended for GitHub READMEs
xearthlayer publish coverage --dark --output coverage.png

# Custom dimensions
xearthlayer publish coverage --dark --width 1600 --height 800 --output coverage.png
```

### Interactive GeoJSON Map

GitHub automatically renders `.geojson` files with an interactive map viewer:

```bash
xearthlayer publish coverage --geojson --output coverage.geojson
```

### Embedding in README

```markdown
## Coverage Map

![Tile Coverage Map](coverage.png)

*NA tiles shown in blue, EU tiles in orange. [View interactive map](coverage.geojson) for exact tile boundaries.*
```

### Coverage Command Options

| Option | Description |
|--------|-------------|
| `--output, -o` | Output file path (default: coverage.png) |
| `--width` | Image width in pixels (PNG only, default: 1200) |
| `--height` | Image height in pixels (PNG only, default: 600) |
| `--dark` | Use dark theme with CartoDB tiles (PNG only) |
| `--geojson` | Generate GeoJSON instead of PNG |

## See Also

- [Content Publishing Guide](../content-publishing.md) - General publishing concepts
- [Package Publisher Design](package-publisher-design.md) - Architecture details
- [Scenery Package Plan](scenery-package-plan.md) - Implementation history
