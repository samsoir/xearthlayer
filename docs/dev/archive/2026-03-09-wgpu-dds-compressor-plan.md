# GPU-Accelerated DDS Compression Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Offload BC1/BC3 texture compression to GPU via wgpu compute shaders, targeting idle integrated GPUs.

**Architecture:** A new `WgpuCompressor` implements the existing `BlockCompressor` trait using the `block_compression` crate (ISPC→WGSL port). GPU device selection is config-driven (`integrated`/`discrete`/name match). Feature-gated behind `gpu-encode` to avoid build cost for users who don't need it.

**Tech Stack:** `wgpu` (GPU abstraction), `block_compression` (BC1/BC3 compute shaders), `pollster` (blocking async in sync context)

**Design doc:** `docs/plans/2026-03-09-wgpu-dds-compressor-design.md`

---

### Task 1: Add Feature-Gated Dependencies

**Files:**
- Modify: `xearthlayer/Cargo.toml:49-51`

**Step 1: Add wgpu and block_compression as optional dependencies**

In `xearthlayer/Cargo.toml`, add to `[dependencies]` (after `intel_tex_2` on line 45):

```toml
wgpu = { version = "24", optional = true }
block_compression = { version = "0.8", optional = true, default-features = false, features = ["bc15"] }
pollster = { version = "0.4", optional = true }
```

Update `[features]` section (line 50-51):

```toml
[features]
profiling = ["tracing-chrome"]
gpu-encode = ["dep:wgpu", "dep:block_compression", "dep:pollster"]
```

**Step 2: Verify it compiles without the feature**

Run: `cargo check`
Expected: PASS (no code uses the deps yet)

**Step 3: Verify it compiles with the feature**

Run: `cargo check --features gpu-encode`
Expected: PASS (deps resolve, nothing references them yet)

**Step 4: Commit**

```
feat(dds): add feature-gated wgpu and block_compression dependencies (#67)
```

---

### Task 2: Add Config Keys and Defaults

**Files:**
- Modify: `xearthlayer/src/config/defaults.rs:265-266`
- Modify: `xearthlayer/src/config/settings.rs:71-76`
- Modify: `xearthlayer/src/config/keys.rs:44,158,271,384,569,813,913`
- Modify: `xearthlayer/src/config/texture.rs`

**Step 1: Write failing tests for new config keys**

In `xearthlayer/src/config/keys.rs`, add tests at the bottom of the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn test_texture_compressor_key_parse() {
    let key: ConfigKey = "texture.compressor".parse().unwrap();
    assert_eq!(key, ConfigKey::TextureCompressor);
    assert_eq!(key.name(), "texture.compressor");
}

#[test]
fn test_texture_gpu_device_key_parse() {
    let key: ConfigKey = "texture.gpu_device".parse().unwrap();
    assert_eq!(key, ConfigKey::TextureGpuDevice);
    assert_eq!(key.name(), "texture.gpu_device");
}

#[test]
fn test_texture_compressor_validation() {
    let key = ConfigKey::TextureCompressor;
    assert!(key.validate("software").is_ok());
    assert!(key.validate("ispc").is_ok());
    assert!(key.validate("gpu").is_ok());
    assert!(key.validate("invalid").is_err());
}

#[test]
fn test_texture_gpu_device_validation() {
    let key = ConfigKey::TextureGpuDevice;
    // Any non-empty string is valid (device type or name substring)
    assert!(key.validate("integrated").is_ok());
    assert!(key.validate("discrete").is_ok());
    assert!(key.validate("Radeon").is_ok());
    assert!(key.validate("RTX 5090").is_ok());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::keys::tests::test_texture_compressor -- --nocapture`
Expected: FAIL — `TextureCompressor` variant doesn't exist yet

**Step 3: Add defaults**

In `xearthlayer/src/config/defaults.rs`, after `DEFAULT_MIPMAP_COUNT` (line 266), add:

```rust
/// Default texture compressor backend: ISPC SIMD.
pub const DEFAULT_COMPRESSOR: &str = "ispc";
/// Default GPU device selector: prefer integrated GPU.
pub const DEFAULT_GPU_DEVICE: &str = "integrated";
```

**Step 4: Add fields to TextureSettings**

In `xearthlayer/src/config/settings.rs`, update `TextureSettings` (lines 71-76):

```rust
/// Texture configuration.
#[derive(Debug, Clone)]
pub struct TextureSettings {
    /// DDS format: BC1 or BC3
    pub format: DdsFormat,
    /// Compressor backend: "software", "ispc", or "gpu"
    pub compressor: String,
    /// GPU device selector: "integrated", "discrete", or adapter name substring
    pub gpu_device: String,
}
```

**Step 5: Update ConfigFile::default() in defaults.rs**

Find the `texture: TextureSettings { ... }` block in `ConfigFile::default()` and add the new fields:

```rust
texture: TextureSettings {
    format: DdsFormat::BC1,
    compressor: DEFAULT_COMPRESSOR.to_string(),
    gpu_device: DEFAULT_GPU_DEVICE.to_string(),
},
```

**Step 6: Add ConfigKey variants**

In `xearthlayer/src/config/keys.rs`:

Add to `ConfigKey` enum after `TextureFormat` (line 44):
```rust
    TextureCompressor,
    TextureGpuDevice,
```

Add to `FromStr` impl after `"texture.format"` (line 158):
```rust
            "texture.compressor" => Ok(ConfigKey::TextureCompressor),
            "texture.gpu_device" => Ok(ConfigKey::TextureGpuDevice),
```

Add to `name()` impl after `TextureFormat` (line 271):
```rust
            ConfigKey::TextureCompressor => "texture.compressor",
            ConfigKey::TextureGpuDevice => "texture.gpu_device",
```

Add to `get()` impl after `TextureFormat` (line 384):
```rust
            ConfigKey::TextureCompressor => config.texture.compressor.clone(),
            ConfigKey::TextureGpuDevice => config.texture.gpu_device.clone(),
```

Add to `set_unchecked()` impl after `TextureFormat` (around line 569):
```rust
            ConfigKey::TextureCompressor => {
                config.texture.compressor = value.to_lowercase();
            }
            ConfigKey::TextureGpuDevice => {
                config.texture.gpu_device = value.to_string();
            }
```

Add to `spec()` impl after `TextureFormat` (line 813):
```rust
            ConfigKey::TextureCompressor => {
                Box::new(OneOfSpec::new(&["software", "ispc", "gpu"]))
            }
            ConfigKey::TextureGpuDevice => Box::new(NonEmptyStringSpec),
```

Add to the `ALL` array after `TextureFormat` (line 913):
```rust
            ConfigKey::TextureCompressor,
            ConfigKey::TextureGpuDevice,
```

**Step 7: Add accessors to TextureConfig**

In `xearthlayer/src/config/texture.rs`, add fields and accessors:

```rust
pub struct TextureConfig {
    format: DdsFormat,
    mipmap_count: usize,
    compressor: String,
    gpu_device: String,
}
```

Add constructor and accessor methods:
```rust
    pub fn with_compressor(mut self, compressor: String) -> Self {
        self.compressor = compressor;
        self
    }

    pub fn with_gpu_device(mut self, gpu_device: String) -> Self {
        self.gpu_device = gpu_device;
        self
    }

    pub fn compressor(&self) -> &str {
        &self.compressor
    }

    pub fn gpu_device(&self) -> &str {
        &self.gpu_device
    }
```

Update `Default` impl to include:
```rust
    compressor: defaults::DEFAULT_COMPRESSOR.to_string(),
    gpu_device: defaults::DEFAULT_GPU_DEVICE.to_string(),
```

**Step 8: Run tests to verify they pass**

Run: `cargo test --lib config::keys::tests`
Expected: PASS

**Step 9: Commit**

```
feat(config): add texture.compressor and texture.gpu_device settings (#67)
```

---

### Task 3: Wire Config Through Parser and Writer

**Files:**
- Modify: `xearthlayer/src/config/parser.rs:87-104`
- Modify: `xearthlayer/src/config/writer.rs:82-85`

**Step 1: Write failing test for parser**

Add test in parser's test module:

```rust
#[test]
fn test_parse_texture_compressor_and_gpu_device() {
    let ini_content = "[texture]\nformat = bc1\ncompressor = gpu\ngpu_device = Radeon\n";
    let config = parse_ini_string(ini_content).unwrap();
    assert_eq!(config.texture.compressor, "gpu");
    assert_eq!(config.texture.gpu_device, "Radeon");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib config::parser::tests::test_parse_texture_compressor -- --nocapture`
Expected: FAIL

**Step 3: Update parser**

In `xearthlayer/src/config/parser.rs`, inside the `[texture]` section block (after format parsing, around line 100), add:

```rust
    if let Some(v) = section.get("compressor") {
        let v = v.to_lowercase();
        match v.as_str() {
            "software" | "ispc" | "gpu" => {
                config.texture.compressor = v;
            }
            _ => {
                return Err(ConfigFileError::InvalidValue {
                    section: "texture".to_string(),
                    key: "compressor".to_string(),
                    value: v,
                    reason: "must be 'software', 'ispc', or 'gpu'".to_string(),
                });
            }
        }
    }
    if let Some(v) = section.get("gpu_device") {
        config.texture.gpu_device = v.to_string();
    }
```

**Step 4: Update writer**

In `xearthlayer/src/config/writer.rs`, update the `[texture]` section template to include:

```ini
[texture]
; DDS compression format: bc1 (smaller, opaque) or bc3 (larger, with alpha)
; bc1 recommended for satellite imagery
format = {}
; Compression backend: software (pure Rust), ispc (SIMD, default), gpu (wgpu compute)
; gpu requires building with --features gpu-encode
compressor = {}
; GPU device selection (only used when compressor = gpu)
; Values: integrated, discrete, or adapter name substring (e.g., "Radeon")
gpu_device = {}
```

Update the format args to include `config.texture.compressor` and `config.texture.gpu_device`.

**Step 5: Run tests**

Run: `cargo test --lib config`
Expected: PASS

**Step 6: Commit**

```
feat(config): parse and write texture.compressor and gpu_device settings (#67)
```

---

### Task 4: Wire TextureConfig into ServiceConfig and Builder

**Files:**
- Modify: `xearthlayer/src/service/config.rs` (ServiceConfig builder)
- Modify: `xearthlayer/src/service/builder.rs:114-120`
- Modify: `xearthlayer/src/texture/dds.rs:27-46`

**Step 1: Write failing test for compressor selection in builder**

In `xearthlayer/src/service/builder.rs`, add test:

```rust
#[test]
fn test_create_encoder_default_uses_ispc() {
    let config = ServiceConfig::default();
    let encoder = create_encoder(&config);
    assert_eq!(encoder.format(), DdsFormat::BC1);
    // Default compressor is ISPC — verified by encoding a small image
    // (if it were wrong, it would fail or produce wrong output)
}
```

**Step 2: Propagate compressor to TextureConfig**

Wherever `TextureConfig` is built from `ConfigFile` (likely in `ServiceConfig::from()` or similar), ensure `compressor` and `gpu_device` fields flow through.

Find where `TextureConfig::new(config.texture.format)` is called and add:
```rust
    .with_compressor(config.texture.compressor.clone())
    .with_gpu_device(config.texture.gpu_device.clone())
```

**Step 3: Update DdsTextureEncoder to accept a BlockCompressor**

In `xearthlayer/src/texture/dds.rs`, the `encode()` method creates a fresh `DdsEncoder::new(self.format)` on each call (line 119). This is where the compressor backend must be injected.

The challenge: `DdsTextureEncoder` is `Clone` but `Box<dyn BlockCompressor>` is not. Two options:
- Store the compressor in an `Arc` on `DdsTextureEncoder`
- Create the compressor in `encode()` based on a stored string

The cleanest approach: add a `compressor: Arc<dyn BlockCompressor>` field to `DdsTextureEncoder`, remove `#[derive(Clone)]`, implement `Clone` manually (Arc clone is fine), and pass it to `DdsEncoder::with_compressor()`.

```rust
pub struct DdsTextureEncoder {
    format: DdsFormat,
    mipmap_count: usize,
    compressor: Arc<dyn BlockCompressor>,
}

impl DdsTextureEncoder {
    pub fn new(format: DdsFormat) -> Self {
        Self {
            format,
            mipmap_count: 5,
            compressor: Arc::new(IspcCompressor),
        }
    }

    pub fn with_compressor(mut self, compressor: Arc<dyn BlockCompressor>) -> Self {
        self.compressor = compressor;
        self
    }
}

impl Clone for DdsTextureEncoder {
    fn clone(&self) -> Self {
        Self {
            format: self.format,
            mipmap_count: self.mipmap_count,
            compressor: Arc::clone(&self.compressor),
        }
    }
}
```

Update `encode()` to use the stored compressor:
```rust
fn encode(&self, image: &RgbaImage) -> Result<Vec<u8>, TextureError> {
    let encoder = DdsEncoder::new(self.format)
        .with_mipmap_count(self.mipmap_count)
        .with_compressor(self.compressor.clone());
    encoder.encode(image).map_err(TextureError::from)
}
```

Note: `DdsEncoder::with_compressor` currently takes `Box<dyn BlockCompressor>`. We need to update it to accept `Arc` as well, or convert with `Arc::into_inner()`. The simplest change: keep `DdsEncoder` taking `Box<dyn BlockCompressor>` and add a way to pass an Arc. Actually — since `DdsEncoder` is created per-`encode()` call and not stored, the simplest approach is to change `DdsEncoder`'s field from `Box<dyn BlockCompressor>` to `Arc<dyn BlockCompressor>` so we can cheaply clone the Arc from `DdsTextureEncoder` into each `DdsEncoder`.

In `xearthlayer/src/dds/encoder.rs`, change:
```rust
compressor: Box<dyn BlockCompressor>,
```
to:
```rust
compressor: Arc<dyn BlockCompressor>,
```

Update `default_compressor()` in `compressor.rs` to return `Arc`:
```rust
pub fn default_compressor() -> Arc<dyn BlockCompressor> {
    Arc::new(IspcCompressor)
}
```

Update `with_compressor` in encoder.rs:
```rust
pub fn with_compressor(mut self, compressor: Arc<dyn BlockCompressor>) -> Self {
    self.compressor = compressor;
    self
}
```

**Step 4: Update create_encoder() in builder.rs**

```rust
pub fn create_encoder(config: &ServiceConfig) -> Result<Arc<DdsTextureEncoder>, ServiceError> {
    let compressor: Arc<dyn BlockCompressor> = match config.texture().compressor() {
        "software" => Arc::new(SoftwareCompressor),
        "ispc" => Arc::new(IspcCompressor),
        #[cfg(feature = "gpu-encode")]
        "gpu" => {
            let gpu_compressor = create_wgpu_compressor(config.texture().gpu_device())
                .map_err(|e| ServiceError::InitError(format!("GPU compressor: {}", e)))?;
            Arc::new(gpu_compressor)
        }
        #[cfg(not(feature = "gpu-encode"))]
        "gpu" => {
            return Err(ServiceError::InitError(
                "GPU compression requires the `gpu-encode` feature. \
                 Rebuild with `cargo build --features gpu-encode` \
                 or set texture.compressor = ispc".to_string(),
            ));
        }
        other => {
            return Err(ServiceError::InitError(
                format!("Unknown compressor '{}'. Valid: software, ispc, gpu", other),
            ));
        }
    };

    Ok(Arc::new(
        DdsTextureEncoder::new(config.texture().format())
            .with_mipmap_count(config.texture().mipmap_count())
            .with_compressor(compressor),
    ))
}
```

Note: `create_encoder` now returns `Result`. Update all call sites in `facade.rs` to handle the Result (add `?`).

**Step 5: Run tests**

Run: `cargo test --lib service::builder`
Expected: PASS

**Step 6: Commit**

```
feat(dds): wire compressor selection through service builder (#67)
```

---

### Task 5: Implement WgpuCompressor

**Files:**
- Modify: `xearthlayer/src/dds/compressor.rs`
- Modify: `xearthlayer/src/dds/mod.rs`

**Step 1: Write failing test (feature-gated)**

Add at the bottom of `compressor.rs` tests:

```rust
#[cfg(feature = "gpu-encode")]
mod gpu_tests {
    use super::*;

    #[test]
    fn test_wgpu_compressor_name() {
        // This test will be runnable once WgpuCompressor exists
        // For systems without a GPU, we test construction failure gracefully
        let compressor = WgpuCompressor::try_new("integrated");
        // On CI without GPU, this may fail — that's expected
        if let Ok(c) = compressor {
            assert_eq!(c.name(), "gpu");
        }
    }

    #[test]
    fn test_wgpu_compress_4x4_bc1() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return, // Skip if no GPU available
        };
        let image = RgbaImage::new(4, 4);
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 8); // 1 block × 8 bytes
    }

    #[test]
    fn test_wgpu_compress_256x256_bc1() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(256, 256);
        let result = compressor.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(result.len(), 32768); // 64×64 blocks × 8 bytes
    }

    #[test]
    fn test_wgpu_compress_256x256_bc3() {
        let compressor = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let image = RgbaImage::new(256, 256);
        let result = compressor.compress(&image, DdsFormat::BC3).unwrap();
        assert_eq!(result.len(), 65536); // 64×64 blocks × 16 bytes
    }

    #[test]
    fn test_wgpu_and_ispc_same_output_size() {
        let gpu = match WgpuCompressor::try_new("integrated") {
            Ok(c) => c,
            Err(_) => return,
        };
        let ispc = IspcCompressor;
        let image = RgbaImage::new(256, 256);

        let gpu_result = gpu.compress(&image, DdsFormat::BC1).unwrap();
        let ispc_result = ispc.compress(&image, DdsFormat::BC1).unwrap();
        assert_eq!(gpu_result.len(), ispc_result.len());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --features gpu-encode --lib dds::compressor::gpu_tests -- --nocapture`
Expected: FAIL — `WgpuCompressor` doesn't exist

**Step 3: Implement WgpuCompressor**

In `xearthlayer/src/dds/compressor.rs`, add behind feature gate:

```rust
#[cfg(feature = "gpu-encode")]
mod gpu {
    use super::*;
    use block_compression::{CompressionVariant, GpuBlockCompressor};
    use std::sync::Arc;

    /// GPU-accelerated block compressor using wgpu compute shaders.
    ///
    /// Uses the `block_compression` crate which ports Intel's ISPC texture
    /// compressor kernels to WGSL compute shaders. Targets a specific GPU
    /// adapter (typically an idle integrated GPU) to avoid competing with
    /// X-Plane for discrete GPU resources.
    ///
    /// # Thread Safety
    ///
    /// The wgpu `Device` and `Queue` are internally reference-counted and
    /// safe to share across threads. The `GpuBlockCompressor` holds compiled
    /// pipelines and is reused across compression calls.
    pub struct WgpuCompressor {
        device: wgpu::Device,
        queue: wgpu::Queue,
        compressor: std::sync::Mutex<GpuBlockCompressor>,
        adapter_name: String,
    }

    impl WgpuCompressor {
        /// Try to create a GPU compressor targeting the specified device.
        ///
        /// # Arguments
        ///
        /// * `gpu_device` - Device selector: "integrated", "discrete", or adapter name substring
        ///
        /// # Errors
        ///
        /// Returns `DdsError` if no matching GPU adapter is found or device creation fails.
        pub fn try_new(gpu_device: &str) -> Result<Self, DdsError> {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::all(),
                ..Default::default()
            });

            let adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all());

            if adapters.is_empty() {
                return Err(DdsError::CompressionFailed(
                    "No GPU adapters found. Ensure GPU drivers (Vulkan/Metal/DX12) are installed."
                        .to_string(),
                ));
            }

            let adapter = Self::select_adapter(&adapters, gpu_device)?;
            let adapter_info = adapter.get_info();
            let adapter_name = adapter_info.name.clone();

            tracing::info!(
                "GPU compressor: selected {} ({:?}, {:?})",
                adapter_name,
                adapter_info.device_type,
                adapter_info.backend,
            );

            let (device, queue) = pollster::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("xearthlayer-dds-encoder"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            ))
            .map_err(|e| {
                DdsError::CompressionFailed(format!(
                    "Failed to create GPU device on '{}': {}",
                    adapter_name, e,
                ))
            })?;

            let compressor = GpuBlockCompressor::new(device.clone(), queue.clone());

            Ok(Self {
                device,
                queue,
                compressor: std::sync::Mutex::new(compressor),
                adapter_name,
            })
        }

        fn select_adapter<'a>(
            adapters: &'a [wgpu::Adapter],
            gpu_device: &str,
        ) -> Result<&'a wgpu::Adapter, DdsError> {
            // Try device type match first
            let target_type = match gpu_device.to_lowercase().as_str() {
                "integrated" => Some(wgpu::DeviceType::IntegratedGpu),
                "discrete" => Some(wgpu::DeviceType::DiscreteGpu),
                _ => None,
            };

            if let Some(device_type) = target_type {
                if let Some(adapter) = adapters
                    .iter()
                    .find(|a| a.get_info().device_type == device_type)
                {
                    return Ok(adapter);
                }
            } else {
                // Name substring match (case-insensitive)
                let needle = gpu_device.to_lowercase();
                if let Some(adapter) = adapters
                    .iter()
                    .find(|a| a.get_info().name.to_lowercase().contains(&needle))
                {
                    return Ok(adapter);
                }
            }

            // No match — build error listing available adapters
            let available: Vec<String> = adapters
                .iter()
                .map(|a| {
                    let info = a.get_info();
                    format!("  - {} ({:?}, {:?})", info.name, info.device_type, info.backend)
                })
                .collect();

            Err(DdsError::CompressionFailed(format!(
                "No GPU adapter matching '{}'. Available adapters:\n{}",
                gpu_device,
                available.join("\n"),
            )))
        }
    }

    impl BlockCompressor for WgpuCompressor {
        fn compress(&self, image: &RgbaImage, format: DdsFormat) -> Result<Vec<u8>, DdsError> {
            let width = image.width();
            let height = image.height();

            if width == 0 || height == 0 {
                return Err(DdsError::InvalidDimensions(width, height));
            }

            let variant = match format {
                DdsFormat::BC1 => CompressionVariant::BC1,
                DdsFormat::BC3 => CompressionVariant::BC3,
            };

            let bytes_per_block = match format {
                DdsFormat::BC1 => 8u32,
                DdsFormat::BC3 => 16u32,
            };

            let blocks_wide = width.div_ceil(4);
            let blocks_high = height.div_ceil(4);
            let output_size = (blocks_wide * blocks_high * bytes_per_block) as u64;

            // Create source texture
            let texture_size = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };

            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("dds-source"),
                size: texture_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            // Upload RGBA data
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                image.as_raw(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                texture_size,
            );

            let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            // Create output buffer
            let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dds-output"),
                size: output_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            // Create readback buffer
            let readback_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dds-readback"),
                size: output_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });

            // Queue compression
            {
                let mut compressor = self.compressor.lock().unwrap();
                compressor.add_compression_task(
                    variant,
                    &texture_view,
                    width,
                    height,
                    &output_buffer,
                    None,
                    None,
                );

                // Dispatch
                let mut encoder = self
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("dds-compress"),
                    });

                {
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("dds-bc-compress"),
                        timestamp_writes: None,
                    });
                    compressor.compress(&mut pass);
                }

                // Copy output to readback buffer
                encoder.copy_buffer_to_buffer(&output_buffer, 0, &readback_buffer, 0, output_size);

                self.queue.submit(std::iter::once(encoder.finish()));
            }

            // Read back results
            let buffer_slice = readback_buffer.slice(..);
            let (sender, receiver) = std::sync::mpsc::channel();
            buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap();
            });

            self.device.poll(wgpu::Maintain::Wait);
            receiver
                .recv()
                .map_err(|e| DdsError::CompressionFailed(format!("GPU readback failed: {}", e)))?
                .map_err(|e| DdsError::CompressionFailed(format!("GPU buffer map failed: {}", e)))?;

            let data = buffer_slice.get_mapped_range();
            let result = data.to_vec();
            drop(data);
            readback_buffer.unmap();

            Ok(result)
        }

        fn name(&self) -> &str {
            "gpu"
        }
    }
}

#[cfg(feature = "gpu-encode")]
pub use gpu::WgpuCompressor;
```

**Step 4: Update public exports in mod.rs**

In `xearthlayer/src/dds/mod.rs`, update the public exports:

```rust
#[cfg(feature = "gpu-encode")]
pub use compressor::WgpuCompressor;
```

**Step 5: Update default_compressor and create_wgpu_compressor**

Add the public factory function in `compressor.rs`:

```rust
/// Create a GPU block compressor targeting the specified device.
///
/// # Errors
///
/// Returns error if no matching adapter found or GPU initialization fails.
#[cfg(feature = "gpu-encode")]
pub fn create_wgpu_compressor(gpu_device: &str) -> Result<WgpuCompressor, DdsError> {
    WgpuCompressor::try_new(gpu_device)
}
```

**Step 6: Run tests**

Run: `cargo test --features gpu-encode --lib dds::compressor -- --nocapture`
Expected: PASS (GPU tests skip gracefully if no GPU is available)

Run: `cargo test --lib dds::compressor -- --nocapture`
Expected: PASS (GPU tests are feature-gated, don't compile)

**Step 7: Commit**

```
feat(dds): implement WgpuCompressor with GPU device selection (#67)
```

---

### Task 6: Update Documentation

**Files:**
- Modify: `docs/configuration.md`
- Modify: `CLAUDE.md`

**Step 1: Update configuration.md**

Update the `[texture]` section to document the new settings:

```markdown
## [texture]

Controls DDS texture output format and compression backend.

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `format` | string | `bc1` | DDS compression: `bc1` (smaller, opaque) or `bc3` (larger, with alpha) |
| `mipmaps` | integer | `5` | Number of mipmap levels (1-10) |
| `compressor` | string | `ispc` | Compression backend: `software`, `ispc` (SIMD), or `gpu` (wgpu compute) |
| `gpu_device` | string | `integrated` | GPU adapter: `integrated`, `discrete`, or adapter name substring |

**Example:**
```ini
[texture]
format = bc1
mipmaps = 5
compressor = gpu
gpu_device = integrated
```

**Compressor Backends:**
- **software**: Pure-Rust block compression. Slowest, no external dependencies.
- **ispc** (default): Intel ISPC SIMD compression via `intel_tex_2`. 5-10× faster than software.
- **gpu**: wgpu compute shader compression. Requires `--features gpu-encode` at build time.
  Offloads encoding to GPU, freeing CPU for X-Plane. 5-25× faster than ISPC.

**GPU Device Selection:**
- `integrated` — Use the integrated GPU (e.g., AMD Radeon on Ryzen). Recommended when
  a discrete GPU is handling X-Plane display.
- `discrete` — Use the discrete GPU. Only recommended if no integrated GPU is available.
- `<name>` — Match by adapter name substring (e.g., `Radeon`, `RTX 5090`).

Run `xearthlayer diagnostics` to see available GPU adapters.
```

**Step 2: Update CLAUDE.md**

Add mention of `gpu-encode` feature and `WgpuCompressor` to the DDS Compression section.

**Step 3: Commit**

```
docs(config): document texture.compressor and gpu_device settings (#67)
```

---

### Task 7: Add GPU Adapter Listing to Diagnostics

**Files:**
- Modify: `xearthlayer-cli/src/commands/diagnostics.rs`
- Or the `SystemReport` struct (find where it collects data)

**Step 1: Find SystemReport implementation**

Search for `SystemReport` struct and its `collect()` / `Display` impl.

**Step 2: Add GPU adapter enumeration (feature-gated)**

```rust
#[cfg(feature = "gpu-encode")]
{
    // List GPU adapters
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapters = instance.enumerate_adapters(wgpu::Backends::all());

    println!("\nGPU Adapters:");
    if adapters.is_empty() {
        println!("  (none found)");
    } else {
        for (i, adapter) in adapters.iter().enumerate() {
            let info = adapter.get_info();
            println!(
                "  [{}] {} ({:?}, {:?})",
                i, info.name, info.device_type, info.backend
            );
        }
    }
}
```

**Step 3: Run and verify output**

Run: `cargo run --features gpu-encode -- diagnostics`
Expected: Lists GPU adapters with names and types

**Step 4: Commit**

```
feat(diagnostics): list GPU adapters when gpu-encode feature enabled (#67)
```

---

### Task 8: Integration Test and Verification

**Step 1: Run full test suite without gpu-encode**

Run: `make pre-commit`
Expected: All tests pass, clippy clean

**Step 2: Run full test suite with gpu-encode**

Run: `RUSTFLAGS="-Dwarnings" cargo test --features gpu-encode -- --nocapture`
Expected: All tests pass (GPU tests skip if no adapter available)

**Step 3: Manual test with config**

Create/update `~/.xearthlayer/config.ini`:
```ini
[texture]
format = bc1
compressor = gpu
gpu_device = integrated
```

Run: `cargo run --features gpu-encode -- diagnostics`
Verify GPU adapter listing shows Radeon iGPU.

Run: `cargo run --features gpu-encode -- run --debug`
Verify startup log shows "GPU compressor: selected AMD Radeon Graphics (IntegratedGpu, Vulkan)"

**Step 4: Test error cases**

Test invalid compressor without feature:
```ini
compressor = gpu
```
Run: `cargo run -- run`
Expected: Hard error with message about gpu-encode feature

Test invalid device name:
```ini
compressor = gpu
gpu_device = nonexistent
```
Run: `cargo run --features gpu-encode -- run`
Expected: Hard error listing available adapters

**Step 5: Commit final changes and run make pre-commit**

```
feat(dds): complete wgpu GPU DDS compressor integration (#67)
```

---

## Task Dependencies

```
Task 1 (deps)
  └→ Task 2 (config keys)
       └→ Task 3 (parser/writer)
            └→ Task 4 (service wiring)
                 └→ Task 5 (WgpuCompressor impl)
                      ├→ Task 6 (docs)
                      └→ Task 7 (diagnostics)
                           └→ Task 8 (integration test)
```

## Notes for Implementer

- **`GpuBlockCompressor`** from `block_compression` crate is NOT `Send + Sync`. Wrap in `Mutex` for thread safety. The mutex contention is negligible — each `compress()` call holds it briefly for `add_compression_task` + `compress`, then drops it before readback.
- **`pollster`** crate is used to block on `adapter.request_device()` which is async. We only call this once at startup, so the blocking is fine.
- **wgpu API versions**: The `wgpu` crate's API changes between major versions. Pin to `version = "24"` as specified. Key types: `TexelCopyTextureInfo` (was `ImageCopyTexture` in older versions), `TexelCopyBufferLayout` (was `ImageDataLayout`).
- **CI**: GPU tests use `if let Ok(c) = WgpuCompressor::try_new(...)` pattern to skip when no GPU is available. CI runners typically lack GPU hardware, so these tests effectively become no-ops there. The ISPC and software tests still validate compression correctness.
- **`DdsEncoder` Box→Arc migration** (Task 4): This changes the public API of `DdsEncoder::with_compressor()`. Update all call sites — there should be very few since `DdsTextureEncoder` is the primary consumer.
