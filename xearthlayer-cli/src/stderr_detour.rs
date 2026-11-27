//! Keep stderr off the terminal while a full-screen UI owns it (#358).
//!
//! `tracing` goes to the log file, so nothing XEarthLayer writes reaches the
//! terminal while the dashboard is up. Code it does not own has no such
//! discipline: the Vulkan loader, Mesa and libxcb write their warnings with
//! a raw `write(2)` on descriptor 2, and the text lands wherever the cursor
//! is, inside the ratatui frame. A panic message goes the same way, and is
//! lost with the alternate screen.
//!
//! A [`StderrDetour`] points descriptor 2 at a file for as long as it lives,
//! the log file by preference (it is opened for append, as `tracing`'s own
//! appender is, so the two writers interleave by line and never overwrite
//! each other), and puts the terminal back when it drops. After SIGHUP it
//! leaves descriptor 2 where it is: the terminal is gone, and the signal
//! handler has already pointed the descriptor at `/dev/null` so that no
//! write to it can fail.
//!
//! Its tests live in `tests/stderr_detour.rs`, a process of their own (#471):
//! they move the process's descriptor 2, so in the unit-test binary any other
//! test's panic message landed in their sink, and the SIGHUP that the
//! `signals` tests raise kept a detour from ever putting stderr back.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::signals::terminal_gone;

/// Descriptor 2 pointed at a file until this drops.
#[derive(Debug)]
pub struct StderrDetour {
    /// The terminal's stderr, to put back.
    saved: OwnedFd,
}

impl StderrDetour {
    /// Point stderr at `sink`, created if missing and appended to. Fails
    /// when the sink cannot be opened or the descriptors cannot be moved,
    /// in which case stderr is untouched.
    pub fn to(sink: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(sink)?;
        // SAFETY: F_DUPFD_CLOEXEC returns a fresh descriptor this process
        // owns and nothing else knows about, so OwnedFd may close it. The
        // close-on-exec flag keeps the terminal out of child processes.
        let saved = unsafe {
            let fd = libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 0);
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };
        // SAFETY: both descriptors are open and owned by this process. dup2
        // replaces descriptor 2 atomically; `file` may close its own
        // descriptor afterwards, descriptor 2 keeps the open file alive.
        if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { saved })
    }
}

impl Drop for StderrDetour {
    fn drop(&mut self) {
        if terminal_gone() {
            // SIGHUP pointed stderr at /dev/null; the terminal it was would
            // only fail every write.
            return;
        }
        // SAFETY: `saved` is open and owned by this struct until the end of
        // this function. A failure leaves stderr on the sink, which is the
        // better of the two outcomes.
        let _ = unsafe { libc::dup2(self.saved.as_raw_fd(), libc::STDERR_FILENO) };
    }
}
