//! Retrying transient Messages API failures.

use std::time::Duration;

use crate::error::ParameterError;

/// The longest `retry-after` the client waits for. A longer one means the
/// limit will not lift within the run, so the error is returned instead.
pub const RETRY_AFTER_LIMIT: Duration = Duration::from_secs(60);

/// How the client retries a transient failure: a `429`, a `5xx` (`529`
/// overloaded included), or a connection that could not be made. Anything
/// else is returned at once.
///
/// A response carrying `retry-after` is retried after that many seconds,
/// up to [`RETRY_AFTER_LIMIT`]; otherwise the delay starts at `base` and
/// doubles each retry, up to `cap`. No jitter: one client retrying a
/// handful of times does not synchronise with anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    retries: u32,
    base: Duration,
    cap: Duration,
}

impl RetryPolicy {
    /// Up to `retries` retries after the first attempt.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroRetryDelay`] when `base` is zero;
    /// [`ParameterError::RetryCapBelowBase`] when `cap` is below `base`.
    pub fn new(retries: u32, base: Duration, cap: Duration) -> Result<Self, ParameterError> {
        if base.is_zero() {
            return Err(ParameterError::ZeroRetryDelay);
        }
        if cap < base {
            return Err(ParameterError::RetryCapBelowBase { base, cap });
        }
        Ok(Self { retries, base, cap })
    }

    /// Never retry: every failure is returned at once.
    #[must_use]
    pub fn none() -> Self {
        Self {
            retries: 0,
            ..Self::default()
        }
    }

    /// Retries after the first attempt.
    #[must_use]
    pub fn retries(&self) -> u32 {
        self.retries
    }

    /// How long to wait before retry number `made + 1`, or `None` when
    /// the retries are spent or the server asked for a longer wait than
    /// [`RETRY_AFTER_LIMIT`].
    pub(crate) fn delay(&self, made: u32, retry_after: Option<Duration>) -> Option<Duration> {
        if made >= self.retries {
            return None;
        }
        match retry_after {
            Some(wait) if wait > RETRY_AFTER_LIMIT => None,
            Some(wait) => Some(wait),
            None => Some(
                2u32.checked_pow(made)
                    .and_then(|factor| self.base.checked_mul(factor))
                    .map_or(self.cap, |d| d.min(self.cap)),
            ),
        }
    }
}

impl Default for RetryPolicy {
    /// Two retries, 0.5 s doubling to an 8 s cap: the official SDKs'
    /// defaults.
    fn default() -> Self {
        Self {
            retries: 2,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(8),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(retries: u32, base_ms: u64, cap_ms: u64) -> RetryPolicy {
        RetryPolicy::new(
            retries,
            Duration::from_millis(base_ms),
            Duration::from_millis(cap_ms),
        )
        .expect("valid policy")
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let p = policy(5, 100, 350);
        let delays: Vec<_> = (0..5).map(|n| p.delay(n, None)).collect();
        let ms = |n| Some(Duration::from_millis(n));
        assert_eq!(delays, [ms(100), ms(200), ms(350), ms(350), ms(350)]);
    }

    #[test]
    fn retries_stop_when_spent() {
        let p = policy(2, 100, 1000);
        assert!(p.delay(1, None).is_some());
        assert_eq!(p.delay(2, None), None);
        assert_eq!(RetryPolicy::none().delay(0, None), None);
    }

    #[test]
    fn retry_after_is_honoured_up_to_the_limit() {
        let p = policy(2, 100, 1000);
        assert_eq!(
            p.delay(0, Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(p.delay(0, Some(RETRY_AFTER_LIMIT)), Some(RETRY_AFTER_LIMIT));
        assert_eq!(
            p.delay(0, Some(RETRY_AFTER_LIMIT + Duration::from_secs(1))),
            None
        );
    }

    #[test]
    fn a_huge_retry_count_saturates_at_the_cap() {
        let p = policy(u32::MAX, 100, 1000);
        assert_eq!(p.delay(40, None), Some(Duration::from_millis(1000)));
    }

    #[test]
    fn rejects_a_zero_base_and_a_cap_below_it() {
        assert_eq!(
            RetryPolicy::new(1, Duration::ZERO, Duration::from_secs(1)),
            Err(ParameterError::ZeroRetryDelay)
        );
        assert!(matches!(
            RetryPolicy::new(1, Duration::from_secs(2), Duration::from_secs(1)),
            Err(ParameterError::RetryCapBelowBase { .. })
        ));
    }
}
