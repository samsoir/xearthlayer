# XEarthLayer SOLID Refactoring Strategy

This document outlines a comprehensive strategy to refactor XEarthLayer to better conform to SOLID principles, building on the foundation established in `DESIGN_PRINCIPLES.md`.

## Executive Summary

The codebase has good foundational patterns (Provider trait, Cache trait, HttpClient trait) but several areas leak abstractions and violate SOLID principles. This strategy addresses:

1. **DDS Encoder abstraction** - Extract trait for swappable encoding strategies
2. **TileGenerator abstraction** - Decouple FUSE from orchestration and encoding
3. **Generic type pollution** - Replace `<P: Provider>` with `Arc<dyn Provider>`
4. **Configuration builders** - Group related parameters into cohesive config objects
5. **Factory patterns** - Centralize component creation and reduce CLI duplication

## Current State Analysis

### Areas of Good SOLID Compliance

| Component | Location | Pattern |
|-----------|----------|---------|
| Provider trait | `provider/types.rs` | Good DIP - trait abstraction |
| Cache trait | `cache/trait.rs` | Good LSP - NoOpCache/CacheSystem |
| HttpClient trait | `provider/http.rs` | Good DIP - enables testing |
| DDS internal structure | `dds/encoder.rs` | Good SRP - delegates to Bc1/Bc3 encoders |

### Areas Requiring Refactoring

| Component | Location | Violations | Priority |
|-----------|----------|------------|----------|
| XEarthLayerFS | `fuse/filesystem.rs` | SRP, DIP | HIGH |
| CLI handlers | `main.rs` | SRP, OCP, ISP, DIP | HIGH |
| TileOrchestrator | `orchestrator/download.rs` | DIP (generic pollution) | MEDIUM |
| CacheSystem | `cache/system.rs` | SRP, OCP | LOW |

---

## Phase 1: Extract TextureEncoder Trait

**Goal**: Decouple FUSE filesystem from DDS encoding implementation.

### Current Problem

```rust
// fuse/filesystem.rs - Direct dependency on concrete DdsEncoder
use crate::dds::{DdsEncoder, DdsFormat, DdsHeader};

impl<P: Provider> XEarthLayerFS<P> {
    fn generate_tile_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        // Hard-coded DDS encoding logic
        let encoder = DdsEncoder::new(self.dds_format)
            .with_mipmap_count(self.mipmap_count);
        encoder.encode(&image)
    }
}
```

### Proposed Solution

```rust
// New file: xearthlayer/src/texture/mod.rs

/// Trait for texture encoding strategies.
///
/// Abstracts the encoding of RGBA image data into texture formats
/// suitable for X-Plane consumption.
pub trait TextureEncoder: Send + Sync {
    /// Encode RGBA image data into the target texture format.
    fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError>;

    /// Return the expected file size for a given image dimension.
    fn expected_size(&self, width: u32, height: u32) -> usize;

    /// Return the file extension for this texture format.
    fn extension(&self) -> &str;
}

/// DDS implementation of TextureEncoder
pub struct DdsTextureEncoder {
    format: DdsFormat,
    mipmap_count: usize,
}

impl DdsTextureEncoder {
    pub fn new(format: DdsFormat) -> Self {
        Self { format, mipmap_count: 5 }
    }

    pub fn with_mipmap_count(mut self, count: usize) -> Self {
        self.mipmap_count = count;
        self
    }
}

impl TextureEncoder for DdsTextureEncoder {
    fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
        let encoder = DdsEncoder::new(self.format)
            .with_mipmap_count(self.mipmap_count);
        encoder.encode(image).map_err(TextureError::from)
    }

    fn expected_size(&self, width: u32, height: u32) -> usize {
        // Calculate based on format and mipmaps
        DdsHeader::new(width, height, self.format, self.mipmap_count)
            .total_size()
    }

    fn extension(&self) -> &str {
        "dds"
    }
}
```

### Benefits

- **OCP**: Add KTX2 encoder without modifying FUSE code
- **DIP**: FUSE depends on `TextureEncoder` trait, not `DdsEncoder`
- **Testability**: Mock encoder for FUSE tests

### Files to Create/Modify

| File | Action |
|------|--------|
| `xearthlayer/src/texture/mod.rs` | CREATE - TextureEncoder trait |
| `xearthlayer/src/texture/dds.rs` | CREATE - DdsTextureEncoder impl |
| `xearthlayer/src/texture/error.rs` | CREATE - TextureError type |
| `xearthlayer/src/fuse/filesystem.rs` | MODIFY - Use Arc<dyn TextureEncoder> |
| `xearthlayer/src/lib.rs` | MODIFY - Export texture module |

---

## Phase 2: Extract TileGenerator Trait

**Goal**: Decouple FUSE from tile generation (download + encode) logic.

### Current Problem

```rust
// fuse/filesystem.rs - Multiple responsibilities
impl<P: Provider> XEarthLayerFS<P> {
    fn generate_tile_dds(&self, coords: &DdsFilename) -> Vec<u8> {
        // 1. Convert coordinates
        let tile = lat_lon_to_tile(coords.row, coords.col, coords.zoom);

        // 2. Download via orchestrator
        let image = self.orchestrator.download_tile(&tile)?;

        // 3. Encode to DDS
        let encoder = DdsEncoder::new(self.dds_format);
        encoder.encode(&image)
    }
}
```

### Proposed Solution

```rust
// New file: xearthlayer/src/tile/generator.rs

/// Trait for generating tiles from coordinates.
///
/// Encapsulates the full pipeline: coordinate conversion → download → encode.
pub trait TileGenerator: Send + Sync {
    /// Generate a tile for the given coordinates.
    fn generate(&self, coords: &TileRequest) -> Result<Vec<u8>, TileGeneratorError>;

    /// Return expected tile size for cache allocation.
    fn expected_size(&self) -> usize;
}

/// Request for tile generation.
pub struct TileRequest {
    pub lat: i32,
    pub lon: i32,
    pub zoom: u8,
}

impl From<&DdsFilename> for TileRequest {
    fn from(filename: &DdsFilename) -> Self {
        Self {
            lat: filename.row,
            lon: filename.col,
            zoom: filename.zoom,
        }
    }
}

/// Default implementation using orchestrator + encoder.
pub struct DefaultTileGenerator<P: Provider> {
    orchestrator: Arc<TileOrchestrator<P>>,
    encoder: Arc<dyn TextureEncoder>,
}

impl<P: Provider> DefaultTileGenerator<P> {
    pub fn new(
        orchestrator: Arc<TileOrchestrator<P>>,
        encoder: Arc<dyn TextureEncoder>,
    ) -> Self {
        Self { orchestrator, encoder }
    }
}

impl<P: Provider + 'static> TileGenerator for DefaultTileGenerator<P> {
    fn generate(&self, coords: &TileRequest) -> Result<Vec<u8>, TileGeneratorError> {
        // Convert lat/lon to tile coordinates
        let tile = lat_lon_to_tile(coords.lat, coords.lon, coords.zoom);

        // Download tile image
        let image = self.orchestrator.download_tile(&tile)?;

        // Encode to texture format
        let data = self.encoder.encode(&image)?;

        Ok(data)
    }

    fn expected_size(&self) -> usize {
        self.encoder.expected_size(4096, 4096)
    }
}
```

### Simplified FUSE

```rust
// fuse/filesystem.rs - Now only handles FUSE operations
pub struct XEarthLayerFS {
    generator: Arc<dyn TileGenerator>,
    cache: Arc<dyn Cache>,
    inode_cache: Arc<Mutex<HashMap<u64, DdsFilename>>>,
}

impl XEarthLayerFS {
    pub fn new(
        generator: Arc<dyn TileGenerator>,
        cache: Arc<dyn Cache>,
    ) -> Self {
        Self {
            generator,
            cache,
            inode_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Filesystem for XEarthLayerFS {
    fn read(&mut self, _req: &Request, ino: u64, ...) {
        let filename = self.get_filename(ino)?;
        let cache_key = CacheKey::from(&filename);

        // Simple: check cache or generate
        let data = match self.cache.get(&cache_key) {
            Some(data) => data,
            None => {
                let request = TileRequest::from(&filename);
                let data = self.generator.generate(&request)?;
                self.cache.put(cache_key, data.clone())?;
                data
            }
        };

        reply.data(&data[offset..]);
    }
}
```

### Benefits

- **SRP**: FUSE only handles filesystem operations
- **DIP**: FUSE depends on TileGenerator trait
- **Testability**: Mock TileGenerator returns pre-built tile data
- **Flexibility**: Different generators for different scenarios (placeholder, real, cached-only)

---

## Phase 3: Remove Generic Type Pollution

**Goal**: Replace generic `<P: Provider>` with trait objects throughout.

### Current Problem

```rust
// Generic parameter propagates everywhere
pub struct TileOrchestrator<P: Provider> { ... }
pub struct XEarthLayerFS<P: Provider> { ... }
pub struct DefaultTileGenerator<P: Provider> { ... }

// Every function must be generic
fn handle_serve<P: Provider>(...) { ... }
```

### Proposed Solution

```rust
// orchestrator/download.rs
pub struct TileOrchestrator {
    provider: Arc<dyn Provider>,  // Trait object instead of generic
    timeout_secs: u64,
    max_retries_per_chunk: u32,
    max_parallel_downloads: usize,
}

impl TileOrchestrator {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            timeout_secs: 30,
            max_retries_per_chunk: 3,
            max_parallel_downloads: 32,
        }
    }

    pub fn with_config(provider: Arc<dyn Provider>, config: OrchestratorConfig) -> Self {
        Self {
            provider,
            timeout_secs: config.timeout_secs,
            max_retries_per_chunk: config.max_retries,
            max_parallel_downloads: config.parallel_downloads,
        }
    }
}
```

### Trade-offs

| Aspect | Generics | Trait Objects |
|--------|----------|---------------|
| Performance | Monomorphized, inlined | Virtual dispatch |
| Binary size | Larger (code duplication) | Smaller |
| Compile time | Longer | Shorter |
| Flexibility | Compile-time | Runtime |
| API simplicity | Complex signatures | Simple signatures |

For XEarthLayer, trait objects are preferred because:
1. Provider methods are already async/IO-bound (dispatch overhead negligible)
2. Simpler API for library consumers
3. Easier testing with mock providers
4. Runtime provider selection (config-based)

---

## Phase 4: Configuration Builders

**Goal**: Group related parameters into cohesive configuration objects.

### Current Problem

```rust
// main.rs - 9 parameters!
fn handle_serve(
    mountpoint: &str,
    provider: ProviderType,
    google_api_key: Option<String>,
    dds_format: &DdsCompressionFormat,
    mipmap_count: usize,
    timeout: u64,
    retries: usize,
    parallel: usize,
    no_cache: bool,
) { ... }
```

### Proposed Solution

```rust
// New file: xearthlayer/src/config/mod.rs

/// Configuration for texture encoding.
#[derive(Debug, Clone, Builder)]
pub struct TextureConfig {
    #[builder(default = "DdsFormat::Bc1")]
    pub format: DdsFormat,

    #[builder(default = "5")]
    pub mipmap_count: usize,
}

/// Configuration for tile downloading.
#[derive(Debug, Clone, Builder)]
pub struct DownloadConfig {
    #[builder(default = "30")]
    pub timeout_secs: u64,

    #[builder(default = "3")]
    pub max_retries: usize,

    #[builder(default = "32")]
    pub parallel_downloads: usize,
}

/// Configuration for caching.
#[derive(Debug, Clone, Builder)]
pub struct CacheConfig {
    #[builder(default = "true")]
    pub enabled: bool,

    #[builder(default = "2 * 1024 * 1024 * 1024")] // 2GB
    pub memory_size: usize,

    #[builder(default = "20 * 1024 * 1024 * 1024")] // 20GB
    pub disk_size: usize,
}

/// Top-level server configuration.
#[derive(Debug, Clone, Builder)]
pub struct ServerConfig {
    pub mountpoint: PathBuf,
    pub texture: TextureConfig,
    pub download: DownloadConfig,
    pub cache: CacheConfig,
}

impl ServerConfig {
    pub fn builder() -> ServerConfigBuilder {
        ServerConfigBuilder::default()
    }
}
```

### Simplified CLI

```rust
// main.rs - Now clean and focused
fn handle_serve(config: ServerConfig, provider: Arc<dyn Provider>) {
    let encoder = DdsTextureEncoder::new(config.texture.format)
        .with_mipmap_count(config.texture.mipmap_count);

    let orchestrator = TileOrchestrator::with_config(
        provider.clone(),
        config.download.into(),
    );

    let cache = if config.cache.enabled {
        CacheSystem::new(config.cache.into())?
    } else {
        NoOpCache::new(provider.name())
    };

    let generator = DefaultTileGenerator::new(
        Arc::new(orchestrator),
        Arc::new(encoder),
    );

    let fs = XEarthLayerFS::new(
        Arc::new(generator),
        Arc::new(cache),
    );

    fuser::mount2(fs, &config.mountpoint, &[])?;
}
```

---

## Phase 5: Provider Factory

**Goal**: Centralize provider creation and eliminate CLI duplication.

### Current Problem

```rust
// main.rs - Duplicated provider selection in handle_download AND handle_serve
match provider {
    ProviderType::Bing => {
        let provider = BingMapsProvider::new(client.clone());
        // ... 50 lines of setup ...
    }
    ProviderType::Google => {
        if let Some(key) = google_api_key {
            let provider = GoogleMapsProvider::new(client.clone(), key);
            // ... 50 lines of IDENTICAL setup ...
        }
    }
}
```

### Proposed Solution

```rust
// New file: xearthlayer/src/provider/factory.rs

/// Factory for creating provider instances.
pub struct ProviderFactory {
    http_client: Arc<dyn HttpClient>,
}

impl ProviderFactory {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self { http_client }
    }

    /// Create a provider from configuration.
    pub fn create(&self, config: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
        match config {
            ProviderConfig::Bing => {
                Ok(Arc::new(BingMapsProvider::new(self.http_client.clone())))
            }
            ProviderConfig::Google { api_key } => {
                Ok(Arc::new(GoogleMapsProvider::new(
                    self.http_client.clone(),
                    api_key.clone(),
                )))
            }
        }
    }
}

/// Provider configuration variants.
#[derive(Debug, Clone)]
pub enum ProviderConfig {
    Bing,
    Google { api_key: String },
}

impl ProviderConfig {
    pub fn from_cli(provider_type: ProviderType, api_key: Option<String>) -> Result<Self, ConfigError> {
        match provider_type {
            ProviderType::Bing => Ok(Self::Bing),
            ProviderType::Google => {
                let key = api_key.ok_or(ConfigError::MissingApiKey("Google"))?;
                Ok(Self::Google { api_key: key })
            }
        }
    }
}
```

---

## Phase 6: Service Facade

**Goal**: Provide high-level API that encapsulates all component wiring.

### Proposed Solution

```rust
// New file: xearthlayer/src/service.rs

/// High-level facade for XEarthLayer operations.
///
/// Encapsulates all component creation and wiring.
pub struct XEarthLayerService {
    config: ServerConfig,
    provider: Arc<dyn Provider>,
    generator: Arc<dyn TileGenerator>,
    cache: Arc<dyn Cache>,
}

impl XEarthLayerService {
    /// Create a new service from configuration.
    pub fn new(config: ServerConfig, provider_config: ProviderConfig) -> Result<Self, ServiceError> {
        // Create HTTP client
        let http_client = Arc::new(ReqwestClient::with_timeout(
            Duration::from_secs(config.download.timeout_secs),
        )?);

        // Create provider
        let factory = ProviderFactory::new(http_client);
        let provider = factory.create(&provider_config)?;

        // Create encoder
        let encoder: Arc<dyn TextureEncoder> = Arc::new(
            DdsTextureEncoder::new(config.texture.format)
                .with_mipmap_count(config.texture.mipmap_count)
        );

        // Create orchestrator
        let orchestrator = Arc::new(TileOrchestrator::with_config(
            provider.clone(),
            config.download.clone().into(),
        ));

        // Create generator
        let generator: Arc<dyn TileGenerator> = Arc::new(
            DefaultTileGenerator::new(orchestrator, encoder)
        );

        // Create cache
        let cache: Arc<dyn Cache> = if config.cache.enabled {
            Arc::new(CacheSystem::new(CacheConfig::new(provider.name())
                .with_memory_size(config.cache.memory_size)
                .with_disk_size(config.cache.disk_size))?)
        } else {
            Arc::new(NoOpCache::new(provider.name()))
        };

        Ok(Self {
            config,
            provider,
            generator,
            cache,
        })
    }

    /// Start the FUSE filesystem server.
    pub fn serve(&self) -> Result<(), ServiceError> {
        let fs = XEarthLayerFS::new(
            self.generator.clone(),
            self.cache.clone(),
        );

        fuser::mount2(fs, &self.config.mountpoint, &[])?;
        Ok(())
    }

    /// Download a single tile (for CLI download command).
    pub fn download_tile(&self, lat: f64, lon: f64, zoom: u8) -> Result<Vec<u8>, ServiceError> {
        let request = TileRequest {
            lat: lat as i32,
            lon: lon as i32,
            zoom
        };
        self.generator.generate(&request).map_err(ServiceError::from)
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> CacheStatistics {
        self.cache.stats()
    }
}
```

### Simplified CLI

```rust
// main.rs - Now minimal
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { mountpoint, provider, google_api_key, ... } => {
            let config = ServerConfig::builder()
                .mountpoint(mountpoint)
                .texture(TextureConfig { format, mipmap_count })
                .download(DownloadConfig { timeout, retries, parallel })
                .cache(CacheConfig { enabled: !no_cache, .. })
                .build()?;

            let provider_config = ProviderConfig::from_cli(provider, google_api_key)?;

            let service = XEarthLayerService::new(config, provider_config)?;
            service.serve()?;
        }

        Command::Download { lat, lon, zoom, output, ... } => {
            let config = ServerConfig::builder()
                .texture(TextureConfig { format, mipmap_count })
                .download(DownloadConfig { timeout, retries, parallel })
                .build()?;

            let provider_config = ProviderConfig::from_cli(provider, google_api_key)?;

            let service = XEarthLayerService::new(config, provider_config)?;
            let data = service.download_tile(lat, lon, zoom)?;
            std::fs::write(output, data)?;
        }
    }
}
```

---

## Implementation Order

### Priority 1: High Impact (Do First)

| Phase | Effort | Impact | Dependencies | Status |
|-------|--------|--------|--------------|--------|
| Phase 1: TextureEncoder | Medium | High | None | ✅ Complete |
| Phase 4: Config Builders | Low | High | None | ✅ Complete |

### Priority 2: Core Architecture

| Phase | Effort | Impact | Dependencies | Status |
|-------|--------|--------|--------------|--------|
| Phase 2: TileGenerator | High | High | Phase 1 | ✅ Complete |
| Phase 3: Remove Generics | Medium | Medium | Phase 2 | ✅ Complete |

### Priority 3: Polish

| Phase | Effort | Impact | Dependencies | Status |
|-------|--------|--------|--------------|--------|
| Phase 5: Provider Factory | Low | Medium | Phase 4 | ✅ Complete |
| Phase 6: Service Facade | Medium | High | All above | ✅ Complete |

---

## New Module Structure

```
xearthlayer/src/
├── lib.rs                  # Public API
├── cache/                  # ✓ Already good
│   ├── mod.rs
│   ├── trait.rs           # Cache trait
│   ├── system.rs
│   └── ...
├── config/                 # NEW
│   ├── mod.rs
│   ├── texture.rs         # TextureConfig
│   ├── download.rs        # DownloadConfig
│   ├── cache.rs           # CacheConfig (move from cache/)
│   └── server.rs          # ServerConfig
├── coord/                  # ✓ Already good
├── dds/                    # ✓ Already good (internal)
├── fuse/
│   ├── mod.rs
│   └── filesystem.rs      # SIMPLIFIED - uses traits only
├── orchestrator/
│   └── download.rs        # MODIFIED - no generics
├── provider/
│   ├── mod.rs
│   ├── types.rs           # Provider trait
│   ├── factory.rs         # NEW - ProviderFactory
│   ├── bing.rs
│   ├── google.rs
│   └── http.rs
├── texture/                # NEW
│   ├── mod.rs
│   ├── encoder.rs         # TextureEncoder trait
│   ├── dds.rs             # DdsTextureEncoder
│   └── error.rs
├── tile/                   # NEW
│   ├── mod.rs
│   ├── generator.rs       # TileGenerator trait
│   └── request.rs         # TileRequest
└── service/                # NEW - High-level service facade
    ├── mod.rs              # Module exports
    ├── config.rs           # ServiceConfig and builder
    ├── error.rs            # ServiceError types
    └── facade.rs           # XEarthLayerService implementation
```

---

## Migration Checklist

### Phase 1: TextureEncoder ✅ COMPLETE
- [x] Create `texture/mod.rs` with TextureEncoder trait
- [x] Create `texture/dds.rs` with DdsTextureEncoder
- [x] Create `texture/error.rs` with TextureError
- [x] Update `fuse/filesystem.rs` to use `Arc<dyn TextureEncoder>`
- [x] Update `lib.rs` exports
- [x] Add tests for TextureEncoder implementations (28 new tests)
- [x] Update CLI to create encoder

### Phase 2: TileGenerator ✅ COMPLETE
- [x] Create `tile/mod.rs` with TileGenerator trait
- [x] Create `tile/generator.rs` with TileGenerator trait definition
- [x] Create `tile/default.rs` with DefaultTileGenerator
- [x] Create `tile/request.rs` with TileRequest
- [x] Create `tile/error.rs` with TileGeneratorError
- [x] Simplify `fuse/filesystem.rs` to use `Arc<dyn TileGenerator>`
- [x] Add tests for TileGenerator (32 new tests)
- [x] Update CLI to create generator

### Phase 3: Remove Generics ✅ COMPLETE
- [x] Modify TileOrchestrator to use `Arc<dyn Provider>`
- [x] Remove generic parameter from DefaultTileGenerator
- [x] Update CLI to create `Arc<dyn Provider>` and pass to orchestrator
- [x] Update all tests to use trait objects
- [x] Verify tests still pass (347 unit tests + 18 doc tests)

### Phase 4: Config Builders ✅ COMPLETE
- [x] Create `config/mod.rs` with TextureConfig and DownloadConfig
- [x] Create TextureConfig for DDS format and mipmap settings
- [x] Create DownloadConfig for timeout, retries, and parallel downloads
- [x] Add `with_config()` constructor to TileOrchestrator
- [x] Update CLI to use config builders (reduced function parameters)
- [x] Verify tests still pass (365 unit tests + 21 doc tests)

### Phase 5: Provider Factory ✅ COMPLETE
- [x] Create `provider/factory.rs` with ProviderFactory struct
- [x] Create ProviderConfig enum (Bing, Google variants)
- [x] Add helper methods: `name()`, `requires_api_key()`
- [x] Update CLI with `to_provider_config()` helper
- [x] Refactor `handle_download` and `handle_serve` to use factory
- [x] Verify tests still pass (369 unit tests + 22 doc tests)

### Phase 6: Service Facade ✅ COMPLETE
- [x] Create `service/` module with XEarthLayerService
- [x] Create `service/config.rs` with ServiceConfig and builder
- [x] Create `service/error.rs` with ServiceError types
- [x] Create `service/facade.rs` with XEarthLayerService implementation
- [x] Implement serve() method for FUSE server
- [x] Implement download_tile() method for single tile downloads
- [x] Simplify CLI to use service (reduced from ~657 to ~470 lines)
- [x] Verify tests still pass (390 unit tests + 23 doc tests)

---

## Testing Strategy

Each phase should maintain test coverage:

1. **Unit tests**: Mock dependencies using traits
2. **Integration tests**: Test component interactions
3. **Regression tests**: Ensure existing functionality works

### Example Test with Mocks

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct MockTextureEncoder;

    impl TextureEncoder for MockTextureEncoder {
        fn encode(&self, _image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
            Ok(vec![0xDD, 0x53]) // DDS magic bytes
        }

        fn expected_size(&self, _w: u32, _h: u32) -> usize {
            1024
        }

        fn extension(&self) -> &str {
            "dds"
        }
    }

    #[test]
    fn test_fuse_with_mock_encoder() {
        let generator = MockTileGenerator::new();
        let cache = NoOpCache::new("test");
        let fs = XEarthLayerFS::new(Arc::new(generator), Arc::new(cache));

        // Test FUSE operations without real encoding
    }
}
```

---

## Success Criteria

After refactoring:

1. **No concrete types in FUSE** - Only trait objects
2. **CLI under 200 lines** - Uses service facade
3. **All components mockable** - Traits everywhere
4. **Config via builders** - No >4 parameter functions
5. **Tests pass** - 289+ tests still green
6. **make verify passes** - No clippy warnings
