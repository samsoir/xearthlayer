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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard};

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
}
