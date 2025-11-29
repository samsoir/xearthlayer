# XEarthLayer Design Principles

This document outlines the core design and coding principles for the XEarthLayer project.

## Development Philosophy

XEarthLayer is built using **SOLID principles** with a **Test-Driven Development (TDD)** approach to ensure:
- High degrees of separation of concerns
- High test coverage
- Components testable in isolation
- Easy substitution of implementations (mocks, alternate providers)

## Makefile as Single Interface

**All project operations must be executed through the Makefile.** The Makefile serves as the single, consistent interface for both development and production tasks.

### Philosophy

The Makefile provides:
- **Consistency**: Same commands work for all developers and CI/CD
- **Discoverability**: `make help` shows all available operations
- **Documentation**: Inline comments document what each target does
- **Simplicity**: Abstract complex commands behind simple targets
- **Quality Gates**: Enforce checks before commits and releases

### Required Targets

All developers must use these Make targets instead of running commands directly:

**Development Workflow:**
- `make test` - Run all tests (primary TDD feedback loop)
- `make watch` - Auto-run tests on file changes
- `make build` - Build debug version
- `make run` - Run the application

**Code Quality:**
- `make lint` - Run linting checks
- `make format` - Format code
- `make verify` - Run all quality checks (lint + format + test)
- `make pre-commit` - Run before committing

**Testing:**
- `make test-unit` - Run unit tests only
- `make integration-tests` - Run integration tests only (requires built binary)
- `make coverage` - Generate coverage report

**Release:**
- `make release` - Build production release
- `make ci` - Run all CI checks

**Documentation:**
- `make doc` - Generate documentation
- `make doc-open` - Generate and open docs

### Enforcement

- **Never run `cargo` commands directly** in documentation or scripts
- **Always add new operations as Make targets** with help documentation
- **CI/CD must use Make targets** to ensure consistency
- **README must reference Make targets** for getting started

### Example

```bash
# GOOD: Using Makefile
make test
make verify
make release

# BAD: Direct cargo commands
cargo test
cargo clippy
cargo build --release
```

## Application Architecture

XEarthLayer follows a **library-first architecture** with clear separation between domain logic and presentation layers.

### Core Principle: Domain Logic as Library

All core functionality resides in a library crate (`xearthlayer`) that can be statically linked to multiple presentation interfaces. This ensures:

- **Testability**: Core logic tested independently of UI
- **Flexibility**: Multiple interfaces (CLI, GUI, web) can use the same core
- **Reusability**: Third-party applications can integrate XEarthLayer
- **Maintainability**: UI changes don't affect domain logic

### Project Structure

```
xearthlayer/                    # Workspace root
├── Cargo.toml                  # Workspace manifest
├── xearthlayer/                # Core library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs              # Public API
│       ├── fuse/               # FUSE filesystem operations
│       ├── tile/               # Tile/chunk management
│       ├── imagery/            # Satellite imagery providers
│       ├── coord/              # Coordinate conversion
│       ├── dds/                # DDS encoding
│       └── config/             # Configuration management
├── xearthlayer-cli/            # CLI binary (Phase 1)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
└── xearthlayer-gtk/            # GTK4 GUI (Phase 2)
    ├── Cargo.toml
    └── src/
        └── main.rs
```

### Library Crate (`xearthlayer`)

**Contains:**
- All domain logic and business rules
- FUSE filesystem operations
- Tile caching and management
- HTTP downloading and chunk assembly
- DDS encoding
- Configuration management
- Coordinate system conversions

**Does NOT contain:**
- CLI argument parsing
- GUI rendering
- User interface logic
- Presentation formatting

**Public API:**
The library exposes a clean, minimal API that presentation layers consume:

```rust
// Example public API (to be designed)
pub struct XEarthLayer {
    // ...
}

impl XEarthLayer {
    pub fn new(config: Config) -> Result<Self>;
    pub fn mount(&mut self, mount_points: Vec<MountPoint>) -> Result<()>;
    pub fn unmount(&mut self) -> Result<()>;
    pub fn cache_stats(&self) -> CacheStats;
}
```

### Binary Crates (Presentation Layer)

#### CLI Binary (`xearthlayer-cli`) - Phase 1

**Purpose**: Command-line interface for immediate functionality

**Responsibilities:**
- Argument parsing (using `clap` or `argh`)
- Configuration file handling
- Terminal output formatting
- Signal handling (SIGINT, SIGTERM)
- Error display

**Does NOT contain**: Domain logic (delegates to library)

#### GTK4 GUI (`xearthlayer-gtk`) - Phase 2

**Purpose**: Native Linux graphical interface

**Responsibilities:**
- GTK4 widgets and windows
- User interaction handling
- Settings UI
- Status display and monitoring
- System tray integration

**Framework**: GTK4 with `gtk4-rs` bindings

**Does NOT contain**: Domain logic (delegates to library)

### Static vs Dynamic Linking

**Decision: Static Linking**

XEarthLayer uses **static linking** for all Rust binaries:

**Rationale:**
- ✅ **Simple deployment** - Single binary per interface
- ✅ **No dependency conflicts** - Each binary is self-contained
- ✅ **Better optimization** - Link-Time Optimization (LTO) across crate boundaries
- ✅ **Rust idiomatic** - Default and recommended approach
- ✅ **No ABI compatibility issues** - Pure Rust types throughout

**Dynamic linking** is NOT used because:
- ❌ Complex deployment (need to ship .so files)
- ❌ Requires C-compatible ABI (limits API design)
- ❌ Version compatibility issues
- ❌ Performance overhead
- ❌ Unnecessary for our use case

**Future Consideration:**
If a C-compatible dynamic library is needed (e.g., for Objective-C AppKit integration on macOS), we can create a separate `xearthlayer-ffi` crate with C bindings while keeping the Rust interfaces statically linked.

### Platform Support Strategy

#### Primary Target: Linux

- Native FUSE support
- Primary development platform
- Best testing coverage
- CLI and GTK4 interfaces

#### Secondary Target: macOS

- FUSE via macFUSE (third-party)
- CLI interface (Phase 1)
- GTK4 GUI works on macOS (Phase 2)
- Native AppKit wrapper possible (Phase 3)

#### Not Supported: Windows

- Other projects (AutoOrtho) already provide Windows support
- FUSE requires third-party drivers (Dokan/WinFSP)
- Different filesystem semantics
- Out of scope for initial development

### Interface Evolution

**Phase 1: CLI** (Current)
- Basic functionality
- Configuration management
- Mount/unmount operations
- Status reporting

**Phase 2: GTK4 GUI** (Future)
- Visual configuration
- Real-time status display
- Cache statistics visualization
- System tray integration
- Native Linux experience

**Phase 3: macOS Native** (Optional)
- AppKit wrapper around library
- macOS Human Interface Guidelines
- System integration (menu bar, notifications)

### Benefits of This Architecture

1. **Separation of Concerns**: UI code never mixed with domain logic
2. **Testability**: Core library tested without UI dependencies
3. **Flexibility**: Easy to add new interfaces (web UI, mobile app, etc.)
4. **Maintainability**: UI changes don't require retesting core logic
5. **Reusability**: Other projects can use XEarthLayer as a library
6. **Performance**: Static linking enables aggressive optimization

### Example Dependency Flow

```
xearthlayer-cli (main.rs)
    ↓ (depends on)
xearthlayer (lib.rs)
    ↓ (uses)
fuser, reqwest, tokio, image, etc.

xearthlayer-gtk (main.rs)
    ↓ (depends on)
xearthlayer (lib.rs) + gtk4
    ↓ (uses)
fuser, reqwest, tokio, image, gtk4, etc.
```

The library crate is always the source of truth for functionality. Binaries are thin wrappers that provide user interaction.

## SOLID Principles in Rust

### 1. Single Responsibility Principle (SRP)

Each module, struct, and function has one clear responsibility and one reason to change.

**Examples:**
- `TileCache` - Only manages tile lifecycle and memory limits
- `ChunkDownloader` - Only fetches chunks via HTTP
- `FuseFilesystem` - Only handles FUSE filesystem operations
- `CoordinateConverter` - Only converts between coordinate systems
- `DdsEncoder` - Only handles DDS compression and mipmap generation

**Anti-pattern to avoid:**
```rust
// BAD: TileManager does too much
struct TileManager {
    // Caching
    cache: HashMap<TileCoord, Tile>,
    // HTTP downloading
    client: HttpClient,
    // DDS encoding
    encoder: DdsEncoder,
    // FUSE operations
    fuse_state: FuseState,
}
```

**Preferred approach:**
```rust
// GOOD: Separate responsibilities
struct TileCache { /* only caching */ }
struct ChunkDownloader { /* only HTTP */ }
struct DdsEncoder { /* only encoding */ }
struct FuseFilesystem { /* only FUSE */ }
```

### 2. Open/Closed Principle (OCP)

Code should be open for extension but closed for modification. Use traits to enable adding new functionality without changing existing code.

**Example: Imagery Providers**
```rust
/// Trait for satellite imagery providers
pub trait ImageryProvider: Send + Sync {
    fn name(&self) -> &str;
    fn fetch_chunk(&self, coords: ChunkCoord) -> Result<Vec<u8>>;
    fn max_zoom(&self) -> u8;
}

// Easily add new providers without modifying existing code
pub struct BingProvider { /* ... */ }
pub struct NaipProvider { /* ... */ }
pub struct EoxProvider { /* ... */ }
pub struct UsgsProvider { /* ... */ }

impl ImageryProvider for BingProvider { /* ... */ }
impl ImageryProvider for NaipProvider { /* ... */ }
// etc.
```

### 3. Liskov Substitution Principle (LSP)

Any implementation of a trait should be substitutable for another without breaking functionality.

**Example:**
```rust
fn download_tile<P: ImageryProvider>(
    provider: &P,
    coords: TileCoord
) -> Result<Tile> {
    // Should work correctly with ANY ImageryProvider implementation
    let chunks = fetch_all_chunks(provider, coords)?;
    assemble_tile(chunks)
}

// All of these should work identically
download_tile(&BingProvider::new(), coords)?;
download_tile(&NaipProvider::new(), coords)?;
download_tile(&MockProvider::new(), coords)?; // for testing
```

### 4. Interface Segregation Principle (ISP)

Use small, focused traits rather than large monolithic interfaces. Clients should not depend on methods they don't use.

**Example:**
```rust
// GOOD: Small, focused traits
pub trait TileReader {
    fn read_tile(&self, coords: TileCoord) -> Result<Vec<u8>>;
}

pub trait CacheManager {
    fn evict_lru(&mut self);
    fn cache_size(&self) -> usize;
    fn set_max_size(&mut self, size: usize);
}

pub trait CacheStats {
    fn hit_rate(&self) -> f64;
    fn total_hits(&self) -> u64;
    fn total_misses(&self) -> u64;
}

// Structs implement only the traits they need
impl TileReader for TileCache { /* ... */ }
impl CacheManager for TileCache { /* ... */ }
impl CacheStats for TileCache { /* ... */ }

// BAD: Single large interface
pub trait TileCacheAll {
    fn read_tile(&self, coords: TileCoord) -> Result<Vec<u8>>;
    fn evict_lru(&mut self);
    fn cache_size(&self) -> usize;
    fn set_max_size(&mut self, size: usize);
    fn hit_rate(&self) -> f64;
    fn total_hits(&self) -> u64;
    fn total_misses(&self) -> u64;
    // ... many more methods
}
```

### 5. Dependency Inversion Principle (DIP)

Depend on abstractions (traits) rather than concrete implementations. High-level modules should not depend on low-level modules.

**Example:**
```rust
// GOOD: Depend on trait abstraction
pub struct TileCache<P: ImageryProvider, E: DdsEncoder> {
    provider: P,      // Injected dependency (trait)
    encoder: E,       // Injected dependency (trait)
    storage: HashMap<TileCoord, Arc<Tile>>,
}

impl<P: ImageryProvider, E: DdsEncoder> TileCache<P, E> {
    pub fn new(provider: P, encoder: E) -> Self {
        Self {
            provider,
            encoder,
            storage: HashMap::new(),
        }
    }
}

// Easy to inject different implementations
let cache = TileCache::new(
    BingProvider::new(),
    IspcDdsEncoder::new()
);

// Easy to inject mocks for testing
let test_cache = TileCache::new(
    MockProvider::new(),
    MockEncoder::new()
);

// BAD: Direct dependency on concrete types
pub struct TileCache {
    provider: BingProvider,  // Tightly coupled!
    encoder: IspcDdsEncoder, // Can't substitute!
}
```

## Test-Driven Development (TDD)

All code must be developed using the **Red-Green-Refactor** cycle.

### The Red-Green-Refactor Cycle

#### 1. RED - Write a Failing Test

Before writing any implementation code, write a test that defines the expected behavior.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lat_lon_to_tile_coords_at_zoom_16() {
        // RED: This test fails because to_tile_coords doesn't exist yet
        let converter = CoordinateConverter::new();
        let result = converter.to_tile_coords(40.7128, -74.0060, 16);

        assert_eq!(result.row, 24341);
        assert_eq!(result.col, 19298);
    }
}
```

Run the test: `cargo test` - **It should FAIL**

#### 2. GREEN - Write Minimal Implementation

Write just enough code to make the test pass. Don't over-engineer.

```rust
pub struct CoordinateConverter;

impl CoordinateConverter {
    pub fn new() -> Self {
        Self
    }

    pub fn to_tile_coords(&self, lat: f64, lon: f64, zoom: u8) -> TileCoord {
        // Minimal implementation using Web Mercator projection
        let n = 2.0_f64.powi(zoom as i32);
        let lat_rad = lat.to_radians();

        let col = ((lon + 180.0) / 360.0 * n) as u32;
        let row = ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;

        TileCoord { row, col, zoom }
    }
}
```

Run the test: `cargo test` - **It should PASS**

#### 3. REFACTOR - Improve Code Quality

Improve the code without changing behavior. Tests should still pass.

```rust
pub struct CoordinateConverter;

impl CoordinateConverter {
    pub fn new() -> Self {
        Self
    }

    pub fn to_tile_coords(&self, lat: f64, lon: f64, zoom: u8) -> TileCoord {
        let num_tiles = self.num_tiles_at_zoom(zoom);

        TileCoord {
            col: self.lon_to_tile_x(lon, num_tiles),
            row: self.lat_to_tile_y(lat, num_tiles),
            zoom,
        }
    }

    fn num_tiles_at_zoom(&self, zoom: u8) -> f64 {
        2.0_f64.powi(zoom as i32)
    }

    fn lon_to_tile_x(&self, lon: f64, num_tiles: f64) -> u32 {
        ((lon + 180.0) / 360.0 * num_tiles) as u32
    }

    fn lat_to_tile_y(&self, lat: f64, num_tiles: f64) -> u32 {
        let lat_rad = lat.to_radians();
        ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * num_tiles) as u32
    }
}
```

Run the test: `cargo test` - **It should STILL PASS**

### TDD Workflow Rules

1. **Never write implementation code without a failing test first**
2. **Write the smallest test that fails**
3. **Write the smallest code that makes the test pass**
4. **Refactor only after tests are green**
5. **Run tests frequently** (after every small change)

## Testing Strategy

### Test Categories

#### 1. Unit Tests
Test individual components in complete isolation using mocks.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_cache_stores_and_retrieves() {
        let mock_provider = MockProvider::new();
        let mock_encoder = MockEncoder::new();
        let mut cache = TileCache::new(mock_provider, mock_encoder);

        let coords = TileCoord { row: 100, col: 200, zoom: 16 };
        let tile = cache.get_or_fetch(coords).unwrap();

        // Should be cached now
        assert_eq!(cache.cache_size(), 1);
    }
}
```

#### 2. Integration Tests
Test interactions between components.

```rust
#[test]
fn test_fuse_to_cache_integration() {
    let provider = BingProvider::new();
    let encoder = IspcDdsEncoder::new();
    let cache = TileCache::new(provider, encoder);
    let fuse = FuseFilesystem::new(cache);

    // Test that FUSE operations correctly use cache
    let result = fuse.read_file("+40-120_BI16.dds");
    assert!(result.is_ok());
}
```

#### 3. Property-Based Tests
Use `proptest` for testing mathematical properties.

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_coordinate_conversion_roundtrip(
        lat in -85.0..85.0,  // Valid Web Mercator range
        lon in -180.0..180.0,
        zoom in 0u8..18u8
    ) {
        let converter = CoordinateConverter::new();
        let tile = converter.to_tile_coords(lat, lon, zoom);
        let (lat2, lon2) = converter.tile_to_lat_lon(tile);

        // Should be approximately equal (within tile precision)
        prop_assert!((lat - lat2).abs() < 0.1);
        prop_assert!((lon - lon2).abs() < 0.1);
    }
}
```

#### 4. Benchmark Tests
Ensure performance targets are met.

```rust
#[bench]
fn bench_dds_encoding(b: &mut Bencher) {
    let encoder = IspcDdsEncoder::new();
    let rgba_data = vec![0u8; 4096 * 4096 * 4]; // 4096x4096 RGBA

    b.iter(|| {
        encoder.encode(&rgba_data, 4096, 4096)
    });
}
```

### Test Coverage Requirements

- **Minimum**: 80% line coverage
- **Target**: 90%+ line coverage
- **Critical paths**: 100% coverage (FUSE operations, coordinate conversion, DDS encoding)

Run coverage with:
```bash
cargo tarpaulin --out Html
```

## Code Organization Principles

### Module Structure

```
src/
├── main.rs              # Minimal, only setup and dependency injection
├── lib.rs               # Public API, re-exports
├── fuse/                # FUSE filesystem layer
│   ├── mod.rs           # Public interface
│   └── operations.rs    # Implementation (private)
├── tile/
│   ├── mod.rs           # Public traits and types
│   ├── cache.rs         # TileCache implementation
│   ├── tile.rs          # Tile struct
│   └── chunk.rs         # Chunk downloader
├── config/
│   ├── mod.rs
│   └── settings.rs
└── ...
```

### Dependency Flow

```
main.rs
  ↓ (creates concrete implementations)
FuseFilesystem<TileCache<BingProvider, IspcEncoder>>
  ↓ (depends on trait)
TileCache (generic over ImageryProvider + DdsEncoder)
  ↓ (depends on traits)
ImageryProvider trait ← BingProvider, NaipProvider, etc.
DdsEncoder trait      ← IspcEncoder, StbEncoder, etc.
```

### Public vs Private

- **Public (`pub`)**: Traits, core types, public API
- **Private (no `pub`)**: Implementation details, internal helpers

```rust
// Public API
pub trait ImageryProvider { /* ... */ }
pub struct TileCoord { /* ... */ }

// Private implementation
struct ChunkDownloader { /* ... */ }
fn validate_jpeg_magic(bytes: &[u8]) -> bool { /* ... */ }
```

## Error Handling

### Error Types

Use `thiserror` for domain-specific errors:

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TileError {
    #[error("HTTP request failed: {0}")]
    NetworkError(#[from] reqwest::Error),

    #[error("Invalid tile coordinates: row={row}, col={col}, zoom={zoom}")]
    InvalidCoordinates { row: u32, col: u32, zoom: u8 },

    #[error("Cache is full, cannot store tile")]
    CacheFull,

    #[error("DDS encoding failed: {0}")]
    EncodingError(String),
}

pub type Result<T> = std::result::Result<T, TileError>;
```

### When to Panic vs Return Error

- **Return `Result`**: Network errors, invalid input, cache misses, encoding failures
- **Panic**: Programming errors, invariant violations, unreachable code

```rust
// GOOD: Return error for expected failures
pub fn fetch_chunk(&self, coords: ChunkCoord) -> Result<Vec<u8>> {
    let response = self.client.get(url).send()?;
    if !response.status().is_success() {
        return Err(TileError::NetworkError(/* ... */));
    }
    Ok(response.bytes()?)
}

// GOOD: Panic for programming errors
fn get_cached_tile(&self, key: &TileCoord) -> Arc<Tile> {
    self.cache.get(key).expect("BUG: tile should be in cache")
}
```

## Performance Considerations

### Async vs Sync

- **FUSE operations**: Sync (blocking) - required by FUSE API
- **HTTP downloads**: Async (tokio) - concurrent chunk fetching
- **DDS encoding**: Sync (CPU-bound) - use rayon for parallelism

### Memory Management

- Use `Arc<Tile>` for shared ownership in cache
- Implement LRU eviction before allocation
- Monitor memory usage with configurable limits

### Zero-Copy Where Possible

```rust
// GOOD: Use slices to avoid copying
pub fn encode_mipmap(&self, data: &[u8]) -> Vec<u8> { /* ... */ }

// BAD: Unnecessary clone
pub fn encode_mipmap(&self, data: Vec<u8>) -> Vec<u8> { /* ... */ }
```

## CLI Command Architecture

When implementing CLI commands with multiple subcommands, use the **Command Pattern with Trait-Based Dependency Injection**. This pattern ensures handlers are testable, maintainable, and follow SOLID principles.

### Pattern Overview

```
commands/<feature>/
├── mod.rs        # Module exports and command dispatch
├── traits.rs     # Core interfaces (Output, Service, CommandHandler)
├── services.rs   # Concrete implementations wrapping library functions
├── args.rs       # CLI argument types (clap-derived)
├── handlers.rs   # Command handlers implementing business logic
└── output.rs     # Shared output formatting utilities
```

### Core Traits

#### 1. Output Trait

Abstracts console output for testable handlers:

```rust
/// Abstracts user-facing output
pub trait Output: Send + Sync {
    fn println(&self, message: &str);
    fn print(&self, message: &str);
    fn newline(&self);
    fn header(&self, title: &str);
    fn indented(&self, message: &str);
}

/// Production implementation
pub struct ConsoleOutput;

impl Output for ConsoleOutput {
    fn println(&self, message: &str) {
        println!("{}", message);
    }
    // ...
}
```

#### 2. Service Trait

Abstracts library operations for dependency injection:

```rust
/// Abstracts all library operations
pub trait FeatureService: Send + Sync {
    fn operation_one(&self, args: Args) -> Result<Output, Error>;
    fn operation_two(&self, args: Args) -> Result<Output, Error>;
    // ...
}

/// Production implementation wrapping library
pub struct DefaultFeatureService;

impl FeatureService for DefaultFeatureService {
    fn operation_one(&self, args: Args) -> Result<Output, Error> {
        // Delegate to library function
        library::operation_one(args)
            .map_err(|e| Error::from(e))
    }
}
```

#### 3. CommandHandler Trait

Uniform interface for all command handlers:

```rust
/// Each command handler implements this trait
pub trait CommandHandler {
    type Args;
    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError>;
}
```

#### 4. CommandContext

Bundles dependencies for injection:

```rust
/// Context providing dependencies to command handlers
pub struct CommandContext<'a> {
    pub output: &'a dyn Output,
    pub service: &'a dyn FeatureService,
}

impl<'a> CommandContext<'a> {
    pub fn new(output: &'a dyn Output, service: &'a dyn FeatureService) -> Self {
        Self { output, service }
    }
}
```

### Handler Implementation

Each handler focuses only on orchestration and output:

```rust
pub struct InitHandler;

impl CommandHandler for InitHandler {
    type Args = InitArgs;

    fn execute(args: Self::Args, ctx: &CommandContext<'_>) -> Result<(), CliError> {
        // Use service for operations
        let result = ctx.service.initialize(&args.path)?;

        // Use output for user feedback
        ctx.output.println("Initialized successfully!");
        ctx.output.indented(&format!("Location: {}", result.path.display()));

        Ok(())
    }
}
```

### Command Dispatch

The module's `run()` function creates the production context and dispatches:

```rust
pub fn run(command: Commands) -> Result<(), CliError> {
    // Create production context
    let output = ConsoleOutput::new();
    let service = DefaultFeatureService::new();
    let ctx = CommandContext::new(&output, &service);

    // Dispatch to appropriate handler
    match command {
        Commands::Init { path } => InitHandler::execute(InitArgs { path }, &ctx),
        Commands::List { verbose } => ListHandler::execute(ListArgs { verbose }, &ctx),
        // ...
    }
}
```

### Testing Benefits

Handlers can be tested with mock implementations:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct MockOutput {
        messages: RefCell<Vec<String>>,
    }

    impl Output for MockOutput {
        fn println(&self, message: &str) {
            self.messages.borrow_mut().push(message.to_string());
        }
        // ...
    }

    struct MockService {
        init_result: Result<InitResult, Error>,
    }

    impl FeatureService for MockService {
        fn initialize(&self, _path: &Path) -> Result<InitResult, Error> {
            self.init_result.clone()
        }
    }

    #[test]
    fn test_init_handler_success() {
        let output = MockOutput::new();
        let service = MockService::with_success();
        let ctx = CommandContext::new(&output, &service);

        let result = InitHandler::execute(InitArgs { path: PathBuf::from(".") }, &ctx);

        assert!(result.is_ok());
        assert!(output.contains("Initialized successfully"));
    }

    #[test]
    fn test_init_handler_failure() {
        let output = MockOutput::new();
        let service = MockService::with_error("disk full");
        let ctx = CommandContext::new(&output, &service);

        let result = InitHandler::execute(InitArgs { path: PathBuf::from(".") }, &ctx);

        assert!(result.is_err());
    }
}
```

### Design Benefits

| Benefit | Description |
|---------|-------------|
| **Single Responsibility** | Each handler owns one command's logic |
| **Dependency Injection** | Handlers depend only on trait interfaces |
| **Testability** | Handlers can be tested with mock implementations |
| **Extensibility** | Adding commands means adding a handler file |
| **Consistent Interface** | CommandHandler trait enforces uniform API |
| **Separation of Concerns** | Output formatting separate from business logic |

### When to Use This Pattern

Use this CLI architecture when:
- A command has **3+ subcommands**
- Subcommands have **non-trivial logic**
- You need **testable command handlers**
- Multiple subcommands share **common dependencies**

For simple commands with 1-2 subcommands and minimal logic, a simpler approach may suffice.

### Reference Implementation

See `xearthlayer-cli/src/commands/publish/` for the complete reference implementation used by the Publisher CLI.

## Summary

1. **Follow SOLID principles** - Each component has clear responsibility and dependencies
2. **Use TDD (Red-Green-Refactor)** - Write tests first, implement second
3. **Depend on traits** - Enable mocking and substitution
4. **Test in isolation** - Mock all external dependencies
5. **Maintain high coverage** - Minimum 80%, target 90%+
6. **Handle errors gracefully** - Return `Result` for expected failures
7. **Optimize judiciously** - Measure before optimizing
8. **Use Command Pattern for CLI** - Trait-based DI for testable command handlers

These principles ensure XEarthLayer remains maintainable, testable, and extensible as it grows.
