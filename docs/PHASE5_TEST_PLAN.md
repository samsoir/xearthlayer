# Phase 5: Test Package Creation - Test Plan

This document outlines the testing strategy for validating the Publisher CLI and creating the first XEarthLayer scenery package.

## Overview

Phase 5 validates the Publisher workflow by creating a real North America ortho package from California Ortho4XP tiles. This serves as both validation of Phases 1-4 and creates test data for Phase 6 (Package Manager).

## Test Data Location

**Ortho4XP Raw Output**: `<ORTHO4XP_ROOT>/Ortho4XP_Data/Tiles`

Replace `<ORTHO4XP_ROOT>` with your Ortho4XP installation directory.

## Test Stages

### Stage 1: Manual Command Verification

Verify each Publisher CLI command works with real Ortho4XP data.

#### 1.1 Initialize Repository

```bash
# Create test repository
mkdir -p /tmp/xel-test-repo
xearthlayer publish init /tmp/xel-test-repo --part-size 500M
```

**Expected**: Repository structure created with `packages/`, `dist/`, `staging/` directories.

#### 1.2 Scan Tiles

```bash
# Scan Ortho4XP output
xearthlayer publish scan \
  --source <ORTHO4XP_ROOT>/Ortho4XP_Data/Tiles
```

**Expected**:
- Lists discovered tiles with coordinates
- Shows file counts (DSF, .ter, masks)
- Suggests region code (should be "na" for California tiles)

#### 1.3 Add Package

```bash
# Process tiles into package
xearthlayer publish add \
  --source <ORTHO4XP_ROOT>/Ortho4XP_Data/Tiles \
  --region na \
  --type ortho \
  --version 0.1.0 \
  --repo /tmp/xel-test-repo
```

**Expected**:
- Files copied to `packages/na/ortho/`
- Metadata file created: `xearthlayer_scenery_package.txt`
- DSF files organized into 10°×10° grid structure

#### 1.4 Build Archives

```bash
# Create distributable archives
xearthlayer publish build \
  --region na \
  --type ortho \
  --repo /tmp/xel-test-repo
```

**Expected**:
- Archive created in `dist/na/ortho/`
- Split into parts if larger than 500MB
- SHA-256 checksums generated for each part
- Metadata updated with part information

#### 1.5 Configure URLs (Local File)

```bash
# Configure with local file URLs for testing
xearthlayer publish urls \
  --region na \
  --type ortho \
  --base-url "file:///tmp/xel-test-repo/dist/na/ortho/" \
  --repo /tmp/xel-test-repo
```

**Expected**:
- URLs configured in metadata
- Parts list shows file:// URLs

#### 1.6 Check Status

```bash
# Verify package is ready for release
xearthlayer publish status --repo /tmp/xel-test-repo
```

**Expected**: Shows "Ready" status for NA ortho package.

#### 1.7 Release to Library

```bash
# Add to library index
xearthlayer publish release \
  --region na \
  --type ortho \
  --metadata-url "file:///tmp/xel-test-repo/packages/na/ortho/xearthlayer_scenery_package.txt" \
  --repo /tmp/xel-test-repo
```

**Expected**:
- Library index created/updated
- Sequence number incremented
- Package entry added with correct metadata URL

#### 1.8 Validate Repository

```bash
# Final validation
xearthlayer publish validate --repo /tmp/xel-test-repo
```

**Expected**: No validation errors.

### Stage 2: Integration Tests

Automated tests that run the full workflow in a controlled environment.

#### Test Categories

1. **Repository Lifecycle Test**
   - Init → Add → Build → URLs → Release → Validate
   - Uses temporary directory with sample tile structure
   - Verifies all file artifacts are created correctly

2. **Multi-Package Test**
   - Add multiple packages to same repository
   - Verify library index contains all entries
   - Test filtering by region/type

3. **Version Management Test**
   - Create v1.0.0, then bump to v1.1.0
   - Rebuild and verify new archive names
   - Update library index with new version

4. **Error Handling Test**
   - Invalid source directory
   - Missing package for build
   - Invalid version strings
   - Repository not initialized

#### Test Implementation Location

```
xearthlayer-cli/tests/
├── integration/
│   ├── mod.rs
│   └── publish_workflow.rs
```

### Stage 3: GitHub Repository Setup

Set up the official XEarthLayer scenery package hosting.

#### Repository Structure

```
xearthlayer-packages/
├── README.md                           # Package index for users
├── xearthlayer_package_library.txt     # Machine-readable library index
└── releases/                           # GitHub Releases for archives
    └── (packages hosted as release assets)
```

#### Hosting Strategy

1. **Library Index**: Hosted in repository root (CDN via raw.githubusercontent.com)
2. **Package Metadata**: Hosted alongside library index or in subdirectories
3. **Archive Parts**: Hosted as GitHub Release assets (up to 2GB per file)

#### Setup Steps

1. Create `xearthlayer-packages` repository on GitHub
2. Configure repository settings (releases enabled)
3. Upload first test package via GitHub Releases
4. Update library index with GitHub URLs
5. Test download with `curl` or similar

### Stage 4: End-User Documentation

Create guide for users who want to create and publish their own packages.

#### Documentation Outline

1. **Prerequisites**
   - Ortho4XP installation and tile generation
   - XEarthLayer CLI installation
   - GitHub account (for hosting)

2. **Creating a Package**
   - Repository initialization
   - Scanning Ortho4XP output
   - Adding tiles to package
   - Building archives

3. **Hosting Options**
   - Self-hosted (web server, cloud storage)
   - GitHub Releases (recommended)
   - Contributing to official library

4. **Publishing Workflow**
   - Upload archives to hosting
   - Configure URLs
   - Create/update library index
   - Distribute library URL to users

## Success Criteria

### Stage 1 (Manual Verification)
- [ ] All commands execute without errors
- [ ] Package directory structure matches specification
- [ ] Archive parts have correct checksums
- [ ] Library index is valid and parseable

### Stage 2 (Integration Tests)
- [ ] Tests run in CI (GitHub Actions)
- [ ] No external dependencies (uses mock tile data)
- [ ] Coverage of happy path and error cases

### Stage 3 (GitHub Hosting)
- [ ] Package downloadable via HTTPS
- [ ] Library index accessible at known URL
- [ ] Documentation for contributors

### Stage 4 (Documentation)
- [ ] Step-by-step guide validated by test user
- [ ] Troubleshooting section for common issues

## Notes

### Tile Data Considerations

- California tiles should produce a package of moderate size (depends on zoom levels and coverage)
- For initial testing, consider using a subset of tiles to speed up iteration
- DDS files are skipped (XEarthLayer generates them on-demand)

### Local File URLs

Using `file://` URLs allows testing the full workflow without hosting infrastructure:
- Works for validating package structure
- Works for testing the Package Manager (Phase 6)
- Does NOT test HTTP download logic

### Parallel Phase 6 Development

While completing Stage 1-2 of Phase 5, begin Phase 6 groundwork:
- Define Manager module structure
- Implement local package discovery
- Design library client interface

This allows the Manager to be tested against real packages as soon as they're available.
