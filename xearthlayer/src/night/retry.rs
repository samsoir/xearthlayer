//! The retry policy of the lit path's file reads: recipes and Laminar art.

use std::time::Duration;

/// Extra attempts at a read that failed with an I/O error (a recipe, or a
/// Laminar art file) before it is given up on for the session.
pub(crate) const READ_RETRIES: u32 = 2;

/// Pause between two attempts at a read.
pub(crate) const READ_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Calls `attempt` until it succeeds, at most `retries + 1` times, sleeping
/// `delay` between attempts. Blocking: call from a blocking context.
pub(crate) fn with_retries<T, E>(
    retries: u32,
    delay: Duration,
    mut attempt: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut left = retries;
    loop {
        match attempt() {
            Err(_) if left > 0 => {
                left -= 1;
                std::thread::sleep(delay);
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_stop_at_the_first_success() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, Duration::ZERO, || {
            calls += 1;
            if calls < 2 {
                Err("transient")
            } else {
                Ok(7)
            }
        });
        assert_eq!((r, calls), (Ok(7), 2));
    }

    #[test]
    fn retries_give_up_after_the_limit() {
        let mut calls = 0;
        let r: Result<u8, &str> = with_retries(2, Duration::ZERO, || {
            calls += 1;
            Err("down")
        });
        assert_eq!((r, calls), (Err("down"), 3));
    }
}
