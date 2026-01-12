# XEarthLayer Configuration

XEarthLayer uses an INI configuration file located at `~/.xearthlayer/config.ini`.

## Setup Wizard (Recommended)

The easiest way to configure XEarthLayer is with the interactive setup wizard:

```bash
xearthlayer setup
```

The wizard auto-detects your X-Plane installation, system hardware (CPU, memory, storage type), and recommends optimal settings. It handles all the configuration below automatically.

## Manual Configuration

For users who prefer manual configuration, this file is created with `xearthlayer init` and can be edited directly.

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

### [pipeline] (DEPRECATED)

> **Deprecated in v0.3.0**: The `[pipeline]` section is deprecated and will be removed in a future release. The pipeline module has been replaced by the new job executor daemon architecture, which uses internal defaults via `ResourcePoolConfig`. These settings are parsed but no longer have any effect.
>
> Run `xearthlayer config upgrade` to remove deprecated settings from your configuration file.

| Setting | Status | Notes |
|---------|--------|-------|
| `max_http_concurrent` | Deprecated | Executor uses internal HTTP concurrency limits |
| `max_cpu_concurrent` | Deprecated | Executor uses `ResourcePoolConfig` defaults |
| `max_prefetch_in_flight` | Deprecated | Prefetch concurrency is now automatic |
| `request_timeout_secs` | Deprecated | Timeout configured via `control_plane` settings |
| `max_retries` | Deprecated | Retry logic built into executor tasks |
| `retry_base_delay_ms` | Deprecated | Built into executor retry policy |
| `coalesce_channel_capacity` | Deprecated | Request coalescing uses dynamic buffering |

### [control_plane]

Controls the job executor daemon for concurrent tile processing and health monitoring.

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

### [executor]

Controls the job executor daemon's resource pools and concurrency limits. The executor is the core tile processing engine that manages parallel downloads, encoding, and caching.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `network_concurrent` | integer | `128` | Concurrent HTTP connections (clamped to 64-256) |
| `cpu_concurrent` | integer | `num_cpus × 1.25` | Concurrent CPU-bound operations (assemble + encode) |
| `disk_io_concurrent` | integer | `64` | Concurrent disk I/O operations (auto-detected from storage type) |
| `max_concurrent_tasks` | integer | `128` | Maximum concurrent tasks the executor can run |
| `job_channel_capacity` | integer | `256` | Internal job queue size |
| `request_channel_capacity` | integer | `1000` | External request queue (from FUSE/prefetch) |
| `request_timeout_secs` | integer | `10` | HTTP request timeout per chunk (seconds) |
| `max_retries` | integer | `3` | Maximum retry attempts per failed chunk |
| `retry_base_delay_ms` | integer | `100` | Base delay for exponential backoff (ms) |

**Example:**
```ini
[executor]
; Resource pool sizing (defaults are tuned for most systems)
network_concurrent = 128         ; HTTP connections (64-256 range)
cpu_concurrent = 10              ; CPU-bound ops (defaults to ~num_cpus * 1.25)
disk_io_concurrent = 64          ; Disk I/O (auto-detected from storage type)

; Task scheduling
max_concurrent_tasks = 128       ; Max tasks in flight
job_channel_capacity = 256       ; Internal job queue
request_channel_capacity = 1000  ; External request queue

; Download behavior
request_timeout_secs = 10        ; Per-chunk timeout
max_retries = 3                  ; Retry attempts
retry_base_delay_ms = 100        ; Exponential backoff base (100ms, 200ms, 400ms...)
```

**Resource Pool Details:**

- **Network pool**: Limits concurrent HTTP connections to prevent overwhelming imagery providers. Values outside 64-256 are automatically clamped.
- **CPU pool**: Limits concurrent encoding operations. Default formula: `max(num_cpus × 1.25, num_cpus + 2)`.
- **Disk I/O pool**: Limits concurrent disk operations. Auto-detected from storage type (HDD: 4, SSD: 64, NVMe: 256).

**Retry Behavior:**

Failed chunk downloads are retried with exponential backoff:
- Attempt 1: immediate
- Attempt 2: 100ms delay
- Attempt 3: 200ms delay
- Attempt 4: 400ms delay

### [prefetch]

Controls predictive tile prefetching based on X-Plane telemetry. The prefetch system pre-loads tiles ahead of the aircraft to reduce FPS drops when new scenery loads.

**Prefetch Architecture:**

XEarthLayer uses a ring-based prefetch system that targets the boundary around X-Plane's ~90nm loaded scenery:

![Heading-Aware Prefetch Zones](images/heading-aware-prefetch.png)

- **Prefetch Zone (85-180nm)**: Ring around aircraft where tiles are prefetched
- **Heading Cone (80° half-angle)**: Forward-biased prefetching along flight path

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `enabled` | bool | `true` | Enable/disable predictive tile prefetching |
| `strategy` | string | `auto` | Prefetch strategy: `auto`, `heading-aware`, or `radial` |
| `udp_port` | integer | `49002` | UDP port for X-Plane telemetry (ForeFlight protocol) |
| `inner_radius_nm` | float | `85` | Inner edge of prefetch zone (5nm inside X-Plane's 90nm boundary) |
| `outer_radius_nm` | float | `180` | Outer edge of prefetch zone |
| `cone_angle` | float | `80` | Half-angle of heading cone (80° = 160° total width) |
| `radial_radius` | integer | `120` | Radial prefetcher tile radius (tiles in each direction) |
| `max_tiles_per_cycle` | integer | `200` | Maximum tiles to submit per prefetch cycle |
| `cycle_interval_ms` | integer | `2000` | Interval between prefetch cycles (milliseconds) |
| `circuit_breaker_threshold` | float | `50.0` | FUSE jobs/sec threshold to pause prefetching |
| `circuit_breaker_open_ms` | integer | `500` | Duration (ms) high load must be sustained to pause |
| `circuit_breaker_half_open_secs` | integer | `2` | Cooloff time (secs) before resuming prefetch |

**Strategy Options:**

| Strategy | Description |
|----------|-------------|
| `auto` | Heading-aware with graceful degradation to radial (recommended) |
| `heading-aware` | Direction-aware cone prefetching, requires telemetry |
| `radial` | Simple grid around current position, works without telemetry |

**Circuit Breaker:**

The circuit breaker automatically pauses prefetching when X-Plane is loading scenery (detected by high FUSE request rate). This prevents prefetch from competing with X-Plane's direct tile requests:

- **Closed (Active)**: Normal prefetching, FUSE rate below threshold
- **Open (Paused)**: Prefetching paused, FUSE rate exceeded threshold
- **Half-Open (Resuming)**: Testing if safe to resume after cooloff

**Zoom Level Handling:**

XEarthLayer automatically prefetches tiles at the correct zoom levels by reading the `.ter` files in your scenery packages. The SceneryIndex knows exactly which tiles exist and at what zoom level, so no manual zoom level configuration is needed.

**Example:**
```ini
[prefetch]
enabled = true
strategy = auto
udp_port = 49002

; Prefetch zone boundaries (nautical miles)
inner_radius_nm = 85           ; Start prefetching 5nm inside X-Plane's 90nm boundary
outer_radius_nm = 180          ; Extended look-ahead coverage

; Heading-aware cone
cone_angle = 80                ; 160° total cone width for forward prefetching

; Radial prefetcher (fallback when no telemetry)
radial_radius = 120            ; Tile radius for radial prefetching

; Rate limiting
max_tiles_per_cycle = 200      ; Tiles per cycle
cycle_interval_ms = 2000       ; Cycle interval (ms)

; Circuit breaker (pause prefetch during scene loading)
circuit_breaker_threshold = 50.0      ; FUSE jobs/sec to trigger pause
circuit_breaker_open_ms = 500         ; Sustained load duration to open
circuit_breaker_half_open_secs = 2    ; Cooloff before resuming
```

**X-Plane Setup:**
To enable prefetching, configure X-Plane to send ForeFlight telemetry:
1. Go to **Settings → Network**
2. Enable **Send to ForeFlight**
3. XEarthLayer will receive position/heading updates on UDP port 49002

**Note:** Even without telemetry, XEarthLayer can infer aircraft position from FUSE file access patterns (FUSE inference mode) and fall back to radial prefetching if needed

### [prewarm]

Controls cold-start cache pre-warming. Use with `xearthlayer run --airport ICAO` to pre-load tiles around an airport before X-Plane starts, reducing initial scenery load times.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `radius_nm` | float | `100` | Radius in nautical miles around the airport to prewarm |

**Understanding X-Plane's DSF Loading:**

X-Plane loads terrain in DSF (Digital Scenery File) tiles, each covering 1° × 1° of lat/lon. The number of tiles loaded depends on the "Extended DSFs" setting:

| Setting | Tiles Loaded | Approximate Area |
|---------|--------------|------------------|
| Standard | 3×2 = 6 tiles | ~180nm × 120nm (equator) |
| Extended DSFs | 4×3 = 12 tiles | ~240nm × 180nm (equator) |

At mid-latitudes (~47°N, typical for Europe), this translates to roughly:
- **Standard**: ~90nm radius equivalent
- **Extended**: ~120nm radius equivalent

The default 100nm prewarm radius is optimized for standard DSF loading—it covers the initial scenery load without wasting bandwidth on tiles X-Plane won't request until you fly further.

**Dynamic Resolution via LOAD_CENTER:**

X-Plane's orthophoto textures use `LOAD_CENTER` directives that enable dynamic resolution loading. As you approach a tile, X-Plane progressively loads higher resolution versions. This means:

1. Distant tiles load at low resolution (fast, small)
2. Nearby tiles load at full resolution (slower, larger)
3. XEarthLayer's prewarm prepares the full-resolution versions for immediate use

**Example:**
```ini
[prewarm]
; Pre-warm tiles within 100nm of departure airport
; This covers X-Plane's standard 3×2 DSF loading region
radius_nm = 100
```

**Increasing the Radius:**

If you use Extended DSFs or want more aggressive pre-warming:
```ini
[prewarm]
radius_nm = 150  ; Cover extended DSF region
```

Note: Larger radii increase pre-warm time and bandwidth usage proportionally (area scales with radius²).

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
| `library_url` | URL | `https://xearthlayer.app/packages/xearthlayer_package_library.txt` | URL to the package library index file |
| `install_location` | path | `~/.xearthlayer/packages` | Directory for storing installed packages |
| `custom_scenery_path` | path | (auto-detect) | X-Plane Custom Scenery directory for overlay symlinks |
| `auto_install_overlays` | bool | `false` | Automatically install matching overlay when installing ortho |
| `temp_dir` | path | system temp | Temporary directory for package downloads |

**Example:**
```ini
[packages]
; library_url defaults to https://xearthlayer.app/packages/xearthlayer_package_library.txt
install_location = ~/.xearthlayer/packages
custom_scenery_path = /home/user/X-Plane 12/Custom Scenery
auto_install_overlays = true
temp_dir = ~/Downloads/xearthlayer-temp
```

**Notes:**
- The default `library_url` points to the official XEarthLayer package library; override only if using a custom package source
- The `temp_dir` is used for downloading archives before extraction; files are cleaned up after installation
- When `auto_install_overlays` is enabled, installing an ortho package will automatically install the matching overlay package for the same region (if available)
- If `custom_scenery_path` is not set, it falls back to `[xplane] scenery_dir` or auto-detects from X-Plane installation

---

### `[patches]` - Tile Patches

Settings for tile patches - pre-built Ortho4XP tiles with custom mesh/elevation from airport addons.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable/disable patches functionality |
| `directory` | path | `~/.xearthlayer/patches` | Directory containing patch tiles |

**Example:**
```ini
[patches]
enabled = true
directory = ~/.xearthlayer/patches
```

**Notes:**
- Patches allow using custom elevation/mesh from airport addons (like SFD KLAX) while XEL generates textures
- Each patch folder must contain `Earth nav data/` with DSF files
- Patches are merged using a union filesystem; alphabetically-first folder wins on collision
- See [docs/patches.md](patches.md) for detailed usage instructions

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

; [pipeline] section is DEPRECATED in v0.3.0 - settings are no longer used
; Run 'xearthlayer config upgrade' to remove deprecated settings

[control_plane]
; Job management and health monitoring (defaults are tuned for most systems)
; max_concurrent_jobs = 16  ; num_cpus × 2
; stall_threshold_secs = 60
; health_check_interval_secs = 5
; semaphore_timeout_secs = 30

[executor]
; Resource pools for job executor daemon (defaults are auto-tuned)
; network_concurrent = 128         ; HTTP connections (64-256 range)
; cpu_concurrent = 10              ; CPU-bound ops (~num_cpus * 1.25)
; disk_io_concurrent = 64          ; Disk I/O (auto-detected)
; max_concurrent_tasks = 128       ; Max tasks in flight
; job_channel_capacity = 256       ; Internal job queue
; request_channel_capacity = 1000  ; External request queue
; request_timeout_secs = 10        ; Per-chunk HTTP timeout
; max_retries = 3                  ; Download retry attempts
; retry_base_delay_ms = 100        ; Backoff base delay

[prefetch]
; Predictive tile prefetching based on X-Plane telemetry
enabled = true
strategy = auto  ; auto, heading-aware, or radial
; udp_port = 49002

; Prefetch zone boundaries (nautical miles)
; inner_radius_nm = 85         ; inner edge (5nm inside X-Plane's 90nm zone)
; outer_radius_nm = 180        ; outer edge of prefetch zone

; Heading-aware cone
; cone_angle = 80              ; half-angle of cone (160° total width)

; Radial prefetcher
; radial_radius = 120          ; tile radius for radial prefetching

; Rate limiting
; max_tiles_per_cycle = 200    ; tiles per cycle (lower = less bandwidth)
; cycle_interval_ms = 2000     ; cycle interval (higher = less aggressive)

; Circuit breaker (pause prefetch during scene loading)
; circuit_breaker_threshold = 50.0      ; FUSE jobs/sec to trigger
; circuit_breaker_open_ms = 500         ; duration to sustain before pause
; circuit_breaker_half_open_secs = 2    ; cooloff before resuming

[prewarm]
; Cold-start cache pre-warming (use with --airport ICAO)
; radius_nm = 100              ; Pre-warm radius (100nm covers standard DSF loading)

[xplane]
; scenery_dir = /path/to/X-Plane 12/Custom Scenery

[logging]
file = ~/.xearthlayer/xearthlayer.log

[packages]
; library_url defaults to https://xearthlayer.app/packages/xearthlayer_package_library.txt
; install_location = ~/.xearthlayer/packages
; custom_scenery_path = /path/to/X-Plane 12/Custom Scenery
; auto_install_overlays = true
; temp_dir = ~/Downloads/xearthlayer-temp

[patches]
; Tile patches for custom mesh/elevation from airport addons
enabled = true
; directory = ~/.xearthlayer/patches
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
| `pipeline.*` | *(deprecated)* | All pipeline settings deprecated in v0.3.0 |
| `control_plane.max_concurrent_jobs` | positive integer | Max concurrent tile jobs |
| `control_plane.stall_threshold_secs` | positive integer | Job stall timeout (seconds) |
| `control_plane.health_check_interval_secs` | positive integer | Health check interval (seconds) |
| `control_plane.semaphore_timeout_secs` | positive integer | Slot acquisition timeout (seconds) |
| `executor.network_concurrent` | positive integer | Concurrent HTTP connections (64-256) |
| `executor.cpu_concurrent` | positive integer | Concurrent CPU-bound operations |
| `executor.disk_io_concurrent` | positive integer | Concurrent disk I/O operations |
| `executor.max_concurrent_tasks` | positive integer | Max concurrent tasks |
| `executor.job_channel_capacity` | positive integer | Internal job queue size |
| `executor.request_channel_capacity` | positive integer | External request queue size |
| `executor.request_timeout_secs` | positive integer | Per-chunk HTTP timeout (seconds) |
| `executor.max_retries` | positive integer | Max retry attempts per chunk |
| `executor.retry_base_delay_ms` | positive integer | Exponential backoff base (ms) |
| `prefetch.enabled` | `true`, `false` | Enable predictive prefetching |
| `prefetch.strategy` | `auto`, `heading-aware`, `radial` | Prefetch strategy |
| `prefetch.udp_port` | positive integer | X-Plane telemetry UDP port |
| `prefetch.inner_radius_nm` | positive number | Inner edge of prefetch zone (nm) |
| `prefetch.outer_radius_nm` | positive number | Outer edge of prefetch zone (nm) |
| `prefetch.cone_angle` | positive number | Half-angle of heading cone (degrees) |
| `prefetch.radial_radius` | positive integer | Radial prefetcher tile radius |
| `prefetch.max_tiles_per_cycle` | positive integer | Max tiles per prefetch cycle |
| `prefetch.cycle_interval_ms` | positive integer | Prefetch cycle interval (ms) |
| `prefetch.circuit_breaker_threshold` | positive number | FUSE jobs/sec to pause prefetch |
| `prefetch.circuit_breaker_open_ms` | positive integer | Sustained load duration (ms) |
| `prefetch.circuit_breaker_half_open_secs` | positive integer | Cooloff time (secs) |
| `prewarm.radius_nm` | positive number | Pre-warm radius around airport (nm) |
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

Or use the setup wizard for guided reconfiguration:

```bash
xearthlayer setup  # Offers to reconfigure or backup existing config
```

## Size Format

Size values support the following suffixes:
- `KB` or `kb` - Kilobytes
- `MB` or `mb` - Megabytes
- `GB` or `gb` - Gigabytes

Examples: `500MB`, `2GB`, `4gb`, `100GB`
