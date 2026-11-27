//! Tests of `StderrDetour`, which moves the process's descriptor 2 (#358).
//!
//! A process of their own (#471). In the binary's unit-test process two
//! neighbours broke them: under `--nocapture` (the Makefile's) any test's
//! panic message is written to descriptor 2 and landed in the sink a test
//! was reading back, and the `signals` tests raise a real SIGHUP, after which
//! `terminal_gone()` is true for the life of the process and no detour puts
//! stderr back. Here nothing else runs, and the tests take turns.
//!
//! `xearthlayer-cli` is a binary with no library, so the module is compiled
//! into this test from its own source file. A library target would either
//! compile `signals` a second time, with a second SIGHUP flag that the
//! running handler never sets, or move the binary's modules behind it.

#[path = "../src/stderr_detour.rs"]
mod stderr_detour;

/// The one item `stderr_detour` reads from the binary's `signals` module.
/// Nothing in this process raises SIGHUP, so the terminal is never gone.
mod signals {
    pub fn terminal_gone() -> bool {
        false
    }
}

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use stderr_detour::StderrDetour;

/// The tests move the process's one descriptor 2, so they take turns.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The device and inode descriptor 2 points at.
fn stderr_identity() -> (libc::dev_t, libc::ino_t) {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat fills the struct it is given; descriptor 2 is open.
    let rc = unsafe { libc::fstat(libc::STDERR_FILENO, stat.as_mut_ptr()) };
    assert_eq!(rc, 0, "fstat(2) failed");
    let stat = unsafe { stat.assume_init() };
    (stat.st_dev, stat.st_ino)
}

/// A write that reaches descriptor 2 itself: the harness captures the
/// print macros, not the handle.
fn write_to_fd_2(text: &str) {
    io::stderr().write_all(text.as_bytes()).unwrap();
}

#[test]
fn stderr_goes_to_the_sink_while_the_detour_lives() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("xearthlayer.log");

    let detour = StderrDetour::to(&sink).unwrap();
    write_to_fd_2("Authorization required, but no authorization protocol specified\n");
    drop(detour);

    assert_eq!(
        std::fs::read_to_string(&sink).unwrap(),
        "Authorization required, but no authorization protocol specified\n"
    );
}

#[test]
fn the_sink_is_appended_to_not_truncated() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("xearthlayer.log");
    std::fs::write(&sink, "earlier log line\n").unwrap();

    let detour = StderrDetour::to(&sink).unwrap();
    write_to_fd_2("stray\n");
    drop(detour);

    assert_eq!(
        std::fs::read_to_string(&sink).unwrap(),
        "earlier log line\nstray\n"
    );
}

#[test]
fn the_original_stderr_comes_back_when_the_detour_drops() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let sink = dir.path().join("xearthlayer.log");
    let before = stderr_identity();

    let detour = StderrDetour::to(&sink).unwrap();
    assert_ne!(stderr_identity(), before, "stderr still on the terminal");
    drop(detour);

    assert_eq!(stderr_identity(), before);
}

#[test]
fn an_unopenable_sink_leaves_stderr_alone() {
    let _serial = serial();
    let before = stderr_identity();

    let result = StderrDetour::to(Path::new("/nonexistent/dir/xearthlayer.log"));

    assert!(result.is_err());
    assert_eq!(stderr_identity(), before);
}
