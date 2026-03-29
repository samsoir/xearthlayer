# Implementation Plan: Setup Wizard (`xearthlayer setup`)

## Overview

Create a TUI-based setup wizard that guides users through initial XEarthLayer configuration. The wizard interrogates the system, detects optimal settings, and writes a configuration file tailored to the user's hardware.

**Complexity**: High
**Estimated effort**: 4-6 hours
**Dependencies**: None (ratatui and crossterm already in Cargo.toml)

## Entry Points

```bash
xearthlayer setup    # Launch the setup wizard
make setup           # Convenience target in Makefile
```

## Wizard Flow

```
┌────────────────────────────────────────────────────────────────┐
│                    XEarthLayer Setup Wizard                    │
├────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Step 1: X-Plane Installation                                  │
│  ─────────────────────────────                                  │
│  Detected installations from ~/.x-plane/x-plane_install_12.txt │
│                                                                 │
│  [1] /home/user/X-Plane 12                                     │
│  [2] /mnt/games/X-Plane 12                                     │
│                                                                 │
│  Select installation: [1]                                       │
│                                                                 │
├────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Step 2: Package Location                                       │
│  ─────────────────────────                                      │
│  Where should XEarthLayer store scenery packages?               │
│                                                                 │
│  Default: ~/.xearthlayer/packages                               │
│                                                                 │
│  [Enter] Accept default                                         │
│  [C] Custom path                                                │
│                                                                 │
├────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Step 3: Cache Location                                         │
│  ──────────────────────                                         │
│  Where should XEarthLayer store cached tiles?                   │
│                                                                 │
│  Default: ~/.xearthlayer/cache                                  │
│  Detected storage: NVMe SSD (recommended for caching)           │
│                                                                 │
│  [Enter] Accept default                                         │
│  [C] Custom path                                                │
│                                                                 │
├────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Step 4: System Configuration                                   │
│  ───────────────────────────                                    │
│                                                                 │
│  Detected Hardware:                                             │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ CPU Cores:      16 logical cores                        │   │
│  │ System Memory:  32 GB                                   │   │
│  │ Cache Storage:  NVMe SSD                                │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  Recommended Settings:                                          │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │ Memory Cache:   8 GB (of 32 GB available)               │   │
│  │ Disk Cache:     40 GB                                   │   │
│  │ I/O Profile:    nvme (high concurrency)                 │   │
│  │ Pipeline Size:  64 concurrent requests                  │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  [Enter] Accept recommended settings                            │
│  [M] Modify settings                                            │
│                                                                 │
├────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Configuration Complete!                                        │
│  ──────────────────────                                         │
│                                                                 │
│  Config written to: ~/.xearthlayer/config.ini                   │
│                                                                 │
│  To view your configuration:                                    │
│    xearthlayer config list                                      │
│                                                                 │
│  To start XEarthLayer:                                          │
│    xearthlayer run                                              │
│                                                                 │
│  [Enter] Exit                                                   │
│                                                                 │
└────────────────────────────────────────────────────────────────┘
```

## Memory Cache Sizing Rules

| System RAM | Memory Cache Size | Rationale |
|------------|-------------------|-----------|
| < 8 GB     | 2 GB              | Conservative, leave room for X-Plane |
| 8-31 GB    | 8 GB              | Good balance for most users |
| 32-63 GB   | 12 GB             | Larger cache for enthusiasts |
| 64+ GB     | 16 GB             | Maximum useful cache size |

## Disk I/O Profile Selection

| Detected Storage | Config Setting | Concurrency |
|------------------|----------------|-------------|
| NVMe             | `nvme`         | High (128-256) |
| SATA SSD         | `auto`         | Medium (32-64) |
| HDD              | `hdd`          | Low (1-4) |

## Implementation

### New Files

| File | Purpose |
|------|---------|
| `xearthlayer-cli/src/commands/setup/mod.rs` | Module root, exports |
| `xearthlayer-cli/src/commands/setup/wizard.rs` | Main wizard orchestration |
| `xearthlayer-cli/src/commands/setup/steps/mod.rs` | Step module exports |
| `xearthlayer-cli/src/commands/setup/steps/xplane.rs` | X-Plane detection step |
| `xearthlayer-cli/src/commands/setup/steps/packages.rs` | Package location step |
| `xearthlayer-cli/src/commands/setup/steps/cache.rs` | Cache location step |
| `xearthlayer-cli/src/commands/setup/steps/system.rs` | System detection step |
| `xearthlayer-cli/src/commands/setup/detection.rs` | Hardware detection utilities |
| `xearthlayer-cli/src/commands/setup/ui.rs` | TUI rendering helpers |

### Modified Files

| File | Changes |
|------|---------|
| `xearthlayer-cli/src/main.rs` | Add `Setup` command variant |
| `xearthlayer-cli/src/commands/mod.rs` | Add `pub mod setup;` |
| `Makefile` | Add `setup` target |

### Key Types

```rust
// xearthlayer-cli/src/commands/setup/mod.rs

/// Setup wizard configuration result.
pub struct SetupConfig {
    /// X-Plane Custom Scenery directory
    pub xplane_scenery_dir: PathBuf,
    /// Package installation directory
    pub package_dir: PathBuf,
    /// Cache directory
    pub cache_dir: PathBuf,
    /// Detected/selected storage profile
    pub disk_io_profile: DiskIoProfile,
    /// Memory cache size in bytes
    pub memory_cache_size: u64,
    /// Disk cache size in bytes
    pub disk_cache_size: u64,
    /// Recommended pipeline concurrency
    pub pipeline_concurrency: usize,
}

/// Detected system information.
pub struct SystemInfo {
    /// Number of logical CPU cores
    pub cpu_cores: usize,
    /// Total system memory in bytes
    pub total_memory: u64,
    /// Detected storage type for cache path
    pub cache_storage: DiskIoProfile,
}
```

### Step 1: X-Plane Detection

Reuse existing detection from `xearthlayer::config::detect_scenery_dir()`:

```rust
// steps/xplane.rs

use xearthlayer::config::{detect_scenery_dir, SceneryDetectionResult};

pub fn detect_xplane() -> XPlaneStep {
    match detect_scenery_dir() {
        SceneryDetectionResult::NotFound => XPlaneStep::NotFound,
        SceneryDetectionResult::Single(path) => XPlaneStep::Single(path),
        SceneryDetectionResult::Multiple(paths) => XPlaneStep::Multiple(paths),
    }
}
```

### Step 2 & 3: Directory Selection

Simple path input with default:

```rust
// steps/packages.rs / steps/cache.rs

pub fn default_package_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".xearthlayer")
        .join("packages")
}

pub fn default_cache_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".xearthlayer")
        .join("cache")
}
```

### Step 4: System Detection

```rust
// detection.rs

use std::thread::available_parallelism;
use xearthlayer::pipeline::storage::DiskIoProfile;

/// Detect system hardware capabilities.
pub fn detect_system(cache_path: &Path) -> SystemInfo {
    let cpu_cores = available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);

    let total_memory = detect_total_memory();
    let cache_storage = DiskIoProfile::Auto.resolve_for_path(cache_path);

    SystemInfo {
        cpu_cores,
        total_memory,
        cache_storage,
    }
}

/// Detect total system memory.
#[cfg(target_os = "linux")]
fn detect_total_memory() -> u64 {
    use std::fs;

    // Parse /proc/meminfo
    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                // Format: "MemTotal:       16384000 kB"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<u64>() {
                        return kb * 1024; // Convert to bytes
                    }
                }
            }
        }
    }
    // Fallback: 8GB
    8 * 1024 * 1024 * 1024
}

/// Calculate recommended memory cache size.
pub fn recommended_memory_cache(total_memory: u64) -> u64 {
    let gb = 1024 * 1024 * 1024;
    match total_memory {
        m if m < 8 * gb => 2 * gb,      // <8GB: 2GB cache
        m if m < 32 * gb => 8 * gb,     // 8-31GB: 8GB cache
        m if m < 64 * gb => 12 * gb,    // 32-63GB: 12GB cache
        _ => 16 * gb,                    // 64+GB: 16GB cache
    }
}

/// Calculate recommended disk I/O profile.
pub fn recommended_io_profile(storage: DiskIoProfile) -> &'static str {
    match storage {
        DiskIoProfile::Nvme => "nvme",
        _ => "auto",
    }
}
```

### TUI Implementation

Use `dialoguer` for simple prompts (lighter than ratatui for this use case):

```rust
// wizard.rs

use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};

pub fn run_wizard() -> Result<SetupConfig, CliError> {
    let theme = ColorfulTheme::default();

    println!("\n{}\n", "XEarthLayer Setup Wizard".bold());

    // Step 1: X-Plane
    let xplane_dir = step_xplane(&theme)?;

    // Step 2: Packages
    let package_dir = step_packages(&theme)?;

    // Step 3: Cache
    let cache_dir = step_cache(&theme)?;

    // Step 4: System
    let system = detect_system(&cache_dir);
    let config = step_system(&theme, &system)?;

    // Write config
    write_config(&config)?;

    Ok(config)
}
```

### CLI Integration

```rust
// main.rs

#[derive(Subcommand)]
enum Commands {
    // ... existing commands ...

    /// Interactive setup wizard for first-time configuration
    ///
    /// Guides you through configuring XEarthLayer for your system.
    /// Detects X-Plane installation, system hardware, and recommends
    /// optimal settings based on your CPU, memory, and storage.
    Setup,
}

// In main():
Some(Commands::Setup) => commands::setup::run(),
```

### Makefile Target

```makefile
.PHONY: setup
setup: build ## Run the setup wizard
	@echo "$(BLUE)Starting XEarthLayer setup wizard...$(NC)"
	@./target/debug/xearthlayer setup
```

### Existing Config Handling

When config already exists, offer three options:

```rust
pub fn handle_existing_config() -> ExistingConfigAction {
    if !config_file_path().exists() {
        return ExistingConfigAction::CreateNew;
    }

    println!("Existing configuration found at: {}", config_file_path().display());

    let choices = vec![
        "Reconfigure (preserve compatible settings)",
        "Backup and replace (start fresh)",
        "Cancel (exit without changes)",
    ];

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What would you like to do?")
        .items(&choices)
        .default(0)
        .interact()?;

    match selection {
        0 => ExistingConfigAction::Reconfigure,
        1 => ExistingConfigAction::BackupAndReplace,
        _ => ExistingConfigAction::Cancel,
    }
}
```

## Dependencies

Add `dialoguer` to `xearthlayer-cli/Cargo.toml`:

```toml
[dependencies]
dialoguer = { version = "0.11", features = ["fuzzy-select"] }
console = "0.15"  # For styled output
```

Note: `ratatui` is already in dependencies but `dialoguer` is simpler for wizard-style prompts.

## Testing Strategy

1. **Unit tests**: Detection functions with mock /proc/meminfo
2. **Integration tests**: Full wizard flow with mock terminal input
3. **Manual testing**: Run on systems with different hardware profiles

### Test Fixtures

```rust
#[test]
fn test_memory_cache_sizing() {
    let gb = 1024 * 1024 * 1024;
    assert_eq!(recommended_memory_cache(4 * gb), 2 * gb);   // 4GB system
    assert_eq!(recommended_memory_cache(16 * gb), 8 * gb);  // 16GB system
    assert_eq!(recommended_memory_cache(32 * gb), 12 * gb); // 32GB system
    assert_eq!(recommended_memory_cache(128 * gb), 16 * gb); // 128GB system
}

#[test]
fn test_io_profile_recommendation() {
    assert_eq!(recommended_io_profile(DiskIoProfile::Nvme), "nvme");
    assert_eq!(recommended_io_profile(DiskIoProfile::Ssd), "auto");
    assert_eq!(recommended_io_profile(DiskIoProfile::Hdd), "auto");
}
```

## Documentation Updates

- Add `xearthlayer setup` to CLAUDE.md CLI reference
- Add "Getting Started" section to README.md mentioning setup wizard
- Update installation documentation to recommend running setup after install

## Future Enhancements

1. **Provider selection**: Ask which imagery provider to use (Bing/GO2/Google)
2. **API key input**: If Google selected, prompt for API key
3. **Pre-flight check**: Validate X-Plane directory structure
4. **Benchmark mode**: Run quick benchmark to fine-tune settings
