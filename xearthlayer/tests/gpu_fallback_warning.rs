//! The one WARN of a GPU encoder failure (#420), counted in a process of its
//! own.
//!
//! Every worker-loop unit test can fail a health and emit this WARN from a
//! blocking thread. A callsite caches its interest when first hit, and while
//! one scoped dispatcher exists tracing-core asks only the hitting thread's,
//! so in the library's test binary a thread-local capture can miss it. Here
//! a global subscriber sees every thread, and nothing else runs.
//!
//! No GPU is needed: the real worker loop runs around a processor that
//! panics.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use image::RgbaImage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xearthlayer::dds::gpu_channel::{
    spawn_tile_worker, GpuEncodeRequest, GpuEncoderChannel, GpuTileTiming, HangRule, TileProcessor,
    CHANNEL_CAPACITY,
};
use xearthlayer::dds::gpu_health::{device_lost, SharedGpuHealth};
use xearthlayer::dds::{DdsEncoder, DdsError, DdsFormat, IspcCompressor, MipmapCompressor};

/// Every WARN event's fields, from every thread.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<HashMap<String, String>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        struct Visitor<'a>(&'a mut HashMap<String, String>);
        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_string(), format!("{v:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, v: &str) {
                self.0.insert(field.name().to_string(), v.to_string());
            }
        }
        let mut fields = HashMap::new();
        event.record(&mut Visitor(&mut fields));
        self.0.lock().unwrap().push(fields);
    }
}

/// Panics on every tile, as `Device::poll` does on a lost device.
struct Panics;

impl TileProcessor for Panics {
    fn process(
        &mut self,
        _image: RgbaImage,
        _format: DdsFormat,
        _levels: usize,
        _deadline: Instant,
        _timing: &mut GpuTileTiming,
    ) -> Result<Vec<u8>, DdsError> {
        panic!("Device::poll: device lost")
    }
}

/// A panic, the queue behind it, a device-lost callback after it and tiles
/// after that: one WARN, naming the panic, however many tiles fall back.
#[tokio::test]
async fn a_gpu_failure_logs_one_warning_however_many_tiles_fall_back() {
    use tracing_subscriber::layer::SubscriberExt;
    let warnings = Warnings::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(warnings.clone()))
        .expect("the only subscriber in this process");

    let (tx, rx) = mpsc::channel::<GpuEncodeRequest>(CHANNEL_CAPACITY);
    let health = SharedGpuHealth::new();
    let channel = Arc::new(GpuEncoderChannel::new(
        tx,
        health.clone(),
        CancellationToken::new(),
    ));
    let encoder = Arc::new(DdsEncoder::new(DdsFormat::BC1).with_mipmap_compressor(
        Arc::clone(&channel) as Arc<dyn MipmapCompressor>,
        Arc::new(IspcCompressor),
    ));
    let worker = spawn_tile_worker(
        Panics,
        rx,
        CancellationToken::new(),
        health.clone(),
        HangRule::default(),
    );

    // A burst: one tile meets the panic, the others are queued or sent
    // after it; every one but the first is encoded on the fallback.
    let burst: Vec<_> = (0..6)
        .map(|_| {
            let encoder = Arc::clone(&encoder);
            tokio::task::spawn_blocking(move || encoder.encode(RgbaImage::new(64, 64)))
        })
        .collect();
    let mut failed = 0;
    for tile in burst {
        failed += usize::from(tile.await.unwrap().is_err());
    }
    worker.await.expect("the loop never panics");
    // A device-lost callback after the panic, and more tiles.
    device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
    for _ in 0..3 {
        let encoder = Arc::clone(&encoder);
        tokio::task::spawn_blocking(move || encoder.encode(RgbaImage::new(64, 64)))
            .await
            .unwrap()
            .expect("a later tile is encoded on the fallback");
    }

    assert_eq!(failed, 1, "only the tile in progress fails");
    let warnings = warnings.0.lock().unwrap().clone();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(
        warnings[0].get("reason").map(String::as_str),
        Some("GPU worker panicked: Device::poll: device lost")
    );
    assert!(warnings[0]
        .get("message")
        .is_some_and(|m| m.contains("encoding on CPU (ISPC)")));
    let sample = channel.stats().take_sample().expect("a busy sample");
    assert_eq!((sample.tiles(), sample.failed, sample.fallback), (0, 1, 8));
}
