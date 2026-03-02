//! Mockable wall-clock abstraction. See [`Clock`].

/// A source of wall-clock time as milliseconds since the Unix epoch.
///
/// Using a trait instead of calling [`std::time::SystemTime`] directly allows
/// tests to inject a [`ManualClock`] and advance time deterministically.
///
/// [`ManualClock`]: test_support::ManualClock
pub(crate) trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Production clock backed by [`std::time::SystemTime`].
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Clock;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A deterministic clock for tests.
    ///
    /// Starts at the value passed to [`new`] and can be advanced by an
    /// arbitrary number of milliseconds with [`advance`], or set to an
    /// absolute value with [`set`].
    ///
    /// [`new`]: ManualClock::new
    /// [`advance`]: ManualClock::advance
    /// [`set`]: ManualClock::set
    pub(crate) struct ManualClock(AtomicU64);

    impl ManualClock {
        pub(crate) const fn new(initial_ms: u64) -> Self {
            Self(AtomicU64::new(initial_ms))
        }

        /// Advances the clock by `ms` milliseconds.
        pub(crate) fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::Relaxed);
        }

        /// Sets the clock to an absolute epoch-millisecond value.
        #[allow(dead_code)]
        pub(crate) fn set(&self, ms: u64) {
            self.0.store(ms, Ordering::Relaxed);
        }
    }

    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }
}
