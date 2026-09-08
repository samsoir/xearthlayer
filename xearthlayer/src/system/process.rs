//! Process liveness.

/// Is a process with this PID currently running?
///
/// Uses signal 0, which performs the permission and existence checks of
/// `kill(2)` without delivering anything. `EPERM` counts as alive: the process
/// exists, it simply belongs to another user.
///
/// Only meaningful on Unix. Windows is not a target for XEarthLayer, and this
/// returns `false` there rather than pretending to know.
pub fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            // Signal 0 to PID 0 addresses the caller's whole process group,
            // which is never the question being asked here.
            return false;
        }
        // SAFETY: kill() with signal 0 sends nothing. It only reports whether
        // the PID exists and whether we may signal it.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_is_alive() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_never_reported_alive() {
        // PID 0 means "my process group" to kill(2), so answering from the
        // syscall would be answering a different question.
        assert!(!is_alive(0));
    }

    #[test]
    fn a_running_child_is_alive() {
        // A real assertion in the direction that can be made deterministic.
        // The dead-PID direction is deliberately not asserted here: after
        // reaping, a busy machine may recycle the PID, and a test that is
        // usually right is worse than one that is always meaningful. The logic
        // that depends on a dead PID is covered where the predicate is
        // injected, in the running-instance preflight check.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(is_alive(pid), "a child we have not reaped must be alive");
        child.kill().expect("kill");
        child.wait().expect("reap");
    }
}
