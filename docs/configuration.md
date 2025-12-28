# XEarthLayer Configuration

XEarthLayer uses an INI configuration file located at `~/.xearthlayer/config.ini`. This file is created automatically on first run with sensible defaults.

## Configuration File Location

- **Config file**: `~/.xearthlayer/config.ini`
- **Log file**: `~/.xearthlayer/xearthlayer.log` (default)
- **Cache directory**: `~/.cache/xearthlayer/` (default)

For best results, set the cache directory to a fast NVMe or SSD that you can fill with data and is not the primary volume for your system.

## Sections

### [provider]

Controls which satellite imagery provider to use.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `type` | string | `bing` | Imagery provider (see below for options) |
| `google_api_key` | string | (empty) | Google Maps API key. Required only when `type = google`. |
| `mapbox_access_token` | string | (empty) | MapBox access token. Required only when `type = mapbox`. |

**Available Providers:**

| Provider | API Key | Cost | Coverage | Max Zoom | Notes |
|----------|---------|------|----------|----------|-------|
| `bing` | Not required | Free | Global | 19 | Same source as MSFS 2020/4 |
| `go2` | Not required | Free | Global | 22 | Recommended for most users, Google Maps via public tile servers |
| `google` | Required | Paid | Global | 22 | Official Google Maps API with usage limits |
| `apple` | Not required | Free | Global | 20 | Recommended for high fidelity flying (VFR), high quality imagery, tokens auto-acquired |
| `arcgis` | Not required | Free | Global | 19 | ESRI World Imagery service |
| `mapbox` | Required | Freemium | Global | 22 | MapBox Satellite, requires access token |
| `usgs` | Not required | Free | US only | 16 | USGS orthoimagery, excellent quality for US |

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

Apple Maps (auto-acquires tokens):
```ini
[provider]
type = apple
```

ArcGIS World Imagery:
```ini
[provider]
type = arcgis
```

MapBox Satellite (requires token):
```ini
[provider]
type = mapbox
mapbox_access_token = pk.eyJ1...your-token-here
```

USGS (US coverage only):
```ini
[provider]
type = usgs
```

**Provider Notes:**

- **Bing Maps** - Recommended for general use due to reliability and global coverage.
- **GO2** - Recommended for most users, the same Google tile servers.
- **Google official API** - Strict rate limits (15,000 requests/day). With 256 chunks per tile, allows ~58 tiles/day.
- **Apple Maps** - Automatically acquires access tokens via DuckDuckGo's MapKit integration. Tokens refresh automatically on authentication errors.
- **ArcGIS** - ESRI's World Imagery service, good global coverage with no authentication required.
- **MapBox** - Requires a free account at mapbox.com. Free tier includes 200,000 tile requests/month.
- **USGS** - Excellent quality orthoimagery for the United States only. Will fail for non-US locations.

### [cache]

Controls tile caching behavior.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `directory` | path | `~/.cache/xearthlayer` | Directory for storing cached tiles. Supports `~` expansion. |
| `memory_size` | size | `2GB` | Maximum RAM for in-memory cache. Supports KB, MB, GB suffixes. |
| `disk_size` | size | `20GB` | Maximum disk space for persistent cache. Supports KB, MB, GB suffixes. |
| `disk_io_profile` | string | `auto` | Disk I/O concurrency profile based on storage type (see below) |

**Disk I/O Profile:**

The `disk_io_profile` setting tunes disk I/O concurrency based on your storage type. Different storage devices have vastly different optimal concurrency levels:

| Profile | Description | Concurrent Ops | Best For |
|---------|-------------|----------------|----------|
| `auto` | Auto-detect storage type (recommended) | Varies | Most users |
| `hdd` | Spinning disk, seek-bound | 1-4 | Traditional hard drives |
| `ssd` | SATA/AHCI SSD | 32-64 | Most SSDs |
| `nvme` | NVMe SSD, multiple queues | 128-256 | NVMe drives |

**Auto-detection (Linux):** When set to `auto`, XEarthLayer detects the storage type by checking `/sys/block/<device>/queue/rotational`. If detection fails, it defaults to the `ssd` profile as a safe middle-ground.

**Example:**
```ini
[cache]
; Use custom cache location on NVMe drive
directory = /mnt/nvme/xearthlayer-cache
memory_size = 8GB
disk_size = 50GB
disk_io_profile = auto
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

### [pipeline]

Advanced concurrency and retry settings for the tile processing pipeline. These defaults are tuned for most systems; only modify if you understand the implications.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `max_http_concurrent` | integer | `128` | Maximum concurrent HTTP requests (hard limits: 64-256) |
| `max_cpu_concurrent` | integer | `num_cpus * 1.25` | Maximum concurrent CPU-bound operations (assembly + encoding) |
| `max_prefetch_in_flight` | integer | `max(num_cpus / 4, 2)` | Maximum concurrent prefetch jobs |
| `request_timeout_secs` | integer | `10` | HTTP request timeout for individual chunk downloads (seconds) |
| `max_retries` | integer | `3` | Maximum retry attempts per failed chunk download |
| `retry_base_delay_ms` | integer | `100` | Base delay for exponential backoff (actual = base * 2^attempt) |
| `coalesce_channel_capacity` | integer | `16` | Broadcast channel capacity for request coalescing |

**Example:**
```ini
[pipeline]
; Increase HTTP concurrency for fast connections
max_http_concurrent = 128
; Allow more prefetch jobs on high-core-count systems
max_prefetch_in_flight = 8
; Increase timeout for slow connections
request_timeout_secs = 15
```

**Concurrency Settings:**
- **HTTP concurrency**: Default 128, hard limits 64-256. Values outside this range are automatically clamped. The ceiling prevents overwhelming imagery providers with too many requests (causes HTTP 429 rate limiting and cascade failures).
- **CPU concurrency**: `num_cpus * 1.25` - modest oversubscription for blocking thread pool efficiency
- **Prefetch in-flight**: `max(num_cpus / 4, 2)` - leaves 75% of resources for on-demand requests

### [control_plane]

Controls the pipeline control plane for job management, health monitoring, and deadlock prevention. The control plane limits how many tiles can be processed simultaneously and recovers stalled jobs.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `max_concurrent_jobs` | integer | `num_cpus × 2` | Maximum concurrent tile processing jobs |
| `stall_threshold_secs` | integer | `60` | Time in seconds before a job is considered stalled |
| `health_check_interval_secs` | integer | `5` | Interval between health checks (seconds) |
| `semaphore_timeout_secs` | integer | `30` | Timeout for job slot acquisition (seconds) |

**Example:**
```ini
[control_plane]
; Increase concurrent jobs for high-core-count systems
max_concurrent_jobs = 32
; Increase stall threshold for slow networks
stall_threshold_secs = 120
```

**How it works:**
- **Job limiting**: Prevents unbounded tile starts that would overwhelm downstream resources. With 8 CPUs, defaults to 16 concurrent tiles.
- **Stall detection**: Jobs exceeding `stall_threshold_secs` are cancelled and recovered automatically.
- **Prefetch behavior**: Prefetch jobs use non-blocking slot acquisition - if no slots are available, they're skipped rather than waiting.
- **On-demand behavior**: On-demand requests (from X-Plane) block up to `semaphore_timeout_secs` waiting for a slot.

**Dashboard display:**
The TUI dashboard shows control plane health including:
- Jobs in progress / max concurrent
- Jobs recovered (stall detection)
- Semaphore timeouts
- Health status (Healthy, Degraded, Critical)

### [prefetch]

Controls predictive tile prefetching based on X-Plane telemetry. The prefetch system pre-loads tiles ahead of the aircraft to reduce FPS drops when new scenery loads.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `enabled` | bool | `true` | Enable/disable predictive tile prefetching |
| `strategy` | string | `auto` | Prefetch strategy: `auto`, `heading-aware`, or `radial` |
| `udp_port` | integer | `49002` | UDP port for X-Plane telemetry (ForeFlight protocol) |
| `cone_angle` | float | `45` | Prediction cone half-angle in degrees |
| `inner_radius_nm` | float | `85` | Inner edge of prefetch zone (nautical miles) |
| `outer_radius_nm` | float | `95` | Outer edge of prefetch zone (nautical miles) |
| `max_tiles_per_cycle` | integer | `50` | Maximum tiles to submit per prefetch cycle |
| `cycle_interval_ms` | integer | `2000` | Interval between prefetch cycles (milliseconds) |
| `radial_radius` | integer | `3` | Radial fallback tile radius (3 = 7×7 = 49 tiles) |
| `enable_zl12` | bool | `true` | Enable ZL12 prefetching for distant terrain |
| `zl12_inner_radius_nm` | float | `88` | Inner edge of ZL12 prefetch zone (nautical miles) |
| `zl12_outer_radius_nm` | float | `100` | Outer edge of ZL12 prefetch zone (nautical miles) |

**Strategy Options:**

| Strategy | Description |
|----------|-------------|
| `auto` | Heading-aware with graceful degradation to radial (recommended) |
| `heading-aware` | Direction-aware cone prefetching, requires telemetry |
| `radial` | Simple grid around current position, works without telemetry |

**Multi-Zoom Prefetching:**

XEarthLayer prefetches tiles at two zoom levels:
- **ZL14** (primary): High-resolution tiles for nearby scenery (85-95nm)
- **ZL12** (secondary): Low-resolution tiles for distant scenery (88-100nm)

This eliminates stutters when transitioning between zoom levels at the ~90nm boundary.

**Example:**
```ini
[prefetch]
enabled = true
strategy = auto
udp_port = 49002

; Primary prefetch zone (ZL14)
cone_angle = 45
inner_radius_nm = 85
outer_radius_nm = 95
max_tiles_per_cycle = 50
cycle_interval_ms = 2000

; Secondary prefetch zone (ZL12) for distant terrain
enable_zl12 = true
zl12_inner_radius_nm = 88
zl12_outer_radius_nm = 100
```

**X-Plane Setup:**
To enable prefetching, configure X-Plane to send ForeFlight telemetry:
1. Go to **Settings → Network**
2. Enable **Send to ForeFlight**
3. XEarthLayer will receive position/heading updates on UDP port 49002

**Note:** Even without telemetry, XEarthLayer can infer aircraft position from FUSE file access patterns (FUSE inference mode) and fall back to radial prefetching if needed

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
; disk_io_profile = auto  ; auto-detect storage type (default)

[texture]
format = bc1
mipmaps = 5

[download]
timeout = 30
retries = 3

[generation]
threads = 8
timeout = 10

[pipeline]
; Advanced concurrency settings (defaults are tuned for most systems)
; max_http_concurrent = 256
; max_cpu_concurrent = 20
; max_prefetch_in_flight = 4
; request_timeout_secs = 10
; max_retries = 3
; retry_base_delay_ms = 100

[control_plane]
; Job management and health monitoring (defaults are tuned for most systems)
; max_concurrent_jobs = 16  ; num_cpus × 2
; stall_threshold_secs = 60
; health_check_interval_secs = 5
; semaphore_timeout_secs = 30

[prefetch]
; Predictive tile prefetching based on X-Plane telemetry
enabled = true
strategy = auto  ; auto, heading-aware, or radial
; udp_port = 49002

; Primary prefetch zone (ZL14 - high resolution near scenery)
; cone_angle = 45              ; half-angle of prediction cone (degrees)
; inner_radius_nm = 85         ; start prefetching just inside 90nm boundary
; outer_radius_nm = 95         ; 10nm prefetch depth
; max_tiles_per_cycle = 50     ; tiles per cycle (lower = less bandwidth)
; cycle_interval_ms = 2000     ; cycle interval (higher = less aggressive)

; Secondary prefetch zone (ZL12 - low resolution distant scenery)
enable_zl12 = true            ; prefetch distant terrain tiles
; zl12_inner_radius_nm = 88
; zl12_outer_radius_nm = 100

; Radial fallback (when telemetry unavailable)
; radial_radius = 3            ; 7×7 tile grid

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

### Upgrade Configuration

When XEarthLayer is updated, new configuration settings may be added. The `config upgrade` command safely adds missing settings with their default values while preserving your existing settings:

```bash
# Preview what would change (dry run)
xearthlayer config upgrade --dry-run

# Perform the upgrade
xearthlayer config upgrade
```

**What it does:**
- **Adds missing settings** with sensible default values
- **Preserves all your existing settings** unchanged
- **Creates a timestamped backup** before modifying (e.g., `config.ini.backup.1735315200`)
- **Flags unknown settings** in case of typos (but doesn't remove them)

**Startup warning:** XEarthLayer will warn you on startup if your configuration is missing new settings:
```
Warning: Your configuration file is missing 3 new setting(s).
Run 'xearthlayer config upgrade' to update your configuration.
```

### Available Configuration Keys

| Key | Valid Values | Description |
|-----|--------------|-------------|
| `provider.type` | `apple`, `arcgis`, `bing`, `go2`, `google`, `mapbox`, `usgs` | Imagery provider |
| `provider.google_api_key` | string | Google Maps API key |
| `provider.mapbox_access_token` | string | MapBox access token |
| `cache.directory` | path | Cache directory |
| `cache.memory_size` | size (e.g., `2GB`) | Memory cache size |
| `cache.disk_size` | size (e.g., `20GB`) | Disk cache size |
| `cache.disk_io_profile` | `auto`, `hdd`, `ssd`, `nvme` | Disk I/O concurrency profile |
| `texture.format` | `bc1`, `bc3` | DDS compression format |
| `download.timeout` | positive integer | Chunk download timeout (seconds) |
| `generation.threads` | positive integer | Worker threads |
| `generation.timeout` | positive integer | Tile generation timeout (seconds) |
| `pipeline.max_http_concurrent` | positive integer | Max concurrent HTTP requests |
| `pipeline.max_cpu_concurrent` | positive integer | Max concurrent CPU operations |
| `pipeline.max_prefetch_in_flight` | positive integer | Max concurrent prefetch jobs |
| `pipeline.request_timeout_secs` | positive integer | HTTP request timeout (seconds) |
| `pipeline.max_retries` | positive integer | Max retry attempts |
| `pipeline.retry_base_delay_ms` | positive integer | Retry backoff base delay (ms) |
| `pipeline.coalesce_channel_capacity` | positive integer | Coalesce channel capacity |
| `control_plane.max_concurrent_jobs` | positive integer | Max concurrent tile jobs |
| `control_plane.stall_threshold_secs` | positive integer | Job stall timeout (seconds) |
| `control_plane.health_check_interval_secs` | positive integer | Health check interval (seconds) |
| `control_plane.semaphore_timeout_secs` | positive integer | Slot acquisition timeout (seconds) |
| `prefetch.enabled` | `true`, `false` | Enable predictive prefetching |
| `prefetch.strategy` | `auto`, `heading-aware`, `radial` | Prefetch strategy |
| `prefetch.udp_port` | positive integer | X-Plane telemetry UDP port |
| `prefetch.cone_angle` | positive number | Prediction cone half-angle (degrees) |
| `prefetch.inner_radius_nm` | positive number | Inner edge of prefetch zone (nm) |
| `prefetch.outer_radius_nm` | positive number | Outer edge of prefetch zone (nm) |
| `prefetch.max_tiles_per_cycle` | positive integer | Max tiles per prefetch cycle |
| `prefetch.cycle_interval_ms` | positive integer | Prefetch cycle interval (ms) |
| `prefetch.radial_radius` | positive integer | Radial fallback tile radius |
| `prefetch.enable_zl12` | `true`, `false` | Enable ZL12 distant terrain prefetch |
| `prefetch.zl12_inner_radius_nm` | positive number | ZL12 zone inner edge (nm) |
| `prefetch.zl12_outer_radius_nm` | positive number | ZL12 zone outer edge (nm) |
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

# Use MapBox with token
xearthlayer start --source ./scenery --provider mapbox --mapbox-token YOUR_TOKEN

# Use Apple Maps (no key needed)
xearthlayer start --source ./scenery --provider apple

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
