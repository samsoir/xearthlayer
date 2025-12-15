# XEarthLayer Configuration

XEarthLayer uses an INI configuration file located at `~/.xearthlayer/config.ini`. This file is created automatically on first run with sensible defaults.

## Configuration File Location

- **Config file**: `~/.xearthlayer/config.ini`
- **Log file**: `~/.xearthlayer/xearthlayer.log` (default)
- **Cache directory**: `~/.cache/xearthlayer/` (default)

## Sections

### [provider]

Controls which satellite imagery provider to use.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `type` | string | `bing` | Imagery provider: `bing`, `go2`, or `google` (see below) |
| `google_api_key` | string | (empty) | Google Maps API key. Required only when `type = google`. |

**Available Providers:**

| Provider | API Key | Cost | Notes |
|----------|---------|------|-------|
| `bing` | Not required | Free | Bing Maps aerial imagery. Recommended for most users. |
| `go2` | Not required | Free | Google Maps via public tile servers. Same endpoint as Ortho4XP's GO2 provider. |
| `google` | Required | Paid | Google Maps official API. Has usage limits (15,000 requests/day). |

**Examples:**

Bing Maps (recommended):
```ini
[provider]
type = bing
```

Google GO2 (free, no API key):
```ini
[provider]
type = go2
```

Google Maps official API (paid):
```ini
[provider]
type = google
google_api_key = AIzaSy...your-key-here
```

**Notes:**
- **GO2** uses the same Google tile servers as Ortho4XP, making it ideal for use with Ortho4XP-generated scenery packs.
- **Google official API** has strict rate limits (15,000 requests/day). With 256 chunks per tile, this allows approximately 58 tiles per day.
- **Bing Maps** is recommended for general use due to its reliability and no rate limits.

### [cache]

Controls tile caching behavior.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `directory` | path | `~/.cache/xearthlayer` | Directory for storing cached tiles. Supports `~` expansion. |
| `memory_size` | size | `2GB` | Maximum RAM for in-memory cache. Supports KB, MB, GB suffixes. |
| `disk_size` | size | `20GB` | Maximum disk space for persistent cache. Supports KB, MB, GB suffixes. |

**Example:**
```ini
[cache]
; Use custom cache location
directory = /mnt/ssd/xearthlayer-cache
memory_size = 4GB
disk_size = 100GB
```

**Cache Structure:**
```
~/.cache/xearthlayer/
├── Bing Maps/
│   └── <zoom>/<row>/<col>_bc1.dds
└── Google Maps/
    └── <zoom>/<row>/<col>_bc1.dds
```

### [texture]

Controls DDS texture output format.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `format` | string | `bc1` | DDS compression: `bc1` (smaller, opaque) or `bc3` (larger, with alpha) |
| `mipmaps` | integer | `5` | Number of mipmap levels (1-10) |

**Example:**
```ini
[texture]
format = bc1
mipmaps = 5
```

**Format Comparison:**
- **BC1 (DXT1)**: 4:1 compression, ~11MB per 4096x4096 tile. Best for satellite imagery.
- **BC3 (DXT5)**: 4:1 compression with alpha, ~22MB per tile. Use if transparency is needed.

### [download]

Controls network download behavior.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `timeout` | integer | `30` | Download timeout in seconds for a single chunk |
| `retries` | integer | `3` | Number of retry attempts for failed downloads |

**Example:**
```ini
[download]
timeout = 30
retries = 3
```

**Note:** Each 4096x4096 tile requires downloading 256 chunks (16x16 grid of 256x256 tiles). HTTP concurrency is automatically tuned based on your system's CPU count to prevent network stack exhaustion while maintaining good throughput.

### [generation]

Controls parallel tile generation.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `threads` | integer | (num CPUs) | Number of worker threads for parallel tile generation |
| `timeout` | integer | `10` | Timeout in seconds for generating a single tile. If exceeded, returns a magenta placeholder. |

**Example:**
```ini
[generation]
threads = 8
timeout = 10
```

**Performance Notes:**
- `threads` defaults to the number of CPU cores
- Do not set `threads` higher than your CPU core count
- The timeout prevents X-Plane from hanging if a tile download stalls
- Magenta placeholder tiles indicate timeouts or download failures

### [xplane]

Controls X-Plane integration.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `scenery_dir` | path | (auto-detect) | X-Plane Custom Scenery directory. If empty, auto-detects from `~/.x-plane/x-plane_install_12.txt` |

**Example:**
```ini
[xplane]
scenery_dir = /home/user/X-Plane 12/Custom Scenery
```

### [logging]

Controls log output.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `file` | path | `~/.xearthlayer/xearthlayer.log` | Log file location. Supports `~` expansion. |

**Example:**
```ini
[logging]
file = ~/.xearthlayer/xearthlayer.log
```

### [packages]

Controls package manager behavior.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `library_url` | URL | (none) | URL to the package library index file |
| `install_location` | path | `~/.xearthlayer/packages` | Directory for storing installed packages |
| `custom_scenery_path` | path | (auto-detect) | X-Plane Custom Scenery directory for overlay symlinks |
| `auto_install_overlays` | bool | `false` | Automatically install matching overlay when installing ortho |
| `temp_dir` | path | system temp | Temporary directory for package downloads |

**Example:**
```ini
[packages]
library_url = https://example.com/xearthlayer_package_library.txt
install_location = ~/.xearthlayer/packages
custom_scenery_path = /home/user/X-Plane 12/Custom Scenery
auto_install_overlays = true
temp_dir = ~/Downloads/xearthlayer-temp
```

**Notes:**
- When `library_url` is set, you don't need to pass `--library-url` to package commands
- The `temp_dir` is used for downloading archives before extraction; files are cleaned up after installation
- When `auto_install_overlays` is enabled, installing an ortho package will automatically install the matching overlay package for the same region (if available)
- If `custom_scenery_path` is not set, it falls back to `[xplane] scenery_dir` or auto-detects from X-Plane installation

## Complete Example

```ini
[provider]
type = bing

[cache]
; directory = /custom/cache/path
memory_size = 4GB
disk_size = 50GB

[texture]
format = bc1
mipmaps = 5

[download]
timeout = 30
retries = 3

[generation]
threads = 8
timeout = 10

[xplane]
; scenery_dir = /path/to/X-Plane 12/Custom Scenery

[logging]
file = ~/.xearthlayer/xearthlayer.log

[packages]
; library_url = https://example.com/xearthlayer_package_library.txt
; install_location = ~/.xearthlayer/packages
; custom_scenery_path = /path/to/X-Plane 12/Custom Scenery
; auto_install_overlays = true
; temp_dir = ~/Downloads/xearthlayer-temp
```

## Config CLI Commands

XEarthLayer provides CLI commands for viewing and modifying configuration settings:

### Show Config File Path

```bash
xearthlayer config path
# Output: /home/user/.xearthlayer/config.ini
```

### List All Settings

```bash
xearthlayer config list
```

This displays all configuration settings grouped by section, showing current values or "(not set)" for unset options.

### Get a Setting

```bash
xearthlayer config get <section.key>

# Examples:
xearthlayer config get provider.type
xearthlayer config get cache.memory_size
xearthlayer config get packages.library_url
```

### Set a Setting

```bash
xearthlayer config set <section.key> <value>

# Examples:
xearthlayer config set provider.type bing
xearthlayer config set cache.memory_size 4GB
xearthlayer config set packages.library_url https://example.com/library.txt
xearthlayer config set packages.auto_install_overlays true
```

Values are validated before being saved. Invalid values will produce an error message explaining the expected format.

### Available Configuration Keys

| Key | Valid Values | Description |
|-----|--------------|-------------|
| `provider.type` | `bing`, `go2`, `google` | Imagery provider |
| `provider.google_api_key` | string | Google Maps API key |
| `cache.directory` | path | Cache directory |
| `cache.memory_size` | size (e.g., `2GB`) | Memory cache size |
| `cache.disk_size` | size (e.g., `20GB`) | Disk cache size |
| `texture.format` | `bc1`, `bc3` | DDS compression format |
| `download.timeout` | positive integer | Chunk download timeout (seconds) |
| `generation.threads` | positive integer | Worker threads |
| `generation.timeout` | positive integer | Tile generation timeout (seconds) |
| `xplane.scenery_dir` | path | X-Plane Custom Scenery directory |
| `packages.library_url` | URL | Package library index URL |
| `packages.install_location` | path | Package installation directory |
| `packages.custom_scenery_path` | path | Custom Scenery for overlays |
| `packages.auto_install_overlays` | `true`, `false` | Auto-install matching overlays |
| `packages.temp_dir` | path | Temporary download directory |
| `logging.file` | path | Log file location |

## CLI Overrides

Most configuration settings can be overridden via CLI arguments:

```bash
# Override provider
xearthlayer start --source ./scenery --provider google --google-api-key YOUR_KEY

# Override DDS format
xearthlayer start --source ./scenery --dds-format bc3

# Override download timeout
xearthlayer start --source ./scenery --timeout 60

# Disable caching
xearthlayer start --source ./scenery --no-cache
```

## Regenerating Config

To regenerate the config file with defaults:

```bash
rm ~/.xearthlayer/config.ini
xearthlayer init
```

## Size Format

Size values support the following suffixes:
- `KB` or `kb` - Kilobytes
- `MB` or `mb` - Megabytes
- `GB` or `gb` - Gigabytes

Examples: `500MB`, `2GB`, `4gb`, `100GB`
