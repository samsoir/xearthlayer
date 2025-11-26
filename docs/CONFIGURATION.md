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
| `type` | string | `bing` | Imagery provider: `bing` (free) or `google` (paid, requires API key) |
| `google_api_key` | string | (empty) | Google Maps API key. Required when `type = google`. Get one at [Google Cloud Console](https://console.cloud.google.com) (enable Map Tiles API) |

**Example:**
```ini
[provider]
type = bing
```

For Google Maps:
```ini
[provider]
type = google
google_api_key = AIzaSy...your-key-here
```

**Note:** Google Maps has strict rate limits (15,000 requests/day for 2D tiles). With 256 chunks per tile, this allows approximately 58 tiles per day. Bing Maps is recommended for most users.

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
| `parallel` | integer | `32` | Maximum concurrent chunk downloads per tile |
| `retries` | integer | `3` | Number of retry attempts for failed downloads |

**Example:**
```ini
[download]
timeout = 30
parallel = 32
retries = 3
```

**Note:** Each 4096x4096 tile requires downloading 256 chunks (16x16 grid of 256x256 tiles). Higher `parallel` values speed up downloads but use more bandwidth.

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
parallel = 32
retries = 3

[generation]
threads = 8
timeout = 10

[xplane]
; scenery_dir = /path/to/X-Plane 12/Custom Scenery

[logging]
file = ~/.xearthlayer/xearthlayer.log
```

## CLI Overrides

Most configuration settings can be overridden via CLI arguments:

```bash
# Override provider
xearthlayer mount --source ./scenery --provider google --google-api-key YOUR_KEY

# Override DDS format
xearthlayer mount --source ./scenery --dds-format bc3

# Override download settings
xearthlayer mount --source ./scenery --timeout 60 --parallel 16

# Disable caching
xearthlayer mount --source ./scenery --no-cache
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
