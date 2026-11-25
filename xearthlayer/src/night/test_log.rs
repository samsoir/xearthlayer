//! Log capture for the night lighting tests.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::subscriber::{DefaultGuard, NoSubscriber};
use tracing::Dispatch;

#[derive(Clone)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything logged at debug and above on this thread while it lives.
///
/// Tests run in parallel and the others log with no subscriber. While at
/// most one dispatcher is registered, tracing computes a callsite's interest
/// from the registering thread's own default, without its dispatcher lock:
/// another thread's first hit of a `warn!` while this test runs would store
/// "never" for every thread, and the line would vanish (about one parallel
/// run in ten). Two dispatchers kept for the life of the test process end
/// that mode, so every interest is computed from the dispatcher list under
/// its lock, and the rebuild after installing the capture settles any
/// interest stored before.
///
/// Only this thread's events are captured: in an async test, use the
/// current-thread runtime (`#[tokio::test]`'s default), and expect nothing
/// from `spawn_blocking` work.
pub(crate) struct LogCapture {
    buf: Buf,
    _guard: DefaultGuard,
}

impl LogCapture {
    pub(crate) fn start() -> Self {
        static KEEP: OnceLock<[Dispatch; 2]> = OnceLock::new();
        KEEP.get_or_init(|| {
            [
                Dispatch::new(NoSubscriber::default()),
                Dispatch::new(NoSubscriber::default()),
            ]
        });
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        Self { buf, _guard: guard }
    }

    /// What was logged so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.buf.0.lock().unwrap().clone()).unwrap()
    }
}

/// Runs `f` under a [`LogCapture`] and returns everything it logged.
pub(crate) fn logged<F: FnOnce()>(f: F) -> String {
    let capture = LogCapture::start();
    f();
    capture.text()
}
