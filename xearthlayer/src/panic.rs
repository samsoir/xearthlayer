//! Panic report for `xearthlayer run`.
//!
//! [`init`] installs a process-wide panic hook that writes a report of every
//! panic to stderr: a banner, the panic's location and message, and a
//! telemetry snapshot when a source was registered with
//! [`set_telemetry_callback`]. It then chains to the hook it replaced. While
//! the dashboard runs, stderr is the flight log (#358), so that is where the
//! report lands.
//!
//! # When the mount is removed
//!
//! The hook never unmounts (#418). It runs at the point of every panic, on the
//! panicking thread, before the stack unwinds and before any `catch_unwind`
//! (or tokio's catching of task panics) sees it, so it cannot know whether the
//! panic will be caught. When it unmounted, one recoverable panic anywhere in
//! the service took all of XEarthLayer's scenery away from X-Plane for the
//! rest of the session.
//!
//! - **A panic on the main thread** during `run` unwinds through `run`, which
//!   drops its `ServiceOrchestrator`. That drops the `MountManager`, whose
//!   `Drop` calls `unmount_all`; dropping the ortho session
//!   (`SpawnedMountHandle`) unmounts it, escalating through
//!   [`crate::system::unmount_fuse`] (`fusermount3`/`fusermount` on Linux,
//!   `umount -f` on macOS) if the volume is still in the mount table. The
//!   session is dropped before the service that owns its Tokio runtime, so the
//!   FUSE task can still answer while it is unmounted.
//! - **A panic on any other thread** is caught or ends only that thread: a
//!   tokio task or `spawn_blocking` closure becomes a `JoinError`, the GPU
//!   worker catches its own. The mount stays and keeps serving.
//! - **Not covered**: a panic raised while another unwinds (a `Drop` that
//!   panics) aborts the process before the drop path runs, as does
//!   `SIGKILL`. The stale mount is then left for the next `run` to clear
//!   ([`crate::system::recover_stale_fuse_mount`], before it mounts).

use std::any::Any;
use std::io::{self, Write};
use std::panic::{self, Location, PanicHookInfo};
use std::sync::{Mutex, Once};

use crate::metrics::TelemetrySnapshot;

/// A source of the telemetry snapshot written with every panic report.
type TelemetrySource = Box<dyn Fn() -> TelemetrySnapshot + Send + Sync>;

/// The registered telemetry source, if any. A `static` because a panic hook
/// must be `'static`.
static TELEMETRY_SOURCE: Mutex<Option<TelemetrySource>> = Mutex::new(None);

/// Guards [`init`], so a second call does not stack a second hook.
static INIT: Once = Once::new();

/// Install the panic hook.
///
/// Call once, early in `run`. For every panic, on any thread, the hook writes
/// the report described in the module docs to stderr, then calls the hook
/// it replaced. It does not unmount anything: see "When the mount is removed"
/// in the module docs. Later calls do nothing.
pub fn init() {
    INIT.call_once(|| {
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            report(info);
            original_hook(info);
        }));
    });
}

/// Register the source of the telemetry snapshot written with every panic
/// report, replacing any earlier one.
///
/// The source runs on the panicking thread, inside the panic hook, so it must
/// not panic or block: a panic there aborts the process.
pub fn set_telemetry_callback<F>(callback: F)
where
    F: Fn() -> TelemetrySnapshot + Send + Sync + 'static,
{
    if let Ok(mut source) = TELEMETRY_SOURCE.lock() {
        *source = Some(Box::new(callback));
    }
}

/// The hook's body: take a snapshot and write the report to stderr.
fn report(info: &PanicHookInfo<'_>) {
    // `try_lock`, not `lock`: a panic hook never waits, on another thread
    // registering a source or on a lock this thread already holds.
    let telemetry = TELEMETRY_SOURCE
        .try_lock()
        .ok()
        .and_then(|source| source.as_ref().map(|snapshot| snapshot()));

    // Straight to stderr, since logging may be what panicked.
    let mut stderr = io::stderr().lock();
    let _ = write_report(
        &mut stderr,
        info.location(),
        info.payload(),
        telemetry.as_ref(),
    );
    let _ = stderr.flush();
}

/// Write the panic report: banner, location, message, the telemetry snapshot
/// when there is one, and what becomes of the scenery mount.
fn write_report<W: Write>(
    out: &mut W,
    location: Option<&Location<'_>>,
    payload: &(dyn Any + Send),
    telemetry: Option<&TelemetrySnapshot>,
) -> io::Result<()> {
    writeln!(out, "\n")?;
    writeln!(
        out,
        "╔══════════════════════════════════════════════════════════════════╗"
    )?;
    writeln!(
        out,
        "║                    XEARTHLAYER PANIC HANDLER                     ║"
    )?;
    writeln!(
        out,
        "╚══════════════════════════════════════════════════════════════════╝"
    )?;
    writeln!(out)?;

    writeln!(out, "━━━ Panic Information ━━━")?;
    if let Some(location) = location {
        writeln!(
            out,
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )?;
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        writeln!(out, "Message: {}", message)?;
    } else if let Some(message) = payload.downcast_ref::<String>() {
        writeln!(out, "Message: {}", message)?;
    }
    writeln!(out)?;

    if let Some(snapshot) = telemetry {
        writeln!(out, "━━━ Telemetry State ━━━")?;
        writeln!(out, "Uptime:           {:?}", snapshot.uptime)?;
        writeln!(out, "Jobs running:     {}", snapshot.jobs_active)?;
        writeln!(out, "Jobs completed:   {}", snapshot.jobs_completed)?;
        writeln!(out, "Jobs failed:      {}", snapshot.jobs_failed)?;
        writeln!(out, "Jobs timed out:   {}", snapshot.jobs_timed_out)?;
        writeln!(out, "Downloads active: {}", snapshot.downloads_active)?;
        writeln!(out, "Encodes active:   {}", snapshot.encodes_active)?;
        writeln!(
            out,
            "Memory cache:     {} bytes",
            snapshot.memory_cache_size_bytes
        )?;
        writeln!(
            out,
            "DDS disk cache:   {} bytes",
            snapshot.dds_disk_cache_size_bytes
        )?;
        writeln!(
            out,
            "Chunk disk cache: {} bytes",
            snapshot.chunk_disk_cache_size_bytes
        )?;
        writeln!(out)?;
    }

    writeln!(out, "━━━ Scenery Mount ━━━")?;
    writeln!(out, "{}", MOUNT_NOTE)?;
    writeln!(out)?;
    writeln!(out, "━━━ End of XEarthLayer Panic Handler ━━━")?;
    writeln!(out)
}

/// What the report says about the mount: the handler leaves it alone (#418).
const MOUNT_NOTE: &str = "Left mounted by this handler. A panic on the main thread unmounts \
as `run` unwinds; on any other thread it is caught or ends only that thread, \
and the scenery keeps being served.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The payload a `panic!` with this message carries, as the hook gets it.
    fn payload_of(panic: impl FnOnce() + std::panic::UnwindSafe) -> Box<dyn Any + Send> {
        // Unit tests never install this module's hook (it is process-wide);
        // the default hook prints the expected panic into captured output.
        std::panic::catch_unwind(panic).expect_err("the closure panics")
    }

    fn report_text(
        location: Option<&Location<'_>>,
        payload: &(dyn Any + Send),
        telemetry: Option<&TelemetrySnapshot>,
    ) -> String {
        let mut out = Vec::new();
        write_report(&mut out, location, payload, telemetry).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_report_has_the_banner_location_and_message() {
        let payload = payload_of(|| panic!("tile decode failed"));
        let location = Location::caller();

        let text = report_text(Some(location), payload.as_ref(), None);

        assert!(text.contains("XEARTHLAYER PANIC HANDLER"));
        assert!(text.contains(&format!(
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )));
        assert!(text.contains("Message: tile decode failed"));
        assert!(text.contains("End of XEarthLayer Panic Handler"));
    }

    #[test]
    fn a_formatted_message_is_reported() {
        let tile = 42;
        let payload = payload_of(move || panic!("tile {tile} failed"));

        let text = report_text(None, payload.as_ref(), None);

        assert!(text.contains("Message: tile 42 failed"));
    }

    #[test]
    fn the_telemetry_snapshot_is_reported_when_there_is_one() {
        let payload = payload_of(|| panic!("boom"));
        let snapshot = TelemetrySnapshot {
            uptime: Duration::from_secs(90),
            jobs_active: 7,
            jobs_failed: 3,
            memory_cache_size_bytes: 1234,
            ..TelemetrySnapshot::default()
        };

        let text = report_text(None, payload.as_ref(), Some(&snapshot));

        assert!(text.contains("Telemetry State"));
        assert!(text.contains("Uptime:           90s"));
        assert!(text.contains("Jobs running:     7"));
        assert!(text.contains("Jobs failed:      3"));
        assert!(text.contains("Memory cache:     1234 bytes"));
    }

    #[test]
    fn without_a_telemetry_source_the_section_is_left_out() {
        let payload = payload_of(|| panic!("boom"));

        let text = report_text(None, payload.as_ref(), None);

        assert!(!text.contains("Telemetry State"));
    }

    #[test]
    fn the_report_says_the_mount_is_left_in_place() {
        // #418: the hook runs before anyone knows whether the panic will be
        // caught, so it must not unmount, and says so rather than claiming a
        // cleanup it no longer does.
        let payload = payload_of(|| panic!("boom"));

        let text = report_text(None, payload.as_ref(), None);

        assert!(text.contains("Scenery Mount"));
        assert!(text.contains(MOUNT_NOTE));
        assert!(!text.contains("Unmounting"));
    }
}
