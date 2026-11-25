//! The queue behind a late GPU tile is handed back before the tile is
//! answered (#420), observed in a process of its own.
//!
//! What goes back must be the queue that waited behind the late tile,
//! never a request its caller sent after the reply. The order is seen from
//! the worker's own thread: when the hand-back's debug event fires, the
//! late tile's reply must not have been sent yet. Answering first fails
//! this every time, whatever the scheduling.
//!
//! In the library's test binary a callsite first hit by another test's
//! thread while a scoped dispatcher is being set up can keep a "never"
//! interest, and the event is then missed (see `tests/gpu_fallback_warning.rs`).
//! Here a global subscriber sees every thread, and nothing else runs.
//!
//! No GPU is needed: the real worker loop runs around a processor that
//! overruns its deadline.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::RgbaImage;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xearthlayer::dds::gpu_channel::{
    spawn_tile_worker, GpuEncodeReply, GpuEncodeRequest, GpuTileTiming, HangRule, TileProcessor,
    CHANNEL_CAPACITY,
};
use xearthlayer::dds::gpu_health::SharedGpuHealth;
use xearthlayer::dds::{DdsError, DdsFormat};

type Reply = Arc<Mutex<oneshot::Receiver<GpuEncodeReply>>>;

/// At each hand-back event (the one with a `handed` field), whether the
/// late tile's reply was already there.
struct AtHandBack(Reply, Arc<Mutex<Vec<bool>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AtHandBack {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().fields().field("handed").is_some() {
            let answered = self.0.lock().unwrap().try_recv().is_ok();
            self.1.lock().unwrap().push(answered);
        }
    }
}

/// Every tile takes longer than the rule's deadline.
struct Overruns;

impl TileProcessor for Overruns {
    fn process(
        &mut self,
        _image: RgbaImage,
        _format: DdsFormat,
        _levels: usize,
        _deadline: Instant,
        _timing: &mut GpuTileTiming,
    ) -> Result<Vec<u8>, DdsError> {
        std::thread::sleep(Duration::from_millis(60));
        Ok(vec![0; 8])
    }
}

fn request(width: u32) -> (GpuEncodeRequest, oneshot::Receiver<GpuEncodeReply>) {
    let (response, reply) = oneshot::channel();
    let request = GpuEncodeRequest {
        image: RgbaImage::new(width, 4),
        format: DdsFormat::BC1,
        mipmap_count: 1,
        sent_at: Instant::now(),
        response,
    };
    (request, reply)
}

#[tokio::test]
async fn the_queue_is_handed_back_before_the_late_tile_is_answered() {
    use tracing_subscriber::layer::SubscriberExt;

    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (late, late_reply) = request(4);
    let (queued, queued_reply) = request(8);
    tx.try_send(late).unwrap();
    tx.try_send(queued).unwrap();
    // The worker serves what is buffered, then ends.
    drop(tx);

    let late_reply: Reply = Arc::new(Mutex::new(late_reply));
    let seen = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(AtHandBack(Arc::clone(&late_reply), Arc::clone(&seen))),
    )
    .expect("the only subscriber in this process");

    let rule = HangRule {
        deadline: Duration::from_millis(40),
        misses: 3,
    };
    spawn_tile_worker(
        Overruns,
        rx,
        CancellationToken::new(),
        SharedGpuHealth::new(),
        rule,
    )
    .await
    .expect("the loop never panics");

    assert_eq!(
        *seen.lock().unwrap(),
        [false],
        "one hand-back, before the late tile's reply"
    );
    // The subscriber keeps its handle: take the receiver out from under it.
    let late_reply = std::mem::replace(&mut *late_reply.lock().unwrap(), oneshot::channel().1);
    match late_reply.await.expect("the late tile is answered") {
        GpuEncodeReply::Processed { result, .. } => assert!(result.is_ok()),
        other => panic!("the late tile was processed, got {other:?}"),
    }
    match queued_reply.await.expect("the queued request is answered") {
        GpuEncodeReply::Returned { image, .. } => assert_eq!(image.width(), 8),
        other => panic!("the queued request came back, got {other:?}"),
    }
}
