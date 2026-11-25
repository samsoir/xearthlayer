//! Health of the GPU encoder (#420): whether the GPU worker can still encode
//! tiles, read by the encoder before each tile and by the dashboard on every
//! tick.
//!
//! The health starts [`GpuHealth::Healthy`] and turns
//! [`GpuHealth::Failed`] once, for good (there is no respawn): on a panic the
//! worker loop caught, on a device lost for a reason other than an explicit
//! `Device::destroy` ([`device_lost`]), or when the worker's tiles miss their
//! deadline too many times in a row (`gpu_channel::HangRule`). The first
//! failure's reason is kept; whoever records it logs the one WARN. From then
//! on every tile is handed back to its caller, which encodes it on the CPU
//! (`DdsEncoder`'s fallback compressor, ISPC in the service).

use std::sync::{Arc, OnceLock};

/// What the GPU encoder is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuHealth {
    /// The GPU worker is encoding tiles.
    Healthy,
    /// The GPU worker has stopped; every tile is encoded on the CPU for the
    /// rest of the session.
    Failed {
        /// Why: the panic's message, the device-lost message, or the missed
        /// deadlines.
        reason: String,
    },
}

/// The GPU encoder's health, shared by the worker, the device-lost callback,
/// the channel's callers and the dashboard. Cloning shares it.
///
/// Reads take no lock: the reason is set once, in a [`OnceLock`].
#[derive(Clone, Debug, Default)]
pub struct SharedGpuHealth(Arc<OnceLock<String>>);

impl SharedGpuHealth {
    /// A healthy GPU.
    pub fn new() -> Self {
        Self::default()
    }

    /// The health now.
    pub fn health(&self) -> GpuHealth {
        match self.failure() {
            None => GpuHealth::Healthy,
            Some(reason) => GpuHealth::Failed {
                reason: reason.to_string(),
            },
        }
    }

    /// Why the GPU failed, or `None` while it is healthy.
    pub fn failure(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }

    /// Whether the GPU has failed.
    pub fn is_failed(&self) -> bool {
        self.0.get().is_some()
    }

    /// Records that the GPU failed for `reason`. The first call turns the
    /// health Failed, logs the one WARN and returns `true`; later calls keep
    /// the first reason, log nothing and return `false`.
    pub fn fail(&self, reason: impl Into<String>) -> bool {
        let mut first = false;
        let reason = self.0.get_or_init(|| {
            first = true;
            reason.into()
        });
        if first {
            tracing::warn!(
                reason = %reason,
                "GPU encoder failed, encoding on CPU (ISPC) for the rest of the session"
            );
        }
        first
    }
}

/// What the device-lost callback registered by
/// [`create_gpu_resources`](super::create_gpu_resources) does: a device lost
/// for [`wgpu::DeviceLostReason::Unknown`] (a driver reset or fault) fails
/// the GPU with `message`; [`wgpu::DeviceLostReason::Destroyed`] follows only
/// an explicit `Device::destroy`, which XEarthLayer never calls, and is
/// logged only.
pub fn device_lost(health: &SharedGpuHealth, reason: wgpu::DeviceLostReason, message: &str) {
    match reason {
        wgpu::DeviceLostReason::Unknown => {
            health.fail(format!("GPU device lost: {message}"));
        }
        wgpu::DeviceLostReason::Destroyed => {
            tracing::info!(message, "GPU device destroyed");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An event's level and fields.
    #[derive(Clone, Debug)]
    pub(crate) struct Event {
        pub level: tracing::Level,
        pub fields: HashMap<String, String>,
    }

    impl Event {
        pub fn message(&self) -> Option<&str> {
            self.fields.get("message").map(String::as_str)
        }
    }

    /// Collects every event it sees, with its level.
    struct Layer(Arc<Mutex<Vec<Event>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
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
            self.0.lock().unwrap().push(Event {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    /// Every event emitted on this thread while `f` runs.
    pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Event>) {
        use tracing_subscriber::layer::SubscriberExt;
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Layer(Arc::clone(&events)));
        let out = tracing::subscriber::with_default(subscriber, f);
        let captured = events.lock().unwrap().clone();
        (out, captured)
    }

    pub(crate) fn warnings(events: &[Event]) -> Vec<&Event> {
        events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect()
    }

    #[test]
    fn a_new_gpu_is_healthy() {
        let health = SharedGpuHealth::new();
        assert_eq!(health.health(), GpuHealth::Healthy);
        assert!(!health.is_failed());
        assert_eq!(health.failure(), None);
    }

    #[test]
    fn the_first_failure_wins_and_is_seen_by_every_clone() {
        let health = SharedGpuHealth::new();
        let dashboard = health.clone();
        assert!(health.fail("first"));
        assert!(!health.fail("second"));
        assert_eq!(
            dashboard.health(),
            GpuHealth::Failed {
                reason: "first".to_string()
            }
        );
        assert!(dashboard.is_failed());
    }

    #[test]
    fn only_the_failure_that_turns_the_health_logs_one_warning() {
        let health = SharedGpuHealth::new();
        let (_, events) = capture(|| {
            health.fail("GPU worker panicked: boom");
            health.fail("again");
        });
        let warnings = warnings(&events);
        assert_eq!(warnings.len(), 1, "{events:?}");
        assert_eq!(
            warnings[0].fields.get("reason").map(String::as_str),
            Some("GPU worker panicked: boom")
        );
        assert!(warnings[0]
            .message()
            .is_some_and(|m| m.contains("encoding on CPU (ISPC)")));
    }

    #[test]
    fn a_device_lost_for_an_unknown_reason_fails_the_gpu() {
        let health = SharedGpuHealth::new();
        device_lost(&health, wgpu::DeviceLostReason::Unknown, "driver reset");
        assert_eq!(health.failure(), Some("GPU device lost: driver reset"));
    }

    #[test]
    fn a_destroyed_device_does_not_fail_the_gpu() {
        let health = SharedGpuHealth::new();
        let (_, events) = capture(|| device_lost(&health, wgpu::DeviceLostReason::Destroyed, ""));
        assert_eq!(health.health(), GpuHealth::Healthy);
        assert!(warnings(&events).is_empty());
    }
}
