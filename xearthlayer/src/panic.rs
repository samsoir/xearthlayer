//! Panic report for `xearthlayer run`.
//!
//! [`init`] installs a process-wide panic hook that writes a report of every
//! panic to stderr: a banner, the panic's location and message, and a note on
//! the scenery mount. It then chains to the hook it replaced. While the
//! dashboard runs, stderr is the flight log (#358), so that is where the
//! report lands.
//!
//! # When the mount is removed
//!
//! This handler never unmounts (#418). It runs at the point of every panic, on
//! the panicking thread, before the stack unwinds and before any
//! `catch_unwind` (or tokio's catching of task panics) sees it, so it cannot
//! know whether the panic will be caught. When it unmounted, one recoverable
//! panic anywhere in the service took all of XEarthLayer's scenery away from
//! X-Plane for the rest of the session.
//!
//! The mount is removed only as the process ends, when `run` unwinds:
//!
//! - **A panic on the main thread** during `run` unwinds through `run`, which
//!   drops its `ServiceOrchestrator`. That drops the `MountManager`, whose
//!   `Drop` calls `unmount_all`; dropping the ortho session
//!   (`SpawnedMountHandle`) unmounts it, escalating through
//!   [`crate::system::unmount_fuse`] (`fusermount3`/`fusermount` on Linux,
//!   `umount -f` on macOS) if the volume is still in the mount table. The
//!   session is dropped before the service that owns its Tokio runtime, so the
//!   FUSE task can still answer while it is unmounted.
//! - **A panic on the dashboard's startup thread** (which mounts, builds the
//!   overlays and the scenery index, and starts prefetch) is carried to the
//!   main thread when the thread ends, and unwinds from there as above.
//! - **A panic on any other thread** does not touch the mount: a tokio task or
//!   `spawn_blocking` closure becomes a `JoinError`, the GPU worker catches its
//!   own, and a plain thread that nothing catches ends alone.
//! - **Not covered**: a panic raised while another unwinds (a `Drop` that
//!   panics) aborts the process before the drop path runs, as does
//!   `SIGKILL`. The stale mount is then left for the next `run` to clear
//!   ([`crate::system::recover_stale_fuse_mount`], before it mounts).

use std::any::Any;
use std::io::{self, Write};
use std::panic::{self, Location, PanicHookInfo};
use std::sync::Once;

/// Guards [`install`], so a second call does not stack a second hook.
static INSTALL: Once = Once::new();

/// Install the panic hook, reporting to stderr.
///
/// Call once, early in `run`. For every panic, on any thread, the hook writes
/// the report described in the module docs, then calls the hook it replaced.
/// It never unmounts: see "When the mount is removed" in the module docs.
/// Once a hook is installed, by this or by [`install`], later calls do
/// nothing.
pub fn init() {
    install(|| io::stderr().lock());
}

/// Install the panic hook, writing each report to the writer `sink` returns.
///
/// [`init`] is this with stderr; a test passes a writer it can read back.
/// `sink` runs on the panicking thread, inside the hook, so it must not panic
/// (a panic there aborts the process). Once a hook is installed, by this or
/// by [`init`], later calls do nothing.
pub fn install<W, S>(sink: S)
where
    W: Write,
    S: Fn() -> W + Send + Sync + 'static,
{
    INSTALL.call_once(|| {
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            // Straight to the sink, not through `tracing`: logging may be
            // what panicked.
            let mut out = sink();
            let _ = write_report(&mut out, info.location(), info.payload());
            let _ = out.flush();
            drop(out);
            original_hook(info);
        }));
    });
}

/// Write the panic report: banner, location, message, and what becomes of
/// the scenery mount.
///
/// The report reaches `out` in one write: panics on two threads at once share
/// the sink, and a report written piece by piece interleaves with the other.
fn write_report<W: Write>(
    out: &mut W,
    location: Option<&Location<'_>>,
    payload: &(dyn Any + Send),
) -> io::Result<()> {
    let mut report = Vec::new();
    format_report(&mut report, location, payload)?;
    out.write_all(&report)
}

/// The report [`write_report`] writes, built in `out`.
fn format_report(
    out: &mut Vec<u8>,
    location: Option<&Location<'_>>,
    payload: &(dyn Any + Send),
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

    writeln!(out, "━━━ Scenery Mount ━━━")?;
    writeln!(out, "{}", MOUNT_NOTE)?;
    writeln!(out)?;
    writeln!(out, "━━━ End of XEarthLayer Panic Handler ━━━")?;
    writeln!(out)
}

/// What the report says about the mount: the handler leaves it alone (#418).
const MOUNT_NOTE: &str = "This handler never unmounts. The mount is removed only as the \
process ends, when `run` unwinds (a panic on the startup thread is carried to the main \
thread first).";

/// The text of a caught panic's payload (from `catch_unwind` or a thread's
/// `join`): the message of `panic!("...")` or `panic!("{}", ..)`, else
/// "unknown panic".
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload a `panic!` with this message carries, as the hook gets it.
    fn payload_of(panic: impl FnOnce() + std::panic::UnwindSafe) -> Box<dyn Any + Send> {
        // Unit tests never install this module's hook (it is process-wide);
        // the default hook prints the expected panic into captured output.
        std::panic::catch_unwind(panic).expect_err("the closure panics")
    }

    fn report_text(location: Option<&Location<'_>>, payload: &(dyn Any + Send)) -> String {
        let mut out = Vec::new();
        write_report(&mut out, location, payload).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_report_reaches_the_writer_in_one_write() {
        // Two threads panicking at once share the sink: a report written in
        // pieces interleaves with the other one, splitting "Message: " from
        // its message (the flaky `panic_hook` integration test).
        struct Writes(Vec<Vec<u8>>);
        impl Write for Writes {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.push(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let payload = payload_of(|| panic!("tile decode failed"));
        let mut out = Writes(Vec::new());

        write_report(&mut out, Some(Location::caller()), payload.as_ref()).unwrap();

        assert_eq!(out.0.len(), 1);
        assert!(String::from_utf8_lossy(&out.0[0]).contains("Message: tile decode failed"));
    }

    #[test]
    fn panic_message_reads_str_string_and_other_payloads() {
        let caught = |f: fn()| panic_message(panic::catch_unwind(f).unwrap_err().as_ref());
        assert_eq!(caught(|| panic!("plain")), "plain");
        assert_eq!(caught(|| panic!("formatted {}", 7)), "formatted 7");
        assert_eq!(caught(|| panic::panic_any(42_u32)), "unknown panic");
    }

    #[test]
    fn the_report_has_the_banner_location_and_message() {
        let payload = payload_of(|| panic!("tile decode failed"));
        let location = Location::caller();

        let text = report_text(Some(location), payload.as_ref());

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

        let text = report_text(None, payload.as_ref());

        assert!(text.contains("Message: tile 42 failed"));
    }

    #[test]
    fn the_report_says_the_handler_never_unmounts() {
        // #418: the hook runs before anyone knows whether the panic will be
        // caught, so it must not unmount, and says so rather than claiming a
        // cleanup it no longer does.
        let payload = payload_of(|| panic!("boom"));

        let text = report_text(None, payload.as_ref());

        assert!(text.contains("Scenery Mount"));
        assert!(text.contains(MOUNT_NOTE));
        assert!(!text.contains("Unmounting"));
    }
}
