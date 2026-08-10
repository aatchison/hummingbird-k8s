//! Retry / poll helper used by live-path subcommands.
//!
//! [`retry`] is the shared building block for CP-IP, kubectl-Ready, and
//! cluster-Ready polls in `deploy-cluster` (S2) and `spawn-workers` (S3).
//! It mirrors the bash pattern:
//!
//! ```bash
//! for i in $(seq 1 "$attempts"); do
//!   if <condition>; then return 0; fi
//!   sleep "$sleep_secs"
//! done
//! return 1
//! ```
//!
//! The sleep between attempts (not after the last one) is the standard
//! exponential/fixed-interval backoff shape.

use std::time::Duration;

/// Retry `f` up to `attempts` times, sleeping `sleep_secs` between each
/// unsuccessful try. Returns `Ok(true)` as soon as `f` returns `Ok(true)`,
/// `Ok(false)` if all attempts are exhausted, or the first `Err` `f` returns.
///
/// Pass `sleep_secs = 0` in tests to avoid real waits.
///
/// # Errors
///
/// Propagates the first `Err` returned by `f` immediately without further
/// retries. If the caller wants to treat errors as "not ready" instead,
/// it should map them to `Ok(false)` inside the closure.
pub fn retry<F, E>(attempts: u32, sleep_secs: u64, mut f: F) -> Result<bool, E>
where
    F: FnMut() -> Result<bool, E>,
{
    for i in 0..attempts {
        if f()? {
            return Ok(true);
        }
        if i + 1 < attempts && sleep_secs > 0 {
            std::thread::sleep(Duration::from_secs(sleep_secs));
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_returns_true_on_immediate_success() {
        let result = retry::<_, ()>(3, 0, || Ok(true));
        assert_eq!(result, Ok(true));
    }

    #[test]
    fn retry_returns_false_when_always_not_ready() {
        let result = retry::<_, ()>(3, 0, || Ok(false));
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn retry_succeeds_after_initial_failures() {
        let mut count = 0u32;
        let result = retry::<_, ()>(5, 0, || {
            count += 1;
            Ok(count >= 3)
        });
        assert_eq!(result, Ok(true));
        assert_eq!(
            count, 3,
            "should have tried exactly 3 times before succeeding"
        );
    }

    #[test]
    fn retry_propagates_error_immediately() {
        let mut calls = 0u32;
        let result = retry(3, 0, || {
            calls += 1;
            Err("boom")
        });
        assert_eq!(result, Err("boom"));
        assert_eq!(calls, 1, "should stop on first error without retrying");
    }

    #[test]
    fn retry_with_zero_attempts_returns_false() {
        let result = retry::<_, ()>(0, 0, || Ok(true));
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn retry_calls_f_exactly_attempts_times_on_all_false() {
        let mut count = 0u32;
        let _ = retry::<_, ()>(4, 0, || {
            count += 1;
            Ok(false)
        });
        assert_eq!(count, 4);
    }
}
